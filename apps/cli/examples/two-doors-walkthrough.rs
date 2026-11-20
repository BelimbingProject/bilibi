//! Two doors, one answer: the drift check for the command line.
//!
//! The command line is a door to the operation contracts, as `/mcp` is. This
//! walkthrough serves the real host on a local port, signs the command line
//! in through the device grant (a fixture account approves the code the way
//! the `/oauth/device` page does), opens an MCP connection for the same
//! account, and sends the same request through both doors: `hello`, a
//! search, a describe, a read, a refused input, an unknown key, a write's
//! review, and a write a second account may not make. Each pair of answers
//! must be the same JSON. It then commits the reviewed write through the
//! command line and reads it back through MCP.
//!
//! It also checks what makes the command line only a door: every request it
//! made reached the versioned API (the server's own exchange log says so), a
//! changed operation is refused as a stale ref before anything is sent, and
//! no token appears in anything it printed, while its saved sign-in is a
//! private file.
//!
//! Each line says what was compared; a line starting `FAIL` makes the run
//! exit non-zero. Environment: BILIBI_TWO_DOORS_DEMO=disposable, DATABASE_URL
//! (an empty disposable database) and APP_ORIGIN (`http://127.0.0.1:<free port>`).
use bilibi_agent::{AgentAccess, Decision, CLIENT_ID};
use bilibi_axi::{run, Invocation};
use bilibi_database::pool;
use bilibi_tenancy::Scope;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

type Outcome<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Checks {
    failures: usize,
}
impl Checks {
    fn expect(&mut self, what: &str, ok: bool, saw: impl std::fmt::Display) {
        if ok {
            println!("ok   {what}");
        } else {
            self.failures += 1;
            println!("FAIL {what}: {saw}");
        }
    }
    /// The same JSON from both doors.
    fn same(&mut self, what: &str, cli: &Value, mcp: &Value) {
        self.expect(
            what,
            cli == mcp && !cli.is_null(),
            format!("command line {cli} / MCP {mcp}"),
        );
    }
}

/// One saved sign-in folder: the command line as one account.
struct Cli {
    home: PathBuf,
    /// Everything it printed, for the token check.
    printed: String,
}
impl Cli {
    /// Runs the command with `--json` and returns what it printed and its exit code.
    async fn json(&mut self, args: &[&str]) -> Outcome<(Value, u8)> {
        self.json_with(args, None).await
    }
    /// `json` with something on standard input.
    async fn json_with(&mut self, args: &[&str], stdin: Option<&str>) -> Outcome<(Value, u8)> {
        let mut args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        args.push("--json".into());
        let outcome = run(Invocation {
            args,
            home: self.home.clone(),
            self_report: Some("two-doors/walkthrough".into()),
            stdin: stdin.map(str::to_owned),
        })
        .await;
        self.printed.push_str(&outcome.stdout);
        Ok((serde_json::from_str(&outcome.stdout)?, outcome.code))
    }
    /// The same, as the TOON an agent reads.
    async fn text(&mut self, args: &[&str]) -> (String, u8) {
        let outcome = run(Invocation {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            home: self.home.clone(),
            self_report: None,
            stdin: None,
        })
        .await;
        self.printed.push_str(&outcome.stdout);
        (outcome.stdout, outcome.code)
    }
}

/// One MCP `tools/call` over HTTP; the tool's structured content and whether
/// the tool reported an error.
async fn mcp(addr: &str, token: &str, tool: &str, arguments: Value) -> Outcome<(Value, bool)> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": arguments}}).to_string();
    let mut stream = TcpStream::connect(addr).await?;
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw);
    let (head, payload) = text.split_once("\r\n\r\n").ok_or("MCP sent no body")?;
    if !head.starts_with("HTTP/1.1 200") {
        return Err(format!("MCP answered {}", head.lines().next().unwrap_or_default()).into());
    }
    let reply: Value = serde_json::from_str(payload)?;
    let result = reply.get("result").ok_or("MCP sent no result")?;
    Ok((
        result["structuredContent"].clone(),
        result["isError"] == true,
    ))
}

/// Signs a command-line folder in as `account`: the command line starts the
/// device grant, the account approves its code, the command line collects.
async fn sign_in(
    access: &AgentAccess,
    account: &Scope,
    origin: &str,
    home: &Path,
    name: &str,
) -> Outcome<Cli> {
    let mut cli = Cli {
        home: home.to_owned(),
        printed: String::new(),
    };
    let (started, _) = cli.json(&["login", origin, "--name", name]).await?;
    let code = started["code"].as_str().ok_or("login printed no code")?;
    access.decide(account, code, Decision::Approve).await?;
    let (finished, exit) = cli.json(&["login", "--wait", "20"]).await?;
    if exit != 0 || finished["login"] != "signed in" {
        return Err(format!("the command line did not sign in: {finished}").into());
    }
    Ok(cli)
}

/// An MCP-audience access token for `account`, through the same device grant.
async fn mcp_token(
    access: &AgentAccess,
    account: &Scope,
    origin: &str,
    name: &str,
) -> Outcome<String> {
    let resource = format!("{origin}/mcp");
    let start = access
        .start_device_for(CLIENT_ID, Some(name), Some(&resource), &resource)
        .await?;
    access
        .decide(account, &start.user_code, Decision::Approve)
        .await?;
    Ok(access
        .exchange(CLIENT_ID, &start.device_code)
        .await?
        .access_token)
}

#[tokio::main]
async fn main() -> Outcome {
    if std::env::var("BILIBI_TWO_DOORS_DEMO").as_deref() != Ok("disposable") {
        return Err("set BILIBI_TWO_DOORS_DEMO=disposable, DATABASE_URL to an empty disposable database and APP_ORIGIN to http://127.0.0.1:<free port>".into());
    }
    bilibi_base::link();
    bilibi_core::link();
    let origin = std::env::var("APP_ORIGIN")?;
    let addr = origin
        .strip_prefix("http://")
        .ok_or("APP_ORIGIN must be http://127.0.0.1:<port> here")?
        .to_owned();
    let db = pool(&std::env::var("DATABASE_URL")?)?;
    bilibi_walkthrough_fixture::install(&db).await?;
    // A Dev copy may issue tokens over loopback HTTP, and asks no 2FA.
    bilibi_walkthrough_fixture::set_instance_role(&db, "development").await?;
    let app = bilibi_web::App::new(db.clone(), &origin)?;
    app.services
        .get::<bilibi_base::authz::grants::Grants>("base/authz.grants")?
        .reconcile()
        .await?;
    let access = app.services.get::<AgentAccess>("core/agent.grants")?;
    // The fixture's administrator works at a desk, as a write needs one.
    db.get()
        .await?
        .execute(
            "INSERT INTO user_desks(tenant_id,person_id,company_id) VALUES(1,3,1) ON CONFLICT DO NOTHING",
            &[],
        )
        .await?;
    let admin_login = app
        .user
        .login("ai@agent.my", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let viewer_login = app
        .user
        .login("viewer@example.test", bilibi_walkthrough_fixture::PASSWORD)
        .await?;
    let admin = app.security.guard(&admin_login.token).await?;
    let viewer = app.security.guard(&viewer_login.token).await?;
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let router = bilibi_web::router(app.clone());
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let scratch = std::env::temp_dir().join(format!("bilibi-two-doors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let mut checks = Checks { failures: 0 };

    println!("== sign in: the command line through the device grant, MCP beside it");
    let mut cli = sign_in(
        &access,
        &admin,
        &origin,
        &scratch.join("admin"),
        "two doors: command line",
    )
    .await?;
    let token = mcp_token(&access, &admin, &origin, "two doors: MCP").await?;
    println!("ok   both doors hold a connection of the same account");

    println!("== the same request through both doors");
    let (cli_hello, _) = cli.json(&["hello", "two", "doors"]).await?;
    let (mcp_hello, _) = mcp(&addr, &token, "hello", json!({"message": "two doors"})).await?;
    checks.same("hello", &cli_hello, &mcp_hello);

    let (cli_found, _) = cli.json(&["search", "company", "--limit", "50"]).await?;
    let (mcp_found, _) = mcp(&addr, &token, "search", json!({"query": "company"})).await?;
    checks.same(
        "search: operations and guides",
        &cli_found["result"],
        &mcp_found["result"],
    );
    checks.same("search: pages", &cli_found["pages"], &mcp_found["pages"]);

    let read_key = "core.company.reachable";
    let (cli_described, _) = cli.json(&["describe", read_key]).await?;
    let (mcp_described, _) = mcp(&addr, &token, "describe", json!({"key": read_key})).await?;
    checks.same("describe", &cli_described, &mcp_described);

    let (cli_read, _) = cli.json(&["read", read_key]).await?;
    let (mcp_read, _) = mcp(&addr, &token, "read", json!({"key": read_key, "input": {}})).await?;
    checks.same("read", &cli_read, &mcp_read);

    println!("== the same refusal through both doors");
    let (cli_bad, exit) = cli.json(&["read", read_key, "nope=1"]).await?;
    let (mcp_bad, failed) = mcp(
        &addr,
        &token,
        "read",
        json!({"key": read_key, "input": {"nope": 1}}),
    )
    .await?;
    checks.same(
        "refused input: field errors",
        &cli_bad["fields"],
        &mcp_bad["fields"],
    );
    checks.expect(
        "refused input: both doors say invalid_input",
        exit == 1
            && failed
            && cli_bad["code"] == "INVALID_INPUT"
            && mcp_bad["error"] == "invalid_input",
        format!("{} / {}", cli_bad["code"], mcp_bad["error"]),
    );
    let (cli_unknown, exit) = cli.json(&["describe", "core.company.nothing_here"]).await?;
    let (mcp_unknown, failed) = mcp(
        &addr,
        &token,
        "describe",
        json!({"key": "core.company.nothing_here"}),
    )
    .await?;
    checks.expect(
        "unknown key: both doors refuse",
        exit == 1 && failed && cli_unknown["code"] == "UNKNOWN_OPERATION",
        format!("{} / {mcp_unknown}", cli_unknown["code"]),
    );

    println!("== a write: one review, whichever door asks");
    let write_key = "core.company.create";
    let operations =
        json!([{"key": write_key, "input": {"name": "Two Doors Trading", "status": "active"}}]);
    let (mut cli_plan, _) = cli
        .json(&["plan", write_key, "name=Two Doors Trading", "status=active"])
        .await?;
    let reference = cli_plan["ref"]
        .as_str()
        .ok_or_else(|| format!("plan printed no ref: {cli_plan}"))?
        .to_owned();
    // The ref is the command line's own; everything else is the server's review.
    if let Some(plan) = cli_plan.as_object_mut() {
        plan.remove("ref");
    }
    let (mcp_plan, _) = mcp(&addr, &token, "plan", json!({"operations": operations})).await?;
    checks.same("plan: the review", &cli_plan, &mcp_plan);

    println!("== a write the account may not make: the same refusal");
    let mut viewer_cli = sign_in(
        &access,
        &viewer,
        &origin,
        &scratch.join("viewer"),
        "two doors: viewer command line",
    )
    .await?;
    let viewer_token = mcp_token(&access, &viewer, &origin, "two doors: viewer MCP").await?;
    let (cli_denied, _) = viewer_cli
        .json(&["plan", write_key, "name=Two Doors Trading", "status=active"])
        .await?;
    let (mcp_denied, _) = mcp(
        &addr,
        &viewer_token,
        "plan",
        json!({"operations": operations}),
    )
    .await?;
    let refused = |answer: &Value| {
        answer["valid"] == false || answer["error"] == "forbidden" || answer["code"] == "FORBIDDEN"
    };
    checks.expect(
        "a viewer's write is refused at both doors",
        refused(&cli_denied) && refused(&mcp_denied),
        format!("command line {cli_denied} / MCP {mcp_denied}"),
    );
    // The command line prints a refusal with its own words around the server's fields.
    for (name, answer) in [("command line", &cli_denied), ("MCP", &mcp_denied)] {
        checks.expect(
            &format!("the viewer's refusal names the missing permission ({name})"),
            answer.to_string().contains("forbidden")
                || answer.to_string().contains("missing_capability"),
            answer,
        );
    }
    checks.same(
        "a viewer's refused plan: the same results",
        &cli_denied
            .get("results")
            .cloned()
            .unwrap_or(json!("refused before review")),
        &mcp_denied
            .get("results")
            .cloned()
            .unwrap_or(json!("refused before review")),
    );

    println!("== the command line commits only what was reviewed");
    let (stale, exit) = cli
        .json(&[
            "commit",
            &reference,
            write_key,
            "name=Two Doors Trading Two",
            "status=active",
        ])
        .await?;
    checks.expect(
        "a changed operation is a stale ref, and nothing is written",
        exit == 1 && stale["code"] == "STALE_REF",
        &stale,
    );
    // The ref names the sign-in it was reviewed under: the same account,
    // signed in again elsewhere, cannot commit under it.
    let mut again = sign_in(
        &access,
        &admin,
        &origin,
        &scratch.join("admin-again"),
        "two doors: second sign-in",
    )
    .await?;
    let (crossed, exit) = again
        .json(&[
            "commit",
            &reference,
            write_key,
            "name=Two Doors Trading",
            "status=active",
        ])
        .await?;
    checks.expect(
        "a ref does not cross to another sign-in",
        exit == 1 && crossed["code"] == "STALE_REF",
        &crossed,
    );
    // Only the whole reviewed set passes: one step of a two-step review is another plan.
    let step = |name: &str| json!({"key": write_key, "input": {"name": name, "status": "active"}});
    let (both, one) = (scratch.join("both.json"), scratch.join("one.json"));
    std::fs::write(
        &both,
        json!([step("Two Doors North"), step("Two Doors South")]).to_string(),
    )?;
    std::fs::write(&one, json!([step("Two Doors North")]).to_string())?;
    let (pair, _) = cli
        .json(&["plan", "--file", &both.to_string_lossy()])
        .await?;
    let pair_ref = pair["ref"].as_str().unwrap_or_default().to_owned();
    let (partial, exit) = cli
        .json(&["commit", &pair_ref, "--file", &one.to_string_lossy()])
        .await?;
    checks.expect(
        "part of a reviewed set is a stale ref",
        pair_ref.len() == 64 && exit == 1 && partial["code"] == "STALE_REF",
        &partial,
    );
    let written: i64 = {
        let client = db.get().await?;
        let row = client
            .query_one(
                "SELECT count(*) FROM companies WHERE name LIKE 'Two Doors%'",
                &[],
            )
            .await?;
        row.get(0)
    };
    checks.expect("no refused commit wrote anything", written == 0, written);
    // The person's 2FA code travels on standard input and is never printed.
    const CODE: &str = "731964-two-doors-code";
    let (committed, exit) = cli
        .json_with(
            &[
                "commit",
                &reference,
                write_key,
                "name=Two Doors Trading",
                "status=active",
                "--code-stdin",
            ],
            Some(CODE),
        )
        .await?;
    let id = committed
        .pointer("/results/0/result/result/record/id")
        .and_then(Value::as_i64);
    checks.expect(
        "the reviewed write commits",
        exit == 0 && id.is_some(),
        &committed,
    );
    let (after, _) = mcp(
        &addr,
        &token,
        "read",
        json!({"key": "core.company.get", "input": {"company_id": id.unwrap_or_default()}}),
    )
    .await?;
    checks.expect(
        "MCP reads the company the command line created",
        after.to_string().contains("Two Doors Trading"),
        &after,
    );
    let (cli_again, _) = cli
        .json(&["plan", write_key, "name=Two Doors Trading", "status=active"])
        .await?;
    let (mcp_again, _) = mcp(&addr, &token, "plan", json!({"operations": operations})).await?;
    checks.same(
        "the same plan again: both doors see the new duplicate",
        &cli_again["results"],
        &mcp_again["results"],
    );

    println!("== only a door");
    let connections = access.list_connections(&admin).await?;
    let client = db.get().await?;
    let ours = connections
        .iter()
        .find(|connection| connection.agent_id == "two doors: command line")
        .ok_or("the command line's connection is not listed")?;
    let rows = client
        .query(
            "SELECT endpoint, count(*) FROM base_integration_exchanges WHERE connection_id=$1 GROUP BY 1 ORDER BY 1",
            &[&ours.id.to_string()],
        )
        .await?;
    let endpoints: Vec<String> = rows.iter().map(|row| row.get::<_, String>(0)).collect();
    checks.expect(
        "every request of the command line reached the versioned API",
        !endpoints.is_empty()
            && endpoints
                .iter()
                .all(|endpoint| endpoint.starts_with("/api/v1/")),
        endpoints.join(", "),
    );
    let (text, _) = cli.text(&["search", "company"]).await;
    checks.expect(
        "the default output is TOON with next steps",
        text.contains("operations[")
            && text.contains("{key,kind,title}:")
            && text.contains("help["),
        &text,
    );
    let saved = std::fs::read_to_string(scratch.join("admin").join(format!(
        "{}.json",
        cli_hello["server_name"].as_str().unwrap_or_default()
    )))?;
    let saved: Value = serde_json::from_str(&saved)?;
    let secrets: Vec<&str> = ["access_token", "refresh_token"]
        .iter()
        .filter_map(|name| saved[*name].as_str())
        .collect();
    checks.expect(
        "no token in anything the command line printed",
        secrets.len() == 2 && secrets.iter().all(|secret| !cli.printed.contains(secret)),
        "a token was printed",
    );
    checks.expect(
        "the 2FA code given on standard input is not printed",
        !cli.printed.contains(CODE),
        "the code was printed",
    );

    println!("== a sign-in answer that is not one prints no credential");
    // A stand-in for a server that answers the token endpoint badly: first a
    // 200 that carries tokens but is not a whole token response, then a
    // refusal that repeats them. Neither may reach the output.
    const LEAK: &str = "LEAK-two-doors";
    let stub = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let stub_origin = format!("http://{}", stub.local_addr()?);
    tokio::spawn(async move {
        let bodies = [
            (
                "200 OK",
                json!({"access_token": format!("{LEAK}-access"), "refresh_token": format!("{LEAK}-refresh")}),
            ),
            (
                "400 Bad Request",
                json!({"error": "invalid_request", "access_token": format!("{LEAK}-access"), "refresh_token": format!("{LEAK}-refresh"), "id_token": format!("{LEAK}-id")}),
            ),
        ];
        for (status, body) in bodies {
            let Ok((mut stream, _)) = stub.accept().await else {
                return;
            };
            // The whole request: its head, then the body its length names.
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    return;
                };
                request.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let Some((head, sent)) = text.split_once("\r\n\r\n") else {
                    if read == 0 {
                        break;
                    }
                    continue;
                };
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|length| length.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if sent.len() >= length || read == 0 {
                    break;
                }
            }
            let body = body.to_string();
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    // A saved sign-in whose access token has run out, so the next command refreshes.
    let stub_home = scratch.join("stub");
    let mut stub_cli = Cli {
        home: stub_home.clone(),
        printed: String::new(),
    };
    {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&stub_home)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let saved = json!({"origin": stub_origin, "server_name": "stub", "client_id": "bilibi-native", "sign_in": "stub", "access_token": null, "access_expires_at": 0, "refresh_token": "spent", "pending": null});
        std::io::Write::write_all(
            &mut options.open(stub_home.join("stub.json"))?,
            saved.to_string().as_bytes(),
        )?;
        std::io::Write::write_all(&mut options.open(stub_home.join("current"))?, b"stub")?;
    }
    let (malformed, exit_malformed) = stub_cli.json(&["hello"]).await?;
    let (refused, exit_refused) = stub_cli.json(&["hello"]).await?;
    checks.expect(
        "a token response that is not one is a plain protocol error",
        exit_malformed == 1 && malformed["code"] == "NOT_BILIBI",
        &malformed,
    );
    checks.expect(
        "a refusal that repeats tokens prints its code alone",
        exit_refused == 1 && refused["code"] == "INVALID_REQUEST",
        &refused,
    );
    checks.expect(
        "neither answer's tokens reach the output",
        !stub_cli.printed.contains(LEAK),
        "a token from a malformed answer was printed",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            |path: &Path| std::fs::metadata(path).map(|meta| meta.permissions().mode() & 0o777);
        let folder = mode(&scratch.join("admin"))?;
        let file = mode(&scratch.join("admin").join(format!(
            "{}.json",
            cli_hello["server_name"].as_str().unwrap_or_default()
        )))?;
        checks.expect(
            "the saved sign-in is private to its owner",
            folder == 0o700 && file == 0o600,
            format!("folder {folder:o}, file {file:o}"),
        );
    }
    let (out, exit) = cli.text(&["logout"]).await;
    checks.expect(
        "logout forgets the saved sign-in",
        exit == 0 && out.contains("forgotten") && !scratch.join("admin").join("current").exists(),
        &out,
    );
    let _ = std::fs::remove_dir_all(&scratch);
    if checks.failures > 0 {
        return Err(format!("{} check(s) failed", checks.failures).into());
    }
    println!("two doors, one answer");
    Ok(())
}
