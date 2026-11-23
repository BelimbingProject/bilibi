//! Observable proof that the socket, not a path or Host header, selects the
//! staff or public site.
use bilibi_database::{pool, Error, Result};
use std::future::IntoFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn get(port: u16, host: &str, path: &str, cookie: Option<&str>) -> Result<(u16, Vec<u8>)> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|_| Error::Internal)?;
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(cookie) = cookie {
        request.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Internal)?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|_| Error::Internal)?;
    let headers_end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or(Error::Internal)?;
    let status = std::str::from_utf8(&response)
        .ok()
        .and_then(|response| response.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or(Error::Internal)?;
    Ok((status, response[(headers_end + 4)..].to_vec()))
}

#[tokio::main]
async fn main() -> Result<()> {
    let db = pool(&std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?)?;
    bilibi_web::migrate(&db).await?;
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
    let app = bilibi_web::App::new(db, &format!("http://{staff_host}"))?;
    let public = app
        .clone()
        .at_site(bilibi_web::Site::Public, &format!("http://{public_host}"));
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
    let public_portal = get(public_port, &public_host, "/portal/sign-in", None).await?;
    assert_ne!(public_portal.0, 404);
    assert_eq!(
        get(staff_port, &staff_host, "/portal/sign-in", None)
            .await?
            .0,
        404
    );
    let staff_login = get(staff_port, &staff_host, "/login", None).await?;
    assert_ne!(staff_login.0, 404);
    assert_eq!(get(public_port, &public_host, "/login", None).await?.0, 404);
    assert_eq!(get(staff_port, &public_host, "/login", None).await?.0, 404);
    assert_eq!(
        get(public_port, &staff_host, "/portal/sign-in", None)
            .await?
            .0,
        404
    );
    assert_ne!(
        get(staff_port, &staff_host, "/api/v1/hello", None).await?.0,
        404
    );
    assert_eq!(
        get(public_port, &public_host, "/api/v1/hello", None)
            .await?
            .0,
        404
    );
    assert_ne!(
        get(public_port, &public_host, "/visitor/v1/client.js", None)
            .await?
            .0,
        404
    );
    assert_eq!(
        get(staff_port, &staff_host, "/visitor/v1/client.js", None)
            .await?
            .0,
        404
    );
    assert_ne!(
        get(
            public_port,
            &public_host,
            "/webhooks/integration-reference",
            None,
        )
        .await?
        .0,
        404
    );
    assert_eq!(
        get(
            staff_port,
            &staff_host,
            "/webhooks/integration-reference",
            None,
        )
        .await?
        .0,
        404
    );

    let plain_staff = get(staff_port, &staff_host, "/login", None).await?;
    let customer_on_staff = get(
        staff_port,
        &staff_host,
        "/login",
        Some("__Host-bilibi_customer=00000000000000000000000000000000"),
    )
    .await?;
    assert_eq!(plain_staff, customer_on_staff);
    let plain_public = get(public_port, &public_host, "/portal/sign-in", None).await?;
    let staff_on_public = get(
        public_port,
        &public_host,
        "/portal/sign-in",
        Some("__Host-bilibi_session=00000000000000000000000000000000"),
    )
    .await?;
    assert_eq!(plain_public, staff_on_public);

    staff_server.abort();
    public_server.abort();
    println!("site boundary: staff and public routes, Host checks and cookie realms passed");
    Ok(())
}
