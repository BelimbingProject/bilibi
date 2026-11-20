//! Host-owned, per-route sender admission for public pages and endpoints.
//! Counters are bounded, fixed-window and local to this server process.
use bilibi_agent::Throttle;
use bilibi_module_registry::contributions::SenderBudget;

pub(crate) struct SenderAdmission {
    budget: SenderBudget,
    throttle: Throttle,
}

impl SenderAdmission {
    pub(crate) fn new(budget: SenderBudget) -> Self {
        Self {
            budget,
            throttle: Throttle::default(),
        }
    }

    pub(crate) fn allow(&self, sender: &str) -> bool {
        self.throttle
            .allow_minute_and_hour(sender, self.budget.per_minute, self.budget.per_hour)
    }
}
