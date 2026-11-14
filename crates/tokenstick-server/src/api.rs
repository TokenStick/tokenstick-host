//! Management API used by the web UI and the tray app.
//!
//! - `GET /api/v1/info`    backend, version, uptime
//! - `GET /api/v1/devices` devices with current telemetry
//! - `GET /api/v1/stats`   request counters and recent requests
//! - `GET /api/v1/events`  server-sent events, one `snapshot` per second

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use axum::{Json, Router};
use libtokenstick::{DeviceInfo, Telemetry};
use serde::Serialize;
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
}

async fn info(State(state): State<AppState>) -> Json<Info> {
    Json(Info {
        name: "tokenstick",
        version: env!("CARGO_PKG_VERSION"),
        backend: state.backend.name(),
        uptime_s: state.started.elapsed().as_secs(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    #[serde(flatten)]
    pub info: DeviceInfo,
    pub telemetry: Telemetry,
}

/// Read devices and their telemetry on the blocking pool (backends block).
async fn read_devices(state: &AppState) -> Result<Vec<DeviceStatus>, ApiError> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        backend
            .devices()?
            .into_iter()
            .map(|info| {
                // A device that fails to report telemetry is still listed.
                let telemetry = backend.telemetry(&info.id).unwrap_or_default();
                Ok(DeviceStatus { info, telemetry })
            })
            .collect::<libtokenstick::Result<Vec<_>>>()
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(ApiError::from)
}

async fn devices(State(state): State<AppState>) -> Result<Json<Vec<DeviceStatus>>, ApiError> {
    Ok(Json(read_devices(&state).await?))
}

async fn stats(State(state): State<AppState>) -> Json<StatsSnapshot> {
    Json(state.monitor.snapshot(usize::MAX))
}

#[derive(Serialize)]
struct Snapshot {
    at_ms: u64,
    devices: Vec<DeviceStatus>,
    stats: StatsSnapshot,
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
