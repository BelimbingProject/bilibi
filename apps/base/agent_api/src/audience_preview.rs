use crate::{invalid, OperationClient, Result};
use bilibi_tenancy::Scope;
use serde_json::{json, Value};

pub struct AudiencePreview(Value);

impl AudiencePreview {
    pub fn everyone() -> Self {
        Self(json!({"everyone": true, "name": "Everyone"}))
    }

    /// The caller must have permission to inspect the audience's role names.
    pub async fn roles(
        client: &dyn OperationClient,
        scope: &Scope,
        mut ids: Vec<i64>,
    ) -> Result<Self> {
        ids.sort_unstable();
        ids.dedup();
        let rows = client
            .query(
                "SELECT id, name FROM roles WHERE tenant_id=$1 AND id=ANY($2) ORDER BY id",
                &[&scope.tenant(), &ids],
            )
            .await?;
        if rows.len() != ids.len() {
            return Err(invalid(
                "audience",
                "choose roles that exist in this tenant",
            ));
        }
        let roles: Vec<_> = rows
            .iter()
            .map(|row| json!({"id": row.get::<_, i64>(0), "name": row.get::<_, String>(1)}))
            .collect();
        Ok(Self(json!({"roles": roles})))
    }

    pub fn review(self, visible_to_current_reader: Value) -> Value {
        json!({
            "audience": self.0,
            "visible_to_current_reader": visible_to_current_reader,
            "notice": "This preview uses your access. Each recipient sees what their own permissions allow.",
        })
    }
}
