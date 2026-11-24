//! Per-sender budgets: fixed one-minute and one-hour windows, bounded in
//! memory and local to this server process. A sender is the key the host
//! derives from trusted transport evidence (an IPv4 address or an IPv6 /64);
//! no owner ever sees it.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// What one sender may ask of one public page, link or the whole listener on
/// one server: both limits apply. Boot refuses a zero, and a minute limit
/// above the hour's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SenderBudget {
    pub per_minute: u32,
    pub per_hour: u32,
}
impl SenderBudget {
    pub(crate) fn valid(self) -> bool {
        self.per_minute > 0 && self.per_hour > 0 && self.per_minute <= self.per_hour
    }
}

struct Window {
    minute: u64,
    in_minute: u32,
    in_hour: u32,
}

/// One budget's counters. At most `capacity` senders are tracked: when full,
/// senders from an earlier hour go first, then those not seen this minute
/// (who lose their hour's count, never gain a refusal). Only a minute with
/// more than `capacity` distinct senders refuses a new one.
pub struct Counter {
    capacity: usize,
    windows: Mutex<HashMap<String, Window>>,
}
/// Senders one page or link counts at once.
const ROUTE_SENDERS: usize = 65_536;

impl Counter {
    /// The counters of one page or link.
    pub fn route() -> Self {
        Self::with_capacity(ROUTE_SENDERS)
    }
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            windows: Mutex::default(),
        }
    }
    /// Whether `sender` may make one more request now under `budget`; the
    /// request is counted when it may.
    pub fn allow(&self, sender: &str, budget: SenderBudget) -> bool {
        let minute = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() / 60)
            .unwrap_or(0);
        let hour = minute / 60;
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        if windows.len() >= self.capacity && !windows.contains_key(sender) {
            windows.retain(|_, window| window.minute / 60 == hour);
            if windows.len() >= self.capacity {
                windows.retain(|_, window| window.minute == minute);
            }
            if windows.len() >= self.capacity {
                return false;
            }
        }
        let window = windows.entry(sender.to_owned()).or_insert(Window {
            minute,
            in_minute: 0,
            in_hour: 0,
        });
        if window.minute / 60 != hour {
            window.in_hour = 0;
        }
        if window.minute != minute {
            window.minute = minute;
            window.in_minute = 0;
        }
        if window.in_minute >= budget.per_minute || window.in_hour >= budget.per_hour {
            return false;
        }
        window.in_minute += 1;
        window.in_hour += 1;
        true
    }
}
