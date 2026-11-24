//! Scope, private preview and revision proof on a migrated disposable database.
mod contract;

use bilibi_agent_api::{
    page::PageContracts,
    plans::{PlanAuthority, PlanIdentity, PlannedOperation, Plans},
};
use bilibi_base::party_ids::OrganisationId;
use bilibi_base::settings::{Settings, Target};
use bilibi_base::templates::{Origin, Templates};
use bilibi_database::{pool, Error, MutationBoundary};
use bilibi_tenancy::{Authentication, ProofFuture, Scope};
use bilibi_ui::download::{DownloadRequest, DownloadService};
use serde_json::{json, Value};
use std::{collections::HashMap, os::unix::fs::PermissionsExt};
use tokio_postgres::Transaction;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Held<'a>(&'a Scope);
impl PlanAuthority for Held<'_> {
    fn verify<'a>(&'a self, _: &'a Transaction<'_>) -> ProofFuture<'a, PlanIdentity> {
        Box::pin(async {
            Ok(PlanIdentity {
                scope: self.0.clone(),
                credential_binding: "walkthrough:print".into(),
            })
        })
    }
}

async fn reviewed(contracts: &PageContracts, scope: &Scope, company: Option<i64>) -> Result<Value> {
    let mut input = json!({"key":contract::KEY,"language":"en","company_id":company});
    let reply = contracts
        .read(scope, "base.templates.print.get", &input)
        .await?;
    input["review"] = json!(reply["result"]["print"]["review"]
        .as_str()
        .ok_or("no review token")?);
    Ok(input)
}

fn publish(mut input: Value, title: &str) -> PlannedOperation {
    input["layout"] = json!(contract::EN.replace("Production sheet", title));
    PlannedOperation {
        key: "base.templates.print.publish".into(),
        input,
    }
}

async fn apply(plans: &Plans, scope: &Scope, change: &PlannedOperation) -> Result<i64> {
    let change = std::slice::from_ref(change);
    assert_eq!(plans.preview(&Held(scope), change).await?["valid"], true);
    let done = plans.commit(scope, &Held(scope), change).await?;
    let revision = done["results"][0]["result"]["result"]["revision"]
        .as_i64()
        .ok_or("the reviewed operation did not return a revision")?;
    Ok(revision)
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_ARTIFACTS_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable database opt-in required".into());
    }
    let root = std::env::var("BILIBI_ARTIFACTS_DEMO_ROOT")?;
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(db.clone(), "https://business-print.localhost")?;
    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let (manager, peer, outsider) = {
        let mut client = db.get().await?;
        client
            .batch_execute(
                "INSERT INTO role_capabilities(tenant_id,role_id,capability) VALUES
                (1,1,'base.templates.view'),(1,1,'base.templates.manage'),
                (1,2,'base.templates.view'),(2,5,'base.templates.view')
             ON CONFLICT DO NOTHING;
             INSERT INTO user_desks(tenant_id,person_id,company_id)
                VALUES(1,1,2),(1,3,1) ON CONFLICT DO NOTHING;",
            )
            .await?;
        let manager = authentication
            .authenticate_session(&**client, 1, 1, None, Some(1), "walkthrough".into())
            .await?;
        let peer = authentication
            .authenticate_session(&**client, 1, 6, None, Some(1), "walkthrough".into())
            .await?;
        let outsider = authentication
            .authenticate(&**client, 2, 5, "walkthrough".into())
            .await?;
        let root_actor = authentication
            .authenticate(&**client, 1, 6, "walkthrough".into())
            .await?;
        let settings = app.services.get::<Settings>("base/settings.store")?;
        let boundary = app
            .services
            .get::<MutationBoundary>("base/database.mutations")?;
        let mut tx = boundary
            .begin(
                &mut client,
                root_actor.mutation_context("print.walkthrough.configure"),
            )
            .await?;
        for (key, value) in [
            ("artifacts.storage_root", json!(root)),
            (
                "pdf.chromium.binary",
                json!(std::env::var("BILIBI_PDF_CHROMIUM")?),
            ),
        ] {
            settings
                .put(&mut tx, &root_actor, Target::Global, key, value)
                .await?;
        }
        tx.commit().await?;
        (manager, peer, outsider)
    };

    let plans = app.services.get::<Plans>("base/agent_api.plans")?;
    let contracts = app
        .services
        .get::<PageContracts>(bilibi_agent_api::page::SERVICE)?;
    let templates = app
        .services
        .get::<Templates>(bilibi_base::templates::SERVICE)?;
    let company = |id| OrganisationId::new(id).map(Some);
    let stale = publish(
        reviewed(&contracts, &manager, Some(1)).await?,
        "Stale inherited layout",
    );
    assert_eq!(
        plans
            .preview(&Held(&manager), std::slice::from_ref(&stale))
            .await?["valid"],
        true
    );
    let workspace = apply(
        &plans,
        &manager,
        &publish(
            reviewed(&contracts, &manager, None).await?,
            "Workspace production sheet",
        ),
    )
    .await?;
    assert!(plans
        .commit(&manager, &Held(&manager), &[stale])
        .await
        .is_err());

    let first = apply(
        &plans,
        &manager,
        &publish(
            reviewed(&contracts, &manager, Some(1)).await?,
            "Company production sheet",
        ),
    )
    .await?;
    let company_review = reviewed(&contracts, &manager, Some(1)).await?;
    let stale = publish(company_review.clone(), "Stale proposed title");
    assert_eq!(
        plans
            .preview(&Held(&manager), std::slice::from_ref(&stale))
            .await?["valid"],
        true
    );
    let latest = apply(
        &plans,
        &manager,
        &publish(company_review, "New company title"),
    )
    .await?;
    assert!(plans
        .commit(&manager, &Held(&manager), &[stale])
        .await
        .is_err());

    {
        let client = db.get().await?;
        let own = templates
            .resolve_print(
                &**client,
                &manager,
                contract::KEY,
                company(1)?,
                "en-MY",
                None,
            )
            .await?;
        assert_eq!(
            own.origin,
            Origin::Company {
                company_id: 1,
                revision: latest
            }
        );
        assert!(own.layout.contains("New company title"));
        let sibling = templates
            .resolve_print(&**client, &manager, contract::KEY, company(2)?, "en", None)
            .await?;
        assert_eq!(
            sibling.origin,
            Origin::Tenant {
                revision: workspace
            }
        );
        let malay = templates
            .resolve_print(
                &**client,
                &manager,
                contract::KEY,
                company(2)?,
                "ms-MY",
                None,
            )
            .await?;
        assert_eq!(malay.language, "ms");
        assert_eq!(
            malay.origin,
            Origin::Packaged {
                version: "1".into()
            }
        );
        let other = templates
            .resolve_print(
                &**client,
                &outsider,
                contract::KEY,
                company(100)?,
                "en",
                None,
            )
            .await?;
        assert_eq!(
            other.origin,
            Origin::Packaged {
                version: "1".into()
            }
        );
    }

    let files = app
        .services
        .get::<DownloadService<Scope, Error>>("base/templates.downloads")?;
    let query = HashMap::new();
    for (revision, current) in [(first, false), (latest, true)] {
        let params = HashMap::from([("revision".into(), revision.to_string())]);
        let result = files
            .0
            .serve(DownloadRequest {
                operation: "published",
                scope: &manager,
                path: &params,
                query: &query,
            })
            .await;
        if current {
            assert!(result?.bytes.starts_with(b"%PDF-"));
        } else {
            assert!(matches!(result, Err(Error::NotFound)));
        }
    }

    let draft = contracts
        .read(
            &manager,
            "base.templates.print.preview",
            &json!({
                "key":contract::KEY,"language":"en","company_id":1,
                "layout":contract::EN.replace("Production sheet","Private draft sample"),
            }),
        )
        .await?;
    let path = draft["result"]["preview_path"]
        .as_str()
        .ok_or("no preview path")?;
    let id = path.rsplit('/').next().ok_or("no preview ID")?;
    let params = HashMap::from([("company".into(), "1".into()), ("id".into(), id.into())]);
    let request = |scope| DownloadRequest {
        operation: "preview",
        scope,
        path: &params,
        query: &query,
    };
    let pdf = files.0.serve(request(&manager)).await?;
    assert!(pdf.bytes.starts_with(b"%PDF-"));
    assert!(files.0.serve(request(&peer)).await.is_err());

    let get = |company| json!({"key":contract::KEY,"language":"en","company_id":company});
    let before = contracts
        .read(&manager, "base.templates.print.get", &get(1))
        .await?;
    assert_eq!(before["result"]["print"]["revision"], latest);
    let mut restore = PlannedOperation {
        key: "base.templates.print.restore".into(),
        input: reviewed(&contracts, &manager, Some(1)).await?,
    };
    assert_eq!(
        plans
            .preview(&Held(&manager), std::slice::from_ref(&restore))
            .await?["valid"],
        true
    );
    let workspace = apply(
        &plans,
        &manager,
        &publish(
            reviewed(&contracts, &manager, None).await?,
            "Updated workspace title",
        ),
    )
    .await?;
    assert!(plans
        .commit(&manager, &Held(&manager), std::slice::from_ref(&restore))
        .await
        .is_err());
    restore.input = reviewed(&contracts, &manager, Some(1)).await?;
    let stale = publish(restore.input.clone(), "Earlier editor");
    let marker = apply(&plans, &manager, &restore).await?;
    let params = HashMap::from([("revision".into(), latest.to_string())]);
    assert!(matches!(
        files
            .0
            .serve(DownloadRequest {
                operation: "published",
                scope: &manager,
                path: &params,
                query: &query,
            })
            .await,
        Err(Error::NotFound)
    ));
    let after = contracts
        .read(&manager, "base.templates.print.get", &get(1))
        .await?;
    assert_eq!(after["result"]["print"]["revision"], marker);
    assert_eq!(after["result"]["print"]["customised"], false);
    assert_eq!(
        after["result"]["print"]["effective"]["origin"]["revision"],
        workspace
    );
    assert!(plans
        .commit(&manager, &Held(&manager), &[stale])
        .await
        .is_err());

    println!("Print scope, private draft PDF, stale approvals, and restore marker passed");
    Ok(())
}
