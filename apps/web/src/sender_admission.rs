//! Host-owned, per-route sender admission for public pages and endpoints.
//! Counters are bounded, fixed-window and local to this server process.
use bilibi_agent::Throttle;
use bilibi_module_registry::contributions::SenderBudget;

#[derive(Default)]
pub(crate) struct SenderAdmission {
    minutes: Throttle,
    hours: Throttle,
}

impl SenderAdmission {
    pub(crate) fn allow(&self, key: &str, budget: SenderBudget) -> bool {
        self.minutes.allow(key, budget.per_minute) && self.hours.allow_hour(key, budget.per_hour)
    }
}
