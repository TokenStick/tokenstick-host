//! Management API used by the web UI and the tray app.
//!
//! - `GET /api/v1/info`    backend, version, uptime
//! - `GET /api/v1/devices` devices with telemetry, loaded model, power mode, busy flag
//! - `GET /api/v1/stats`   request counters, recent requests, queue
//! - `GET /api/v1/events`  server-sent events, one `snapshot` per second

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use libtokenstick::{DeviceInfo, PowerMode, Telemetry};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::monitor::StatsSnapshot;
use crate::{ApiError, AppState};

/// Interval between `snapshot` events.
pub const EVENT_INTERVAL: Duration = Duration::from_secs(1);
/// Recent requests included in each `snapshot` event.
const EVENT_RECENT: usize = 20;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/info", get(info))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/events", get(events))
}

#[derive(Serialize)]
struct Info {
    name: &'static str,
    version: &'static str,
    backend: &'static str,
    uptime_s: u64,
    max_pending: usize,
}

async fn info(State(state): State<AppState>) -> Json<Info> {
    Json(Info {
        name: "tokenstick",
        version: env!("CARGO_PKG_VERSION"),
        backend: state.backend.name(),
        uptime_s: state.started.elapsed().as_secs(),
        max_pending: state.max_pending,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    #[serde(flatten)]
    pub info: DeviceInfo,
    pub telemetry: Telemetry,
    pub loaded_model: Option<String>,
    pub power_mode: PowerMode,
    /// A job holds the device right now.
    pub busy: bool,
}

/// Server-side request queue across all devices.
#[derive(Debug, Clone, Serialize)]
pub struct QueueStatus {
    /// Jobs running or waiting for a device.
    pub pending: usize,
    /// Above this, new requests get 503.
    pub max_pending: usize,
}

fn queue_status(state: &AppState) -> QueueStatus {
    QueueStatus {
        pending: state.pending.load(Ordering::Acquire),
        max_pending: state.max_pending,
    }
}

#[derive(Serialize)]
struct StatsResponse {
    #[serde(flatten)]
    stats: StatsSnapshot,
    queue: QueueStatus,
}

/// Read devices, telemetry, loaded model and power mode on the blocking pool (backends block).
pub(crate) async fn read_devices(state: &AppState) -> Result<Vec<DeviceStatus>, ApiError> {
    let backend = state.backend.clone();
    let rows = tokio::task::spawn_blocking(move || {
        backend
            .devices()?
            .into_iter()
            .map(|info| {
                // A device that fails to report telemetry is still listed.
                let telemetry = backend.telemetry(&info.id).unwrap_or_default();
                let loaded_model = backend.loaded_model(&info.id).unwrap_or(None);
                let power_mode = backend.power_mode(&info.id).unwrap_or_default();
                Ok((info, telemetry, loaded_model, power_mode))
            })
            .collect::<libtokenstick::Result<Vec<_>>>()
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(ApiError::from)?;
    Ok(rows
        .into_iter()
        .map(|(info, telemetry, loaded_model, power_mode)| DeviceStatus {
            busy: state.is_busy(&info.id),
            info,
            telemetry,
            loaded_model,
            power_mode,
        })
        .collect())
}

async fn devices(State(state): State<AppState>) -> Result<Json<Vec<DeviceStatus>>, ApiError> {
    Ok(Json(read_devices(&state).await?))
}

async fn stats(State(state): State<AppState>) -> Json<Value> {
    let body = StatsResponse {
        stats: state.monitor.snapshot(usize::MAX),
        queue: queue_status(&state),
    };
    Json(serde_json::to_value(body).unwrap_or_default())
}

#[derive(Serialize)]
struct Snapshot {
    at_ms: u64,
    devices: Vec<DeviceStatus>,
    stats: StatsSnapshot,
    queue: QueueStatus,
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Event>(4);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVENT_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let devices = match read_devices(&state).await {
                Ok(d) => d,
                Err(ApiError(_, msg)) => {
                    tracing::warn!(error = %msg, "telemetry read failed");
                    Vec::new()
                }
            };
            let snap = Snapshot {
                at_ms: crate::monitor::unix_ms(),
                devices,
                stats: state.monitor.snapshot(EVENT_RECENT),
                queue: queue_status(&state),
            };
            let Ok(event) = Event::default().event("snapshot").json_data(&snap) else {
                continue;
            };
            // Receiver dropped: the client went away.
            if tx.send(event).await.is_err() {
                break;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default())
}
