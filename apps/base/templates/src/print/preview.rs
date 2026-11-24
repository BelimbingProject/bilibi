use super::PrintContract;
use crate::{Templates, VIEW};
use bilibi_artifacts::{Artifacts, Operation, Owner, PdfOptions, Reference};
use bilibi_branding::Branding;
use bilibi_database::{Db, Error, Result};
use bilibi_module_registry::{
    contributions::{Access, Download as Route, DOWNLOADS as DOWNLOAD_CONSUMER},
    Contribution, RegistryError,
};
use bilibi_party_ids::OrganisationId;
use bilibi_tenancy::{ProofClient, ProofFuture, Scope};
use bilibi_ui::download::{Download, DownloadFuture, DownloadRequest, FileBody};
use std::sync::{Arc, OnceLock};

pub(crate) const DOWNLOADS: &str = "base/templates.downloads";
pub(super) const OWNER: PreviewOwner = PreviewOwner;
static PREVIEWS: OnceLock<Arc<Previews>> = OnceLock::new();

pub(crate) struct Previews {
    pub(crate) templates: Arc<Templates>,
    pub(crate) artifacts: Arc<Artifacts>,
    pub(crate) branding: Arc<Branding>,
    pub(crate) db: Db,
}

pub(crate) fn bind(previews: Arc<Previews>) -> bilibi_module_registry::Result<()> {
    PREVIEWS
        .set(previews)
        .map_err(|_| RegistryError::Service("duplicate print preview service".into()))
}

pub(crate) fn service() -> Result<&'static Previews> {
    PREVIEWS.get().map(Arc::as_ref).ok_or(Error::Internal)
}

pub(super) struct PreviewOwner;

impl Owner for PreviewOwner {
    fn owner_id(&self) -> &'static str {
        "base/templates.print_preview"
    }

    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        _: OrganisationId,
        operation: Operation,
        reference: Option<&'a Reference>,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            if !matches!(operation, Operation::Create | Operation::Read)
                || !reference.is_some_and(|reference| reference.subject == scope.user().to_string())
            {
                return Err(Error::Forbidden);
            }
            bilibi_authz::authorize_proof(client, scope, VIEW).await
        })
    }
}

pub(crate) struct StagedPreview {
    company: OrganisationId,
    id: String,
}

impl StagedPreview {
    pub(crate) fn path(&self) -> String {
        format!(
            "/admin/customisations/print-previews/{}/{}",
            self.company.get(),
            self.id
        )
    }
}

impl Previews {
    pub(crate) async fn stage(
        &self,
        scope: &Scope,
        target: Option<OrganisationId>,
        contract: &PrintContract,
        language: &str,
        layout: &str,
    ) -> Result<StagedPreview> {
        let mut client = self.db.try_get_detached("an agent preview may hold a read connection; the temporary artifact needs an independent write").await?;
        self.templates
            .authorize(&**client, scope, target, false)
            .await?;
        let company = target
            .or_else(|| scope.current_company())
            .ok_or_else(|| Error::refused("Select a company before opening a print preview."))?;
        let reference = Reference {
            subject: scope.user().to_string(),
            kind: contract.key.into(),
        };
        let accent = self.branding.accent(&**client).await?;
        let document = contract
            .compile(language, layout)
            .and_then(|compiled| compiled.preview_document(accent))
            .map_err(|problem| Error::refused(problem.to_string()))?;
        let options = PdfOptions::default()
            .expires_in_days(1)
            .template_version(contract.version);
        let stored = self
            .artifacts
            .for_owner(scope, company, &OWNER)
            .render_inline_document(&mut client, reference, document, &options)
            .await
            .map_err(|error| match error {
                bilibi_artifacts::Error::Database(error) => error,
                _ => Error::refused("The PDF preview could not be rendered."),
            })?;
        Ok(StagedPreview {
            company,
            id: stored.document.id,
        })
    }

    async fn read(&self, scope: &Scope, company: OrganisationId, id: &str) -> Result<FileBody> {
        let mut client = self.db.get().await?;
        self.templates
            .authorize(&**client, scope, Some(company), false)
            .await?;
        let document = self
            .artifacts
            .for_owner(scope, company, &OWNER)
            .read(&mut client, id)
            .await
            .map_err(|_| Error::NotFound)?;
        Ok(FileBody::inline(
            document.bytes,
            "application/pdf",
            "print-preview.pdf",
        ))
    }
}

impl Download<Scope, Error> for Previews {
    fn serve<'a>(&'a self, request: DownloadRequest<'a, Scope>) -> DownloadFuture<'a, Error> {
        Box::pin(async move {
            match request.operation {
                "preview" => {
                    let company = request
                        .path
                        .get("company")
                        .and_then(|id| id.parse::<i64>().ok())
                        .and_then(|id| OrganisationId::try_from(id).ok())
                        .ok_or(Error::NotFound)?;
                    let id = request.path.get("id").ok_or(Error::NotFound)?;
                    self.read(request.scope, company, id).await
                }
                "published" => {
                    let revision = request
                        .path
                        .get("revision")
                        .and_then(|id| id.parse::<i64>().ok())
                        .ok_or(Error::NotFound)?;
                    let (contract, company, language, layout) = {
                        let client = self.db.get().await?;
                        self.templates
                            .print_revision(&**client, request.scope, revision)
                            .await?
                    };
                    let preview = self
                        .stage(request.scope, company, contract, &language, &layout)
                        .await?;
                    self.read(request.scope, preview.company, &preview.id).await
                }
                _ => Err(Error::NotFound),
            }
        })
    }
}

inventory::submit! { Contribution::new(DOWNLOAD_CONSUMER, "base/templates.print-preview", crate::provenance!(), &Route {
    path: "/admin/customisations/print-previews/{company}/{id}", service: DOWNLOADS,
    operation: "preview", access: Access::Capability(VIEW),
}) }

inventory::submit! { Contribution::new(DOWNLOAD_CONSUMER, "base/templates.published-print-preview", crate::provenance!(), &Route {
    path: "/admin/customisations/print-layouts/{revision}/preview", service: DOWNLOADS,
    operation: "published", access: Access::Capability(VIEW),
}) }
