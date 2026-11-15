mod cache;
mod config;
mod db;
mod http;
mod imaging;
mod metrics;
mod scan;
mod schedule;

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

fn usage() -> ! {
    eprintln!("usage: frameshuffle [--config FILE] [--version]\n  config default: $FRAMESHUFFLE_CONFIG or /config/config.toml (optional; defaults + environment work too)");
    std::process::exit(2)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let mut path = std::env::var("FRAMESHUFFLE_CONFIG")
        .ok()
        .or(Some("/config/config.toml".into()));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" | "-c" => path = Some(args.next().unwrap_or_else(|| usage())),
            "--version" | "-V" => {
                println!("frameshuffle {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => usage(),
        }
    }

    let cfg = Arc::new(config::Config::load(path.as_deref()).context("loading configuration")?);
    info!(
        library = %cfg.library.dir, db = %cfg.storage.db_path, cache = %cfg.storage.cache_dir,
        profiles = cfg.profiles.len(), clients = cfg.clients.len(), tz = %cfg.server.timezone,
        heic = cfg!(feature = "heic"), "frameshuffle {} starting", env!("CARGO_PKG_VERSION")
    );
    if !std::path::Path::new(&cfg.library.dir).is_dir() {
        warn!(
            "library directory {} does not exist or is not a directory",
            cfg.library.dir
        );
    }
    std::fs::create_dir_all(&cfg.storage.cache_dir).context("creating cache dir")?;

    let db = db::Db::open(&cfg.storage.db_path)?;
    let metrics = Arc::new(metrics::Metrics::new());
    let scanner = scan::Scanner::new(db.clone(), cfg.clone());
    let state = http::AppState::new(cfg.clone(), db, scanner.clone(), metrics.clone())?;

    cache::spawn_janitor(cfg.clone(), state.fingerprints(), metrics.clone());

    if cfg.scan.on_startup {
        scanner.trigger(false);
    } else {
        scanner
            .state
            .first_walk_done
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if cfg.scan.watch {
        if let Err(e) = scan::spawn_watcher(
            scanner.clone(),
            cfg.library.dir.clone(),
            Duration::from_secs(cfg.scan.watch_debounce_secs),
        ) {
            warn!("could not start inotify watcher: {e:#} (use /admin/rescan or scan.periodic_minutes instead)");
        }
    }
    if cfg.scan.periodic_minutes > 0 {
        scan::spawn_periodic(
            scanner.clone(),
            Duration::from_secs(cfg.scan.periodic_minutes * 60),
        );
    }

    let listener = tokio::net::TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    info!("listening on {}", cfg.server.bind);
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    info!("bye");
    Ok(())
}

async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
    }
    info!("shutting down");
}
