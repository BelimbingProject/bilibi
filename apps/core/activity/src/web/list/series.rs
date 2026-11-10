//! The Delete series action of a repeating row on the Activities list. The
//! dialog keeps the occurrence's version from the moment it opened, so the
//! contract's version check catches an edit made while it was up.
use super::Index;
use crate::web::{refusal, sentences, series_delete, Refusal};
use bilibi_database::{Error, Result};
use bilibi_tenancy::Scope;
use bilibi_ui::{params, StatusKind};
use serde_json::Value;

impl Index {
    /// Opens the dialog for the row's occurrence, as it is now.
    pub(super) async fn open_series_delete(&mut self, scope: &Scope, value: &Value) -> Result<()> {
        let id = value["id"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or(Error::Invalid)?;
        let activity = self.pages.activities.get(scope, id).await?;
        if activity.series_id.is_some() && self.pages.activities.may_edit(scope, &activity).await? {
            self.series_delete = Some(series_delete::Open {
                id,
                title: activity.title,
                version: activity.version,
                errors: Vec::new(),
            });
        }
        Ok(())
    }

    /// Deletes the occurrence the dialog opened for and the later ones, or
    /// all of them, naming the version the dialog opened with.
    pub(super) async fn delete_series(
        &mut self,
        scope: &Scope,
        credential: &str,
        value: &Value,
    ) -> Result<()> {
        let Some(open) = &mut self.series_delete else {
            return Ok(());
        };
        let form = params::form_values_checked(value, "delete", params::FIELD_LIMIT)
            .map_err(Error::refused)?;
        let apply = match form.get("apply").map(String::as_str) {
            Some("all") => "all",
            _ => "following",
        };
        let call = self.pages.call(
            scope,
            credential,
            "delete_series",
            "core.activity.delete_series",
        );
        match self
            .pages
            .contracts
            .submit(
                &call,
                serde_json::json!({"id": open.id, "version": open.version, "apply": apply}),
            )
            .await
        {
            Ok(result) => {
                let count = result["result"]["deleted"].as_array().map_or(0, Vec::len);
                self.series_delete = None;
                self.flash = Some((StatusKind::Success, series_delete::deleted(count)));
            }
            Err(error) => {
                open.errors = vec![match refusal(
                    error,
                    "You do not have permission to change this activity.",
                    "The series was not deleted. Try again.",
                )? {
                    Refusal::Message(message) => series_delete::refused(message),
                    Refusal::Fields(fields) => sentences(&fields),
                }];
            }
        }
        Ok(())
    }
}
