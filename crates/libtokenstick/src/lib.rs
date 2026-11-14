//! Core library for TokenStick.
//!
//! Everything above this crate (CLI, server, daemon) talks to hardware only
//! through the [`Backend`] trait. Backends:
//!
//! - `mock`: no hardware, fake tokens and telemetry (development, CI)
//! - `pcie-vendor`: RK1828 over PCIe through the Rockchip RKNN3 runtime (Phase 0, planned)
//! - `usb`: TokenStick hardware over USB 3.0 (own board, planned)

pub mod backend;
mod error;
mod types;

pub use backend::{Backend, BackendKind};
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
