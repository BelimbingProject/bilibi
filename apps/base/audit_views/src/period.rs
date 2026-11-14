//! The period an Audit list reads. Both ledgers can be large, so each list reads
//! one period at a time (the last 30 days unless the reader chooses another)
//! and never the tenant's whole ledger. The control is the shared
//! `bilibi_datetime::PeriodFilter`; this is the state a list keeps around it:
//! the URL's `period`, `from` and `to`, and the browser's zone for readers in
//! local mode.
use bilibi_database::{Error, Result};
use bilibi_datetime::{
    filter::{Outcome, PeriodFilter},
    DisplayContext, Period,
};
use jiff::Timestamp;
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

const KEYS: [&str; 3] = ["period", "from", "to"];

/// What a period event did to the list.
pub(crate) enum Changed {
    /// Another period applies: read the list again from its first page.
    Applied,
    /// Only the control changed (the custom range opened or closed).
    Control,
}

pub(crate) struct ListPeriod {
    params: HashMap<String, String>,
    /// What the keys meant at the last read, for the URL.
    applied: Option<Period>,
    pub(crate) filter: PeriodFilter,
    browser_zone: Option<String>,
}

/// The period keys as the URL carries them, bounded; `Period::parse` decides
/// what they mean against the reader's clock.
fn keys(values: &HashMap<String, String>) -> HashMap<String, String> {
    KEYS.iter()
        .filter_map(|key| {
            let value = values.get(*key)?;
            (value.len() <= 32).then(|| ((*key).to_owned(), value.clone()))
        })
        .collect()
}

impl ListPeriod {
    pub(crate) fn new(id: &'static str, query: &HashMap<String, String>) -> Self {
        let params = keys(query);
        // The real clock needs the reader's display policy, read with the
        // list; until then the control carries the UTC reading of the keys.
        let clock = DisplayContext::anonymous().clock(None, Timestamp::now());
        let period = Period::parse(|key| params.get(key).cloned(), clock.today);
        Self {
            params,
            applied: None,
            filter: PeriodFilter::new(id, period, &clock),
            browser_zone: None,
        }
    }

    /// The address changed (Back, a link): read its period keys again.
    pub(crate) fn read(&mut self, query: &HashMap<String, String>) {
        self.params = keys(query);
    }

    /// The `BrowserTimeZone` hook's report, used only in local mode.
    pub(crate) fn browser_timezone(&mut self, zone: Option<&str>) {
        self.browser_zone = zone.filter(|zone| zone.len() <= 64).map(str::to_owned);
    }

    /// The period as `[since, until)` on the reader's calendar. A range that
    /// cannot be resolved lists nothing and says why.
    pub(crate) fn resolve(
        &mut self,
        display: &DisplayContext,
    ) -> (SystemTime, SystemTime, Option<&'static str>) {
        let clock = display.clock(self.browser_zone.as_deref(), Timestamp::now());
        let period = Period::parse(|key| self.params.get(key).cloned(), clock.today);
        self.applied = Some(period);
        self.filter.set(period, &clock);
        match period.bounds(&clock.timezone) {
            Ok((since, until)) => (since.into(), until.into(), None),
            Err(error) => (UNIX_EPOCH, UNIX_EPOCH, Some(error)),
        }
    }

    /// The period's URL keys, beside the list's own.
    pub(crate) fn url(&self) -> Vec<(String, String)> {
        self.applied
            .iter()
            .flat_map(Period::params)
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    /// Hands a live event to the control; `None` when it is not the control's.
    pub(crate) fn event(
        &mut self,
        kind: &str,
        payload: &Value,
        display: &DisplayContext,
    ) -> Result<Option<Changed>> {
        let clock = display.clock(self.browser_zone.as_deref(), Timestamp::now());
        Ok(
            match self
                .filter
                .event_checked(kind, payload, &clock)
                .map_err(Error::refused)?
            {
                Some(Outcome::Applied(period)) => {
                    self.params = period
                        .params()
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), value))
                        .collect();
                    Some(Changed::Applied)
                }
                Some(Outcome::Update) => Some(Changed::Control),
                None => None,
            },
        )
    }
}
