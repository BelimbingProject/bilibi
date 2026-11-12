//! Per-node fixed-window limiter for the public registration, device and token endpoints,
//! driven by `agent_access.oauth_requests_per_minute`. Keyed by endpoint and
//! sender address; memory is bounded; nodes do not share counts.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_KEYS: usize = 4096;

#[derive(Default)]
pub struct Throttle {
    windows: Mutex<HashMap<String, (u64, u32)>>,
}
impl Throttle {
    /// True when this request fits the current minute for `key`.
    pub fn allow(&self, key: &str, per_minute: u32) -> bool {
        self.allow_among(key, per_minute, MAX_KEYS)
    }
    /// The same, for a caller whose keys are bounded by something larger
    /// than the default: at most `max_keys` are counted in one minute, and a
    /// new key beyond that is refused.
    pub fn allow_among(&self, key: &str, per_minute: u32, max_keys: usize) -> bool {
        let minute = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0);
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        if windows.len() >= max_keys && !windows.contains_key(key) {
            windows.retain(|_, (window, _)| *window == minute);
            if windows.len() >= max_keys {
                return false;
            }
        }
        let entry = windows.entry(key.to_owned()).or_insert((minute, 0));
        if entry.0 != minute {
            *entry = (minute, 0);
        }
        if entry.1 >= per_minute {
            return false;
        }
        entry.1 += 1;
        true
    }
}
