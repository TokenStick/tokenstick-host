/// Errors returned by libtokenstick and its backends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, Error>;
