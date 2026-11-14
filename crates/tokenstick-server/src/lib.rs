//! OpenAI-compatible HTTP API on top of a libtokenstick [`Backend`].
//!
//! Endpoints: `GET /health`, `GET /v1/models`, `POST /v1/chat/completions`
//! (with `stream: true` served as server-sent events).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use libtokenstick::{
    Backend, Error as BackendError, FinishReason, GenerationParams, GenerationRequest, Message,
    Role,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct AppState {
    backend: Arc<dyn Backend>,
    next_id: Arc<AtomicU64>,
}

impl AppState {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn completion_id(&self) -> String {
        format!("chatcmpl-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

/// Serve until Ctrl-C.
pub async fn serve(backend: Arc<dyn Backend>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, backend = backend.name(), "tokenstick server listening");
    axum::serve(listener, router(AppState::new(backend)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// OpenAI-style error body.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let kind = if self.0.is_client_error() {
            "invalid_request_error"
        } else {
            "server_error"
        };
        (
            self.0,
            Json(json!({ "error": { "message": self.1, "type": kind } })),
        )
            .into_response()
    }
}

impl From<BackendError> for ApiError {
    fn from(e: BackendError) -> Self {
        let status = match e {
            BackendError::ModelNotFound(_) | BackendError::DeviceNotFound(_) => {
                StatusCode::NOT_FOUND
            }
            BackendError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            BackendError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            BackendError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.to_string())
    }
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "status": "ok", "backend": state.backend.name() }))
}

async fn list_models(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let models = state.backend.models()?;
    let data: Vec<Value> = models
        .iter()
        .map(|m| json!({ "id": m.id, "object": "model", "created": 0, "owned_by": "tokenstick" }))
        .collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StopField {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Option<StopField>,
}

fn to_generation_request(req: ChatRequest) -> Result<GenerationRequest, ApiError> {
    let messages = req
        .messages
        .into_iter()
        .map(|m| {
            let role = match m.role.as_str() {
                "system" | "developer" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                other => {
                    return Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        format!("unsupported message role '{other}'"),
                    ))
                }
            };
            Ok(Message::new(role, m.content))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let defaults = GenerationParams::default();
    Ok(GenerationRequest {
        model: req.model,
        messages,
        params: GenerationParams {
            max_tokens: req.max_tokens.unwrap_or(defaults.max_tokens),
            temperature: req.temperature.unwrap_or(defaults.temperature),
            top_p: req.top_p.unwrap_or(defaults.top_p),
            stop: match req.stop {
                None => Vec::new(),
                Some(StopField::One(s)) => vec![s],
                Some(StopField::Many(v)) => v,
            },
        },
    })
}

fn finish_str(f: FinishReason) -> &'static str {
    match f {
        FinishReason::Stop | FinishReason::Cancelled => "stop",
        FinishReason::Length => "length",
    }
}

async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    let stream = req.stream;
    let gen_req = to_generation_request(req)?;
    if stream {
        Ok(stream_completion(state, gen_req).into_response())
    } else {
        Ok(full_completion(state, gen_req).await?.into_response())
    }
}

async fn full_completion(state: AppState, req: GenerationRequest) -> Result<Json<Value>, ApiError> {
    let backend = state.backend.clone();
    let model = req.model.clone();
    let (text, stats) = tokio::task::spawn_blocking(move || {
        let mut text = String::new();
        let stats = backend.generate(&req, &mut |t| {
            text.push_str(t);
            ControlFlow::Continue(())
        })?;
        Ok::<_, BackendError>((text, stats))
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;

    Ok(Json(json!({
        "id": state.completion_id(),
        "object": "chat.completion",
        "created": unix_now(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": finish_str(stats.finish_reason),
        }],
        "usage": {
            "prompt_tokens": stats.prompt_tokens,
            "completion_tokens": stats.completion_tokens,
            "total_tokens": stats.prompt_tokens + stats.completion_tokens,
        },
    })))
}

fn chunk(id: &str, created: u64, model: &str, delta: Value, finish: Option<&str>) -> Event {
    Event::default().data(
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
        .to_string(),
    )
}

fn stream_completion(
    state: AppState,
    req: GenerationRequest,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let id = state.completion_id();
    let created = unix_now();
    let model = req.model.clone();
    let (tx, rx) = mpsc::channel::<Event>(64);
    let backend = state.backend.clone();

    tokio::task::spawn_blocking(move || {
        let _ = tx.blocking_send(chunk(
            &id,
            created,
            &model,
            json!({ "role": "assistant" }),
            None,
        ));
        let result = backend.generate(&req, &mut |t| {
            // A failed send means the client went away: cancel generation.
            match tx.blocking_send(chunk(&id, created, &model, json!({ "content": t }), None)) {
                Ok(()) => ControlFlow::Continue(()),
                Err(_) => ControlFlow::Break(()),
            }
        });
        match result {
            Ok(stats) => {
                let _ = tx.blocking_send(chunk(
                    &id,
                    created,
                    &model,
                    json!({}),
                    Some(finish_str(stats.finish_reason)),
                ));
            }
            Err(e) => {
                let body = json!({ "error": { "message": e.to_string(), "type": "server_error" } });
                let _ = tx.blocking_send(Event::default().data(body.to_string()));
            }
        }
        let _ = tx.blocking_send(Event::default().data("[DONE]"));
    });

    Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use libtokenstick::backend::mock::MockBackend;
    use std::time::Duration;
    use tower::ServiceExt;

    fn app() -> Router {
        router(AppState::new(Arc::new(MockBackend::with_delay(
            Duration::ZERO,
        ))))
    }

    async fn body_string(resp: Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn chat(body: Value) -> Request<Body> {
        Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn lists_models() {
        let resp = app()
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(v["object"], "list");
        assert!(!v["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_streaming_completion() {
        let resp = app()
            .oneshot(chat(json!({
                "model": "qwen2.5-1.5b-instruct-w4a16",
                "messages": [{ "role": "user", "content": "ping" }]
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = serde_json::from_str(&body_string(resp).await).unwrap();
        let content = v["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(content.contains("ping"));
        assert!(v["usage"]["completion_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn streaming_completion_ends_with_done() {
        let resp = app()
            .oneshot(chat(json!({
                "model": "qwen2.5-1.5b-instruct-w4a16",
                "messages": [{ "role": "user", "content": "ping" }],
                "stream": true,
                "max_tokens": 4
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("chat.completion.chunk"));
        assert!(body.contains("\"finish_reason\":\"length\""));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn unknown_model_is_404() {
        let resp = app()
            .oneshot(chat(json!({
                "model": "missing",
                "messages": [{ "role": "user", "content": "ping" }]
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
