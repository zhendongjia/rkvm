use crate::updates::{self, Repeater};
use rkvm_input::writer::Writer;
use rkvm_net::auth::{AuthChallenge, AuthStatus};
use rkvm_net::message::Message;
use rkvm_net::version::Version;
use rkvm_net::{Pong, Update};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io;
#[cfg(target_os = "windows")]
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::time;
use tokio_rustls::rustls::ServerName;
use tokio_rustls::TlsConnector;

struct Writers {
    devices: HashMap<usize, Writer>,
    #[cfg(target_os = "windows")]
    errors: InputErrorReporter,
}

impl Writers {
    fn new() -> Self {
        Self {
            devices: HashMap::new(),
            #[cfg(target_os = "windows")]
            errors: InputErrorReporter::default(),
        }
    }

    async fn write(&mut self, id: usize, event: &rkvm_input::event::Event) -> Result<(), Error> {
        let result = self
            .devices
            .get_mut(&id)
            .ok_or_else(|| {
                Error::Network(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Server sent an event to a nonexistent device",
                ))
            })?
            .write(event)
            .await;

        match result {
            Ok(()) => Ok(()),
            #[cfg(target_os = "windows")]
            Err(err) => {
                self.errors.report(id, "event", &err);
                Ok(())
            }
            #[cfg(not(target_os = "windows"))]
            Err(err) => Err(Error::Input(err)),
        }
    }
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct InputErrorReporter {
    last_warning: Option<Instant>,
    suppressed: usize,
}

#[cfg(target_os = "windows")]
impl InputErrorReporter {
    fn report(&mut self, id: usize, operation: &'static str, err: &io::Error) {
        let now = Instant::now();
        if self
            .last_warning
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            self.suppressed += 1;
            return;
        }

        let suppressed = std::mem::take(&mut self.suppressed);
        self.last_warning = Some(now);
        tracing::warn!(
            id,
            operation,
            suppressed,
            error = %err,
            "Windows rejected remote input; keeping the server connection alive"
        );
    }
}

impl Repeater for Writers {
    fn deadline(&self) -> Option<Instant> {
        #[cfg(target_os = "windows")]
        {
            self.devices.values().filter_map(Writer::next_repeat).min()
        }
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    }

    fn repeat(&mut self, now: Instant) -> io::Result<()> {
        #[cfg(target_os = "windows")]
        for (id, writer) in &mut self.devices {
            if let Err(err) = writer.repeat(now) {
                self.errors.report(*id, "repeat", &err);
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = now;
        }
        Ok(())
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Network error: {0}")]
    Network(io::Error),
    #[error("Input error: {0}")]
    Input(io::Error),
    #[error("Incompatible server version (got {server}, expected {client})")]
    Version { server: Version, client: Version },
    #[error("Invalid password")]
    Auth,
}

pub async fn run(
    hostname: &ServerName,
    port: u16,
    connector: TlsConnector,
    password: &str,
) -> Result<(), Error> {
    // Intentionally don't impose any timeout for TCP connect.
    let stream = match hostname {
        ServerName::DnsName(name) => TcpStream::connect(&(name.as_ref(), port)).await,
        ServerName::IpAddress(address) => TcpStream::connect(&(*address, port)).await,
        _ => unimplemented!("Unhandled rustls ServerName variant: {:?}", hostname),
    }
    .map_err(Error::Network)?;

    tracing::info!("Connected to server");

    let stream = rkvm_net::timeout(
        rkvm_net::TLS_TIMEOUT,
        connector.connect(hostname.clone(), stream),
    )
    .await
    .map_err(Error::Network)?;

    tracing::info!("TLS connected");

    let mut stream = BufStream::with_capacity(1024, 1024, stream);

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        Version::CURRENT.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await
    .map_err(Error::Network)?;

    let version = rkvm_net::timeout(rkvm_net::READ_TIMEOUT, Version::decode(&mut stream))
        .await
        .map_err(Error::Network)?;

    if version != Version::CURRENT {
        return Err(Error::Version {
            server: Version::CURRENT,
            client: version,
        });
    }

    let challenge = rkvm_net::timeout(rkvm_net::READ_TIMEOUT, AuthChallenge::decode(&mut stream))
        .await
        .map_err(Error::Network)?;

    let response = challenge.respond(password);

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        response.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await
    .map_err(Error::Network)?;

    let status = rkvm_net::timeout(rkvm_net::READ_TIMEOUT, AuthStatus::decode(&mut stream))
        .await
        .map_err(Error::Network)?;

    match status {
        AuthStatus::Passed => {}
        AuthStatus::Failed => return Err(Error::Auth),
    }

    tracing::info!("Authenticated successfully");

    let mut start = Instant::now();

    let mut interval = time::interval(rkvm_net::PING_INTERVAL + rkvm_net::READ_TIMEOUT);
    let mut writers = Writers::new();

    // Interval ticks immediately after creation.
    interval.tick().await;

    loop {
        let update = updates::receive(&mut stream, &mut interval, &mut writers)
            .await
            .map_err(|err| match err {
                updates::ReceiveError::Network(err) => Error::Network(err),
                updates::ReceiveError::Input(err) => Error::Input(err),
            })?;

        match update {
            Update::CreateDevice {
                id,
                name,
                vendor,
                product,
                version,
                rel,
                abs,
                keys,
                delay,
                period,
            } => {
                let entry = writers.devices.entry(id);
                if let Entry::Occupied(_) = entry {
                    return Err(Error::Network(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Server created the same device twice",
                    )));
                }

                let writer = async {
                    Writer::builder()?
                        .name(&name)
                        .vendor(vendor)
                        .product(product)
                        .version(version)
                        .rel(rel)?
                        .abs(abs)?
                        .key(keys)?
                        .delay(delay)?
                        .period(period)?
                        .build()
                        .await
                }
                .await
                .map_err(Error::Input)?;

                entry.or_insert(writer);

                tracing::info!(
                    id = %id,
                    name = ?name,
                    vendor = %vendor,
                    product = %product,
                    version = %version,
                    "Created new device"
                );
            }
            Update::DestroyDevice { id } => {
                if writers.devices.remove(&id).is_none() {
                    return Err(Error::Network(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Server destroyed a nonexistent device",
                    )));
                }

                tracing::info!(id = %id, "Destroyed device");
            }
            Update::Event { id, event } => {
                writers.write(id, &event).await?;

                tracing::trace!(id = %id, "Wrote an event to device");
            }
            Update::Ping => {
                let duration = start.elapsed();
                tracing::debug!(duration = ?duration, "Received ping");

                start = Instant::now();
                interval.reset();

                rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
                    Pong.encode(&mut stream).await?;
                    stream.flush().await?;

                    Ok(())
                })
                .await
                .map_err(Error::Network)?;

                let duration = start.elapsed();
                tracing::debug!(duration = ?duration, "Sent pong");
            }
        }
    }
}
