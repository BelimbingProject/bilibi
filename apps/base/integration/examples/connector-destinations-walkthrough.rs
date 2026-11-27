//! Connector destination policy walkthrough against a disposable database.
//!
//! Sets the two operator settings through Base Settings, loads the policy the
//! way a connector does, and shows which hosts it refuses before anything is
//! sent: loopback, link-local and cloud metadata, unspecified, multicast and
//! broadcast addresses, their IPv4-mapped IPv6 forms, private ranges unless the
//! operator opens them, and names that resolve to any of these. A name that
//! gives no address is the same refusal, so a tenant cannot tell which internal
//! names exist. Public addresses are admitted; an allowed host or block
//! connects to a local server on the checked address only, within one time
//! limit. Refused setting values are named. Exits non-zero when a check fails.
//! Run against a freshly migrated database.
use bilibi_database::{pool, MutationBoundary};
use bilibi_integration::{
    ConnectorPolicy, Unusable, CONNECTOR_ALLOWED_DESTINATIONS, CONNECTOR_PRIVATE_NETWORKS,
    DESTINATION_REFUSED,
};
use bilibi_module_registry::Services;
use bilibi_settings::{Settings, Target};
use bilibi_tenancy::{Authentication, Scope};
use serde_json::{json, Value};
use std::{
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::{Duration, Instant},
};

type Outcome<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Proof {
    settings: Arc<Settings>,
    boundary: Arc<MutationBoundary>,
    db: bilibi_database::Db,
    operator: Scope,
    failed: usize,
}

impl Proof {
    fn check(&mut self, label: &str, okay: bool) {
        println!("{} {label}", if okay { "ok  " } else { "FAIL" });
        if !okay {
            self.failed += 1;
        }
    }

    /// Writes one global setting as the operator; `false` when it is refused.
    async fn put(&self, key: &str, value: Value) -> Outcome<bool> {
        let mut client = self.db.get().await?;
        let mut tx = self
            .boundary
            .begin(
                &mut client,
                self.operator
                    .mutation_context("integration.walkthrough.connector_destinations"),
            )
            .await?;
        match self
            .settings
            .put(&mut tx, &self.operator, Target::Global, key, value)
            .await
        {
            Ok(()) => {
                tx.commit().await?;
                Ok(true)
            }
            Err(bilibi_database::Error::Invalid) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn policy(&self) -> Outcome<ConnectorPolicy> {
        let client = self.db.get().await?;
        Ok(ConnectorPolicy::load(&self.settings, &**client).await?)
    }
}

/// What the policy says about a host and port, as a connector would report it.
fn verdict(policy: &ConnectorPolicy, host: &str, port: u16) -> String {
    match policy.resolve(host, port) {
        Ok(destination) => format!("admitted {:?}", destination.addresses()),
        Err(Unusable) => DESTINATION_REFUSED.into(),
    }
}

/// Whether a name that gives no address and a name that gives a forbidden
/// address get the very same answer: a tenant must not learn which internal
/// names exist.
fn indistinguishable(policy: &ConnectorPolicy, missing: &str, forbidden: &str, port: u16) -> bool {
    let (missing, forbidden) = (
        policy.resolve(missing, port),
        policy.resolve(forbidden, port),
    );
    missing.is_err() && missing.err() == forbidden.err()
}

/// Every one of `hosts` gets `expected` (`admitted` matches any address list).
fn all(
    proof: &mut Proof,
    policy: &ConnectorPolicy,
    label: &str,
    hosts: &[(&str, u16)],
    expected: &str,
) {
    for (host, port) in hosts {
        let got = verdict(policy, host, *port);
        proof.check(
            &format!("{label}: {host} port {port} -> {got}"),
            got.starts_with(expected),
        );
    }
}

#[tokio::main]
async fn main() -> Outcome {
    if std::env::var("BILIBI_DISPOSABLE_INTEGRATION_DEMO").as_deref() != Ok("yes") {
        return Err("set BILIBI_DISPOSABLE_INTEGRATION_DEMO=yes".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    let modules = bilibi_module_registry::discover()?;
    bilibi_database::migrate(&db, &modules).await?;
    let mut services = Services::default();
    services.provide("bootstrap.database", Arc::new(db.clone()))?;
    services.provide("bootstrap.composition", Arc::new(bilibi_web::composition()))?;
    services.install(&modules)?;
    let authentication = services.get::<Authentication>("base/tenancy.authentication")?;
    // Fixture: tenant 1 is the operator; user 4 is its grant_all admin.
    let operator = {
        let client = db.get().await?;
        client.execute("INSERT INTO platform_operator(singleton,tenant_id) VALUES(true,1) ON CONFLICT DO NOTHING", &[]).await?;
        authentication
            .authenticate(&**client, 1, 4, "walkthrough".into())
            .await?
    };
    let mut proof = Proof {
        settings: services.get::<Settings>("base/settings.store")?,
        boundary: services.get::<MutationBoundary>("base/database.mutations")?,
        db: db.clone(),
        operator,
        failed: 0,
    };

    println!("== the default policy: public addresses only");
    let policy = proof.policy().await?;
    let local = [
        ("127.0.0.1", 993),
        ("::1", 993),
        ("[::1]", 993),
        ("localhost", 993),
        ("0.0.0.0", 993),
        ("0.1.2.3", 993),
        ("::", 993),
        ("169.254.169.254", 80),
        ("fe80::1", 993),
        ("224.0.0.1", 993),
        ("ff02::1", 993),
        ("255.255.255.255", 993),
        ("::ffff:127.0.0.1", 993),
        ("::ffff:169.254.169.254", 80),
        ("fd00:ec2::254", 80),
        ("100.100.100.200", 80),
        ("192.0.0.192", 80),
        ("64:ff9b::a9fe:a9fe", 80),
        ("2002:a9fe:a9fe::1", 80),
        ("100.64.0.1", 993),
        ("192.0.2.10", 993),
        ("198.18.0.1", 993),
        ("240.0.0.1", 993),
    ];
    all(
        &mut proof,
        &policy,
        "loopback, link-local, metadata and other local addresses are refused",
        &local,
        DESTINATION_REFUSED,
    );
    let private = [
        ("10.0.0.1", 993),
        ("172.16.0.1", 5039),
        ("192.168.1.1", 465),
        ("fd12:3456::1", 993),
        ("::ffff:10.0.0.1", 993),
    ];
    all(
        &mut proof,
        &policy,
        "private ranges are refused by default",
        &private,
        DESTINATION_REFUSED,
    );
    // Spellings a resolver may accept for loopback: whatever they resolve to,
    // they never connect.
    for host in ["127.1", "2130706433", "0x7f.0.0.1"] {
        let got = verdict(&policy, host, 993);
        proof.check(
            &format!("another spelling of loopback is never admitted: {host} -> {got}"),
            !got.starts_with("admitted"),
        );
    }
    proof.check(
        "port 0 is refused",
        verdict(&policy, "1.1.1.1", 0) == DESTINATION_REFUSED,
    );
    all(
        &mut proof,
        &policy,
        "public addresses are admitted",
        &[
            ("1.1.1.1", 993),
            ("8.8.8.8", 465),
            ("2606:4700:4700::1111", 993),
        ],
        "admitted",
    );
    proof.check(
        "a name that gives no address is refused with the one code",
        verdict(&policy, "no-such-host.invalid", 993) == DESTINATION_REFUSED,
    );
    proof.check(
        "and it cannot be told from a name that gives a forbidden address, so internal names cannot be enumerated",
        indistinguishable(&policy, "no-such-host.invalid", "localhost", 993),
    );
    // Public names that resolve to local addresses (a wildcard DNS service):
    // the check reads the resolved address, never the name. Without a public
    // resolver they give no address, which is the same refusal.
    for host in [
        "127.0.0.1.nip.io",
        "10.0.0.1.nip.io",
        "169.254.169.254.nip.io",
    ] {
        let got = verdict(&policy, host, 993);
        proof.check(
            &format!("a public name resolving to a local address is refused: {host} -> {got}"),
            got == DESTINATION_REFUSED,
        );
    }

    println!("== the operator opens private networks");
    proof.check(
        "the operator turns private networks on",
        proof.put(CONNECTOR_PRIVATE_NETWORKS, json!(true)).await?,
    );
    let policy = proof.policy().await?;
    all(
        &mut proof,
        &policy,
        "private ranges are admitted",
        &private,
        "admitted",
    );
    all(
        &mut proof,
        &policy,
        "an IPv4-mapped private address is admitted like its IPv4 form",
        &[("::ffff:10.0.0.1", 993)],
        "admitted",
    );
    all(
        &mut proof,
        &policy,
        "loopback, link-local, metadata and other local addresses stay refused",
        &local,
        DESTINATION_REFUSED,
    );
    proof.check(
        "with private networks open, a missing internal name still cannot be told from loopback",
        indistinguishable(&policy, "pbx.invalid", "localhost", 5039),
    );
    proof.check(
        "the operator turns private networks off again",
        proof.put(CONNECTOR_PRIVATE_NETWORKS, json!(false)).await?,
    );

    println!("== the operator allows named destinations");
    // A local server stands in for the operator's own mail server or phone
    // system. Accepting never waits: a connection that failed leaves nothing.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let other = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let timeout = Duration::from_secs(2);
    let loopback: SocketAddr = ([127, 0, 0, 1], port).into();

    proof.check(
        "an allowed host on one port is saved",
        proof
            .put(
                CONNECTOR_ALLOWED_DESTINATIONS,
                json!([format!("localhost:{port}")]),
            )
            .await?,
    );
    let policy = proof.policy().await?;
    let destination = policy.resolve("localhost", port);
    let connected = destination
        .as_ref()
        .ok()
        .and_then(|destination| destination.connect(timeout).ok())
        .and_then(|stream| stream.peer_addr().ok());
    proof.check(
        &format!("the allowed host connects, to a checked address: {connected:?}"),
        connected == Some(loopback)
            && destination
                .as_ref()
                .is_ok_and(|destination| destination.addresses().contains(&loopback)),
    );
    let _ = listener.accept();
    proof.check(
        "the same host on another port is refused",
        verdict(&policy, "localhost", other) == DESTINATION_REFUSED,
    );
    proof.check(
        "the address it resolves to is not allowed by the name entry alone",
        verdict(&policy, "127.0.0.1", port) == DESTINATION_REFUSED,
    );
    proof.check(
        "a name entry is matched without case or a trailing dot",
        verdict(&policy, "LocalHost.", port).starts_with("admitted"),
    );
    proof.check(
        "a connection kept open to the allowed host is still admitted",
        policy.admits("localhost", loopback),
    );

    proof.check(
        "an allowed address and an address block are saved",
        proof
            .put(
                CONNECTOR_ALLOWED_DESTINATIONS,
                json!([
                    format!("127.0.0.1:{port}"),
                    "10.20.0.0/16",
                    "[fd00:aa::/64]:5039",
                    "192.0.2.0/24:993"
                ]),
            )
            .await?,
    );
    let policy = proof.policy().await?;
    let connected = policy
        .resolve("127.0.0.1", port)
        .ok()
        .and_then(|destination| destination.connect(timeout).ok())
        .and_then(|stream| stream.peer_addr().ok());
    proof.check("the allowed address connects", connected == Some(loopback));
    let _ = listener.accept();
    proof.check(
        "the allowed address on another port is refused",
        verdict(&policy, "127.0.0.1", other) == DESTINATION_REFUSED,
    );
    all(
        &mut proof,
        &policy,
        "an address in an allowed block is admitted, on any port the entry allows",
        &[
            ("10.20.30.40", 993),
            ("10.20.0.1", 5039),
            ("fd00:aa::7", 5039),
        ],
        "admitted",
    );
    all(
        &mut proof,
        &policy,
        "an address outside the block, or on a port the entry does not allow, is refused",
        &[
            ("10.21.0.1", 993),
            ("fd00:aa::7", 993),
            ("fd00:ab::7", 5039),
        ],
        DESTINATION_REFUSED,
    );
    // A documentation address the operator allowed: nothing answers there, so
    // connecting either fails at once or waits out its one time limit.
    let limit = Duration::from_millis(500);
    let started = Instant::now();
    let attempt = policy
        .resolve("192.0.2.1", 993)
        .map(|destination| destination.connect(limit));
    let took = started.elapsed();
    proof.check(
        &format!(
            "an allowed address that does not answer gives up within the one time limit ({took:?})"
        ),
        matches!(attempt, Ok(Err(_))) && took < limit + Duration::from_secs(1),
    );

    proof.check(
        "entries naming link-local, metadata and unspecified addresses are saved",
        proof
            .put(
                CONNECTOR_ALLOWED_DESTINATIONS,
                json!([
                    "169.254.0.0/16",
                    "100.100.100.200",
                    "[fd00:ec2::254]",
                    "0.0.0.0/0",
                    "[::/0]"
                ]),
            )
            .await?,
    );
    let policy = proof.policy().await?;
    all(
        &mut proof,
        &policy,
        "but they still never admit those addresses",
        &[
            ("169.254.169.254", 80),
            ("100.100.100.200", 80),
            ("fd00:ec2::254", 80),
            ("::ffff:169.254.169.254", 80),
            ("0.0.0.0", 993),
            ("224.0.0.1", 993),
            ("fe80::1", 993),
        ],
        DESTINATION_REFUSED,
    );

    println!("== the operator's entries are checked when saved");
    for value in [
        json!(["http://mail.example.com"]),
        json!(["mail.example.com:0"]),
        json!(["mail.example.com:70000"]),
        json!(["mail.example.com:+993"]),
        json!(["fd00::1"]),
        json!(["[::1"]),
        json!(["[10.0.0.1]"]),
        json!(["10.0.0.0/33"]),
        json!(["[fd00::/129]"]),
        json!(["-bad.example"]),
        json!(["mail example"]),
        json!([" mail.example.com"]),
        json!([993]),
        json!({"host": "mail.example.com"}),
        Value::from(vec![Value::from("mail.example.com"); 201]),
    ] {
        let saved = proof
            .put(CONNECTOR_ALLOWED_DESTINATIONS, value.clone())
            .await?;
        let shown = value.to_string();
        let shown = if shown.len() > 60 {
            format!("{}…", &shown[..60])
        } else {
            shown
        };
        proof.check(&format!("refused: {shown}"), !saved);
    }
    proof.check(
        "the forms the help names are accepted",
        proof
            .put(
                CONNECTOR_ALLOWED_DESTINATIONS,
                json!([
                    "mail.example.com",
                    "Mail.Example.COM.:993",
                    "pbx.internal:5039",
                    "10.0.0.5:5039",
                    "10.20.0.0/16",
                    "[fd00::5]:993",
                    "[fd00::/8]"
                ]),
            )
            .await?,
    );
    proof.check(
        "the operator clears the list",
        proof.put(CONNECTOR_ALLOWED_DESTINATIONS, json!([])).await?,
    );
    let policy = proof.policy().await?;
    proof.check(
        "with the list cleared, loopback is refused again",
        verdict(&policy, "127.0.0.1", port) == DESTINATION_REFUSED,
    );
    proof.check(
        "and a connection kept open to it is no longer admitted",
        !policy.admits("localhost", loopback),
    );

    if proof.failed > 0 {
        eprintln!("{} checks failed", proof.failed);
        std::process::exit(1);
    }
    println!("all checks passed");
    Ok(())
}
