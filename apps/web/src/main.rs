use bilibi_database::{pool, Error, Result};
use std::future::IntoFuture;
use std::io::IsTerminal;
use std::sync::Arc;
fn main() -> Result<()> {
    bilibi_base::watchers::enter_worker();
    serve()
}

#[tokio::main]
async fn serve() -> Result<()> {
    bilibi_web::verify_descriptors()?;
    // What this program is made of needs no database (docs/versions.md).
    if std::env::args().nth(1).as_deref() == Some("status") {
        print!("{}", bilibi_web::status());
        return Ok(());
    }
    let url = std::env::var("DATABASE_URL").map_err(|_| Error::Invalid)?;
    let db = pool(&url)?;
    bilibi_web::verify_composition(&db).await?;
    match std::env::args().nth(1).as_deref() {
        Some("migrate") => {
            let migrated = bilibi_web::migrate(&db).await?;
            println!("Migrations applied: {}", migrated.applied.len());
            for version in &migrated.applied {
                println!("  applied {version}");
            }
            let mut moved = std::collections::BTreeMap::<(&str, &str), usize>::new();
            for item in &migrated.moved {
                *moved.entry((item.from.as_str(), item.to)).or_default() += 1;
            }
            for ((from, to), versions) in moved {
                println!("  moved {versions} migrations recorded under {from} to {to} (same SQL)");
            }
            for (version, module) in &migrated.unmounted {
                println!("  kept {version} of unmounted {module}");
            }
        }
        Some("authz-reconcile") => {
            let app = bilibi_web::App::new(db.clone(), "http://localhost")?;
            app.services
                .get::<bilibi_base::authz::grants::Grants>("base/authz.grants")?
                .reconcile()
                .await?;
            println!("Authorization system roles reconciled");
        }
        Some("geonames-import") => {
            use bilibi_geonames::{ImportOptions, ReferenceImport, IMPORT_SERVICE, USAGE};
            let args: Vec<String> = std::env::args().skip(2).collect();
            let options = ImportOptions::from_args(&args).map_err(|message| {
                eprintln!("{message}\n{USAGE}");
                Error::Invalid
            })?;
            let app = bilibi_web::App::new(db.clone(), "http://localhost")?;
            let importer = app.services.get::<ReferenceImport>(IMPORT_SERVICE)?;
            println!("GeoNames cache: {}", options.cache_dir.display());
            match importer.run(&options).await {
                Ok(reports) => reports.iter().for_each(|report| println!("{report}")),
                Err(error) => {
                    eprintln!("GeoNames import failed: {error}");
                    return Err(Error::Internal);
                }
            }
        }
        Some("public-hostname") => {
            // Names a tenant's public site by a hostname (the launcher and the
            // installer call it); staff change the rest on the Public site page.
            let mut args: Vec<String> = std::env::args().skip(2).collect();
            let on = args.iter().any(|arg| arg == "--on");
            args.retain(|arg| arg != "--on");
            let parsed = flags(&args, ["--tenant", "--hostname"])
                .and_then(|[tenant, hostname]| Some((tenant.parse::<i64>().ok()?, hostname)));
            let Some((tenant, hostname)) = parsed.filter(|(tenant, _)| *tenant > 0) else {
                eprintln!("usage: bilibi-web public-hostname --tenant ID --hostname NAME [--on]");
                return Err(Error::Invalid);
            };
            let origin = std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost".into());
            let app = bilibi_web::App::new(db.clone(), &origin)?;
            let public = app
                .services
                .get::<bilibi_base::public::Public>(bilibi_base::public::SERVICE)?;
            match public
                .bind_hostname(&app.mutations, tenant, hostname, on)
                .await?
            {
                Ok(site) => println!(
                    "Public site of tenant {tenant}: {} ({}), {}",
                    hostname,
                    site.title,
                    if site.enabled {
                        "on"
                    } else {
                        "off until staff turn it on"
                    }
                ),
                Err(errors) => {
                    for error in errors {
                        eprintln!("{}: {}", error.field, error.message);
                    }
                    return Err(Error::Invalid);
                }
            }
        }
        Some("create-owner") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            bilibi_web::create_owner(&db, &args).await?;
        }
        Some("two-factor-reset") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            bilibi_web::two_factor_reset(&db, &args).await?;
        }
        Some("identity-recover") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            bilibi_web::recover_identity(&db, &args).await?;
        }
        Some("identity-delete") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            bilibi_web::delete_identity(&db, &args).await?;
        }
        Some("identity-retention") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            bilibi_web::retain_identities(&db, &args).await?;
        }
        Some(command @ ("tenant-provision" | "platform-provision")) => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            bilibi_web::provision(&db, command, &args).await?;
        }
        Some("plugin-export") => {
            use bilibi_agent::{AgentPlugins, ExportError, PluginError, PLUGIN_SERVICE, USAGE};
            let args: Vec<String> = std::env::args().skip(2).collect();
            let Some([harness, output]) = flags(&args, ["--harness", "--output"]) else {
                eprintln!("{USAGE}");
                return Err(Error::Invalid);
            };
            // The package names the configured public address, never a guess.
            let origin = std::env::var("APP_ORIGIN").map_err(|_| {
                eprintln!("Set APP_ORIGIN to this installation's public HTTPS address first");
                Error::Invalid
            })?;
            let app = bilibi_web::App::new(db.clone(), &origin)?;
            let plugins = app.services.get::<AgentPlugins>(PLUGIN_SERVICE)?;
            match plugins
                .export(harness, std::path::Path::new(output))
                .await
            {
                Ok(name) => println!(
                    "Exported {output}. Connect {}/mcp in {name} using OAuth with automatic client registration.",
                    plugins.origin()
                ),
                Err(error) => {
                    eprintln!(
                        "{}",
                        match error {
                            ExportError::Plugin(PluginError::UnknownHarness) =>
                                format!(
                                    "Unknown harness; use one of: {}",
                                    AgentPlugins::harness_keys().join(", ")
                                ),
                            ExportError::Plugin(PluginError::HttpsRequired) =>
                                "Configure the host's public HTTPS URL (APP_ORIGIN) first".into(),
                            other => other.to_string(),
                        }
                    );
                    return Err(Error::Invalid);
                }
            }
        }
        Some("seed-production") => bilibi_web::seed::production(&db).await?,
        Some("seed") => {
            let at_startup = match std::env::args().nth(2).as_deref() {
                None => false,
                Some("--at-startup") => true,
                Some(_) => {
                    eprintln!("usage: bilibi-web seed [--at-startup]");
                    return Err(Error::Invalid);
                }
            };
            bilibi_web::seed::seed(&db, at_startup).await?;
        }
        Some("media-cleanup") => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            let parsed: Option<(i64, u64)> =
                match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                    ["--tenant", tenant] => tenant.parse().ok().map(|tenant| (tenant, 24)),
                    ["--tenant", tenant, "--hours", hours] => {
                        tenant.parse().ok().zip(hours.parse().ok())
                    }
                    _ => None,
                };
            let Some((tenant, hours)) = parsed.filter(|(tenant, hours)| *tenant > 0 && *hours > 0)
            else {
                eprintln!("usage: bilibi-web media-cleanup --tenant ID [--hours POSITIVE_INTEGER]");
                return Err(Error::Invalid);
            };
            bilibi_web::media_cleanup(&db, tenant, hours).await?;
        }
        Some("media-reconcile") => bilibi_web::media_reconcile(&db).await?,
        None | Some("run") => {
            let origin =
                std::env::var("APP_ORIGIN").unwrap_or_else(|_| "http://localhost:3000".into());
            let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
            // Each request's hostname names the tenant's public site; the
            // origin gives the listener's scheme and port.
            let public_site = match (
                std::env::var("PUBLIC_ORIGIN").ok(),
                std::env::var("PUBLIC_BIND_ADDR").ok(),
            ) {
                (Some(origin), Some(bind)) => Some((origin, bind)),
                (None, None) => None,
                _ => {
                    eprintln!("PUBLIC_ORIGIN and PUBLIC_BIND_ADDR must be set together");
                    return Err(Error::Invalid);
                }
            };
            if public_site
                .as_ref()
                .is_some_and(|(public_origin, public_bind)| {
                    public_bind == &bind
                        || origin_host(public_origin)
                            .zip(origin_host(&origin))
                            .is_none_or(|(public, staff)| public.eq_ignore_ascii_case(&staff))
                })
            {
                eprintln!("The public site must use a different hostname and bind address from the staff site");
                return Err(Error::Invalid);
            }
            let mut app = bilibi_web::App::new(db, &origin)?;
            match app
                .services
                .get::<bilibi_base::system::Instances>(bilibi_base::system::SERVICE)?
                .validate_composition()
                .await
            {
                Ok(()) => {}
                Err(Error::Invalid) => {
                    eprintln!("Startup refused: this Shared service release contains a customer code Extension. Install a code-free release, or have the installation owner switch an isolated installation before composing customer code.");
                    return Err(Error::Invalid);
                }
                Err(error) => return Err(error),
            }
            match app.secure_origin().await {
                Ok(true) => {}
                Ok(false) => {
                    eprintln!("APP_ORIGIN must use HTTPS. Only a Dev copy on this computer may use HTTP on a loopback address.");
                    return Err(Error::Invalid);
                }
                Err(error) => {
                    eprintln!("Cannot read the instance settings ({error}). Run `bilibi-web migrate` and `bilibi-web seed-production` first; APP_ORIGIN must also be a valid address.");
                    return Err(error);
                }
            }
            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .map_err(|_| Error::Internal)?;
            let public = if let Some((public_origin, public_bind)) = &public_site {
                let public = app.clone().at_public_site(public_origin);
                match public.secure_origin().await {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!("PUBLIC_ORIGIN must use HTTPS. Only a Dev copy on this computer may use HTTP on a loopback address.");
                        return Err(Error::Invalid);
                    }
                    Err(error) => return Err(error),
                }
                let listener = tokio::net::TcpListener::bind(public_bind)
                    .await
                    .map_err(|_| Error::Internal)?;
                Some((public, listener, public_origin.clone(), public_bind.clone()))
            } else {
                None
            };
            // Colour only on a terminal without NO_COLOR, so logs and services stay plain.
            let colour = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            if let Some(seconds) = launcher_start_seconds() {
                if colour {
                    println!("\x1b[1;32mStarted in {seconds:.1} s\x1b[0m");
                } else {
                    println!("Started in {seconds:.1} s");
                }
            }
            if colour {
                println!("\x1b[1;32mBilibi listening at \x1b[4m{origin}\x1b[0m");
            } else {
                println!("Bilibi listening at {origin}");
            }
            if let Some((_, _, public_origin, _)) = &public {
                if colour {
                    println!("\x1b[1;32mPublic site listening at \x1b[4m{public_origin}\x1b[0m");
                } else {
                    println!("Public site listening at {public_origin}");
                }
            }
            app.listen_address = Some(listen_host(&bind).into());
            let executor = bilibi_base::queue::Executor::from_services(
                &app.services,
                bilibi_base::queue::Config::default(),
            )?;
            let scheduler = bilibi_base::schedule::Scheduler::from_services(
                &app.services,
                bilibi_base::schedule::scheduler::Config::default(),
            )?;
            // Long-lived readers run beside the job slots, one owner per key.
            let residents = bilibi_base::residents::Supervisor::from_services(
                &app.services,
                bilibi_base::residents::Config::default(),
            )?;
            // The outcomes the installer write land in the audit
            // trail through the owning module, not through the workers.
            let software: Arc<bilibi_base::software::Software> =
                app.services.get(bilibi_base::software::SERVICE)?;
            let (stop, stopped) = tokio::sync::watch::channel(false);
            let settling = tokio::spawn(software.clone().run_settling(stopped.clone()));
            let updates = tokio::spawn(software.clone().run_update_checks(stopped.clone()));
            // This program downloads a GitHub release, then hands the checked
            // folder to the installer.
            let downloading = tokio::spawn(software.run_downloads(stopped.clone()));
            let history = tokio::spawn(app.perf.clone().run(stopped.clone()));
            let worker = tokio::spawn(executor.run(stopped.clone()));
            let readers = tokio::spawn(residents.run(stopped.clone()));
            let ticker = tokio::spawn(scheduler.run(stopped));
            let signal = stop.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = signal.send(true);
            });
            let mut staff_stopped = stop.subscribe();
            // The peer address feeds the OAuth per-sender throttle.
            let mut staff_server = Box::pin(
                axum::serve(
                    listener,
                    bilibi_web::staff_router(app)
                        .into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move {
                    while !*staff_stopped.borrow() && staff_stopped.changed().await.is_ok() {}
                })
                .into_future(),
            );
            let result = if let Some((public_app, public_listener, _, _)) = public {
                let mut public_stopped = stop.subscribe();
                let mut public_server = Box::pin(
                    axum::serve(
                        public_listener,
                        bilibi_web::public_router(public_app)
                            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .with_graceful_shutdown(async move {
                        while !*public_stopped.borrow() && public_stopped.changed().await.is_ok() {}
                    })
                    .into_future(),
                );
                let (first, staff_finished) = tokio::select! {
                    result = &mut staff_server => (result, true),
                    result = &mut public_server => (result, false),
                };
                let _ = stop.send(true);
                if staff_finished {
                    let _ = public_server.await;
                } else {
                    let _ = staff_server.await;
                }
                first.map_err(|_| Error::Internal)
            } else {
                staff_server.await.map_err(|_| Error::Internal)
            };
            let _ = stop.send(true);
            updates.await.map_err(|_| Error::Internal)?;
            settling.await.map_err(|_| Error::Internal)??;
            downloading.await.map_err(|_| Error::Internal)??;
            worker.await.map_err(|_| Error::Internal)??;
            readers.await.map_err(|_| Error::Internal)??;
            history.await.map_err(|_| Error::Internal)??;
            ticker.await.map_err(|_| Error::Internal)??;
            result?;
        }
        _ => return Err(Error::Invalid),
    }
    Ok(())
}

/// Seconds since `./bilibi start` began. The launcher leaves its start time (Unix
/// nanoseconds) in the file named by `BILIBI_START_FILE`; the first listening app
/// takes it, so a watched rebuild or a direct run prints nothing.
fn launcher_start_seconds() -> Option<f64> {
    let path = std::env::var_os("BILIBI_START_FILE")?;
    let started: u128 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    std::fs::remove_file(&path).ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(now.saturating_sub(started) as f64 / 1e9)
}

/// The host part of a bind address (`127.0.0.1:3000` is `127.0.0.1`,
/// `[::1]:3000` is `::1`), the "dev" status bar's listen address.
fn listen_host(bind: &str) -> &str {
    let host = bind.rsplit_once(':').map_or(bind, |(host, _)| host);
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

fn origin_host(origin: &str) -> Option<String> {
    origin
        .parse::<axum::http::Uri>()
        .ok()?
        .host()
        .map(str::to_owned)
}

/// Exactly the named `--flag value` pairs, each once, in any order.
fn flags<'a, const N: usize>(args: &'a [String], names: [&str; N]) -> Option<[&'a str; N]> {
    let mut values: [Option<&str>; N] = [None; N];
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let index = names.iter().position(|name| name == flag)?;
        let value = rest.next().filter(|value| !value.starts_with("--"))?;
        if values[index].replace(value).is_some() {
            return None;
        }
    }
    let mut found = [""; N];
    for (slot, value) in found.iter_mut().zip(values) {
        *slot = value?;
    }
    Some(found)
}
