//! Manual running-service walkthrough for the A4 HTML PDF engine and the
//! single-use signed `/pdf/render/{token}` route. Explicit disposable DB only:
//! it starts the real Axum app on a local port, drives real Chromium, and prints
//! what happened for inspection. One check exits non-zero: a browser that
//! starts slowly still prints, because the render timeout counts from its
//! first answer and its start has its own limit (#707).
//!
//! Needs `BILIBI_PDF_DEMO_OUT` (output directory), `BILIBI_PDF_CHROMIUM`
//! (absolute path of a sandbox-capable Chromium/Chrome), optional
//! `BILIBI_PDF_DEMO_PORT` (first of three free local ports), the disposable-DB
//! variables from the README and a database that is migrated.
use bilibi_artifacts::{Artifacts, HtmlFuture, HtmlOwner, Operation, Owner, Reference, Served};
use bilibi_database::{pool, MutationBoundary};
use bilibi_party_ids::OrganisationId;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, ProofClient, ProofFuture, Scope};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_postgres::Client;
bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Embedded red square: the only image the document may legitimately show.
const RED: &str = "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSI2MCIgaGVpZ2h0PSI2MCI+PHJlY3Qgd2lkdGg9IjYwIiBoZWlnaHQ9IjYwIiBmaWxsPSIjZmYwMDAwIi8+PC9zdmc+";

struct Orders {
    db: bilibi_database::Db,
    probe: u16,
    marker: String,
}
impl Owner for Orders {
    fn owner_id(&self) -> &'static str {
        "walkthrough.html"
    }
    fn authorize<'a>(
        &'a self,
        client: &'a dyn ProofClient,
        scope: &'a Scope,
        company: OrganisationId,
        _: Operation,
        reference: Option<&'a Reference>,
    ) -> ProofFuture<'a, ()> {
        Box::pin(async move {
            let subject = reference.map(|r| r.subject.as_str()).unwrap_or("order-1");
            let row = client.query_opt("SELECT 1 FROM walkthrough_html_orders WHERE subject=$1 AND tenant_id=$2 AND company_id=$3 AND account_id=$4", &[&subject,&scope.tenant(),&company.get(),&scope.user()]).await?;
            row.ok_or(bilibi_database::Error::Forbidden)?;
            Ok(())
        })
    }
}
/// The owner selects the content; every resource the template tries to pull in
/// from outside the document is hostile on purpose and must never be fetched.
impl HtmlOwner for Orders {
    fn render_html<'a>(
        &'a self,
        _: &'a Scope,
        _: OrganisationId,
        reference: &'a Reference,
        data: &'a Value,
    ) -> HtmlFuture<'a> {
        Box::pin(async move {
            if data["fail"].as_bool() == Some(true) {
                return Err(bilibi_artifacts::Error::Invalid);
            }
            if data["revoke_after"].as_bool() == Some(true) {
                // Permission disappears after the document was selected but
                // before it is published: Base must refuse to store it.
                if let Ok(client) = self.db.get().await {
                    let _ = client
                        .execute(
                            "DELETE FROM walkthrough_html_orders WHERE subject='order-1'",
                            &[],
                        )
                        .await;
                }
            }
            let p = self.probe;
            let marker = &self.marker;
            Ok(format!(
                r#"<!doctype html><html><head><meta charset="utf-8"><title>Order</title>
<style>@import url(http://127.0.0.1:{p}/import.css);
body{{font-family:sans-serif;margin:0}} h1{{color:#134}} .total{{background:#def;padding:8px}}
.bg{{height:20px;background:url(http://127.0.0.1:{p}/background.png)}}</style>
<link rel="stylesheet" href="http://127.0.0.1:{p}/remote.css">
{refresh}</head><body>
<h1>Order {subject}</h1>
<p class="total">Embedded style works. Widget x12, Gadget x3.</p>
<p>Embedded image: <img alt="embedded" src="data:image/svg+xml;base64,{RED}"></p>
<p>Local file image: <img alt="local file" src="file://{marker}"></p>
<p>Remote image: <img alt="remote" src="http://127.0.0.1:{p}/remote-image.png"></p>
<div class="bg"></div>
<iframe src="http://127.0.0.1:{p}/frame.html" width="50" height="20"></iframe>
<script>document.body.insertAdjacentHTML('beforeend','<p>SCRIPT RAN</p>');
fetch('http://127.0.0.1:{p}/script-fetch');</script>
</body></html>"#,
                subject = reference.subject,
                refresh = if data["refresh"].as_bool() == Some(true) {
                    format!(
                        r#"<meta http-equiv="refresh" content="0;url=http://127.0.0.1:{p}/refresh">"#
                    )
                } else {
                    String::new()
                }
            ))
        })
    }
}

/// Counts connections to a port nothing legitimate should ever reach.
async fn probe(port: u16) -> Result<(u16, Arc<AtomicUsize>)> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut first = [0u8; 256];
            let read =
                tokio::time::timeout(Duration::from_millis(500), stream.read(&mut first)).await;
            eprintln!(
                "probe connection: {:?}",
                read.map(|r| r.map(|n| String::from_utf8_lossy(&first[..n.min(40)]).into_owned()))
            );
            let _ = stream.shutdown().await;
        }
    });
    Ok((port, hits))
}

#[derive(Default)]
struct Wire {
    /// Request paths the browser sent, so the walkthrough can replay them.
    paths: Vec<String>,
    delay: Duration,
    revoke: bool,
}
/// Transparent pass-through between the browser and the app. It stands in for
/// a reverse proxy: it records the signed path and can stall the request past
/// the token lifetime or revoke the actor's permission while it is in flight.
async fn proxy(
    port: u16,
    app: u16,
    wire: Arc<Mutex<Wire>>,
    db: bilibi_database::Db,
) -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut browser, _)) = listener.accept().await {
            let (wire, db) = (wire.clone(), db.clone());
            tokio::spawn(async move {
                let mut first = vec![0u8; 8192];
                let Ok(read) = browser.read(&mut first).await else {
                    return;
                };
                first.truncate(read);
                let line = String::from_utf8_lossy(&first);
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let (delay, revoke) = {
                    let mut wire = wire.lock().unwrap();
                    if path.starts_with("/pdf/render/") {
                        wire.paths.push(path);
                    }
                    (wire.delay, wire.revoke)
                };
                if revoke {
                    if let Ok(client) = db.get().await {
                        let _ = client
                            .execute(
                                "DELETE FROM walkthrough_html_orders WHERE subject='order-1'",
                                &[],
                            )
                            .await;
                    }
                }
                tokio::time::sleep(delay).await;
                let Ok(mut upstream) = TcpStream::connect(("127.0.0.1", app)).await else {
                    return;
                };
                if upstream.write_all(&first).await.is_ok() {
                    let _ = tokio::io::copy_bidirectional(&mut browser, &mut upstream).await;
                }
            });
        }
    });
    Ok(port)
}

fn show(label: &str, result: std::result::Result<impl std::fmt::Debug, impl std::fmt::Display>) {
    match result {
        Ok(value) => println!("{label}: ok {value:?}"),
        Err(error) => println!("{label}: refused — {error}"),
    }
}
async fn ledger(client: &Client) -> Result<()> {
    let rows = client.query("SELECT public_id,content_type,byte_size,ready_at IS NOT NULL FROM base_artifacts WHERE owner_id='walkthrough.html' ORDER BY id", &[]).await?;
    println!("ledger: {} walkthrough.html artifact(s)", rows.len());
    for row in rows {
        println!(
            "  {} {} {} bytes ready={}",
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, i64>(2),
            row.get::<_, bool>(3)
        );
    }
    Ok(())
}
async fn count(client: &Client) -> Result<i64> {
    let row = client
        .query_one(
            "SELECT count(*) FROM base_artifacts WHERE owner_id='walkthrough.html'",
            &[],
        )
        .await?;
    Ok(row.get(0))
}
async fn get(port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let text = String::from_utf8_lossy(&response);
    let status = text.lines().next().unwrap_or_default().to_owned();
    let headers = text
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().len();
    let mut kept: Vec<&str> = headers
        .lines()
        .filter(|l| {
            l.starts_with("cache-control")
                || l.starts_with("content-security")
                || l.starts_with("x-content")
        })
        .collect();
    kept.sort();
    Ok(format!("{status} body={body}B [{}]", kept.join("; ")))
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_ARTIFACTS_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    let out = std::path::PathBuf::from(std::env::var("BILIBI_PDF_DEMO_OUT")?);
    let root = std::env::var("BILIBI_ARTIFACTS_DEMO_ROOT")?;
    let chromium = std::env::var("BILIBI_PDF_CHROMIUM")?;
    std::fs::create_dir_all(&out)?;
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    let marker = out.join("local-marker.svg");
    std::fs::write(
        &marker,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="60"><rect width="60" height="60" fill="#ff00ff"/></svg>"##,
    )?;
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let app = bilibi_web::App::new(db.clone(), "http://127.0.0.1")?;
    let store = app.services.get::<Artifacts>("base/artifacts.store")?;
    // Three consecutive free ports; pick a base outside the ephemeral range.
    let base: u16 = std::env::var("BILIBI_PDF_DEMO_PORT")
        .ok()
        .and_then(|port| port.parse().ok())
        .unwrap_or(48210);
    let listener = TcpListener::bind(("127.0.0.1", base)).await?;
    let app_port = listener.local_addr()?.port();
    let router = bilibi_web::router(app.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let wire = Arc::new(Mutex::new(Wire::default()));
    let proxy_port = proxy(base + 1, app_port, wire.clone(), db.clone()).await?;
    let (probe_port, hits) = probe(base + 2).await?;
    println!("app :{app_port}, browser-facing proxy :{proxy_port}, hostile probe :{probe_port}");

    let authentication = app
        .services
        .get::<Authentication>("base/tenancy.authentication")?;
    let mut client = db
        .get_detached("the walkthrough's own checking connection")
        .await?;
    let actor = authentication
        .authenticate(&**client, 1, 1, "walkthrough".into())
        .await?;
    let peer = authentication
        .authenticate(&**client, 1, 2, "walkthrough".into())
        .await?;
    let company = OrganisationId::new(1).map_err(|_| "invalid company")?;
    let owner = Orders {
        db: db.clone(),
        probe: probe_port,
        marker: marker.display().to_string(),
    };
    client.batch_execute("CREATE TABLE IF NOT EXISTS walkthrough_html_orders(subject text PRIMARY KEY,tenant_id bigint,company_id bigint,account_id bigint); INSERT INTO walkthrough_html_orders VALUES('order-1',1,1,1) ON CONFLICT DO NOTHING; INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1) ON CONFLICT DO NOTHING;").await?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let configure = |key: &'static str, value: Value| {
        let (settings, boundary, actor) = (settings.clone(), boundary.clone(), actor.clone());
        let db = db.clone();
        async move {
            let mut client = db.get().await?;
            let mut tx = boundary
                .begin(
                    &mut client,
                    actor.mutation_context("artifacts.walkthrough.configure"),
                )
                .await?;
            let result = settings
                .put(&mut tx, &actor, Target::Global, key, value)
                .await;
            if result.is_ok() {
                tx.commit().await?;
            }
            Result::Ok(result.map_err(|error| error.to_string()))
        }
    };
    println!("== Settings (operator-gated, validated)");
    for (key, value) in [
        ("artifacts.storage_root", json!(root)),
        ("artifacts.retention_days", json!(30)),
        ("pdf.chromium.binary", json!(chromium)),
        (
            "pdf.render_origin",
            json!(format!("http://127.0.0.1:{proxy_port}")),
        ),
        ("pdf.signed_url_ttl_seconds", json!(2)),
        ("pdf.render_origin", json!("http://user@host")),
        ("pdf.paper.format", json!("Folio")),
        ("pdf.chromium.binary", json!("relative/chrome")),
    ] {
        println!(
            "{key} = {value}: {:?}",
            configure(key, value.clone()).await?
        );
    }
    configure("pdf.chromium.binary", json!(chromium)).await??;
    configure(
        "pdf.render_origin",
        json!(format!("http://127.0.0.1:{proxy_port}")),
    )
    .await??;
    configure("pdf.signed_url_ttl_seconds", json!(20)).await??;

    let reference = || Reference {
        subject: "order-1".into(),
        kind: "invoice".into(),
    };
    let documents = store.for_owner(&actor, company, &owner);

    println!("== render_inline: offline browser, hostile resources blocked");
    let inline = documents
        .render_inline(&mut client, reference(), &json!({}))
        .await;
    show(
        "render_inline",
        inline
            .as_ref()
            .map(|m| (&m.id, &m.content_type, m.byte_size)),
    );
    let inline = inline?;
    let read = documents.read(&mut client, &inline.id).await?;
    std::fs::write(out.join("inline.pdf"), &read.bytes)?;
    println!("probe hits after inline: {}", hits.load(Ordering::SeqCst));

    println!("== render_view: browser fetches the single-use signed URL");
    let view = documents
        .render_view(&mut client, reference(), &json!({}))
        .await;
    show(
        "render_view",
        view.as_ref().map(|m| (&m.id, &m.content_type, m.byte_size)),
    );
    let view = view?;
    let read = documents.read(&mut client, &view.id).await?;
    std::fs::write(out.join("view.pdf"), &read.bytes)?;
    println!("probe hits after view: {}", hits.load(Ordering::SeqCst));
    let token_path = wire
        .lock()
        .unwrap()
        .paths
        .last()
        .cloned()
        .unwrap_or_default();
    println!(
        "browser requested {} signed path(s)",
        wire.lock().unwrap().paths.len()
    );

    println!("== route behaviour (token path is not printed)");
    println!(
        "replay of the used token: {}",
        get(app_port, &token_path).await?
    );
    let mut forged = token_path.clone();
    forged.pop();
    forged.push(if token_path.ends_with('A') { 'B' } else { 'A' });
    println!("altered signature: {}", get(app_port, &forged).await?);
    println!(
        "unsigned id only: {}",
        get(app_port, "/pdf/render/abc").await?
    );
    println!("no token: {}", get(app_port, "/pdf/render/").await?);
    println!(
        "serve_render(garbage): {}",
        matches!(store.serve_render("x.y").await, Served::NotFound)
    );

    println!("== refusals store nothing");
    let before = hits.load(Ordering::SeqCst);
    show(
        "peer without permission (no browser started)",
        store
            .for_owner(&peer, company, &owner)
            .render_inline(&mut client, reference(), &json!({}))
            .await
            .map(|m| m.id),
    );
    show(
        "non-object data",
        documents
            .render_inline(&mut client, reference(), &json!("x"))
            .await
            .map(|m| m.id),
    );
    show(
        "template navigates away (meta refresh)",
        documents
            .render_inline(&mut client, reference(), &json!({"refresh":true}))
            .await
            .map(|m| m.id),
    );
    show(
        "template failure (inline)",
        documents
            .render_inline(&mut client, reference(), &json!({"fail":true}))
            .await
            .map(|m| m.id),
    );
    show(
        "template failure (view)",
        documents
            .render_view(&mut client, reference(), &json!({"fail":true}))
            .await
            .map(|m| m.id),
    );

    println!("== token expiry: proxy stalls the browser past the 2 second lifetime");
    configure("pdf.signed_url_ttl_seconds", json!(1)).await??;
    wire.lock().unwrap().delay = Duration::from_secs(2);
    show(
        "expired token",
        documents
            .render_view(&mut client, reference(), &json!({}))
            .await
            .map(|m| m.id),
    );
    wire.lock().unwrap().delay = Duration::ZERO;
    configure("pdf.signed_url_ttl_seconds", json!(20)).await??;

    println!("== reauthorization on the route: permission revoked while the request is in flight");
    wire.lock().unwrap().revoke = true;
    show(
        "revoked mid-flight",
        documents
            .render_view(&mut client, reference(), &json!({}))
            .await
            .map(|m| m.id),
    );
    wire.lock().unwrap().revoke = false;
    client
        .batch_execute(
            "INSERT INTO walkthrough_html_orders VALUES('order-1',1,1,1) ON CONFLICT DO NOTHING",
        )
        .await?;

    println!("== reauthorization at publication: permission revoked after the template ran");
    let rows_before = count(&client).await?;
    show(
        "revoked after render_inline template",
        documents
            .render_inline(&mut client, reference(), &json!({"revoke_after":true}))
            .await
            .map(|m| m.id),
    );
    client
        .batch_execute(
            "INSERT INTO walkthrough_html_orders VALUES('order-1',1,1,1) ON CONFLICT DO NOTHING",
        )
        .await?;
    show(
        "revoked after render_view template",
        documents
            .render_view(&mut client, reference(), &json!({"revoke_after":true}))
            .await
            .map(|m| m.id),
    );
    client
        .batch_execute(
            "INSERT INTO walkthrough_html_orders VALUES('order-1',1,1,1) ON CONFLICT DO NOTHING",
        )
        .await?;
    println!(
        "artifact rows added by those two: {}",
        count(&client).await? - rows_before
    );

    println!("== engine limits");
    configure("pdf.render_origin", json!("")).await??;
    show(
        "view without pdf.render_origin",
        documents
            .render_view(&mut client, reference(), &json!({}))
            .await
            .map(|m| m.id),
    );
    configure(
        "pdf.render_origin",
        json!(format!("http://127.0.0.1:{proxy_port}")),
    )
    .await??;
    configure("pdf.chromium.binary", json!("/nonexistent/chrome")).await??;
    show(
        "missing browser",
        documents
            .render_inline(&mut client, reference(), &json!({}))
            .await
            .map(|m| m.id),
    );
    // A cold start (the browser's files read from a slow disk): 7 s before
    // the browser runs, longer than the 5 s the render itself may take.
    let cold = out.join("cold-start.sh");
    std::fs::write(
        &cold,
        format!("#!/bin/sh\nsleep 7\nexec '{chromium}' \"$@\"\n"),
    )?;
    std::fs::set_permissions(&cold, std::fs::Permissions::from_mode(0o700))?;
    configure("pdf.chromium.binary", json!(cold.display().to_string())).await??;
    configure("pdf.render_timeout_seconds", json!(5)).await??;
    let started = std::time::Instant::now();
    let printed = documents
        .render_inline(&mut client, reference(), &json!({}))
        .await
        .map(|m| m.id);
    let elapsed = started.elapsed().as_secs_f32();
    show(
        "browser that takes 7 s to start (5 s render limit)",
        printed.as_ref(),
    );
    println!("returned after {elapsed:.1}s");
    if printed.is_err() {
        return Err("a slow browser start used up the render timeout (#707)".into());
    }
    let slow = out.join("hang.sh");
    std::fs::write(&slow, "#!/bin/sh\nsleep 60\n")?;
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o700))?;
    configure("pdf.chromium.binary", json!(slow.display().to_string())).await??;
    configure("pdf.launch_timeout_seconds", json!(2)).await??;
    let started = std::time::Instant::now();
    show(
        "browser that never answers (2 s start limit)",
        documents
            .render_inline(&mut client, reference(), &json!({}))
            .await
            .map(|m| m.id),
    );
    println!("returned after {:.1}s", started.elapsed().as_secs_f32());
    configure("pdf.chromium.binary", json!(chromium)).await??;
    configure("pdf.launch_timeout_seconds", json!(60)).await??;
    configure("pdf.render_timeout_seconds", json!(30)).await??;
    println!(
        "probe hits across all refusals: {}",
        hits.load(Ordering::SeqCst) - before
    );
    ledger(&client).await?;
    Ok(())
}
