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
    DeviceInfo, Error, FinishReason, GenerationRequest, GenerationStats, ModelInfo, PowerMode,
    Result, Role, Telemetry,
};

const DEVICE_ID: &str = "mock0";
const DEFAULT_TOKEN_DELAY: Duration = Duration::from_millis(25);
const DEFAULT_LOAD_DELAY: Duration = Duration::from_millis(300);

const IDLE_TEMP_C: f32 = 42.0;
const LOAD_TEMP_C: f32 = 63.0;
/// Thermal time constant: how fast the simulated die approaches its target.
const THERMAL_TAU_S: f32 = 25.0;
const IDLE_POWER_W: f32 = 2.5;

/// First-order thermal model so dashboards see the stick warm up under load
/// and cool down when idle.
struct Thermal {
    temp_c: f32,
    updated: Instant,
}

pub struct MockBackend {
    token_delay: Duration,
    load_delay: Duration,
    loaded: Mutex<Option<String>>,
    power: Mutex<PowerMode>,
    busy: AtomicBool,
    thermal: Mutex<Thermal>,
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
            power: Mutex::new(PowerMode::Balanced),
            busy: AtomicBool::new(false),
            thermal: Mutex::new(Thermal {
                temp_c: IDLE_TEMP_C,
                updated: Instant::now(),
            }),
            tokens_generated: AtomicU64::new(0),
        }
    }

    /// Total tokens generated since the backend was opened.
    pub fn tokens_generated(&self) -> u64 {
        self.tokens_generated.load(Ordering::Relaxed)
    }

    /// Advance the thermal model to now, using the load state of the elapsed interval.
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

    fn load_power_w(&self) -> f32 {
        match *self.power.lock().expect("mock state poisoned") {
            PowerMode::LowPower => 4.0,
            PowerMode::Balanced => 6.8,
            PowerMode::Performance => 9.5,
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

/// Qwen2.5 family: id, params (B), hidden size. Vocab is 151,936 for all three.
const MOCK_MODELS: [(&str, f32, u64); 3] = [
    ("qwen2.5-1.5b-instruct-w4a16", 1.5, 1536),
    ("qwen2.5-3b-instruct-w4a16", 3.0, 2048),
    ("qwen2.5-7b-instruct-w4a16", 7.0, 3584),
];

fn mock_models() -> Vec<ModelInfo> {
    MOCK_MODELS
        .into_iter()
        .map(|(id, params_b, hidden)| ModelInfo {
            id: id.into(),
            family: "qwen2.5".into(),
            params_b,
            quantization: "w4a16".into(),
            context_len: 4096,
            // FP16 embedding table kept on the host: vocab x hidden x 2 bytes.
            host_mem_mib: Some((151_936 * hidden * 2 / (1024 * 1024)) as u32),
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
        let busy = self.busy.load(Ordering::Relaxed);
        let temp = self.step_thermal(busy);
        Ok(Telemetry {
            temperature_c: Some((temp * 10.0).round() / 10.0),
            power_w: Some(if busy {
                self.load_power_w()
            } else {
                IDLE_POWER_W
            }),
            npu_load_pct: Some(if busy { 85.0 } else { 0.0 }),
            // Simulates the TokenStick PD sink contract (9 V / 2 A = 18 W budget).
            pd_contract: Some("9V/2A".into()),
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

    fn power_mode(&self, device_id: &str) -> Result<PowerMode> {
        Self::check_device(device_id)?;
        Ok(*self.power.lock().expect("mock state poisoned"))
    }

    fn set_power_mode(&self, device_id: &str, mode: PowerMode) -> Result<()> {
        Self::check_device(device_id)?;
        *self.power.lock().expect("mock state poisoned") = mode;
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
        // Bring the thermal model up to now at idle before the job starts.
        self.step_thermal(false);
        let _busy = BusyGuard::acquire(&self.busy)?;

        let prompt_tokens = req.messages.iter().map(|m| approx_tokens(&m.content)).sum();
        let reply = Self::reply_for(req);
        // Rough shape of the real modes: low power is slower, performance faster.
        let token_delay = match *self.power.lock().expect("mock state poisoned") {
            PowerMode::LowPower => self.token_delay * 2,
            PowerMode::Balanced => self.token_delay,
            PowerMode::Performance => self.token_delay * 7 / 10,
        };

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
            std::thread::sleep(token_delay);
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
    fn telemetry_reflects_load() {
        let b = backend();
        let idle = b.telemetry(DEVICE_ID).unwrap();
        assert_eq!(idle.npu_load_pct, Some(0.0));
        assert_eq!(idle.power_w, Some(IDLE_POWER_W));
        let mut busy = None;
        b.generate(DEVICE_ID, &request(2), &mut |_| {
            busy.get_or_insert_with(|| b.telemetry(DEVICE_ID).unwrap());
            ControlFlow::Continue(())
        })
        .unwrap();
        let busy = busy.unwrap();
        assert!(busy.npu_load_pct.unwrap() > 0.0);
        assert_eq!(busy.power_w, Some(6.8));
        assert_eq!(b.telemetry(DEVICE_ID).unwrap().npu_load_pct, Some(0.0));
    }

    #[test]
    fn power_mode_round_trip_and_host_mem() {
        let b = backend();
        assert_eq!(b.power_mode(DEVICE_ID).unwrap(), PowerMode::Balanced);
        b.set_power_mode(DEVICE_ID, PowerMode::Performance).unwrap();
        assert_eq!(b.power_mode(DEVICE_ID).unwrap(), PowerMode::Performance);
        let seven_b = b.models().unwrap().into_iter().last().unwrap();
        assert_eq!(seven_b.host_mem_mib, Some(1038));
        assert_eq!("2".parse::<PowerMode>().unwrap(), PowerMode::Performance);
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
