//! Real owner and Records calls on an explicitly disposable migrated database.
//! This installs the walkthrough fixture. Never use a shared database.
use bilibi_database::{pool, MutationBoundary};
use bilibi_product::{PriceInput, ProductInput, Products};
use bilibi_records::{Records, Reference};
use bilibi_tenancy::Scope;
use serde_json::json;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
inventory::submit! {
    bilibi_module_registry::Contribution::new(bilibi_authz::VOCABULARY, "core/product.walkthrough", bilibi_module_registry::Provenance { module: "core/product", source: file!(), line: line!() }, &bilibi_authz::VocabularyContribution {
        domains: &[], verbs: &[], capabilities: &[],
        roles: &[bilibi_authz::RoleDefinition { code: "manager", name: "manager", description: "Walkthrough fixture role", grant_all: false, capabilities: &[bilibi_product::VIEW, bilibi_product::MANAGE, bilibi_product::PRICES_MANAGE] }],
        system_principals: &[],
    })
}

type Error = Box<dyn std::error::Error>;
async fn login(app: &bilibi_web::App, email: &str) -> Result<Scope, Error> {
    let login = app
        .user
        .login(email, bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    Ok(app.security.guard(&login.token).await?)
}
#[tokio::main]
async fn main() -> Result<(), Error> {
    if std::env::var("BILIBI_PRODUCT_WALKTHROUGH").as_deref() != Ok("disposable") {
        return Err("Set BILIBI_PRODUCT_WALKTHROUGH=disposable on a disposable database.".into());
    }
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "https://core-product-build.localhost")?;
    let products = app.services.get::<Products>(bilibi_product::SERVICE)?;
    let currencies = app
        .services
        .get::<bilibi_currency::Currencies>(bilibi_currency::SERVICE)?;
    let records = app.services.get::<Records>(bilibi_records::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let grants = app
        .services
        .get::<bilibi_authz::grants::Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let admin = login(&app, "admin@example.test").await?;
    let manager = login(&app, "manager@example.test").await?;
    let viewer = login(&app, "viewer@example.test").await?;
    let other = login(&app, "manager@other.example.test").await?;
    let mut client = db.get().await?;
    let input = ProductInput {
        code: String::new(),
        name: "Walkthrough annual support".into(),
        kind: "service".into(),
        base_unit: "year".into(),
        category: Some("Support".into()),
    };
    println!(
        "viewer create preview: {:?}",
        products.preview_create(&**client, &viewer, &input).await
    );
    let mut tx = boundary
        .begin(&mut client, manager.mutation_context("product.walkthrough"))
        .await?;
    let product = products
        .create(&mut tx, &manager, &input)
        .await?
        .map_err(|e| format!("create refused: {e:?}"))?;
    tx.commit().await?;
    println!("created: {}", product.to_json());
    println!(
        "cross-tenant product: {:?}",
        products.get(&**client, &other, product.id).await
    );
    for code in ["MYR", "JPY", "KWD"] {
        let mut tx = boundary
            .begin(&mut client, admin.mutation_context("currency.walkthrough"))
            .await?;
        currencies
            .set_enabled(&mut tx, &admin, code, true)
            .await?
            .map_err(|e| format!("currency refused: {e:?}"))?;
        tx.commit().await?;
    }
    let price = PriceInput {
        product: product.id,
        selling_company: 1,
        currency: "MYR".into(),
        effective_on: "2026-01-01".parse()?,
        amount: "1200.00".parse()?,
    };
    for (currency, amount) in [
        ("MYR", "12.345"),
        ("JPY", "12.1"),
        ("KWD", "12.3456"),
        ("USD", "12.00"),
        ("MYR", "-1.00"),
    ] {
        let attempt = PriceInput {
            currency: currency.into(),
            amount: amount.parse()?,
            ..price.clone()
        };
        println!(
            "invalid {currency} {amount}: {:?}",
            products
                .preview_price(&**client, &manager, &attempt)
                .await?
        );
    }
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("product.price.walkthrough"),
        )
        .await?;
    let saved = products
        .add_price(&mut tx, &manager, &price)
        .await?
        .map_err(|e| format!("price refused: {e:?}"))?;
    tx.commit().await?;
    println!("first price: {}", saved.to_json());
    println!(
        "duplicate: {:?}",
        products.preview_price(&**client, &manager, &price).await?
    );
    println!(
        "unassigned selling company: {:?}",
        products
            .preview_price(
                &**client,
                &manager,
                &PriceInput {
                    selling_company: 2,
                    ..price.clone()
                }
            )
            .await
    );
    let later = PriceInput {
        effective_on: "2026-07-01".parse()?,
        amount: "1350.00".parse()?,
        ..price.clone()
    };
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("product.price.walkthrough"),
        )
        .await?;
    products
        .add_price(&mut tx, &manager, &later)
        .await?
        .map_err(|e| format!("later price refused: {e:?}"))?;
    tx.commit().await?;
    for (currency, on) in [
        ("MYR", "2025-12-31"),
        ("MYR", "2026-06-30"),
        ("MYR", "2026-07-01"),
        ("USD", "2026-07-01"),
    ] {
        println!(
            "lookup {currency} {on}: {:?}",
            products
                .price(&**client, &manager, product.id, 1, currency, on.parse()?)
                .await?
                .map(|p| p.to_json())
        );
    }
    // Release our connection before Records opens its own bounded queries.
    drop(client);
    let hits = records
        .search(&manager, &["product"], "Walkthrough annual")
        .await?;
    println!("active picker: {hits:?}");
    let mut client = db.get().await?;
    let mut tx = boundary
        .begin(
            &mut client,
            manager.mutation_context("product.retire.walkthrough"),
        )
        .await?;
    let retired = products
        .update(
            &mut tx,
            &manager,
            product.id,
            product.version,
            &json!({"status":"retired"}),
        )
        .await?
        .map_err(|e| format!("retire refused: {e:?}"))?;
    tx.commit().await?;
    println!("retired: {}", retired.to_json());
    println!(
        "new reference refused: {:?}",
        records
            .check(&**client, &manager, "product", &product.id.to_string())
            .await?
    );
    println!(
        "stale edit: {:?}",
        products
            .preview_update(
                &**client,
                &manager,
                product.id,
                product.version,
                &json!({"name":"Stale"})
            )
            .await?
    );
    println!(
        "retired price append: {:?}",
        products
            .preview_price(
                &**client,
                &manager,
                &PriceInput {
                    effective_on: "2026-08-01".parse()?,
                    ..price
                }
            )
            .await?
    );
    drop(client);
    println!(
        "retired picker: {:?}",
        records
            .search(&manager, &["product"], "Walkthrough annual")
            .await?
    );
    println!(
        "earlier reference: {:?}",
        records
            .resolve(
                &manager,
                &[Reference::new("product", product.id.to_string())]
            )
            .await?
    );
    Ok(())
}
