use serde::{Deserialize, Serialize};

/// A physical (or mock) accelerator visible to a backend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    /// Stable identifier, e.g. `mock0`, `pcie-0000:01:00.0`, USB serial.
    pub id: String,
    /// Backend that owns the device (`mock`, `pcie-vendor`, `usb`).
    pub backend: String,
    /// Chip name, e.g. `RK1828`.
    pub chip: String,
    /// On-device memory in MiB.
    pub memory_mib: u32,
    /// Firmware version loaded on the device, if known.
    pub firmware: Option<String>,
}

/// Point-in-time health readings. Every field is optional because not every
/// backend or board revision exposes every sensor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Telemetry {
    pub temperature_c: Option<f32>,
    pub power_w: Option<f32>,
    pub npu_load_pct: Option<f32>,
    /// USB PD contract, e.g. `15V/3A` (USB backend only).
    pub pd_contract: Option<String>,
}

/// A model that a backend can run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// Model id used in requests, e.g. `qwen2.5-1.5b-instruct-w4a16`.
    pub id: String,
    pub family: String,
    /// Parameter count in billions.
    pub params_b: f32,
    pub quantization: String,
    pub context_len: u32,
    /// Estimated host RAM the model needs while loaded, in MiB (RKNN3 keeps
    /// the embedding table on the host). `None` when unknown.
    #[serde(default)]
    pub host_mem_mib: Option<u32>,
}

/// NPU power profile. Maps to RKNN3 `work_mode` 0 / 1 / 2.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PowerMode {
    LowPower,
    #[default]
    Balanced,
    Performance,
}

impl std::str::FromStr for PowerMode {
    type Err = crate::Error;

    fn from_str(s: &str) -> crate::Result<Self> {
        match s {
            "low-power" | "low" | "0" => Ok(Self::LowPower),
            "balanced" | "1" => Ok(Self::Balanced),
            "performance" | "perf" | "2" => Ok(Self::Performance),
            other => Err(crate::Error::InvalidRequest(format!(
                "unknown power mode '{other}' (expected low-power, balanced or performance)"
            ))),
        }
    }
}

impl std::fmt::Display for PowerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::LowPower => "low-power",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerationParams {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub stop: Vec<String>,
}

impl Default for GenerationParams {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            stop: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerationRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub params: GenerationParams,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Model finished or a stop sequence matched.
    Stop,
    /// `max_tokens` reached.
    Length,
    /// The token sink asked to stop (e.g. client disconnected).
    Cancelled,
}

/// Numbers every generation reports; the benchmark harness is built on these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GenerationStats {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Time to first token in milliseconds.
    pub ttft_ms: f64,
    /// Decode throughput after the first token.
    pub tokens_per_s: f64,
    pub finish_reason: FinishReason,
}
