//! Running-service walkthrough of the request repairs (#561, #562, #564,
//! #565, #598). A walkthrough store (`wt_probe/probe.records`) counts its
//! locates and lists 250 records matched only by email. It shows:
//!
//! - the request page locates once per mounted page, again only on Find
//!   again (#565);
//! - matches left unconfirmed are counted on the store's answer and in
//!   `export.json` (#564);
//! - 250 event payloads holding an email page past 200 by the events store's
//!   cursor; the verify dialog moves past its 60 across two stores and keeps
//!   every tick; verify confirms records from any page and refuses a stale,
//!   forged or other tenant's one (#564);
//! - a finish attempt that stored its export and failed to file it leaves no
//!   second copy, and an erasure deletes an export no request references;
//!   a certificate rendered by a failed attempt is filed, not rendered again
//!   (#562);
//! - a failed access request drops its staged copy; a store mounted while it
//!   was failed is asked on Run again, a store no longer mounted is answered
//!   `unmounted`, and the request completes (#561, #598);
//! - placing a hold while an erasure holds the tenant's guard gives up after
//!   five seconds with "try again" (#598).
//!
//! Explicit disposable DB only (`seed` first, same variables as
//! `requests-walkthrough`). Each check fails the executable when the stored
//! state breaks the contract.
use bilibi_authz::grants::Grants;
use bilibi_database::{pool, Error, MutationBoundary, Result as DbResult};
use bilibi_module_registry::{Contribution, Module, Provenance};
use bilibi_party_ids::{OrganisationId, PersonId};
use bilibi_personal_data::{
    after_id, Candidate, Confirmed, Disclosure, Erased, Erasure, Exported, Handler, HandlerFuture,
    Holds, Identifier, KindCount, Located, PersonalData, Subject, MAX_CANDIDATES, PERSONAL_DATA,
};
use bilibi_privacy::{ConfirmedRecord, HoldTarget, NewRequest, Privacy, RequestKind, RequestView};
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::Scope;
use bilibi_ui::page::{LiveInput, LiveRequest, PageRequest, PageService};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_postgres::Client;

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[allow(dead_code)]
#[path = "support/erasure.rs"]
mod erasure_approval;

// ------------------------------------------------------------ probe store

const MODULE: &str = "wt_probe/probe";
const STORE: &str = "wt_probe/probe.records";
const KIND: &str = "wt_probe.record";
/// Records the probe matches only by an identifier: more than the 200 a
/// request lists.
const MATCHES: usize = 250;
const PROVENANCE: Provenance = Provenance {
    module: MODULE,
    source: file!(),
    line: line!(),
};
inventory::submit! { Module {
    id: MODULE, layer: bilibi_module_registry::Layer::Domains,
    dependencies: &["base/personal_data", "base/privacy"], migrations: &[], services: &[],
} }
inventory::submit! { Contribution::new(PERSONAL_DATA, STORE, PROVENANCE,
&PersonalData { label: "Probe records (walkthrough)", kinds: &[], person_subjects: &[], handler: &PROBE }) }

/// How many times any request's locate asked the probe.
static LOCATES: AtomicUsize = AtomicUsize::new(0);
struct Probe;
static PROBE: Probe = Probe;
impl Handler for Probe {
    fn locate<'a>(
        &'a self,
        _client: &'a Client,
        _scope: &'a Scope,
        subject: &'a Subject,
        after: Option<&'a str>,
    ) -> HandlerFuture<'a, Located> {
        Box::pin(async move {
            LOCATES.fetch_add(1, Ordering::SeqCst);
            let from = after_id(after)? as usize;
            let matches = if subject.identifiers.is_empty() {
                0
            } else {
                MATCHES
            };
            let kinds = vec![KindCount {
                kind: KIND.into(),
                by_person: 0,
                by_identifier: matches as i64,
            }];
            let candidates = (from + 1..=matches)
                .take(MAX_CANDIDATES + 1)
                .map(|n| Candidate {
                    kind: KIND.into(),
                    id: n.to_string(),
                    summary: format!("Probe record {n}"),
                })
                .collect();
            Ok(Located::page(kinds, candidates, |last| last.id.clone()))
        })
    }
    fn confirmable<'a>(
        &'a self,
        _client: &'a Client,
        _scope: &'a Scope,
        subject: &'a Subject,
        records: &'a [Confirmed],
    ) -> HandlerFuture<'a, Vec<Confirmed>> {
        Box::pin(async move {
            Ok(records
                .iter()
                .filter(|r| !subject.identifiers.is_empty() && r.kind == KIND)
                .filter(|r| {
                    r.id.parse::<usize>()
                        .is_ok_and(|n| (1..=MATCHES).contains(&n))
                })
                .cloned()
                .collect())
        })
    }
    fn export<'a>(
        &'a self,
        _client: &'a Client,
        _scope: &'a Scope,
        subject: &'a Subject,
        _grant: &'a Disclosure,
    ) -> HandlerFuture<'a, Exported> {
        Box::pin(async move {
            Ok(Exported {
                records: json!({"confirmed": subject.confirmed.len()}),
                files: Vec::new(),
            })
        })
    }
    fn erase<'a>(
        &'a self,
        _tx: &'a mut bilibi_database::MutationTransaction<'_>,
        _scope: &'a Scope,
        _subject: &'a Subject,
        _grant: &'a Erasure,
        _holds: &'a Holds,
    ) -> HandlerFuture<'a, Erased> {
        Box::pin(async { Ok(Erased::default()) })
    }
}

// ------------------------------------------------------------ #564 paging

const EVENTS: &str = "base/events.payloads";
const EVENT: &str = "base/events.event";
/// Records `n` events in `tenant` whose payload holds `email`.
async fn contact_events(client: &Client, tenant: i64, n: i64, email: &str) -> Result<Vec<i64>> {
    Ok(client
        .query(
            "INSERT INTO base_events(tenant_id,publisher,name,version,subject,event_key,payload,trace_id) \
             SELECT $1,'walkthrough','walkthrough.contact_logged',1,'contact:'||g,'paging-'||$3||'-'||g,jsonb_build_object('from',$3::text,'n',g),'walkthrough' \
             FROM generate_series(1,$2::bigint) g RETURNING id",
            &[&tenant, &n, &email],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}
fn event(id: i64) -> ConfirmedRecord {
    ConfirmedRecord {
        store: EVENTS.into(),
        kind: EVENT.into(),
        id: id.to_string(),
    }
}
fn probe(n: usize) -> ConfirmedRecord {
    ConfirmedRecord {
        store: STORE.into(),
        kind: KIND.into(),
        id: n.to_string(),
    }
}
/// The opening tag of the element with `id` in `html`.
fn tag<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let at = html.find(&format!("id=\"{id}\""))?;
    let start = html[..at].rfind('<')?;
    let end = at + html[at..].find('>')?;
    Some(&html[start..=end])
}
/// Whether a tag carries the `disabled` attribute (its classes name
/// `disabled:` variants too).
fn disabled(tag: &str) -> bool {
    tag.contains(" disabled ") || tag.contains(" disabled>") || tag.contains(" disabled/")
}
fn ticked(html: &str, index: usize) -> bool {
    tag(html, &format!("privacy-verify-candidate-{index}")).is_some_and(|t| t.contains(" checked"))
}
/// The candidate boxes the verify dialog shows, by index.
fn boxes(html: &str) -> Vec<usize> {
    html.match_indices("id=\"privacy-verify-candidate-")
        .filter_map(|(at, found)| {
            let rest = &html[at + found.len()..];
            rest[..rest.find('"')?].parse().ok()
        })
        .collect()
}
async fn event_input(
    live: &mut Box<dyn bilibi_ui::page::LiveState<Scope, Error>>,
    account: &Account,
    event: Value,
) -> Result<String> {
    Ok(live
        .update(LiveRequest {
            scope: &account.scope,
            credential: &account.token,
            input: LiveInput::Event(&event),
        })
        .await?
        .html)
}

/// #564: a store's candidates page past 200 by an opaque cursor, the verify
/// dialog moves past its 60 across stores and keeps every tick, and verify
/// confirms a record from any page only when its store says it is a
/// candidate now: never a stale, forged or other tenant's record.
async fn paging(
    app: &bilibi_web::App,
    privacy: &Privacy,
    client: &mut Client,
    receiver: &Account,
) -> Result<()> {
    println!("== #564: 250 event payloads hold the requester's email; the dialog pages");
    let email = "pat-paging@example.test";
    let pat = new_person(client, "Pat Paging", email).await?;
    let ids = contact_events(client, 1, 250, email).await?;
    let request = receive(
        privacy,
        client,
        &receiver.scope,
        RequestKind::Access,
        pat,
        email,
    )
    .await?;
    let pages = app
        .services
        .get::<PageService<Scope, Error>>("base/privacy.pages")?;
    let path = HashMap::from([("id".to_owned(), request.id.to_string())]);
    let query = HashMap::new();
    let Some(mut live) = pages
        .0
        .mount(PageRequest {
            operation: "request",
            scope: Some(&receiver.scope),
            path: &path,
            query: &query,
            body: &[],
            framed: false,
        })
        .await?
    else {
        return Err("the request page is not live".into());
    };
    // Every event's candidates come first (stores in key order), then the
    // probe's: 500 candidates over two stores.
    let html = event_input(&mut live, receiver, json!({"type": "open_verify"})).await?;
    check(
        boxes(&html) == (0..60).collect::<Vec<_>>(),
        &format!(
            "the dialog shows the first 60 candidates: {:?}",
            boxes(&html).len()
        ),
    )?;
    check(
        tag(&html, "privacy-verify-next").is_some_and(|t| !disabled(t))
            && !html.contains("confirm the rest through the agent operation"),
        "the dialog offers Next, not the agent operation, for the rest",
    )?;
    event_input(
        &mut live,
        receiver,
        json!({"type": "verify_ticks", "note[c0]": "true", "note[c59]": "true", "note[c1]": "false", "note[c900]": "true"}),
    )
    .await?;
    let mut html = String::new();
    for _ in 0..4 {
        html = event_input(&mut live, receiver, json!({"type": "verify_next"})).await?;
    }
    check(
        boxes(&html) == (240..300).collect::<Vec<_>>(),
        &format!(
            "four Nexts show candidates 241 to 300, across the two stores: {:?}..{:?}",
            boxes(&html).first(),
            boxes(&html).last()
        ),
    )?;
    check(
        html.contains("Showing 241 to 300, more after these; 2 ticked."),
        "the dialog says where it is and counts the ticks made on other pages",
    )?;
    event_input(
        &mut live,
        receiver,
        json!({"type": "verify_ticks", "note[c245]": "true", "note[c250]": "true", "note[c299]": "true", "note[note]": "Matched NRIC at the counter"}),
    )
    .await?;
    for _ in 0..4 {
        html = event_input(&mut live, receiver, json!({"type": "verify_next"})).await?;
    }
    check(
        boxes(&html) == (480..500).collect::<Vec<_>>()
            && tag(&html, "privacy-verify-next").is_some_and(disabled),
        &format!(
            "the last page shows candidates 481 to 500 and Next is off: {:?}..{:?}",
            boxes(&html).first(),
            boxes(&html).last()
        ),
    )?;
    for _ in 0..8 {
        html = event_input(&mut live, receiver, json!({"type": "verify_previous"})).await?;
    }
    check(
        boxes(&html) == (0..60).collect::<Vec<_>>()
            && ticked(&html, 0)
            && ticked(&html, 59)
            && !ticked(&html, 1),
        "Previous returns to the first page with its ticks kept",
    )?;
    html = event_input(
        &mut live,
        receiver,
        json!({"type": "verify", "note[c0]": "true", "note[c59]": "true", "note[note]": "Matched NRIC at the counter"}),
    )
    .await?;
    let verified = privacy
        .request(&*client, &receiver.scope, request.id)
        .await?;
    let want = vec![
        event(ids[0]),
        event(ids[59]),
        event(ids[245]),
        probe(1),
        probe(50),
    ];
    let mut got = verified.confirmed.clone();
    got.sort();
    let mut sorted = want.clone();
    sorted.sort();
    check(
        verified.status == "verified" && got == sorted,
        &format!(
            "Verify confirms the five ticked in two views of the dialog, among them event {} (past the first 200) and the probe's 50 (past the first 60 of its store) (status {}, confirmed {})",
            ids[245],
            verified.status,
            got.len()
        ),
    )?;
    check(!html.contains("privacy-verify-modal"), "the dialog closed")?;

    println!("== #564: locate pages by an opaque cursor; verify proves each record now");
    let email = "quinn-paging@example.test";
    let quinn = new_person(client, "Quinn Paging", email).await?;
    let ids = contact_events(client, 1, 250, email).await?;
    let other_tenant = contact_events(client, 2, 5, email).await?;
    let unrelated = contact_events(client, 1, 1, "someone-else@example.test").await?;
    let request = receive(
        privacy,
        client,
        &receiver.scope,
        RequestKind::Access,
        quinn,
        email,
    )
    .await?;
    let first = privacy
        .locate(&*client, &receiver.scope, request.id)
        .await?;
    let events = &first
        .iter()
        .find(|(store, _, _)| store == EVENTS)
        .ok_or("the events store did not answer")?
        .2;
    check(
        events.candidates.len() == 200
            && events.kinds.iter().map(|k| k.by_identifier).sum::<i64>() == 250,
        &format!(
            "the first page lists 200 of 250 matched (listed {}, next {:?})",
            events.candidates.len(),
            events.next
        ),
    )?;
    let after = events
        .next
        .clone()
        .ok_or("the events store gave no next page")?;
    let second = privacy
        .locate_page(&*client, &receiver.scope, request.id, EVENTS, &after)
        .await?;
    let listed: Vec<i64> = events
        .candidates
        .iter()
        .chain(&second.candidates)
        .filter_map(|c| c.id.parse().ok())
        .collect();
    check(
        second.candidates.len() == 50 && second.next.is_none() && second.kinds.is_empty() && listed == ids,
        &format!(
            "the next page lists the other 50 and is the last; together exactly the 250, none of tenant 2's (page {}, next {:?})",
            second.candidates.len(),
            second.next
        ),
    )?;
    let long = "9".repeat(129);
    for (cursor, store, what) in [
        ("not-a-cursor", EVENTS, "an unreadable cursor"),
        ("0", EVENTS, "a cursor before any record"),
        (
            long.as_str(),
            EVENTS,
            "a cursor longer than any store hands out",
        ),
    ] {
        let refused = privacy
            .locate_page(&*client, &receiver.scope, request.id, store, cursor)
            .await;
        check(
            matches!(refused, Err(Error::Invalid)),
            &format!("{what} is Invalid: {:?}", refused.err()),
        )?;
    }
    let unknown = privacy
        .locate_page(
            &*client,
            &receiver.scope,
            request.id,
            "wt_none/none.records",
            "1",
        )
        .await;
    check(
        matches!(unknown, Err(Error::NotFound)),
        &format!("a store not mounted is NotFound: {:?}", unknown.err()),
    )?;

    // The event 220 stops matching after it was listed.
    client
        .execute(
            "UPDATE base_events SET payload=jsonb_build_object('n',0) WHERE id=$1",
            &[&ids[220]],
        )
        .await?;
    let refusals = [
        (
            vec![event(ids[210]), event(ids[220])],
            "a record listed on page 2 that no longer matches (stale)",
        ),
        (
            vec![event(other_tenant[0])],
            "another tenant's event holding the same email",
        ),
        (
            vec![event(unrelated[0])],
            "an event that never matched (forged)",
        ),
        (
            vec![event(ids[230]), probe(MATCHES + 1)],
            "a record past the probe's last",
        ),
        (
            vec![ConfirmedRecord {
                store: "wt_none/none.records".into(),
                kind: EVENT.into(),
                id: ids[0].to_string(),
            }],
            "a store not mounted",
        ),
        (
            (1..=801).map(probe).collect(),
            "801 records, more than one verification confirms",
        ),
    ];
    for (confirm, what) in refusals {
        let refused = verify(privacy, client, &receiver.scope, &request, &confirm).await;
        let invalid = refused
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<Error>())
            .is_some_and(|e| matches!(e, Error::Invalid));
        check(invalid, &format!("verify refuses {what} as Invalid"))?;
    }
    let still = privacy
        .request(&*client, &receiver.scope, request.id)
        .await?;
    check(
        still.status == "received" && still.confirmed.is_empty(),
        "a refused verify confirms nothing",
    )?;
    let mut confirm: Vec<ConfirmedRecord> = ids
        .iter()
        .filter(|id| **id != ids[220])
        .map(|id| event(*id))
        .collect();
    confirm.extend((201..=MATCHES).map(probe));
    verify(privacy, client, &receiver.scope, &request, &confirm).await?;
    let verified = privacy
        .request(&*client, &receiver.scope, request.id)
        .await?;
    check(
        verified.status == "verified" && verified.confirmed.len() == 249 + 50,
        &format!(
            "verify confirms 249 events and the probe's 201 to 250, all past one page (confirmed {})",
            verified.confirmed.len()
        ),
    )?;

    println!("== #564: Consent's candidates, one per address, page by rank and ID");
    const CONSENT: &str = "core/consent.consent";
    let ria = new_person(client, "Ria Rank", "ria-rank@example.test").await?;
    // 230 checks of numbers holding the digits the requester gave, each its
    // own address, so its own candidate.
    client
        .execute(
            "INSERT INTO core_consent_log(tenant_id,company_id,address,channel,purpose,key,at,outcome,caller) \
             SELECT 1,1,'wa:6012777888'||g,'messaging','walkthrough','paging-'||g,clock_timestamp(),'allowed','walkthrough' \
             FROM generate_series(1,230) g",
            &[],
        )
        .await?;
    let request = privacy
        .receive(
            client,
            &receiver.scope,
            NewRequest {
                kind: RequestKind::Access,
                person: ria,
                identifiers: vec![Identifier::Phone("+60 12-777 888".into())],
                company: OrganisationId::new(1)?,
            },
        )
        .await?;
    let first = privacy
        .locate(&*client, &receiver.scope, request.id)
        .await?;
    let consent = &first
        .iter()
        .find(|(store, _, _)| store == CONSENT)
        .ok_or("the consent store did not answer")?
        .2;
    let after = consent
        .next
        .clone()
        .ok_or("the consent store gave no next page")?;
    let rest = privacy
        .locate_page(&*client, &receiver.scope, request.id, CONSENT, &after)
        .await?;
    let all: Vec<ConfirmedRecord> = consent
        .candidates
        .iter()
        .chain(&rest.candidates)
        .map(|c| ConfirmedRecord {
            store: CONSENT.into(),
            kind: c.kind.clone(),
            id: c.id.clone(),
        })
        .collect();
    let distinct: std::collections::BTreeSet<_> = all.iter().collect();
    check(
        consent.candidates.len() == 200
            && after.starts_with("3:")
            && rest.candidates.len() == 30
            && rest.next.is_none()
            && distinct.len() == 230,
        &format!(
            "230 addresses page as 200 and 30 after cursor {after} (next {:?}), each once",
            rest.next
        ),
    )?;
    verify(privacy, client, &receiver.scope, &request, &all).await?;
    let verified = privacy
        .request(&*client, &receiver.scope, request.id)
        .await?;
    check(
        verified.confirmed.len() == 230,
        &format!(
            "verify confirms all 230 consent candidates from both pages (confirmed {})",
            verified.confirmed.len()
        ),
    )?;
    Ok(())
}

// ----------------------------------------------------------------- helpers

fn now_step() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        / 30
}
/// The authenticator code for `key` (base32) at the current 30-second step.
fn totp(key: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut secret = Vec::new();
    let (mut buffer, mut bits) = (0u64, 0u32);
    for c in key.chars().filter(|c| c.is_ascii_alphanumeric()) {
        let value = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
            .find(c.to_ascii_uppercase())
            .expect("base32") as u64;
        buffer = buffer << 5 | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            secret.push((buffer >> bits) as u8);
        }
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(&secret).expect("key");
    mac.update(&(now_step() as u64).to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let at = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = u32::from_be_bytes([
        digest[at] & 0x7f,
        digest[at + 1],
        digest[at + 2],
        digest[at + 3],
    ]);
    format!("{:06}", binary % 1_000_000)
}

/// One signed-in account, its session token and its authenticator.
struct Account {
    scope: Scope,
    token: String,
    key: String,
    used: i64,
}
impl Account {
    async fn enroll(app: &bilibi_web::App, email: &str) -> Result<Self> {
        let login = app
            .user
            .login(email, bilibi_walkthrough_fixture::PASSWORD)
            .await?;
        let scope = app.security.guard(&login.token).await?;
        let enrollment = app
            .user
            .begin_two_factor(&scope, bilibi_walkthrough_fixture::PASSWORD)
            .await
            .map_err(|e| format!("begin two-factor: {e:?}"))?;
        let mut account = Self {
            scope,
            token: login.token,
            key: enrollment.key,
            used: 0,
        };
        let first = account.code().await;
        app.user
            .confirm_two_factor(&account.scope, &first)
            .await
            .map_err(|e| format!("confirm two-factor: {e:?}"))?;
        Ok(account)
    }
    async fn code(&mut self) -> String {
        while now_step() <= self.used {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        self.used = now_step();
        totp(&self.key)
    }
    async fn step_up(&mut self) -> Scope {
        let code = self.code().await;
        self.scope.clone().presenting_step_up(&code)
    }
}

/// Fails the walkthrough unless `ok`, after printing the check.
fn check(ok: bool, what: &str) -> Result<()> {
    println!("  [{}] {what}", if ok { "ok" } else { "FAILED" });
    if ok {
        Ok(())
    } else {
        Err(format!("check failed: {what}").into())
    }
}
async fn count(client: &Client, sql: &str) -> Result<i64> {
    Ok(client.query_one(sql, &[]).await?.get(0))
}
/// Waits up to `seconds` for `sql` (one bigint) to reach at least `want`.
async fn reach(client: &Client, sql: &str, want: i64, seconds: u64) -> Result<i64> {
    let mut seen = 0;
    for _ in 0..seconds {
        seen = count(client, sql).await?;
        if seen >= want {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(seen)
}
/// Waits up to `seconds` while the request is `approved` or `running`.
async fn settle(
    privacy: &Privacy,
    client: &Client,
    scope: &Scope,
    id: i64,
    seconds: u64,
) -> Result<RequestView> {
    for _ in 0..seconds {
        let request = privacy.request(client, scope, id).await?;
        if !matches!(request.status.as_str(), "running" | "approved") {
            return Ok(request);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(privacy.request(client, scope, id).await?)
}
fn summary(request: &RequestView) {
    println!(
        "  request {} {:?} status={} export={} certificate={} finish_attempts={}",
        request.id,
        request.kind,
        request.status,
        request.export_artifact.is_some(),
        request.certificate_artifact.is_some(),
        request.finish_attempts
    );
    for store in &request.stores {
        println!(
            "    store {:<32} {:<10} attempts={} error={:?}",
            store.store, store.state, store.attempts, store.last_error
        );
    }
}
async fn new_person(client: &Client, name: &str, email: &str) -> Result<PersonId> {
    let id: i64 = client
        .query_one(
            "INSERT INTO persons(tenant_id,code,name,kind,email) \
             SELECT 1, trim(both '_' from lower(regexp_replace($1,'[^A-Za-z0-9]+','_','g'))) || '_' || (coalesce(max(id),0)+1), $1, 'human', $2 FROM persons RETURNING id",
            &[&name, &email],
        )
        .await?
        .get(0);
    Ok(PersonId::new(id)?)
}
async fn receive(
    privacy: &Privacy,
    client: &mut Client,
    receiver: &Scope,
    kind: RequestKind,
    person: PersonId,
    email: &str,
) -> Result<RequestView> {
    Ok(privacy
        .receive(
            client,
            receiver,
            NewRequest {
                kind,
                person,
                identifiers: vec![Identifier::Email(email.into())],
                company: OrganisationId::new(1)?,
            },
        )
        .await?)
}
async fn verify(
    privacy: &Privacy,
    client: &mut Client,
    receiver: &Scope,
    request: &RequestView,
    confirm: &[ConfirmedRecord],
) -> Result<RequestView> {
    Ok(privacy
        .verify(
            client,
            receiver,
            request.id,
            request.version,
            "Identity checked at the counter",
            confirm,
        )
        .await?)
}
/// Live request documents of `kind` filed under `request`'s Artifacts subject.
async fn documents(client: &Client, request: i64, kind: &str) -> Result<Vec<String>> {
    Ok(client
        .query(
            "SELECT public_id FROM base_artifacts WHERE owner_id='base.privacy' AND subject=$1 AND kind=$2 \
             AND ready_at IS NOT NULL AND deleted_at IS NULL ORDER BY id",
            &[&format!("request:{request}"), &kind],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}
/// Makes a retrying job of `request` on `worker` run now (disposable DB only).
async fn hurry(client: &Client, worker: &str, request: i64, last: bool) -> Result<()> {
    client
        .execute(
            &format!(
                "UPDATE base_queue_jobs SET available_at=clock_timestamp(){} WHERE worker_id=$1 AND args->>'request_id'=$2 AND state='retryable'",
                if last { ",max_attempts=attempt+1" } else { "" }
            ),
            &[&worker, &request.to_string()],
        )
        .await?;
    Ok(())
}
/// `export.json` of a completed access request, read as the operator does.
async fn export_json(
    privacy: &Privacy,
    client: &mut Client,
    scope: &Scope,
    id: i64,
) -> Result<Value> {
    let (_, bytes, _) = privacy
        .request_document(client, scope, id, Some("Walkthrough check"))
        .await?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let mut text = String::new();
    archive.by_name("export.json")?.read_to_string(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}
fn store_entry<'a>(export: &'a Value, store: &str) -> Option<&'a Value> {
    export["stores"]
        .as_array()?
        .iter()
        .find(|s| s["store"] == store)
}

/// Mounts the request page as `account` and checks how many locates its
/// first render, four more inputs and Find again cost.
async fn page_locates(app: &bilibi_web::App, account: &Account, id: i64) -> Result<()> {
    let pages = app
        .services
        .get::<PageService<Scope, Error>>("base/privacy.pages")?;
    let path = HashMap::from([("id".to_owned(), id.to_string())]);
    let query = HashMap::new();
    let Some(mut live) = pages
        .0
        .mount(PageRequest {
            operation: "request",
            scope: Some(&account.scope),
            path: &path,
            query: &query,
            body: &[],
            framed: false,
        })
        .await?
    else {
        return Err("the request page is not live".into());
    };
    let start = LOCATES.load(Ordering::SeqCst);
    let refresh = live
        .update(LiveRequest {
            scope: &account.scope,
            credential: &account.token,
            input: LiveInput::Refresh,
        })
        .await?;
    let mounted = LOCATES.load(Ordering::SeqCst) - start;
    check(
        mounted == 1,
        &format!("the mounted page located once (asked the probe {mounted} time(s))"),
    )?;
    check(
        refresh.html.contains("privacy-locate-again"),
        "the open request offers Find again",
    )?;
    for event in [
        json!({"type": "open_refuse"}),
        json!({"type": "close_modal"}),
        json!({"type": "open_verify"}),
    ] {
        live.update(LiveRequest {
            scope: &account.scope,
            credential: &account.token,
            input: LiveInput::Event(&event),
        })
        .await?;
    }
    live.update(LiveRequest {
        scope: &account.scope,
        credential: &account.token,
        input: LiveInput::Refresh,
    })
    .await?;
    let inputs = LOCATES.load(Ordering::SeqCst) - start;
    check(
        inputs == 1,
        &format!("a refresh, a dialog opened and closed and the verify dialog located nothing more (total {inputs})"),
    )?;
    let again = json!({"type": "locate_again"});
    live.update(LiveRequest {
        scope: &account.scope,
        credential: &account.token,
        input: LiveInput::Event(&again),
    })
    .await?;
    let found = LOCATES.load(Ordering::SeqCst) - start;
    check(
        found == 2,
        &format!("Find again located once more (total {found})"),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var("BILIBI_DISPOSABLE_PRIVACY_DEMO").as_deref() != Ok("yes") {
        return Err("explicit disposable DB opt-in required".into());
    }
    let root = std::env::var("BILIBI_PRIVACY_DEMO_ROOT")?;
    std::fs::create_dir_all(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    bilibi_walkthrough_fixture::install(&db).await?;
    if std::env::args().nth(1).as_deref() == Some("seed") {
        return Ok(bilibi_web::seed::production(&db).await?);
    }
    // ADR 19: the file handlers need explicit desks, not an unlinked admin.
    db.get().await?.batch_execute("INSERT INTO persons(id,tenant_id,code,name,kind) VALUES(4,1,'alex_admin','Alex Admin','human') ON CONFLICT DO NOTHING; \
        UPDATE users SET person_id=4 WHERE tenant_id=1 AND id=4; \
        INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,4,1),(1,3,1),(1,4,2),(1,3,2) ON CONFLICT DO NOTHING; \
        SELECT setval(pg_get_serial_sequence('persons','id'),(SELECT max(id) FROM persons));").await?;
    let app = bilibi_web::App::new(db.clone(), "http://localhost:35575")?;
    let grants = app.services.get::<Grants>("base/authz.grants")?;
    grants.reconcile().await?;
    let mut admin = Account::enroll(&app, "admin@example.test").await?;
    let second = Account::enroll(&app, "ai@agent.my").await?;
    let privacy = app.services.get::<Privacy>(bilibi_privacy::SERVICE)?;
    let boundary = app
        .services
        .get::<MutationBoundary>("base/database.mutations")?;
    let settings = app.services.get::<Settings>("base/settings.store")?;
    let mut client = db.get().await?;
    erasure_approval::publish_rules(&app, &mut client, &admin.scope, 1).await?;
    {
        let mut tx = boundary
            .begin(
                &mut client,
                admin
                    .scope
                    .mutation_context("privacy.walkthrough.configure"),
            )
            .await?;
        let chromium =
            std::env::var("BILIBI_PRIVACY_DEMO_CHROMIUM").unwrap_or_else(|_| "chromium".into());
        for (key, value) in [
            ("media.disks", json!({"local": root})),
            ("media.attachment_disk", json!("local")),
            ("artifacts.storage_root", json!(root)),
            ("pdf.chromium.binary", json!(chromium)),
        ] {
            settings
                .put(&mut tx, &admin.scope, Target::Global, key, value)
                .await?;
        }
        tx.commit().await?;
    }
    // A request document's filing fails while its request is listed here.
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS walkthrough_unfiled(request_id bigint PRIMARY KEY); \
             CREATE OR REPLACE FUNCTION walkthrough_unfiled() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               IF EXISTS (SELECT 1 FROM walkthrough_unfiled WHERE request_id = NEW.id) \
                  AND ((NEW.export_artifact IS NOT NULL AND OLD.export_artifact IS NULL) \
                    OR (NEW.certificate_artifact IS NOT NULL AND OLD.certificate_artifact IS NULL)) THEN \
                 RAISE EXCEPTION 'walkthrough: filing the document fails'; END IF; RETURN NEW; END $$; \
             DROP TRIGGER IF EXISTS walkthrough_unfiled ON base_privacy_requests; \
             CREATE TRIGGER walkthrough_unfiled BEFORE UPDATE ON base_privacy_requests \
               FOR EACH ROW EXECUTE FUNCTION walkthrough_unfiled();",
        )
        .await?;
    let executor = bilibi_base::queue::Executor::from_services(
        &app.services,
        bilibi_base::queue::Config::default(),
    )?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let workers = tokio::spawn(executor.run(stopped));

    println!("== #565: the request page locates once per mounted page");
    let pat = new_person(&client, "Pat Page", "pat-page@example.test").await?;
    let paged = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        pat,
        "pat-page@example.test",
    )
    .await?;
    page_locates(&app, &admin, paged.id).await?;

    println!("== #564: matches past the candidate cap are counted, not lost");
    let located = privacy.locate(&client, &admin.scope, paged.id).await?;
    let probe = located
        .iter()
        .find(|(store, _, _)| store == STORE)
        .map(|(_, _, found)| found)
        .ok_or("the probe store did not answer locate")?;
    check(
        probe.candidates.len() == 200 && probe.next.is_some(),
        &format!(
            "{MATCHES} matches are listed as a first page of 200 (listed {}, next {:?})",
            probe.candidates.len(),
            probe.next
        ),
    )?;
    let confirm: Vec<ConfirmedRecord> = ["1", "2", "3"]
        .iter()
        .map(|id| ConfirmedRecord {
            store: STORE.into(),
            kind: KIND.into(),
            id: (*id).into(),
        })
        .collect();
    let paged = verify(&privacy, &mut client, &second.scope, &paged, &confirm).await?;
    let paged = privacy
        .approve(&mut client, &admin.scope, paged.id, paged.version)
        .await?;
    let paged = settle(&privacy, &client, &second.scope, paged.id, 90).await?;
    summary(&paged);
    check(paged.status == "completed", "the access request completed")?;
    let left = paged
        .stores
        .iter()
        .find(|s| s.store == STORE)
        .and_then(|s| s.result.as_ref())
        .and_then(|r| r["unconfirmed"].as_i64());
    check(
        left == Some(MATCHES as i64 - 3),
        &format!(
            "the probe's answer counts {} left unconfirmed: {left:?}",
            MATCHES - 3
        ),
    )?;
    let export = export_json(&privacy, &mut client, &admin.scope, paged.id).await?;
    let entry = store_entry(&export, STORE).ok_or("export.json has no probe store")?;
    check(
        entry["unconfirmed_matches"].as_i64() == Some(MATCHES as i64 - 3),
        &format!(
            "export.json names {} matches left unconfirmed: {}",
            MATCHES - 3,
            entry["unconfirmed_matches"]
        ),
    )?;

    paging(&app, &privacy, &mut client, &second).await?;

    println!("== #562: a finish attempt that fails to file its export leaves no second copy");
    let eve = new_person(&client, "Eve Unfiled", "eve-unfiled@example.test").await?;
    let first = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        eve,
        "eve-unfiled@example.test",
    )
    .await?;
    let first = verify(&privacy, &mut client, &second.scope, &first, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&first.id],
        )
        .await?;
    privacy
        .approve(&mut client, &admin.scope, first.id, first.version)
        .await?;
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        first.id
    );
    check(
        reach(&client, &attempts, 1, 90).await? >= 1,
        "the first finish attempt failed to file its export",
    )?;
    let orphans = documents(&client, first.id, "export").await?;
    check(
        orphans.len() == 1,
        &format!("one unfiled export is stored ({})", orphans.len()),
    )?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, first.id, false).await?;
    check(
        reach(&client, &attempts, 2, 90).await? >= 2,
        "the second finish attempt failed too",
    )?;
    let after = documents(&client, first.id, "export").await?;
    check(
        after.len() == 1 && after != orphans,
        &format!(
            "the retry deleted the unfiled export before building its own: {} live, first gone {}",
            after.len(),
            !after.contains(&orphans[0])
        ),
    )?;
    client
        .execute(
            "DELETE FROM walkthrough_unfiled WHERE request_id=$1",
            &[&first.id],
        )
        .await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, first.id, false).await?;
    let first = settle(&privacy, &client, &second.scope, first.id, 90).await?;
    summary(&first);
    let live = documents(&client, first.id, "export").await?;
    check(
        first.status == "completed"
            && live.len() == 1
            && first.export_artifact.as_ref() == live.first(),
        &format!(
            "completed with exactly its filed export live ({} live)",
            live.len()
        ),
    )?;

    println!("== #598: a failed access request keeps no staged copy");
    let unfiled = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        eve,
        "eve-unfiled@example.test",
    )
    .await?;
    let unfiled = verify(&privacy, &mut client, &second.scope, &unfiled, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&unfiled.id],
        )
        .await?;
    privacy
        .approve(&mut client, &admin.scope, unfiled.id, unfiled.version)
        .await?;
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        unfiled.id
    );
    reach(&client, &attempts, 1, 90).await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, unfiled.id, true).await?;
    let failed = format!(
        "SELECT (status='failed')::int::bigint FROM base_privacy_requests WHERE id={}",
        unfiled.id
    );
    check(
        reach(&client, &failed, 1, 90).await? == 1,
        "the access request whose finish ran out of attempts is failed",
    )?;
    let staged = count(
        &client,
        &format!(
            "SELECT count(*) FROM base_privacy_request_exports WHERE request_id={}",
            unfiled.id
        ),
    )
    .await?;
    check(
        staged == 0,
        &format!("its staged copy is dropped ({staged} staged rows)"),
    )?;
    check(
        documents(&client, unfiled.id, "export").await?.len() == 1,
        "its last attempt left one export no request references",
    )?;

    println!("== #562: the person's erasure deletes every export, filed or not, and files one certificate");
    let erasure = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Erasure,
        eve,
        "eve-unfiled@example.test",
    )
    .await?;
    let erasure = verify(&privacy, &mut client, &second.scope, &erasure, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&erasure.id],
        )
        .await?;
    let erasure = erasure_approval::decide(
        &app,
        &privacy,
        &mut client,
        &second.scope,
        &admin.step_up().await,
        &erasure,
        true,
    )
    .await?;
    println!("  erasure {} approved: {}", erasure.id, erasure.status);
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        erasure.id
    );
    check(
        reach(&client, &attempts, 1, 120).await? >= 1,
        "the erasure's first finish attempt failed to file its certificate",
    )?;
    let rendered = documents(&client, erasure.id, "certificate").await?;
    check(
        rendered.len() == 1,
        &format!("one unfiled certificate is stored ({})", rendered.len()),
    )?;
    client
        .execute(
            "DELETE FROM walkthrough_unfiled WHERE request_id=$1",
            &[&erasure.id],
        )
        .await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, erasure.id, false).await?;
    let erasure = settle(&privacy, &client, &second.scope, erasure.id, 120).await?;
    summary(&erasure);
    let certificates = documents(&client, erasure.id, "certificate").await?;
    check(
        erasure.status == "completed"
            && certificates == rendered
            && erasure.certificate_artifact.as_ref() == certificates.first(),
        &format!(
            "completed, filing the certificate already rendered ({} certificate(s))",
            certificates.len()
        ),
    )?;
    let left: usize = documents(&client, first.id, "export").await?.len()
        + documents(&client, unfiled.id, "export").await?.len();
    check(
        left == 0,
        &format!("no export of the person is left, filed or unfiled ({left})"),
    )?;

    println!("== #561: a store mounted and a store removed while a request is failed");
    let mo = new_person(&client, "Mo Mounted", "mo-mounted@example.test").await?;
    let moved = receive(
        &privacy,
        &mut client,
        &second.scope,
        RequestKind::Access,
        mo,
        "mo-mounted@example.test",
    )
    .await?;
    let moved = verify(&privacy, &mut client, &second.scope, &moved, &[]).await?;
    client
        .execute(
            "INSERT INTO walkthrough_unfiled(request_id) VALUES($1)",
            &[&moved.id],
        )
        .await?;
    privacy
        .approve(&mut client, &admin.scope, moved.id, moved.version)
        .await?;
    let attempts = format!(
        "SELECT finish_attempts::bigint FROM base_privacy_requests WHERE id={}",
        moved.id
    );
    reach(&client, &attempts, 1, 90).await?;
    hurry(&client, bilibi_privacy::FINISH_WORKER, moved.id, true).await?;
    let failed = format!(
        "SELECT (status='failed')::int::bigint FROM base_privacy_requests WHERE id={}",
        moved.id
    );
    check(
        reach(&client, &failed, 1, 90).await? == 1,
        "the access request is failed",
    )?;
    client
        .execute(
            "DELETE FROM walkthrough_unfiled WHERE request_id=$1",
            &[&moved.id],
        )
        .await?;
    // As if the probe's module were mounted after the request ran, and a
    // store's module removed since (disposable database only).
    client
        .execute(
            "DELETE FROM base_privacy_request_stores WHERE request_id=$1 AND store=$2",
            &[&moved.id, &STORE],
        )
        .await?;
    client
        .execute(
            "INSERT INTO base_privacy_request_stores(request_id,tenant_id,store) VALUES($1,1,'wt_gone/gone.records')",
            &[&moved.id],
        )
        .await?;
    println!("  the probe store's row is gone and a row of a removed store waits");
    let current = privacy.request(&**client, &second.scope, moved.id).await?;
    let again = privacy
        .run_again(&mut client, &second.scope, moved.id, current.version)
        .await;
    check(
        again.is_ok(),
        &format!(
            "Run again is accepted: {:?}",
            again.as_ref().map(|r| &r.status)
        ),
    )?;
    let moved = settle(&privacy, &client, &second.scope, moved.id, 120).await?;
    summary(&moved);
    let state = |store: &str| {
        moved
            .stores
            .iter()
            .find(|s| s.store == store)
            .map(|s| s.state.clone())
    };
    check(moved.status == "completed", "the request completed")?;
    check(
        state(STORE).as_deref() == Some("answered"),
        &format!("the store mounted since was asked: {:?}", state(STORE)),
    )?;
    check(
        state("wt_gone/gone.records").as_deref() == Some("unmounted"),
        &format!(
            "the removed store is answered unmounted: {:?}",
            state("wt_gone/gone.records")
        ),
    )?;
    let export = export_json(&privacy, &mut client, &admin.scope, moved.id).await?;
    check(
        export["stores_not_installed"] == json!(["wt_gone/gone.records"])
            && store_entry(&export, STORE).is_some(),
        &format!(
            "export.json has the probe store and names the removed one: {}",
            export["stores_not_installed"]
        ),
    )?;

    println!("== #598: a hold placed while an erasure holds the guard gives up with try again");
    let guard = db.get().await?;
    guard
        .batch_execute("BEGIN; SELECT pg_advisory_xact_lock_shared(841330, 1)")
        .await?;
    let started = std::time::Instant::now();
    let placed = {
        let mut client = db.get().await?;
        privacy
            .place_hold(
                &mut client,
                &admin.scope,
                HoldTarget::Person { person: mo },
                "Placed during an erasure",
            )
            .await
    };
    let waited = started.elapsed();
    guard.batch_execute("COMMIT").await?;
    let refused = match &placed {
        Err(Error::Refused(message)) => message.contains("Try again"),
        _ => false,
    };
    check(
        refused && waited >= Duration::from_secs(4) && waited < Duration::from_secs(30),
        &format!(
            "refused after {} ms: {:?}",
            waited.as_millis(),
            placed
                .as_ref()
                .map(|hold| hold.id)
                .map_err(|e| e.to_string())
        ),
    )?;
    let placed: DbResult<_> = privacy
        .place_hold(
            &mut client,
            &admin.scope,
            HoldTarget::Person { person: mo },
            "Placed once the erasure committed",
        )
        .await;
    check(
        placed.is_ok(),
        "the same hold is placed once the guard is free",
    )?;

    stop.send(true)?;
    workers.await??;
    println!("all checks passed");
    Ok(())
}
