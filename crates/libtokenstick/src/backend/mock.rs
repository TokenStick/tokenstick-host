//! Hardware-free backend for development and CI.
//!
//! Streams a deterministic reply word by word with a configurable per-token
//! delay, and reports telemetry that moves with load, so the CLI, server and
//! benchmark harness can be built before the kit arrives.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{Backend, TokenSink};
use crate::{
    DeviceInfo, Error, FinishReason, GenerationRequest, GenerationStats, ModelInfo, Result, Role,
    Telemetry,
};

const DEVICE_ID: &str = "mock0";
const DEFAULT_DELAY: Duration = Duration::from_millis(25);

const IDLE_TEMP_C: f32 = 42.0;
const LOAD_TEMP_C: f32 = 63.0;
/// Thermal time constant: how fast the simulated die approaches its target.
const THERMAL_TAU_S: f32 = 25.0;
const IDLE_POWER_W: f32 = 2.5;
const LOAD_POWER_W: f32 = 6.8;

/// First-order thermal model so dashboards see the stick warm up under load
/// and cool down when idle.
struct Thermal {
    temp_c: f32,
    updated: Instant,
}

pub struct MockBackend {
    token_delay: Duration,
    tokens_generated: AtomicU64,
    active: AtomicU32,
    thermal: Mutex<Thermal>,
}

/// Decrements the active-generation counter when a generation ends, even on
/// early return.
struct ActiveGuard<'a>(&'a AtomicU32);

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl MockBackend {
    /// Per-token delay defaults to 25 ms and can be overridden with
    /// `TOKENSTICK_MOCK_DELAY_MS`.
    pub fn new() -> Self {
        let delay = std::env::var("TOKENSTICK_MOCK_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_DELAY);
        Self::with_delay(delay)
    }

    pub fn with_delay(token_delay: Duration) -> Self {
        Self {
            token_delay,
            tokens_generated: AtomicU64::new(0),
            active: AtomicU32::new(0),
            thermal: Mutex::new(Thermal {
                temp_c: IDLE_TEMP_C,
                updated: Instant::now(),
            }),
        }
    }

    /// Total tokens generated since the backend was opened.
    pub fn tokens_generated(&self) -> u64 {
        self.tokens_generated.load(Ordering::Relaxed)
    }

    fn step_thermal(&self, busy: bool) -> f32 {
        let mut th = self.thermal.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let dt = now.duration_since(th.updated).as_secs_f32();
        th.updated = now;
        let target = if busy { LOAD_TEMP_C } else { IDLE_TEMP_C };
        let alpha = 1.0 - (-dt / THERMAL_TAU_S).exp();
        th.temp_c += (target - th.temp_c) * alpha;
        th.temp_c
    }

    fn reply_for(req: &GenerationRequest) -> String {
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.trim())
            .unwrap_or("");
        format!(
            "[mock:{}] You said: \"{}\". This reply comes from the TokenStick mock backend, \
             which streams fake tokens so the CLI, server and benchmarks can be developed \
             without hardware.",
            req.model, last_user
        )
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn mock_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "qwen2.5-1.5b-instruct-w4a16".into(),
            family: "qwen2.5".into(),
            params_b: 1.5,
            quantization: "w4a16".into(),
            context_len: 4096,
        },
        ModelInfo {
            id: "qwen2.5-3b-instruct-w4a16".into(),
            family: "qwen2.5".into(),
            params_b: 3.0,
            quantization: "w4a16".into(),
            context_len: 4096,
        },
        ModelInfo {
            id: "qwen2.5-7b-instruct-w4a16".into(),
            family: "qwen2.5".into(),
            params_b: 7.0,
            quantization: "w4a16".into(),
            context_len: 4096,
        },
    ]
}

fn approx_tokens(text: &str) -> u32 {
    text.split_whitespace().count() as u32
}

impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(vec![DeviceInfo {
            id: DEVICE_ID.into(),
            backend: "mock".into(),
            chip: "RK1828 (simulated)".into(),
            memory_mib: 5 * 1024,
            firmware: Some("mock-0.1.0".into()),
        }])
    }

    fn telemetry(&self, device_id: &str) -> Result<Telemetry> {
        if device_id != DEVICE_ID {
            return Err(Error::DeviceNotFound(device_id.into()));
        }
        let active = self.active.load(Ordering::Relaxed);
        let busy = active > 0;
        let temp = self.step_thermal(busy);
        let (power, load) = if busy {
            (LOAD_POWER_W, (70.0 + 12.0 * active as f32).min(100.0))
        } else {
            (IDLE_POWER_W, 0.0)
        };
        Ok(Telemetry {
            temperature_c: Some((temp * 10.0).round() / 10.0),
            power_w: Some(power),
            npu_load_pct: Some(load),
            pd_contract: Some("9V/2A".into()),
        })
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(mock_models())
    }

    fn generate(
        &self,
        req: &GenerationRequest,
        sink: &mut TokenSink<'_>,
    ) -> Result<GenerationStats> {
        if !mock_models().iter().any(|m| m.id == req.model) {
            return Err(Error::ModelNotFound(req.model.clone()));
        }
        if req.messages.is_empty() {
            return Err(Error::InvalidRequest("messages must not be empty".into()));
        }

        // Warm the thermal model up to "now" at the previous load before going busy.
        self.step_thermal(self.active.load(Ordering::Relaxed) > 0);
        self.active.fetch_add(1, Ordering::Relaxed);
        let _active = ActiveGuard(&self.active);

        let prompt_tokens = req.messages.iter().map(|m| approx_tokens(&m.content)).sum();
        let reply = Self::reply_for(req);

        let start = Instant::now();
        let mut first_token_at = None;
        let mut emitted = 0u32;
        let mut text = String::new();
        let mut finish = FinishReason::Stop;

        for piece in reply.split_inclusive(' ') {
            if emitted >= req.params.max_tokens {
                finish = FinishReason::Length;
                break;
            }
            std::thread::sleep(self.token_delay);
            text.push_str(piece);
            if req
                .params
                .stop
                .iter()
                .any(|s| !s.is_empty() && text.contains(s.as_str()))
            {
                finish = FinishReason::Stop;
                break;
            }
            first_token_at.get_or_insert_with(Instant::now);
            emitted += 1;
            self.tokens_generated.fetch_add(1, Ordering::Relaxed);
            if let ControlFlow::Break(()) = sink(piece) {
                finish = FinishReason::Cancelled;
                break;
            }
        }

        let first = first_token_at.unwrap_or_else(Instant::now);
        let ttft_ms = first.duration_since(start).as_secs_f64() * 1000.0;
        let decode_s = first.elapsed().as_secs_f64();
        let tokens_per_s = if emitted > 1 && decode_s > 0.0 {
            f64::from(emitted - 1) / decode_s
        } else {
            0.0
        };

        Ok(GenerationStats {
            prompt_tokens,
            completion_tokens: emitted,
            ttft_ms,
            tokens_per_s,
            finish_reason: finish,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GenerationParams, Message};

    fn request(max_tokens: u32) -> GenerationRequest {
        GenerationRequest {
            model: "qwen2.5-1.5b-instruct-w4a16".into(),
            messages: vec![Message::new(Role::User, "hello stick")],
            params: GenerationParams {
                max_tokens,
                ..Default::default()
            },
        }
    }

    #[test]
    fn streams_reply_containing_prompt() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let mut out = String::new();
        let stats = backend
            .generate(&request(256), &mut |t| {
                out.push_str(t);
                ControlFlow::Continue(())
            })
            .unwrap();
        assert!(out.contains("hello stick"));
        assert_eq!(stats.finish_reason, FinishReason::Stop);
        assert!(stats.completion_tokens > 5);
    }

    #[test]
    fn respects_max_tokens() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let stats = backend
            .generate(&request(3), &mut |_| ControlFlow::Continue(()))
            .unwrap();
        assert_eq!(stats.completion_tokens, 3);
        assert_eq!(stats.finish_reason, FinishReason::Length);
    }

    #[test]
    fn sink_can_cancel() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let stats = backend
            .generate(&request(256), &mut |_| ControlFlow::Break(()))
            .unwrap();
        assert_eq!(stats.completion_tokens, 1);
        assert_eq!(stats.finish_reason, FinishReason::Cancelled);
    }

    #[test]
    fn telemetry_reflects_load() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let idle = backend.telemetry(DEVICE_ID).unwrap();
        assert_eq!(idle.npu_load_pct, Some(0.0));
        assert_eq!(idle.power_w, Some(IDLE_POWER_W));

        let mut busy = None;
        backend
            .generate(&request(2), &mut |_| {
                busy.get_or_insert_with(|| backend.telemetry(DEVICE_ID).unwrap());
                ControlFlow::Continue(())
            })
            .unwrap();
        let busy = busy.unwrap();
        assert!(busy.npu_load_pct.unwrap() > 0.0);
        assert_eq!(busy.power_w, Some(LOAD_POWER_W));
        // Back to idle once generation is done.
        assert_eq!(
            backend.telemetry(DEVICE_ID).unwrap().npu_load_pct,
            Some(0.0)
        );
    }

    #[test]
    fn unknown_model_is_rejected() {
        let backend = MockBackend::with_delay(Duration::ZERO);
        let mut req = request(8);
        req.model = "nope".into();
        let err = backend
            .generate(&req, &mut |_| ControlFlow::Continue(()))
            .unwrap_err();
        assert!(matches!(err, Error::ModelNotFound(_)));
    }
}
