//! Observable proof of staff portal setup and the staff/public socket boundary.
//! On the public socket, the hostname names the tenant's site.
use bilibi_base::session::session_id;
use bilibi_database::{pool, Db, Error, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{future::IntoFuture, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

bilibi_walkthrough_fixture::walkthrough_vocabulary!();
bilibi_walkthrough_fixture::walkthrough_module_roles!();

const STAFF_COOKIE: &str = "__Host-bilibi_session";
const CUSTOMER_COOKIE: &str = "__Host-bilibi_customer";

async fn status(
    port: u16,
    host: &str,
    method: &str,
    path: &str,
    cookie: Option<(&str, &str)>,
) -> Result<u16> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|_| Error::Internal)?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n"
    );
    if let Some((name, token)) = cookie {
        request.push_str(&format!("Cookie: {name}={token}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Internal)?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .map_err(|_| Error::Internal)?;
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or(Error::Internal)
}

async fn sessions(db: &Db) -> Result<(String, String)> {
    bilibi_walkthrough_fixture::install(db).await?;
    let mut client = db.get().await?;
    let tx = client.transaction().await?;
    let customer: i64 = tx
        .query_one(
            "WITH identity AS (
            INSERT INTO user_identities(kind,tenant_id,email,password_hash,email_verified_at)
            VALUES ('customer',1,'boundary-customer@example.test','unused',now())
            ON CONFLICT (tenant_id,lower(email)) WHERE deleted_at IS NULL AND kind='customer'
            DO UPDATE SET email=EXCLUDED.email RETURNING id
        )
        INSERT INTO users(id,tenant_id,identity_id,kind,name,person_id)
        SELECT 901,1,id,'customer','Boundary Customer',1 FROM identity
        ON CONFLICT (id) DO UPDATE SET name=EXCLUDED.name RETURNING id",
            &[],
        )
        .await?
        .get(0);
    let staff_token = uuid::Uuid::new_v4().simple().to_string();
    let customer_token = uuid::Uuid::new_v4().simple().to_string();
    for (user, token) in [(4_i64, &staff_token), (customer, &customer_token)] {
        tx.execute(
            "INSERT INTO sessions(id,tenant_id,user_id,csrf,expires_at,authentication_authority)
             VALUES($1,1,$2,$3,now()+interval '1 hour','portable')",
            &[
                &session_id(token),
                &user,
                &uuid::Uuid::new_v4().simple().to_string(),
            ],
        )
        .await?;
    }
    tx.commit().await?;
    Ok((staff_token, customer_token))
}

async fn check_staff_form(
    port: u16,
    host: &str,
    token: &str,
    path: &str,
    steps: [(&str, Value, &str); 4],
) -> Result<()> {
    assert_eq!(
        status(port, host, "GET", path, Some((STAFF_COOKIE, token))).await?,
        200,
        "staff page {path}"
    );
    let mut request = format!("ws://127.0.0.1:{port}/live")
        .into_client_request()
        .map_err(|_| Error::Internal)?;
    request
        .headers_mut()
        .insert("Host", host.parse().map_err(|_| Error::Internal)?);
    request.headers_mut().insert(
        "Origin",
        format!("http://{host}")
            .parse()
            .map_err(|_| Error::Internal)?,
    );
    request.headers_mut().insert(
        "Cookie",
        format!("{STAFF_COOKIE}={token}")
            .parse()
            .map_err(|_| Error::Internal)?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|_| Error::Internal)?;

    for (index, (label, mut request, expected)) in steps.into_iter().enumerate() {
        let id = index + 1;
        request["request_id"] = json!(id);
        socket
            .send(Message::text(request.to_string()))
            .await
            .map_err(|_| Error::Internal)?;
        let reply = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(message) = socket.next().await {
                let message = message.map_err(|_| Error::Internal)?;
                if let Message::Text(text) = message {
                    let reply: Value = serde_json::from_str(&text).map_err(|_| Error::Internal)?;
                    if reply["request_id"] == id {
                        return Ok(reply);
                    }
                }
            }
            Err(Error::Internal)
        })
        .await
        .map_err(|_| {
            eprintln!("walkthrough timed out: {label}");
            Error::Internal
        })??;
        assert_eq!(reply["type"], "page", "{label}");
        assert!(
            reply["html"]
                .as_str()
                .is_some_and(|html| html.contains(expected)),
            "{label}: expected {expected}"
        );
        println!("ok    {label}");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_web::migrate(&db).await?;
    let (staff_token, customer_token) = sessions(&db).await?;
    let staff_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| Error::Internal)?;
    let public_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| Error::Internal)?;
    let staff_port = staff_listener
        .local_addr()
        .map_err(|_| Error::Internal)?
        .port();
    let public_port = public_listener
        .local_addr()
        .map_err(|_| Error::Internal)?
        .port();
    let staff_host = format!("staff.localhost:{staff_port}");
    let public_host = format!("shop.staff.localhost:{public_port}");
    // Tenant 2's site is on its own hostname at the same public listener.
    let other_host = format!("other.staff.localhost:{public_port}");
    let app = bilibi_web::App::new(db, &format!("http://{staff_host}"))?;
    let sites = app
        .services
        .get::<bilibi_base::public::Public>(bilibi_base::public::SERVICE)?;
    for (tenant, host) in [(1, "shop.staff.localhost"), (2, "other.staff.localhost")] {
        sites
            .bind_hostname(&app.mutations, tenant, host, true)
            .await?
            .map_err(|_| Error::Invalid)?;
    }
    let public = app.clone().at_public_site(&format!("http://{public_host}"));
    let staff_server = tokio::spawn(
        axum::serve(
            staff_listener,
            bilibi_web::staff_router(app)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .into_future(),
    );
    let public_server = tokio::spawn(
        axum::serve(
            public_listener,
            bilibi_web::public_router(public)
                .into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .into_future(),
    );

    let event = |payload| json!({"type": "event", "target": "page", "payload": payload});
    let path = "/public-site";
    let public_site_steps = [
        (
            "mount public site",
            json!({"type": "mount", "path": path, "params": {}}),
            "id=\"public-site-page\"",
        ),
        (
            "open site form",
            event(json!({"type": "edit_site"})),
            "id=\"public-site-address\"",
        ),
        (
            "save site",
            event(json!({
                "type": "save_site",
                "site[address]": "boundary",
                "site[title]": "Boundary <Portal> & Co.",
                "site[enabled]": "true"
            })),
            "Public site saved.",
        ),
        (
            "reopen site form",
            event(json!({"type": "edit_site"})),
            "value=\"Boundary &#60;Portal&#62; &#38; Co.\"",
        ),
    ];
    check_staff_form(
        staff_port,
        &staff_host,
        &staff_token,
        path,
        public_site_steps,
    )
    .await?;

    let path = "/customer-portal";
    let portal_steps = [
        (
            "mount customer portal",
            json!({"type": "mount", "path": path, "params": {}}),
            "id=\"customer-portal-page\"",
        ),
        (
            "open portal options",
            event(json!({"type": "edit_options"})),
            "id=\"customer-options-registration\"",
        ),
        (
            "enable account requests",
            event(json!({"type": "save_options", "options[registration]": "true"})),
            "Taken; each waits for approval",
        ),
        (
            "reopen portal options",
            event(json!({"type": "edit_options"})),
            "name=\"options[registration]\" value=\"true\"",
        ),
    ];
    check_staff_form(staff_port, &staff_host, &staff_token, path, portal_steps).await?;

    for (method, path, staff, public) in [
        ("GET", "/login", 200, 404),
        ("GET", "/portal/sign-in", 404, 200),
        ("GET", "/api/v1/hello", 401, 404),
        ("GET", "/visitor/v1/client.js", 404, 200),
        ("POST", "/webhooks/integration-reference", 404, 403),
    ] {
        assert_eq!(
            status(staff_port, &staff_host, method, path, None).await?,
            staff,
            "staff {method} {path}"
        );
        assert_eq!(
            status(public_port, &public_host, method, path, None).await?,
            public,
            "public {method} {path}"
        );
    }
    assert_eq!(
        status(staff_port, &public_host, "GET", "/login", None).await?,
        404
    );
    assert_eq!(
        status(public_port, &staff_host, "GET", "/portal/sign-in", None).await?,
        404
    );
    // The hostname names the site: tenant 2's answers on its own name, an
    // unknown name and the right name at another port do not.
    assert_eq!(
        status(public_port, &other_host, "GET", "/portal/sign-in", None).await?,
        200
    );
    assert_eq!(
        status(
            public_port,
            &format!("nobody.staff.localhost:{public_port}"),
            "GET",
            "/portal/sign-in",
            None
        )
        .await?,
        404
    );
    assert_eq!(
        status(
            public_port,
            "shop.staff.localhost:1",
            "GET",
            "/portal/sign-in",
            None
        )
        .await?,
        404
    );
    // A customer of tenant 1 is nobody on tenant 2's hostname.
    assert_eq!(
        status(
            public_port,
            &other_host,
            "GET",
            "/portal/sign-in",
            Some((CUSTOMER_COOKIE, customer_token.as_str()))
        )
        .await?,
        200,
        "tenant 1's customer on tenant 2's site sees the sign-in card"
    );

    for (port, host, path, own_name, own_token, other_name, other_token) in [
        (
            staff_port,
            &staff_host,
            "/login",
            STAFF_COOKIE,
            staff_token.as_str(),
            CUSTOMER_COOKIE,
            customer_token.as_str(),
        ),
        (
            public_port,
            &public_host,
            "/portal/sign-in",
            CUSTOMER_COOKIE,
            customer_token.as_str(),
            STAFF_COOKIE,
            staff_token.as_str(),
        ),
    ] {
        for (name, token, expected) in [
            (own_name, own_token, 303),
            (other_name, own_token, 200),
            (own_name, other_token, 200),
            (other_name, other_token, 200),
        ] {
            assert_eq!(
                status(port, host, "GET", path, Some((name, token))).await?,
                expected,
                "{host} {name}"
            );
        }
    }

    staff_server.abort();
    public_server.abort();
    println!("site boundary: staff and public routes, Host checks, per-tenant hostnames and cookie realms passed");
    Ok(())
}
