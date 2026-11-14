//! OpenAI-compatible HTTP API on top of a libtokenstick [`Backend`].
//!
//! Endpoints:
//! - `GET /health`
//! - `GET /v1/models`
//! - `POST /v1/chat/completions` (`stream: true` served as server-sent events)
//! - `GET /v1/tokenstick/status` (devices, loaded models, telemetry, queue)
//!
//! A device runs one job at a time, so requests wait in a FIFO queue per
//! device. When more than `max_pending` jobs are running or waiting, new
//! requests get `503` instead of piling up.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use libtokenstick::{
    ensure_loaded, resolve_device, Backend, Error as BackendError, FinishReason, GenerationParams,
    GenerationRequest, GenerationStats, Message, Role, TokenSink,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

mod management;

/// Default limit for running + queued jobs across all devices.
pub const DEFAULT_MAX_PENDING: usize = 8;

#[derive(Clone)]
pub struct AppState {
    backend: Arc<dyn Backend>,
    next_id: Arc<AtomicU64>,
    queues: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    pending: Arc<AtomicUsize>,
    max_pending: usize,
    metrics: Arc<management::Metrics>,
}

impl AppState {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            next_id: Arc::new(AtomicU64::new(1)),
            queues: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(AtomicUsize::new(0)),
            max_pending: DEFAULT_MAX_PENDING,
            metrics: Arc::new(management::Metrics::default()),
        }
    }

    pub fn with_max_pending(mut self, max_pending: usize) -> Self {
        self.max_pending = max_pending;
        self
    }

    fn completion_id(&self) -> String {
        format!("chatcmpl-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// One FIFO queue (a single-permit semaphore) per device, created on first use.
    fn queue(&self, device: &str) -> Arc<Semaphore> {
        self.queues
            .lock()
            .expect("queue map poisoned")
            .entry(device.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone()
    }

    fn is_busy(&self, device: &str) -> bool {
        self.queue(device).available_permits() == 0
    }

    /// Check the model, pick a device and reserve a queue slot.
    fn admit(&self, model: &str) -> Result<Admission, ApiError> {
        if !self.backend.models()?.iter().any(|m| m.id == model) {
            return Err(BackendError::ModelNotFound(model.into()).into());
        }
        let device = resolve_device(self.backend.as_ref(), None, model)?;
        if self.pending.fetch_add(1, Ordering::AcqRel) >= self.max_pending {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "all devices busy ({} jobs pending), retry later",
                    self.max_pending
                ),
            ));
        }
        Ok(Admission {
            queue: self.queue(&device),
            device,
            _pending: PendingGuard(self.pending.clone()),
        })
    }
}

/// Keeps one slot of the pending count until the job finishes or is dropped.
struct PendingGuard(Arc<AtomicUsize>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Admission {
    device: String,
    queue: Arc<Semaphore>,
    _pending: PendingGuard,
}

/// An admitted job that owns its device until dropped.
struct DeviceSlot {
    device: String,
    _permit: OwnedSemaphorePermit,
    _pending: PendingGuard,
}

impl Admission {
    async fn wait_turn(self) -> DeviceSlot {
        let permit = self
            .queue
            .acquire_owned()
            .await
            .expect("device queue semaphore is never closed");
        DeviceSlot {
            device: self.device,
            _permit: permit,
            _pending: self._pending,
        }
    }
}

/// Load the model if needed, then generate. Runs on a blocking thread while
/// `slot` holds the device.
fn run_on_device(
    backend: &dyn Backend,
    slot: &DeviceSlot,
    req: &GenerationRequest,
    sink: &mut TokenSink<'_>,
) -> libtokenstick::Result<GenerationStats> {
    if let Some(took) = ensure_loaded(backend, &slot.device, &req.model)? {
        tracing::info!(device = %slot.device, model = %req.model, ms = took.as_millis() as u64, "model loaded");
    }
    backend.generate(&slot.device, req, sink)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/tokenstick/status", get(status))
        .merge(management::routes())
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
            BackendError::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
            BackendError::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            BackendError::ModelNotLoaded { .. } | BackendError::Backend(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        ApiError(status, e.to_string())
    }
}

fn join_error(e: tokio::task::JoinError) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
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

async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let devices = management::device_snapshot(&state).await?;
    Ok(Json(json!({
        "backend": state.backend.name(),
        "pending": state.pending.load(Ordering::Acquire),
        "max_pending": state.max_pending,
        "devices": devices,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StopField {
    One(String),
    Many(Vec<String>),
}

/// OpenAI allows `content` as a plain string or as an array of typed parts.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {},
    InputAudio {},
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: Option<Content>,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    max_tokens: Option<u32>,
    max_completion_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    stop: Option<StopField>,
}

fn flatten_content(content: Option<Content>) -> Result<String, ApiError> {
    match content {
        None => Ok(String::new()),
        Some(Content::Text(s)) => Ok(s),
        Some(Content::Parts(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                match p {
                    ContentPart::Text { text } => out.push(text),
                    ContentPart::ImageUrl {} | ContentPart::InputAudio {} => {
                        return Err(ApiError(
                            StatusCode::BAD_REQUEST,
                            "image and audio inputs are not supported yet".into(),
                        ))
                    }
                    ContentPart::Other => {
                        return Err(ApiError(
                            StatusCode::BAD_REQUEST,
                            "unsupported content part type".into(),
                        ))
                    }
                }
            }
            Ok(out.join("\n"))
        }
    }
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
            Ok(Message::new(role, flatten_content(m.content)?))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if messages.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "messages must not be empty".into(),
        ));
    }

    let defaults = GenerationParams::default();
    Ok(GenerationRequest {
        model: req.model,
        messages,
        params: GenerationParams {
            max_tokens: req
                .max_completion_tokens
                .or(req.max_tokens)
                .unwrap_or(defaults.max_tokens),
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
    // Admission happens before any response is sent, so an unknown model is a
    // 404 and a full queue is a 503 for streaming requests too.
    let admission = state.admit(&gen_req.model)?;
    if stream {
        Ok(stream_completion(state, gen_req, admission).into_response())
    } else {
        Ok(full_completion(state, gen_req, admission)
            .await?
            .into_response())
    }
}

async fn full_completion(
    state: AppState,
    req: GenerationRequest,
    admission: Admission,
) -> Result<Json<Value>, ApiError> {
    let slot = admission.wait_turn().await;
    let backend = state.backend.clone();
    let model = req.model.clone();
    let (text, stats) = tokio::task::spawn_blocking(move || {
        let mut text = String::new();
        let stats = run_on_device(backend.as_ref(), &slot, &req, &mut |t| {
            text.push_str(t);
            ControlFlow::Continue(())
        })?;
        Ok::<_, BackendError>((text, stats))
    })
    .await
    .map_err(join_error)??;
    state.metrics.record(&model, &stats);

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
    admission: Admission,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let id = state.completion_id();
    let created = unix_now();
    let model = req.model.clone();
    let (tx, rx) = mpsc::channel::<Event>(64);
    let backend = state.backend.clone();
    let metrics = state.metrics.clone();

    tokio::spawn(async move {
        let slot = admission.wait_turn().await;
        if tx.is_closed() {
            return; // client left while queued
        }
        let _ = tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(chunk(
                &id,
                created,
                &model,
                json!({ "role": "assistant" }),
                None,
            ));
            let result = run_on_device(backend.as_ref(), &slot, &req, &mut |t| {
                // A failed send means the client went away: cancel generation.
                match tx.blocking_send(chunk(&id, created, &model, json!({ "content": t }), None)) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(_) => ControlFlow::Break(()),
                }
            });
            match result {
                Ok(stats) => {
                    metrics.record(&model, &stats);
                    let _ = tx.blocking_send(chunk(
                        &id,
                        created,
                        &model,
                        json!({}),
                        Some(finish_str(stats.finish_reason)),
                    ));
                }
                Err(e) => {
                    let body =
                        json!({ "error": { "message": e.to_string(), "type": "server_error" } });
                    let _ = tx.blocking_send(Event::default().data(body.to_string()));
                }
            }
            let _ = tx.blocking_send(Event::default().data("[DONE]"));
        })
        .await;
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

    const MODEL: &str = "qwen2.5-1.5b-instruct-w4a16";

    fn state(token_delay: Duration) -> AppState {
        AppState::new(Arc::new(MockBackend::with_delay(token_delay)))
    }

    fn app() -> Router {
        router(state(Duration::ZERO))
    }

    async fn body_string(resp: Response) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn body_json(resp: Response) -> Value {
        serde_json::from_str(&body_string(resp).await).unwrap()
    }

    fn chat(body: Value) -> Request<Body> {
        Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn simple(stream: bool, max_tokens: u32) -> Value {
        json!({
            "model": MODEL,
            "messages": [{ "role": "user", "content": "ping" }],
            "stream": stream,
            "max_tokens": max_tokens
        })
    }

    #[tokio::test]
    async fn lists_models() {
        let resp = app()
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["object"], "list");
        assert!(!v["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_streaming_completion_loads_model_on_demand() {
        let resp = app().oneshot(chat(simple(false, 64))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let content = v["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(content.contains("ping"));
        assert!(v["usage"]["completion_tokens"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn streaming_completion_ends_with_done() {
        let resp = app().oneshot(chat(simple(true, 4))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("chat.completion.chunk"));
        assert!(body.contains("\"finish_reason\":\"length\""));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn unknown_model_is_404_streaming_or_not() {
        for stream in [false, true] {
            let resp = app()
                .oneshot(chat(json!({
                    "model": "missing",
                    "stream": stream,
                    "messages": [{ "role": "user", "content": "ping" }]
                })))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "stream={stream}");
        }
    }

    #[tokio::test]
    async fn accepts_content_parts_and_rejects_images() {
        let resp = app()
            .oneshot(chat(json!({
                "model": MODEL,
                "messages": [{ "role": "user", "content": [
                    { "type": "text", "text": "part one" },
                    { "type": "text", "text": "part two" }
                ]}]
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("part two"));

        let resp = app()
            .oneshot(chat(json!({
                "model": MODEL,
                "messages": [{ "role": "user", "content": [
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                ]}]
            })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn concurrent_requests_are_serialized_per_device() {
        // The mock returns Busy if two jobs overlap, so all-200 proves queuing.
        let router = router(state(Duration::from_millis(2)));
        let reqs = (0..4).map(|_| router.clone().oneshot(chat(simple(false, 5))));
        for resp in futures_join_all(reqs).await {
            assert_eq!(resp.unwrap().status(), StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn full_queue_returns_503() {
        let router = router(state(Duration::from_millis(50)).with_max_pending(1));
        // A streaming request returns at once and keeps its slot while it runs.
        let first = router.clone().oneshot(chat(simple(true, 4))).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = router
            .clone()
            .oneshot(chat(simple(false, 4)))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(body_string(first).await);
    }

    #[tokio::test]
    async fn status_reports_loaded_model() {
        let st = state(Duration::ZERO);
        let router = router(st);
        let resp = router
            .clone()
            .oneshot(chat(simple(false, 2)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = router
            .oneshot(
                Request::get("/v1/tokenstick/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["backend"], "mock");
        assert_eq!(v["devices"][0]["loaded_model"], MODEL);
        assert_eq!(v["devices"][0]["busy"], false);
        assert_eq!(v["pending"], 0);
    }

    async fn get_json(router: &Router, path: &str) -> Value {
        let resp = router
            .clone()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        body_json(resp).await
    }

    #[tokio::test]
    async fn management_api_tracks_generations() {
        let router = router(state(Duration::ZERO));
        let info = get_json(&router, "/api/v1/info").await;
        assert_eq!(info["backend"], "mock");

        let before = get_json(&router, "/api/v1/stats").await;
        assert_eq!(before["total_requests"], 0);
        assert!(before["last"].is_null());

        for stream in [false, true] {
            let resp = router
                .clone()
                .oneshot(chat(simple(stream, 3)))
                .await
                .unwrap();
            drop(body_string(resp).await);
        }
        let after = get_json(&router, "/api/v1/stats").await;
        assert_eq!(after["total_requests"], 2);
        assert_eq!(after["total_completion_tokens"], 6);
        assert_eq!(after["last"]["model"], MODEL);

        let devices = get_json(&router, "/api/v1/devices").await;
        assert_eq!(devices["devices"][0]["loaded_model"], MODEL);
    }

    #[tokio::test]
    async fn serves_web_ui() {
        let resp = app()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_string(resp)
            .await
            .contains("<title>TokenStick</title>"));
    }

    /// Minimal join_all so tests need no extra dependency.
    async fn futures_join_all<F: std::future::Future + Send + 'static>(
        futs: impl Iterator<Item = F>,
    ) -> Vec<F::Output>
    where
        F::Output: Send + 'static,
    {
        let handles: Vec<_> = futs.map(tokio::spawn).collect();
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }
}
