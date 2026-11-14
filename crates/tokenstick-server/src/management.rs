//! Management API and embedded web UI.
//!
//! - `GET /`                 web UI (Dashboard, Playground, Server)
//! - `GET /api/v1/info`      backend, version, uptime
//! - `GET /api/v1/devices`   devices with telemetry, loaded model, busy flag
//! - `GET /api/v1/stats`     last generation, totals, pending jobs
//! - `GET /api/v1/events`    SSE: `{devices, stats}` once per second

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::get;
use axum::{Json, Router};
use libtokenstick::GenerationStats;
use serde::Serialize;
use serde_json::{json, Value};
use tokio_stream::StreamExt;

use crate::{join_error, unix_now, ApiError, AppState};

const INDEX_HTML: &str = include_str!("ui/index.html");

#[derive(Debug, Clone, Serialize)]
struct LastGeneration {
    model: String,
    at: u64,
    #[serde(flatten)]
    stats: GenerationStats,
}

#[derive(Debug, Default)]
struct Totals {
    requests: u64,
    completion_tokens: u64,
    last: Option<LastGeneration>,
}

/// Counters shared by all request handlers.
#[derive(Debug)]
pub(crate) struct Metrics {
    started: Instant,
    totals: Mutex<Totals>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            totals: Mutex::new(Totals::default()),
        }
    }
}

impl Metrics {
    pub(crate) fn record(&self, model: &str, stats: &GenerationStats) {
        let mut t = self.totals.lock().expect("metrics poisoned");
        t.requests += 1;
        t.completion_tokens += u64::from(stats.completion_tokens);
        t.last = Some(LastGeneration {
            model: model.to_owned(),
            at: unix_now(),
            stats: stats.clone(),
        });
    }

    fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/api/v1/info", get(info))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/events", get(events))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn info(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": "tokenstick",
        "version": env!("CARGO_PKG_VERSION"),
        "backend": state.backend.name(),
        "uptime_s": state.metrics.uptime_s(),
        "max_pending": state.max_pending,
    }))
}

/// Devices with telemetry, loaded model and queue state.
pub(crate) async fn device_snapshot(state: &AppState) -> Result<Vec<Value>, ApiError> {
    let backend = state.backend.clone();
    let rows = tokio::task::spawn_blocking(move || {
        backend
            .devices()?
            .into_iter()
            .map(|d| {
                let telemetry = backend.telemetry(&d.id)?;
                let loaded = backend.loaded_model(&d.id)?;
                let power = backend.power_mode(&d.id)?;
                Ok((d, telemetry, loaded, power))
            })
            .collect::<libtokenstick::Result<Vec<_>>>()
    })
    .await
    .map_err(join_error)??;

    Ok(rows
        .into_iter()
        .map(|(d, t, loaded, power)| {
            json!({
                "busy": state.is_busy(&d.id),
                "loaded_model": loaded,
                "power_mode": power,
                "telemetry": t,
                "device": d,
            })
        })
        .collect())
}

fn stats_value(state: &AppState) -> Value {
    let t = state.metrics.totals.lock().expect("metrics poisoned");
    json!({
        "pending": state.pending.load(Ordering::Acquire),
        "max_pending": state.max_pending,
        "total_requests": t.requests,
        "total_completion_tokens": t.completion_tokens,
        "last": t.last,
    })
}

async fn devices(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "devices": device_snapshot(&state).await? })))
}

async fn stats(State(state): State<AppState>) -> Json<Value> {
    Json(stats_value(&state))
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let ticks =
        tokio_stream::wrappers::IntervalStream::new(tokio::time::interval(Duration::from_secs(1)));
    let stream = ticks.then(move |_| {
        let state = state.clone();
        async move {
            let devices = device_snapshot(&state).await.unwrap_or_default();
            let body = json!({
                "uptime_s": state.metrics.uptime_s(),
                "devices": devices,
                "stats": stats_value(&state),
            });
            Ok(Event::default().event("tick").data(body.to_string()))
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
