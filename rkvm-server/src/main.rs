#[cfg(target_os = "linux")]
mod config;
#[cfg(any(target_os = "linux", test))]
mod held_inputs;
#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
mod tls;

#[cfg(target_os = "linux")]
use clap::Parser;
#[cfg(target_os = "linux")]
use config::Config;
#[cfg(target_os = "linux")]
use std::future;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::{fs, signal, time};
#[cfg(target_os = "linux")]
use tracing::subscriber;
#[cfg(target_os = "linux")]
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
#[cfg(target_os = "linux")]
use tracing_subscriber::fmt;
#[cfg(target_os = "linux")]
use tracing_subscriber::prelude::*;

#[cfg(target_os = "linux")]
#[derive(Parser)]
#[structopt(name = "rkvm-server", about = "The rkvm server application")]
struct Args {
    #[structopt(help = "Path to configuration file")]
    config_path: PathBuf,
    #[structopt(help = "Shutdown after N seconds", long, short)]
    shutdown_after: Option<u64>,
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().without_time());

    subscriber::set_global_default(registry).unwrap();

    let args = Args::parse();
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

    let acceptor = match tls::configure(&config.certificate, &config.key).await {
        Ok(acceptor) => acceptor,
        Err(err) => {
            tracing::error!("Error configuring TLS: {}", err);
            return ExitCode::FAILURE;
        }
    };

    let shutdown = async {
        match args.shutdown_after {
            Some(shutdown_after) => time::sleep(Duration::from_secs(shutdown_after)).await,
            None => future::pending().await,
        }
    };

    let switch_keys = config.switch_keys.into_iter().map(Into::into).collect();
    let propagate_switch_keys = config.propagate_switch_keys.unwrap_or(true);

    tokio::select! {
        result = server::run(config.listen, acceptor, &config.password, &switch_keys, propagate_switch_keys) => {
            if let Err(err) = result {
                tracing::error!("Error: {}", err);
                return ExitCode::FAILURE;
            }
        }
        // This is needed to properly clean libevdev stuff up.
        result = signal::ctrl_c() => {
            if let Err(err) = result {
                tracing::error!("Error setting up signal handler: {}", err);
                return ExitCode::FAILURE;
            }

            tracing::info!("Exiting on signal");
        }
        _ = shutdown => {
            tracing::info!("Shutting down as requested");
        }
    }

    ExitCode::SUCCESS
}

#[cfg(target_os = "windows")]
fn main() -> ExitCode {
    eprintln!("rkvm-server is only supported on Linux");
    ExitCode::FAILURE
}
