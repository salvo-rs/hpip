mod config;
mod encoding;
mod error;
mod handler;
mod hoops;
mod options;
mod port;
mod tls;
pub mod util;

use std::hash::{BuildHasher, RandomState};
use std::mem;
use std::net::IpAddr;
use std::process::exit;
use std::sync::Arc;

use config::AppConfig;
use error::Error;
use options::Options;
use salvo::prelude::*;

fn main() {
    let result = actual_main();
    exit(result);
}

fn actual_main() -> i32 {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    rt.block_on(async {
        if let Err(err) = result_main().await {
            eprintln!("{}", err.0);
            1
        } else {
            0
        }
    })
}

async fn result_main() -> Result<(), Error> {
    let mut opts = Options::parse()?;

    if opts.generate_tls {
        opts.tls_data = Some(tls::generate_tls_data(&opts.temp_directory)?);
    }

    for path in mem::take(&mut opts.generate_path_auth) {
        opts.path_auth_data.insert(path, Some(generate_auth_data()));
    }

    let config = Arc::new(AppConfig::new(&opts));

    // Build router
    let router = build_router(config.clone());

    // Determine port
    let port = if let Some(p) = opts.port {
        p
    } else {
        port::find_port(
            opts.bind_address,
            util::PORT_SCAN_LOWEST,
            util::PORT_SCAN_HIGHEST,
        )
        .await
        .ok_or_else(|| Error("Starting server: no free ports".into()))?
    };

    // Print startup info
    if opts.loglevel < options::LogLevel::NoStartup {
        if opts.log_colour {
            use colored::Colorize;
            print!(
                "Hosting \"{}\" on port {}",
                opts.hosted_directory.0.bold(),
                port.to_string().bold()
            );
        } else {
            print!("Hosting \"{}\" on port {}", opts.hosted_directory.0, port);
        }
        if opts.bind_address != IpAddr::from([0, 0, 0, 0]) {
            print!(" under address {}", opts.bind_address);
        }
        print!(" with");
        match opts.tls_data.as_ref() {
            Some(((id, _), _)) => print!(" TLS certificate from \"{}\"", id),
            None => print!("out TLS"),
        }
        println!(
            " and {} authentication...",
            if opts.path_auth_data.is_empty() {
                "no"
            } else {
                "basic"
            }
        );

        let scheme = if opts.tls_data.is_some() {
            "https"
        } else {
            "http"
        };
        if opts.bind_address.is_unspecified() {
            if let Ok(ifas) = local_ip_address::list_afinet_netifas() {
                for (_name, ip) in &ifas {
                    match ip {
                        IpAddr::V4(v4) => println!("  {}://{}:{}", scheme, v4, port),
                        IpAddr::V6(v6) => println!("  {}://[{}]:{}", scheme, v6, port),
                    }
                }
            }
        } else {
            match opts.bind_address {
                IpAddr::V4(v4) => println!("  {}://{}:{}", scheme, v4, port),
                IpAddr::V6(v6) => println!("  {}://[{}]:{}", scheme, v6, port),
            }
        }

        if let Some(band) = opts.request_bandwidth {
            println!("Requests limited to {}B/s.", band);
        }

        for (ext, mime_type) in &opts.mime_type_overrides {
            match ext.to_string_lossy().as_ref() {
                "" => println!("Serving files with no extension as {}.", mime_type),
                ext => println!("Serving files with .{} extension as {}.", ext, mime_type),
            }
        }
    }

    if !opts.path_auth_data.is_empty() && opts.loglevel < options::LogLevel::NoAuth {
        println!("Basic authentication credentials:");
        for (path, creds) in &opts.path_auth_data {
            if let Some(ad) = creds {
                let mut itr = ad.split(':');
                let user = itr.next().unwrap_or("");
                let pass = itr.next().unwrap_or("");
                println!("  /{}\t{}\t{}", path, user, pass);
            } else {
                println!("  /{}\t(disabled)", path);
            }
        }
    }

    if opts.loglevel < options::LogLevel::NoStartup {
        println!("Ctrl-C to stop.");
        println!();
    }

    let bind_addr = format!("{}:{}", opts.bind_address, port);

    // Start the server
    let acceptor = TcpListener::new(bind_addr).bind().await;

    // Spawn cache pruning task if needed
    let config_prune = config.clone();
    if opts.encoded_prune.is_some() {
        let interval = config.prune_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                config_prune.prune();
            }
        });
    }

    // Graceful shutdown on Ctrl-C
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl-C");
        let _ = shutdown_tx.send(());
    });

    let server = Server::new(acceptor);

    tokio::select! {
        _ = server.serve(router) => {}
        _ = shutdown_rx => {
            config::log_msg(config.log, "Shutting down...");
        }
    }

    config.clean_temp_dirs(&opts.temp_directory, opts.generate_tls);
    Ok(())
}

/// Middleware that injects the AppConfig into the Depot.
struct InjectConfig {
    config: Arc<AppConfig>,
}

#[handler]
impl InjectConfig {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        depot.inject(self.config.clone());
        ctrl.call_next(req, depot, res).await;
    }
}

fn build_router(config: Arc<AppConfig>) -> Router {
    let inject = InjectConfig {
        config: config.clone(),
    };

    let webdav_level = config.webdav;

    // Build a function that adds common method handlers to a router
    fn add_methods(router: Router, webdav_level: options::WebDavLevel) -> Router {
        let mut r = router
            .get(handler::get::handle_get)
            .put(handler::put::handle_put)
            .delete(handler::delete::handle_delete)
            .post(handler::archive::handle_post_archive)
            .options(handler::options_handler::handle_options);

        // WebDAV methods via custom filter
        if webdav_level == options::WebDavLevel::All {
            r = r
                .push(
                    Router::new()
                        .filter_fn(|req, _| req.method().as_str() == "PROPFIND")
                        .goal(handler::webdav::handle_propfind),
                )
                .push(
                    Router::new()
                        .filter_fn(|req, _| req.method().as_str() == "PROPPATCH")
                        .goal(handler::webdav::handle_proppatch),
                );
        }
        if webdav_level >= options::WebDavLevel::MkColMoveOnly {
            r = r
                .push(
                    Router::new()
                        .filter_fn(|req, _| req.method().as_str() == "MKCOL")
                        .goal(handler::webdav::handle_mkcol),
                )
                .push(
                    Router::new()
                        .filter_fn(|req, _| req.method().as_str() == "MOVE")
                        .goal(handler::webdav::handle_move),
                );
        }
        if webdav_level == options::WebDavLevel::All {
            r = r.push(
                Router::new()
                    .filter_fn(|req, _| req.method().as_str() == "COPY")
                    .goal(handler::webdav::handle_copy),
            );
        }

        r
    }

    let mut router = Router::new()
        .hoop(inject)
        .hoop(hoops::additional_headers::AdditionalHeadersHoop)
        .hoop(hoops::logging::LoggingHoop);

    // Add DAV header for WebDAV mode
    if webdav_level >= options::WebDavLevel::MkColMoveOnly {
        router = router.hoop(DavHeaderHoop);
    }

    router = router.push(add_methods(Router::with_path("{**rest}"), webdav_level));

    add_methods(router, webdav_level)
}

/// Hoop to add the DAV: 1 header to all responses
struct DavHeaderHoop;

#[handler]
impl DavHeaderHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        ctrl.call_next(req, depot, res).await;
        res.headers_mut().insert("DAV", "1".parse().unwrap());
    }
}

/// Generate random username:password auth credentials.
fn generate_auth_data() -> String {
    const USERNAME_SET_LEN: usize =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789".len();
    const PASSWORD_SET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789~!@#$%^&*()_+`-=[]{}|;',./<>?";

    let rnd = RandomState::new();
    let username_len = (rnd.hash_one((0u64, 0u64)) % (12 - 6) + 6) as usize;
    let password_len = (rnd.hash_one((0u64, 1u64)) % (25 - 10) + 10) as usize;

    let mut res = String::with_capacity(username_len + 1 + password_len);
    for b in 0..username_len {
        res.push(
            PASSWORD_SET[(rnd.hash_one((1u64, b as u64)) % (USERNAME_SET_LEN as u64)) as usize]
                as char,
        );
    }
    res.push(':');
    for b in 0..password_len {
        res.push(
            PASSWORD_SET[(rnd.hash_one((2u64, b as u64)) % (PASSWORD_SET.len() as u64)) as usize]
                as char,
        );
    }
    res
}
