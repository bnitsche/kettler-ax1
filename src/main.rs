mod protocol;
mod keypad;
mod key_actions;
mod keys;
mod recovery;
mod elevation;
mod program;
mod session;
mod db;
mod bridge;
mod device;
mod sim;
mod transport;
mod ride;
mod web;
mod monitor;
mod adapter;
mod hrm;
mod strap;
mod demo;
#[cfg(target_os = "linux")]
mod strap_bluez;

use std::path::PathBuf;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kettler", about = "Kettler AX1 training server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web server
    Server {
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,
    },
    /// Run the serial-to-TCP bridge
    Bridge {
        #[arg(long)]
        port: String,
        #[arg(long, default_value = "0.0.0.0:4001")]
        listen: String,
        /// Where the handlebar key presses are published
        #[arg(long, default_value = "0.0.0.0:4002")]
        keys_listen: String,
    },
    /// Run the AX1 simulator
    Simulate {
        #[arg(long, default_value = "127.0.0.1:4001")]
        listen: String,
        #[arg(long, default_value_t = 70)]
        rpm: u32,
    },
    /// Write a new demo database with Bob and Alice (for screenshots)
    #[command(hide = true)]
    DemoData {
        #[arg(long)]
        db: PathBuf,
    },
}

/// The clap command; `kettler --version` prints the same version and commit as GET /api/version.
fn cli_command() -> clap::Command {
    let version: &'static str = Box::leak(format!("{} ({})", web::VERSION, web::commit()).into_boxed_str());
    Cli::command().version(version)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    // `kettler --version` prints the same version and commit as GET /api/version.
    let cli = Cli::from_arg_matches(&cli_command().get_matches())?;
    match cli.command {
        Command::Server { listen } => server(listen.parse()?).await,
        Command::Bridge { port, listen, keys_listen } => bridge::run(port, listen.parse()?, keys_listen.parse()?).await,
        Command::Simulate { listen, rpm } => sim::serve_tcp(listen.parse()?, rpm.try_into()?).await,
        Command::DemoData { db } => demo::generate(&db, monitor::now_ms() / 1000),
    }
}

async fn server(listen: std::net::SocketAddr) -> anyhow::Result<()> {
    let started_at_ms = monitor::now_ms();
    let url = std::env::var("KETTLER_DEVICE").unwrap_or_else(|_| {
        tracing::warn!("KETTLER_DEVICE nicht gesetzt – Simulation aktiv");
        "sim://".into()
    });
    let db_path = PathBuf::from(std::env::var("KETTLER_DB").unwrap_or_else(|_| "./kettler.db".into()));
    tracing::info!("device {url}, database {}", db_path.display());
    let device_url = url.clone();
    let url = url.parse::<transport::DeviceUrl>().map_err(anyhow::Error::msg)?;
    let serial_port = match &url {
        transport::DeviceUrl::Serial(port) => Some(port.clone()),
        _ => None,
    };
    let keys_setting = std::env::var("KETTLER_KEYS").ok();
    let keys_source = keys::resolve(keys_setting.as_deref(), &url).map_err(anyhow::Error::msg)?;
    tracing::info!(?keys_source, "key source");
    let db = db::open(&db_path)?;
    // The handle goes straight into the manager so no Connected event is missed.
    let device = device::spawn_device(url, Duration::from_secs(1));
    let device_monitor = device.monitor();
    let keys = match &keys_source {
        keys::KeysSource::Device => "device",
        keys::KeysSource::Tcp(_) => "tcp",
        keys::KeysSource::Off => "off",
    };
    let modem_keys = matches!(keys_source, keys::KeysSource::Device);
    let bluetooth: strap::Mode =
        std::env::var("KETTLER_BLUETOOTH").unwrap_or_default().parse().map_err(anyhow::Error::msg)?;
    let straps = strap::StrapHub::spawn(bluetooth, device_monitor.clone());
    tracing::info!(?bluetooth, availability = ?straps.availability(), "chest straps");
    straps.set_known(db.list_straps()?);
    let rides = ride::RideManager::with_straps(device, db.clone(), modem_keys, straps.clone());
    // Serial keys reach the manager as device events.
    if let keys::KeysSource::Tcp(addr) = keys_source {
        let keys_rides = rides.clone();
        keys::spawn_tcp(addr, move |input| keys_rides.key(input));
    }
    let allowed_hosts = std::env::var("KETTLER_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    let manager = rides.clone();
    let scratch_dir = match db_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let service = web::ServiceInfo {
        monitor: device_monitor,
        device_url,
        serial_port,
        keys,
        db_path: db_path.clone(),
        started_at_ms,
    };
    let state =
        web::AppState { rides, db, limits: protocol::DeviceLimits::default(), allowed_hosts, scratch_dir, service };
    // On Ctrl+C or SIGTERM: end the ride, set the minimum load, let the straps go, then
    // stop serving.
    web::run(listen, state, async move {
        shutdown_signal().await;
        tracing::info!("shutdown requested");
        manager.shutdown().await;
        straps.shutdown().await;
    })
    .await?;
    tracing::info!("server stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for Ctrl+C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(e) => {
                tracing::error!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_version_contains_version_and_commit() {
        let text = cli_command().render_version();
        assert!(text.contains(web::VERSION), "{text}");
        assert!(text.contains(web::commit()), "{text}");
    }
}
