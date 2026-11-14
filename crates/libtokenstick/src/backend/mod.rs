use std::fmt;
use std::ops::ControlFlow;
use std::str::FromStr;

use crate::{
    DeviceInfo, Error, GenerationRequest, GenerationStats, ModelInfo, PowerMode, Result, Telemetry,
};

pub mod mock;

/// Callback that receives generated text piece by piece.
/// Return `ControlFlow::Break(())` to cancel the generation.
pub type TokenSink<'a> = dyn FnMut(&str) -> ControlFlow<()> + 'a;

/// The only interface the rest of the stack uses to reach hardware.
///
/// Contract:
/// - Methods are blocking: backends wrap vendor C runtimes and USB transfers
///   that are synchronous. Async callers run them on a blocking thread pool.
/// - A device runs one job at a time. Callers serialize `load_model`,
///   `unload_model` and `generate` per device (the server queues requests).
///   A backend may return [`Error::Busy`] when that rule is broken.
/// - `generate` requires the model to be loaded on that device first
///   (see [`crate::ensure_loaded`]).
pub trait Backend: Send + Sync {
    /// Short backend name, e.g. `mock`.
    fn name(&self) -> &'static str;

    /// Devices currently visible to this backend.
    fn devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Health readings for one device.
    fn telemetry(&self, device_id: &str) -> Result<Telemetry>;

    /// Models this backend can run.
    fn models(&self) -> Result<Vec<ModelInfo>>;

    /// Model currently resident on the device, if any.
    fn loaded_model(&self, device_id: &str) -> Result<Option<String>>;

    /// Load a model onto the device, replacing whatever was loaded.
    /// Slow on real hardware (several seconds for a 7B model).
    fn load_model(&self, device_id: &str, model_id: &str) -> Result<()>;

    /// Free the device memory.
    fn unload_model(&self, device_id: &str) -> Result<()>;

    /// Current NPU power profile. Backends without power control report `Balanced`.
    fn power_mode(&self, device_id: &str) -> Result<PowerMode> {
        let _ = device_id;
        Ok(PowerMode::Balanced)
    }

    /// Change the NPU power profile. Benchmarks must record the mode they ran in.
    fn set_power_mode(&self, device_id: &str, mode: PowerMode) -> Result<()> {
        let _ = (device_id, mode);
        Err(Error::Unsupported(format!(
            "{} backend has no power control",
            self.name()
        )))
    }

    /// Run a chat completion on `device_id`, streaming text into `sink`.
    fn generate(
        &self,
        device_id: &str,
        req: &GenerationRequest,
        sink: &mut TokenSink<'_>,
    ) -> Result<GenerationStats>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Mock,
    PcieVendor,
    Usb,
}

impl FromStr for BackendKind {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "mock" => Ok(Self::Mock),
            "pcie-vendor" | "pcie" => Ok(Self::PcieVendor),
            "usb" => Ok(Self::Usb),
            other => Err(Error::InvalidRequest(format!(
                "unknown backend '{other}' (expected mock, pcie-vendor or usb)"
            ))),
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mock => "mock",
            Self::PcieVendor => "pcie-vendor",
            Self::Usb => "usb",
        })
    }
}
