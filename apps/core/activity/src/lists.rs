//! The facade's list reads: a page of activities, the activities on one
//! record, and the per-day counts of a year. All three answer under the
//! visibility rule in `store` (README, Who sees an activity).
use crate::{store, Activities, Filter, Page, Reference, State};
use bilibi_database::{Error, Result};
use bilibi_tenancy::{ReadClient, Scope};

/// One day of [`Activities::day_counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DayCount {
    /// The calls, meetings and dated tasks on the day.
    pub count: i64,
    /// Those of them still open whose start or due time has passed.
    pub overdue: i64,
}

impl Activities {
    /// `filter` with its related record checked for the reader: the resolved
    /// reference, and whether the shared activities on it count.
    async fn checked(
        &self,
        client: &dyn ReadClient,
        scope: &Scope,
        filter: &Filter,
    ) -> Result<(Filter, bool)> {
        let mut filter = filter.clone();
        let mut readable = false;
        if let Some(related) = &filter.related {
            if let Ok(reference) = self
                .records
                .check(client, scope, &related.kind, &related.id)
                .await?
            {
                filter.related = Some(reference);
                readable = true;
            }
        }
        Ok((filter, readable))
    }
    /// A page of the activities the reader may see. With `related`, shared
    /// activities on that record count when the reader may view it.
    pub async fn list(&self, scope: &Scope, filter: &Filter) -> Result<Page> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let (filter, readable) = self.checked(&**client, &scope, filter).await?;
        store::list(&**client, &scope, &filter, readable).await
    }
    /// How many of the activities `filter` matches fall on each day, one
    /// [`DayCount`] per entry of `days`: the instants each day begins at, in
    /// order (at most 366), the last day ending at `end`. `filter.between` is
    /// replaced by that range. One grouped read under the list's own rule, so
    /// a year view counts every activity, however many there are.
    pub async fn day_counts(
        &self,
        scope: &Scope,
        filter: &Filter,
        days: &[jiff::Timestamp],
        end: jiff::Timestamp,
    ) -> Result<Vec<DayCount>> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let (filter, readable) = self.checked(&**client, &scope, filter).await?;
        store::day_counts(&**client, &scope, &filter, readable, days, end).await
    }
    /// The activities on one record the reader may see: related to it, and
    /// for a person also those they are a guest of. The record itself must
    /// be one the reader may view (`base/records.check`).
    pub async fn for_record(
        &self,
        scope: &Scope,
        record: &Reference,
        state: State,
        page_size: i64,
    ) -> Result<Page> {
        let client = self.db.get().await?;
        let scope = self.fresh(&**client, scope).await?;
        let record = match self
            .records
            .check(&**client, &scope, &record.kind, &record.id)
            .await?
        {
            Ok(record) => record,
            Err(bilibi_records::Refusal::Forbidden) => return Err(Error::Forbidden),
            Err(_) => return Err(Error::NotFound),
        };
        let filter = Filter {
            state,
            related: Some(record),
            descending: state == State::Closed,
            page_size,
            ..Filter::default()
        };
        store::list(&**client, &scope, &filter, true).await
    }
}
