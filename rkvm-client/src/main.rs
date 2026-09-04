mod client;
mod config;
mod tls;
mod updates;
#[cfg(target_os = "windows")]
mod windows_service;

use clap::Parser;
use config::Config;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use tokio::{fs, signal};
use tracing::subscriber;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::{self, writer::BoxMakeWriter};
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[structopt(name = "rkvm-client", about = "The rkvm client application")]
struct Args {
    #[cfg(target_os = "windows")]
    #[clap(long, hide = true, conflicts_with = "desktop_agent")]
    service: bool,

    #[cfg(target_os = "windows")]
    #[clap(long, hide = true, conflicts_with = "service")]
    desktop_agent: bool,

    #[cfg(target_os = "windows")]
    #[clap(long, hide = true, requires = "desktop_agent")]
    stop_event: Option<String>,

    #[cfg(target_os = "windows")]
    #[clap(long, hide = true, requires = "desktop_agent")]
    log_file: Option<PathBuf>,

    #[clap(help = "Path to configuration file")]
    config_path: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    #[cfg(target_os = "windows")]
    if args.service {
        return match windows_service::run(args.config_path.clone()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Windows service failed: {err}");
                ExitCode::FAILURE
            }
        };
    }

    #[cfg(target_os = "windows")]
    let log_file = args.log_file.as_deref();
    #[cfg(not(target_os = "windows"))]
    let log_file = None;

    if let Err(err) = configure_tracing(log_file) {
        eprintln!("Error configuring logging: {err}");
        return ExitCode::FAILURE;
    }

    run_client(args).await
}

fn configure_tracing(log_file: Option<&Path>) -> io::Result<()> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let writer = match log_file {
        Some(path) => {
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            BoxMakeWriter::new(Mutex::new(file))
        }
        None => BoxMakeWriter::new(io::stdout),
    };
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().without_time().with_writer(writer));

    subscriber::set_global_default(registry).map_err(io::Error::other)?;
    Ok(())
}

async fn run_client(args: Args) -> ExitCode {
    let config = match fs::read_to_string(&args.config_path).await {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("Error reading config: {}", err);
            return ExitCode::FAILURE;
        }
    };

    let config = match toml::from_str::<Config>(&config) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!("Error parsing config: {}", err);
            return ExitCode::FAILURE;
        }
    };

    let connector = match tls::configure(&config.certificate).await {
        Ok(connector) => connector,
        Err(err) => {
            tracing::error!("Error configuring TLS: {}", err);
            return ExitCode::FAILURE;
        }
    };

    tokio::select! {
        result = client::run(&config.server.hostname, config.server.port, connector, &config.password) => {
            if let Err(err) = result {
                tracing::error!("Error: {}", err);
                return ExitCode::FAILURE;
            }
        }
        // This is needed to properly clean libevdev stuff up.
        result = shutdown_signal(stop_event(&args)) => {
            if let Err(err) = result {
                tracing::error!("Error setting up signal handler: {}", err);
                return ExitCode::FAILURE;
            }

            tracing::info!("Exiting on signal");
        }
    }

    ExitCode::SUCCESS
}

#[cfg(target_os = "windows")]
fn stop_event(args: &Args) -> Option<String> {
    args.stop_event.clone()
}

#[cfg(not(target_os = "windows"))]
fn stop_event(_args: &Args) -> Option<String> {
    None
}

async fn shutdown_signal(stop_event: Option<String>) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    if let Some(name) = stop_event {
        return windows_service::wait_for_stop_event(name).await;
    }

    let _ = stop_event;
    signal::ctrl_c().await
}
