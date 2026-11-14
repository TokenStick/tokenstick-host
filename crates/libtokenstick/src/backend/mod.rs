use std::fmt;
use std::ops::ControlFlow;
use std::str::FromStr;

use crate::{DeviceInfo, Error, GenerationRequest, GenerationStats, ModelInfo, Result, Telemetry};

pub mod mock;

/// Callback that receives generated text piece by piece.
/// Return `ControlFlow::Break(())` to cancel the generation.
pub type TokenSink<'a> = dyn FnMut(&str) -> ControlFlow<()> + 'a;

/// The only interface the rest of the stack uses to reach hardware.
///
/// Methods are blocking: backends wrap vendor C runtimes and USB transfers that
/// are synchronous. Async callers run them on a blocking thread pool.
pub trait Backend: Send + Sync {
    /// Short backend name, e.g. `mock`.
    fn name(&self) -> &'static str;

    /// Devices currently visible to this backend.
    fn devices(&self) -> Result<Vec<DeviceInfo>>;

    /// Health readings for one device.
    fn telemetry(&self, device_id: &str) -> Result<Telemetry>;

    /// Models this backend can run.
    fn models(&self) -> Result<Vec<ModelInfo>>;

    /// Run a chat completion, streaming text into `sink`.
    fn generate(
        &self,
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
