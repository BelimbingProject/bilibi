//! Structural hygiene uses system identity and an opt-in owner locking seam.
//! It runs no owner business hooks and preserves owner -> run -> work lock order.
use super::*;
impl Engine {
    /// One bounded pass: at most `limit` running runs, continuing after the
    /// last run of the previous pass, so the cost of a pass does not grow
    /// with the number of open runs and an unavailable run cannot starve the
    /// ones after it. Every running run is visited once per lap.
    pub(crate) async fn sweep_processes(&self, limit: i64) -> Result<(usize, usize)> {
        let (mut reconciled, mut skipped) = (0, 0);
        for (id, tenant) in self.process_maintenance_page(limit).await? {
            let engine = self.clone();
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn(async move { engine.maintain_process(id, tenant).await });
            let result = match tasks.join_next().await {
                Some(Ok(result)) => result,
                _ => {
                    eprintln!("workflow maintenance skipped run {id}: owner panicked");
                    skipped += 1;
                    continue;
                }
            };
            match result {
                Ok(()) => reconciled += 1,
                Err(
                    Error::Invalid
                    | Error::Conflict
                    | Error::NotFound
                    | Error::Forbidden
                    | Error::Unauthorized,
                ) => {
                    eprintln!(
                        "workflow maintenance skipped run {id}: owner or definition unavailable"
                    );
                    skipped += 1;
                }
                Err(error) => return Err(error),
            }
        }
        Ok((reconciled, skipped))
    }
    /// The runs of this pass, and the saved place moved past them. The place
    /// is the sweep's own transport state, like an outbox lease: one row for
    /// the installation, no tenant's business record, so it is written here
    /// without an Audit row. A pass that finds another one choosing its page
    /// visits nothing. After the last running run the next page starts again
    /// from the first.
    async fn process_maintenance_page(&self, limit: i64) -> Result<Vec<(i64, i64)>> {
        const PAGE: &str = "SELECT id,tenant_id FROM base_workflow_process_runs WHERE status='running' AND id>$1 ORDER BY id LIMIT $2";
        let mut client = self.db.get().await?;
        let tx = client.transaction().await?;
        let Some(row) = tx
            .query_opt(
                "SELECT last_run FROM base_workflow_process_maintenance WHERE singleton FOR UPDATE SKIP LOCKED",
                &[],
            )
            .await?
        else {
            return Ok(Vec::new());
        };
        let previous: i64 = row.get(0);
        let mut rows = tx.query(PAGE, &[&previous, &limit]).await?;
        if rows.is_empty() && previous > 0 {
            rows = tx.query(PAGE, &[&0_i64, &limit]).await?;
        }
        let runs: Vec<(i64, i64)> = rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        let next = runs.last().map_or(0, |run| run.0);
        tx.execute(
            "UPDATE base_workflow_process_maintenance SET last_run=$1 WHERE singleton",
            &[&next],
        )
        .await?;
        tx.commit().await?;
        Ok(runs)
    }
    /// Repairs one run and records it only when something changed: a run with
    /// nothing to repair is rolled back, so it leaves no Audit row.
    async fn maintain_process(&self, id: i64, tenant: i64) -> Result<()> {
        let mut client = self.db.get().await?;
        let scope = bilibi_authz::SystemScope::rehydrate(&**client, tenant, None).await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                scope.mutation_context("workflow.process.maintained"),
            )
            .await?;
        let mut changes = Vec::new();
        {
            // Any error below drops the transaction, which rolls it back.
            let sql = tx.sql();
            let candidate=sql.query_opt(&format!("SELECT {RUN_COLUMNS} FROM base_workflow_process_runs WHERE id=$1 AND tenant_id=$2"),&[&id,&tenant]).await?.map(run_row).ok_or(Error::NotFound)?;
            let owner = self.registry.owned_subject(&candidate.subject_type)?;
            if candidate.owner != owner.owner {
                return Err(Error::Invalid);
            }
            let subject = owner
                .adapter
                .maintenance_load(sql, &scope, candidate.subject_id)
                .await?;
            if subject.tenant != tenant
                || subject.id != candidate.subject_id
                || !crate::definitions::key(&subject.status)
                || subject.version < 0
                || !subject.facts.is_object()
            {
                return Err(Error::Invalid);
            }
            let run=sql.query_opt(&format!("SELECT {RUN_COLUMNS} FROM base_workflow_process_runs WHERE id=$1 AND tenant_id=$2 FOR UPDATE"),&[&id,&tenant]).await?.map(run_row).ok_or(Error::NotFound)?;
            if run.subject_type != candidate.subject_type
                || run.subject_id != candidate.subject_id
                || run.owner != candidate.owner
            {
                return Err(Error::Conflict);
            }
            let definition = self.supported(sql, &run, owner).await?;
            let (items, dependencies) = graph(sql, &run, &definition, true).await?;
            settle(sql, &run, items, &dependencies, &mut changes).await?;
        }
        if changes.is_empty() {
            return tx.rollback().await;
        }
        changes.push(Change {
            kind: "workflow.process_maintenance".into(),
            id,
            old: json!({}),
            new: json!({"tenant":tenant}),
        });
        tx.mutate(move |_| Box::pin(async move { Ok(((), changes)) }))
            .await?;
        tx.commit().await
    }
}
