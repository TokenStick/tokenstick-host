//! Hardware-free backend for development and CI.
//!
//! Streams a deterministic reply word by word with a configurable per-token
//! delay, simulates model load time, reports telemetry that moves with load,
//! and returns [`Error::Busy`] if a caller breaks the one-job-per-device rule,
//! so the CLI, server and benchmark harness can be built before the kit arrives.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{Backend, TokenSink};
use crate::{
    DeviceInfo, Error, FinishReason, GenerationRequest, GenerationStats, ModelInfo, Result, Role,
    Telemetry,
};

const DEVICE_ID: &str = "mock0";
const DEFAULT_TOKEN_DELAY: Duration = Duration::from_millis(25);
const DEFAULT_LOAD_DELAY: Duration = Duration::from_millis(300);

pub struct MockBackend {
    token_delay: Duration,
    load_delay: Duration,
    loaded: Mutex<Option<String>>,
    busy: AtomicBool,
    tokens_generated: AtomicU64,
}

/// Marks the device busy for the lifetime of a job.
struct BusyGuard<'a>(&'a AtomicBool);

impl<'a> BusyGuard<'a> {
    fn acquire(flag: &'a AtomicBool) -> Result<Self> {
        if flag.swap(true, Ordering::AcqRel) {
            return Err(Error::Busy(DEVICE_ID.into()));
        }
        Ok(Self(flag))
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn env_ms(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
}

impl MockBackend {
    /// Delays default to 25 ms per token and 300 ms per model load, overridable
    /// with `TOKENSTICK_MOCK_DELAY_MS` and `TOKENSTICK_MOCK_LOAD_MS`.
    pub fn new() -> Self {
        Self::with_delays(
            env_ms("TOKENSTICK_MOCK_DELAY_MS").unwrap_or(DEFAULT_TOKEN_DELAY),
            env_ms("TOKENSTICK_MOCK_LOAD_MS").unwrap_or(DEFAULT_LOAD_DELAY),
        )
    }

    /// Same delay per token; model load takes 10x that.
    pub fn with_delay(token_delay: Duration) -> Self {
        Self::with_delays(token_delay, token_delay * 10)
    }

    pub fn with_delays(token_delay: Duration, load_delay: Duration) -> Self {
        Self {
            token_delay,
            load_delay,
            loaded: Mutex::new(None),
            busy: AtomicBool::new(false),
            tokens_generated: AtomicU64::new(0),
        }
    }

    fn check_device(device_id: &str) -> Result<()> {
        if device_id == DEVICE_ID {
            Ok(())
        } else {
            Err(Error::DeviceNotFound(device_id.into()))
        }
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
    [
        ("qwen2.5-1.5b-instruct-w4a16", 1.5),
        ("qwen2.5-3b-instruct-w4a16", 3.0),
        ("qwen2.5-7b-instruct-w4a16", 7.0),
    ]
    .into_iter()
    .map(|(id, params_b)| ModelInfo {
        id: id.into(),
        family: "qwen2.5".into(),
        params_b,
        quantization: "w4a16".into(),
        context_len: 4096,
    })
    .collect()
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
        Self::check_device(device_id)?;
        // Warm up slowly with generated tokens, capped like a real thermal limit.
        let load = self.tokens_generated.load(Ordering::Relaxed) as f32;
        let busy = self.busy.load(Ordering::Relaxed);
        Ok(Telemetry {
            temperature_c: Some((42.0 + load / 200.0).min(88.0)),
            power_w: Some(if busy { 9.5 } else { 2.5 }),
            npu_load_pct: Some(if busy { 85.0 } else { 0.0 }),
            pd_contract: None,
        })
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(mock_models())
    }

    fn loaded_model(&self, device_id: &str) -> Result<Option<String>> {
        Self::check_device(device_id)?;
        Ok(self.loaded.lock().expect("mock state poisoned").clone())
    }

    fn load_model(&self, device_id: &str, model_id: &str) -> Result<()> {
        Self::check_device(device_id)?;
        if !mock_models().iter().any(|m| m.id == model_id) {
            return Err(Error::ModelNotFound(model_id.into()));
        }
        let _busy = BusyGuard::acquire(&self.busy)?;
        std::thread::sleep(self.load_delay);
        *self.loaded.lock().expect("mock state poisoned") = Some(model_id.into());
        Ok(())
    }

    fn unload_model(&self, device_id: &str) -> Result<()> {
        Self::check_device(device_id)?;
        let _busy = BusyGuard::acquire(&self.busy)?;
        *self.loaded.lock().expect("mock state poisoned") = None;
        Ok(())
    }

    fn generate(
        &self,
        device_id: &str,
        req: &GenerationRequest,
        sink: &mut TokenSink<'_>,
    ) -> Result<GenerationStats> {
        Self::check_device(device_id)?;
        if !mock_models().iter().any(|m| m.id == req.model) {
            return Err(Error::ModelNotFound(req.model.clone()));
        }
        if self.loaded_model(device_id)?.as_deref() != Some(req.model.as_str()) {
            return Err(Error::ModelNotLoaded {
                device: device_id.into(),
                model: req.model.clone(),
            });
        }
        if req.messages.is_empty() {
            return Err(Error::InvalidRequest("messages must not be empty".into()));
        }
        let _busy = BusyGuard::acquire(&self.busy)?;

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

    const MODEL: &str = "qwen2.5-1.5b-instruct-w4a16";

    fn backend() -> MockBackend {
        let b = MockBackend::with_delay(Duration::ZERO);
        b.load_model(DEVICE_ID, MODEL).unwrap();
        b
    }

    fn request(max_tokens: u32) -> GenerationRequest {
        GenerationRequest {
            model: MODEL.into(),
            messages: vec![Message::new(Role::User, "hello stick")],
            params: GenerationParams {
                max_tokens,
                ..Default::default()
            },
        }
    }

    fn run(b: &MockBackend, req: &GenerationRequest) -> Result<GenerationStats> {
        b.generate(DEVICE_ID, req, &mut |_| ControlFlow::Continue(()))
    }

    #[test]
    fn streams_reply_containing_prompt() {
        let b = backend();
        let mut out = String::new();
        let stats = b
            .generate(DEVICE_ID, &request(256), &mut |t| {
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
        let stats = run(&backend(), &request(3)).unwrap();
        assert_eq!(stats.completion_tokens, 3);
        assert_eq!(stats.finish_reason, FinishReason::Length);
    }

    #[test]
    fn sink_can_cancel() {
        let stats = backend()
            .generate(DEVICE_ID, &request(256), &mut |_| ControlFlow::Break(()))
            .unwrap();
        assert_eq!(stats.completion_tokens, 1);
        assert_eq!(stats.finish_reason, FinishReason::Cancelled);
    }

    #[test]
    fn requires_loaded_model() {
        let b = MockBackend::with_delay(Duration::ZERO);
        assert!(matches!(
            run(&b, &request(8)),
            Err(Error::ModelNotLoaded { .. })
        ));
        b.load_model(DEVICE_ID, MODEL).unwrap();
        assert!(run(&b, &request(8)).is_ok());
        b.unload_model(DEVICE_ID).unwrap();
        assert_eq!(b.loaded_model(DEVICE_ID).unwrap(), None);
    }

    #[test]
    fn rejects_concurrent_jobs() {
        let b = backend();
        let mut nested = None;
        b.generate(DEVICE_ID, &request(2), &mut |_| {
            nested.get_or_insert_with(|| run(&b, &request(1)).map(|_| ()));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert!(matches!(nested, Some(Err(Error::Busy(_)))));
    }

    #[test]
    fn unknown_model_and_device_are_rejected() {
        let b = backend();
        let mut req = request(8);
        req.model = "nope".into();
        assert!(matches!(run(&b, &req), Err(Error::ModelNotFound(_))));
        assert!(matches!(b.telemetry("x"), Err(Error::DeviceNotFound(_))));
    }
}
