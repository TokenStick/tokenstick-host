//! HTTP server on top of a libtokenstick [`Backend`].
//!
//! - OpenAI-compatible API: `GET /v1/models`, `POST /v1/chat/completions`
//!   (with `stream: true` served as server-sent events)
//! - Management API for the UI: `/api/v1/*` (see [`api`])
//! - Embedded web UI at `/` (see [`ui`])
//! - `GET /health`

pub mod api;
pub mod monitor;
pub mod ui;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
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

use crate::monitor::Monitor;

/// Header a caller can set to label its requests on the dashboard.
pub const CLIENT_HEADER: &str = "x-tokenstick-client";

#[derive(Clone)]
pub struct AppState {
    backend: Arc<dyn Backend>,
    next_id: Arc<AtomicU64>,
    monitor: Arc<Monitor>,
    started: Instant,
}

impl AppState {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            next_id: Arc::new(AtomicU64::new(1)),
            monitor: Arc::new(Monitor::default()),
            started: Instant::now(),
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
        .merge(api::routes())
        .merge(ui::routes())
        .with_state(state)
}

/// Serve until Ctrl-C.
pub async fn serve(backend: Arc<dyn Backend>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, backend = backend.name(), "tokenstick server listening");
    tracing::info!("web UI: http://{addr}/");
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
pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);

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

fn client_label(headers: &HeaderMap) -> String {
    headers
        .get(CLIENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().filter(|c| !c.is_control()).take(64).collect())
        .filter(|s: &String| !s.is_empty())
        .unwrap_or_else(|| "api".into())
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    let stream = req.stream;
    let client = client_label(&headers);
    let gen_req = to_generation_request(req)?;
    if stream {
        Ok(stream_completion(state, gen_req, client).into_response())
    } else {
        Ok(full_completion(state, gen_req, client)
            .await?
            .into_response())
    }
}

async fn full_completion(
    state: AppState,
    req: GenerationRequest,
    client: String,
) -> Result<Json<Value>, ApiError> {
    let backend = state.backend.clone();
    let monitor = state.monitor.clone();
    let model = req.model.clone();
    let (text, stats) = tokio::task::spawn_blocking(move || {
        let _active = monitor.begin();
        let mut text = String::new();
        let stats = backend.generate(&req, &mut |t| {
            text.push_str(t);
            ControlFlow::Continue(())
        })?;
        monitor.record(&req.model, &client, false, &stats);
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
    client: String,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let id = state.completion_id();
    let created = unix_now();
    let model = req.model.clone();
    let (tx, rx) = mpsc::channel::<Event>(64);
    let backend = state.backend.clone();
    let monitor = state.monitor.clone();

    tokio::task::spawn_blocking(move || {
        let _active = monitor.begin();
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
                monitor.record(&model, &client, true, &stats);
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

    async fn get_json(app: Router, path: &str) -> Value {
        let resp = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        serde_json::from_str(&body_string(resp).await).unwrap()
    }

    #[tokio::test]
    async fn info_reports_backend() {
        let v = get_json(app(), "/api/v1/info").await;
        assert_eq!(v["backend"], "mock");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn devices_include_telemetry() {
        let v = get_json(app(), "/api/v1/devices").await;
        let dev = &v[0];
        assert_eq!(dev["id"], "mock0");
        assert!(dev["telemetry"]["temperature_c"].as_f64().is_some());
    }

    #[tokio::test]
    async fn completions_show_up_in_stats_with_client_label() {
        let app = app();
        let req = Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .header(CLIENT_HEADER, "playground")
            .body(Body::from(
                json!({
                    "model": "qwen2.5-1.5b-instruct-w4a16",
                    "messages": [{ "role": "user", "content": "ping" }],
                    "max_tokens": 3
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let v = get_json(app, "/api/v1/stats").await;
        assert_eq!(v["total_requests"], 1);
        assert_eq!(v["active_requests"], 0);
        assert_eq!(v["recent"][0]["client"], "playground");
        assert_eq!(v["recent"][0]["completion_tokens"], 3);
        assert_eq!(v["recent"][0]["finish_reason"], "length");
    }

    #[tokio::test]
    async fn streamed_completion_is_recorded() {
        let app = app();
        let resp = app
            .clone()
            .oneshot(chat(json!({
                "model": "qwen2.5-1.5b-instruct-w4a16",
                "messages": [{ "role": "user", "content": "ping" }],
                "stream": true,
                "max_tokens": 2
            })))
            .await
            .unwrap();
        // Drain the stream so generation finishes.
        body_string(resp).await;
        let v = get_json(app, "/api/v1/stats").await;
        assert_eq!(v["recent"][0]["stream"], true);
        assert_eq!(v["recent"][0]["client"], "api");
    }

    #[tokio::test]
    async fn serves_ui() {
        for (path, ty) in [
            ("/", "text/html"),
            ("/ui/app.js", "text/javascript"),
            ("/ui/app.css", "text/css"),
            ("/ui/logo.svg", "image/svg+xml"),
        ] {
            let resp = app()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            let ct = resp.headers()[axum::http::header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .to_string();
            assert!(ct.starts_with(ty), "{path}: {ct}");
        }
    }

    #[tokio::test]
    async fn events_stream_sends_snapshot() {
        let resp = app()
            .oneshot(Request::get("/api/v1/events").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(3), body.frame())
            .await
            .expect("snapshot within 3s")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap();
        assert!(text.starts_with("event: snapshot"), "{text}");
        assert!(text.contains("\"devices\""));
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
