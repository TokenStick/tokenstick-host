/// Errors returned by libtokenstick and its backends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("model '{model}' is not loaded on device '{device}'")]
    ModelNotLoaded { device: String, model: String },

    /// The device is already running a job. Callers must serialize work per device.
    #[error("device busy: {0}")]
    Busy(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, Error>;
