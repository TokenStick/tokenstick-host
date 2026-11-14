//! Request bookkeeping for the management API and the web UI.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use libtokenstick::GenerationStats;
use serde::Serialize;

/// How many finished requests are kept for the dashboard.
pub const RECENT_CAPACITY: usize = 50;

/// One finished generation.
#[derive(Debug, Clone, Serialize)]
pub struct RequestRecord {
    /// Unix time in milliseconds when the request finished.
    pub finished_at_ms: u64,
    pub model: String,
    /// Caller label from the `x-tokenstick-client` header, else `api`.
    pub client: String,
    pub stream: bool,
    #[serde(flatten)]
    pub stats: GenerationStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatsSnapshot {
    pub active_requests: u32,
    pub total_requests: u64,
    pub total_completion_tokens: u64,
    /// Newest first.
    pub recent: Vec<RequestRecord>,
}

#[derive(Default)]
pub struct Monitor {
    active: AtomicU32,
    total_requests: AtomicU64,
    total_completion_tokens: AtomicU64,
    recent: Mutex<VecDeque<RequestRecord>>,
}

/// Held for the lifetime of a generation; keeps `active_requests` accurate
/// even when a request fails or the client disconnects.
pub struct ActiveRequest<'a>(&'a Monitor);

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Monitor {
    pub fn begin(&self) -> ActiveRequest<'_> {
        self.active.fetch_add(1, Ordering::Relaxed);
        ActiveRequest(self)
    }

    pub fn record(&self, model: &str, client: &str, stream: bool, stats: &GenerationStats) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.total_completion_tokens
            .fetch_add(u64::from(stats.completion_tokens), Ordering::Relaxed);
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        if recent.len() == RECENT_CAPACITY {
            recent.pop_back();
        }
        recent.push_front(RequestRecord {
            finished_at_ms: unix_ms(),
            model: model.to_string(),
            client: client.to_string(),
            stream,
            stats: stats.clone(),
        });
    }

    pub fn snapshot(&self, limit: usize) -> StatsSnapshot {
        let recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        StatsSnapshot {
            active_requests: self.active.load(Ordering::Relaxed),
            total_requests: self.total_requests.load(Ordering::Relaxed),
            total_completion_tokens: self.total_completion_tokens.load(Ordering::Relaxed),
            recent: recent.iter().take(limit).cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libtokenstick::FinishReason;

    fn stats(tokens: u32) -> GenerationStats {
        GenerationStats {
            prompt_tokens: 1,
            completion_tokens: tokens,
            ttft_ms: 1.0,
            tokens_per_s: 10.0,
            finish_reason: FinishReason::Stop,
        }
    }

    #[test]
    fn tracks_active_and_recent() {
        let m = Monitor::default();
        {
            let _a = m.begin();
            assert_eq!(m.snapshot(10).active_requests, 1);
            m.record("m", "api", false, &stats(5));
        }
        let s = m.snapshot(10);
        assert_eq!(s.active_requests, 0);
        assert_eq!(s.total_requests, 1);
        assert_eq!(s.total_completion_tokens, 5);
        assert_eq!(s.recent[0].model, "m");
    }

    #[test]
    fn recent_is_bounded_and_newest_first() {
        let m = Monitor::default();
        for i in 0..(RECENT_CAPACITY as u32 + 5) {
            m.record("m", "api", false, &stats(i));
        }
        let s = m.snapshot(usize::MAX);
        assert_eq!(s.recent.len(), RECENT_CAPACITY);
        assert_eq!(
            s.recent[0].stats.completion_tokens,
            RECENT_CAPACITY as u32 + 4
        );
    }
}
