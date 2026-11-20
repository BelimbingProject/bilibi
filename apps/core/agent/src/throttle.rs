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
    windows: Mutex<HashMap<String, Counts>>,
}

struct Counts {
    minute: u64,
    in_minute: u32,
    in_hour: u32,
}

impl Throttle {
    /// True when this request fits the current minute for `key`.
    pub fn allow(&self, key: &str, per_minute: u32) -> bool {
        self.allow_among(key, per_minute, MAX_KEYS)
    }
    /// True when this request fits both the current minute and hour for `key`.
    pub fn allow_minute_and_hour(&self, key: &str, per_minute: u32, per_hour: u32) -> bool {
        self.allow_in_windows(key, per_minute, Some(per_hour), MAX_KEYS)
    }
    /// The same, for a caller whose keys are bounded by something larger
    /// than the default: at most `max_keys` are counted in one minute, and a
    /// new key beyond that is refused.
    pub fn allow_among(&self, key: &str, per_minute: u32, max_keys: usize) -> bool {
        self.allow_in_windows(key, per_minute, None, max_keys)
    }

    fn allow_in_windows(
        &self,
        key: &str,
        per_minute: u32,
        per_hour: Option<u32>,
        max_keys: usize,
    ) -> bool {
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let minute = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0);
        let hour = minute / 60;
        if windows.len() >= max_keys && !windows.contains_key(key) {
            windows.retain(|_, counts| match per_hour {
                Some(_) => counts.minute / 60 == hour,
                None => counts.minute == minute,
            });
            if windows.len() >= max_keys {
                return false;
            }
        }
        let counts = windows.entry(key.to_owned()).or_insert(Counts {
            minute,
            in_minute: 0,
            in_hour: 0,
        });
        if counts.minute / 60 != hour {
            counts.in_hour = 0;
        }
        if counts.minute != minute {
            counts.minute = minute;
            counts.in_minute = 0;
        }
        if counts.in_minute >= per_minute || per_hour.is_some_and(|limit| counts.in_hour >= limit) {
            return false;
        }
        counts.in_minute += 1;
        if per_hour.is_some() {
            counts.in_hour += 1;
        }
        true
    }
}
