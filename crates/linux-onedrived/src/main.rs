//! linux-onedrived: the OneDrive sync daemon (systemd user service).

mod autostart;
mod backup;
mod conditions;
mod daemon;
mod dbus_iface;
mod mounts;
mod notify;
mod roots;
mod secrets;
mod sharing;
mod update;

use clap::Parser;
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(name = "linux-onedrived", version, about = "Linux OneDrive sync daemon")]
struct Args {
    /// Unmount every configured OneDrive folder and exit (systemd ExecStopPost).
    #[arg(long)]
    cleanup: bool,
    /// Log level filter (overrides the config file), e.g. "debug" or "od_engine=trace".
    #[arg(long)]
    log: Option<String>,
}

/// A plain level applies to this program; libraries stay at "info" (at
/// "debug" the FUSE and HTTP layers log every file operation and request).
/// A filter with targets ("od_engine=trace,info") is used as given.
fn log_filter(level: &str) -> String {
    let level = level.trim();
    if matches!(level, "debug" | "trace") {
        format!("info,od_core={level},od_graph={level},od_engine={level},linux_onedrived={level}")
    } else {
        level.to_string()
    }
}

fn init_logging(filter: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_filter(filter)));
    let registry = tracing_subscriber::registry().with(filter);
    // Under systemd, log to the journal; otherwise to stderr.
    if std::env::var_os("JOURNAL_STREAM").is_some()
        && let Ok(j) = tracing_journald::layer()
    {
        registry.with(j.with_syslog_identifier("linux-onedrived".into())).init();
        return;
    }
    registry.with(tracing_subscriber::fmt::layer().with_target(false)).init();
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = od_core::config::Config::load().unwrap_or_default();
    init_logging(args.log.as_deref().unwrap_or(&config.general.log_level));

    if args.cleanup {
        for a in &config.accounts {
            if mounts::is_mounted(&a.mount_point) {
                mounts::force_unmount(&a.mount_point);
            }
        }
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("onedrived").build()?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    use od_core::dbus::BUS_NAME;
    let conn = zbus::Connection::session().await?;
    let daemon = daemon::Daemon::new(conn.clone()).await?;
    daemon.start().await?;
    // Own the name last, so activation requests only arrive once we're ready.
    if let Err(e) = conn.request_name(BUS_NAME).await {
        tracing::error!("another instance is already running ({e})");
        daemon.stop_all().await;
        return Ok(());
    }
    tracing::info!(version = od_core::VERSION, "linux-onedrived started");
    mounts::sd_notify("READY=1\nSTATUS=Running");

    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received"),
        _ = int.recv() => tracing::info!("SIGINT received"),
        _ = daemon.shutdown.cancelled() => tracing::info!("quit requested"),
    }
    mounts::sd_notify("STOPPING=1");
    daemon.shutdown.cancel();
    daemon.stop_all().await;
    tracing::info!("stopped");
    Ok(())
}
