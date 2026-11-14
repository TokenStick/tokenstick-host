//! Core library for TokenStick.
//!
//! Everything above this crate (CLI, server, daemon) talks to hardware only
//! through the [`Backend`] trait. Backends:
//!
//! - `mock`: no hardware, fake tokens and telemetry (development, CI)
//! - `pcie-vendor`: RK1828 over PCIe through the Rockchip RKNN3 runtime (Phase 0, planned)
//! - `usb`: TokenStick hardware over USB 3.0 (own board, planned)

use std::time::{Duration, Instant};

pub mod backend;
mod error;
pub mod model_store;
mod types;

pub use backend::{Backend, BackendKind, TokenSink};
pub use error::{Error, Result};
pub use types::*;

/// Open a backend by kind.
pub fn open(kind: BackendKind) -> Result<Box<dyn Backend>> {
    match kind {
        BackendKind::Mock => Ok(Box::new(backend::mock::MockBackend::new())),
        BackendKind::PcieVendor => Err(Error::Unsupported(
            "pcie-vendor backend is not implemented yet (Phase 0d)".into(),
        )),
        BackendKind::Usb => Err(Error::Unsupported(
            "usb backend is not implemented yet (needs TokenStick hardware)".into(),
        )),
    }
}

/// Pick the device for a job.
///
/// An explicit `requested` id must exist. Otherwise prefer a device that
/// already has `model` loaded (no reload), then the first device.
pub fn resolve_device(
    backend: &dyn Backend,
    requested: Option<&str>,
    model: &str,
) -> Result<String> {
    let devices = backend.devices()?;
    if let Some(id) = requested {
        return devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.id.clone())
            .ok_or_else(|| Error::DeviceNotFound(id.into()));
    }
    for d in &devices {
        if backend.loaded_model(&d.id)?.as_deref() == Some(model) {
            return Ok(d.id.clone());
        }
    }
    devices
        .first()
        .map(|d| d.id.clone())
        .ok_or_else(|| Error::DeviceNotFound("no devices available".into()))
}

/// Make sure `model` is resident on `device_id`.
/// Returns the load time when a load was needed, `None` when it was already loaded.
pub fn ensure_loaded(
    backend: &dyn Backend,
    device_id: &str,
    model: &str,
) -> Result<Option<Duration>> {
    if backend.loaded_model(device_id)?.as_deref() == Some(model) {
        return Ok(None);
    }
    if !backend.models()?.iter().any(|m| m.id == model) {
        return Err(Error::ModelNotFound(model.into()));
    }
    let start = Instant::now();
    backend.load_model(device_id, model)?;
    Ok(Some(start.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use backend::mock::MockBackend;

    #[test]
    fn resolve_prefers_loaded_device_and_validates_explicit_id() {
        let b = MockBackend::with_delay(Duration::ZERO);
        assert_eq!(resolve_device(&b, None, "x").unwrap(), "mock0");
        assert!(matches!(
            resolve_device(&b, Some("nope"), "x"),
            Err(Error::DeviceNotFound(_))
        ));
    }

    #[test]
    fn ensure_loaded_loads_once() {
        let b = MockBackend::with_delay(Duration::ZERO);
        let m = "qwen2.5-1.5b-instruct-w4a16";
        assert!(ensure_loaded(&b, "mock0", m).unwrap().is_some());
        assert!(ensure_loaded(&b, "mock0", m).unwrap().is_none());
        assert!(matches!(
            ensure_loaded(&b, "mock0", "missing"),
            Err(Error::ModelNotFound(_))
        ));
    }
}
