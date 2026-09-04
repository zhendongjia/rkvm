use crate::held_inputs::HeldInputs;
use rkvm_input::abs::{AbsAxis, AbsInfo};
use rkvm_input::event::Event;
use rkvm_input::key::{Key, KeyEvent};
use rkvm_input::monitor::Monitor;
use rkvm_input::rel::RelAxis;
use rkvm_input::sync::SyncEvent;
use rkvm_net::auth::{AuthChallenge, AuthResponse, AuthStatus};
use rkvm_net::message::Message;
use rkvm_net::version::Version;
use rkvm_net::{Pong, Update};
use slab::Slab;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::time::Instant;
use thiserror::Error;
use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::watch;
use tokio::time;
use tokio_rustls::TlsAcceptor;
use tracing::Instrument;

type ClientEntry = (Sender<Update>, SocketAddr);

#[derive(Error, Debug)]
pub enum Error {
    #[error("Network error: {0}")]
    Network(io::Error),
    #[error("Input error: {0}")]
    Input(io::Error),
    #[error("Event queue overflow")]
    Overflow,
}

pub async fn run(
    listen: SocketAddr,
    acceptor: TlsAcceptor,
    password: &str,
    switch_keys: &HashSet<Key>,
    propagate_switch_keys: bool,
) -> Result<(), Error> {
    let listener = TcpListener::bind(&listen).await.map_err(Error::Network)?;
    tracing::info!("Listening on {}", listen);

    let mut monitor = Monitor::new();
    let mut devices = Slab::<Device>::new();
    let mut clients = Slab::<ClientEntry>::new();
    let mut current = 0;
    let mut previous = 0;
    let mut changed = false;
    let mut pressed_keys = HashSet::new();
    let mut held_inputs = HeldInputs::default();

    let (events_sender, mut events_receiver) = mpsc::channel(1);

    loop {
        let event = async { events_receiver.recv().await.unwrap() };

        tokio::select! {
            result = listener.accept() => {
                let (stream, addr) = result.map_err(Error::Network)?;
                let acceptor = acceptor.clone();
                let password = password.to_owned();

                remove_closed_clients(&mut clients, &mut held_inputs, &mut current);

                let init_updates = devices
                    .iter()
                    .map(|(id, device)| Update::CreateDevice {
                        id,
                        name: device.name.clone(),
                        version: device.version,
                        vendor: device.vendor,
                        product: device.product,
                        rel: device.rel.clone(),
                        abs: device.abs.clone(),
                        keys: device.keys.clone(),
                        delay: device.delay,
                        period: device.period,
                    })
                    .collect();

                let (sender, receiver) = mpsc::channel(1);
                clients.insert((sender, addr));

                let span = tracing::info_span!("connection", addr = %addr);
                tokio::spawn(
                    async move {
                        tracing::info!("Connected");

                        match client(init_updates, receiver, stream, acceptor, &password).await {
                            Ok(()) => tracing::info!("Disconnected"),
                            Err(err) => tracing::error!("Disconnected: {}", err),
                        }
                    }
                    .instrument(span),
                );
            }
            result = monitor.read() => {
                let mut interceptor = result.map_err(Error::Input)?;

                let name = interceptor.name().to_owned();
                let id = devices.vacant_key();
                let version = interceptor.version();
                let vendor = interceptor.vendor();
                let product = interceptor.product();
                let rel = interceptor.rel().collect::<HashSet<_>>();
                let abs = interceptor.abs().collect::<HashMap<_,_>>();
                let keys = interceptor.key().collect::<HashSet<_>>();
                let repeat = interceptor.repeat();

                for (_, (sender, _)) in &clients {
                    let update = Update::CreateDevice {
                        id,
                        name: name.clone(),
                        version: version.clone(),
                        vendor: vendor.clone(),
                        product: product.clone(),
                        rel: rel.clone(),
                        abs: abs.clone(),
                        keys: keys.clone(),
                        delay: repeat.delay,
                        period: repeat.period,
                    };

                    let _ = sender.send(update).await;
                }

                let (interceptor_sender, mut interceptor_receiver) = mpsc::channel(32);
                devices.insert(Device {
                    name,
                    version,
                    vendor,
                    product,
                    rel,
                    abs,
                    keys,
                    delay: repeat.delay,
                    period: repeat.period,
                    sender: interceptor_sender,
                });

                let events_sender = events_sender.clone();
                tokio::spawn(async move {
                    'intercept: loop {
                        tokio::select! {
                            event = interceptor.read() => {
                                if event.is_err() | events_sender.send((id, event)).await.is_err() {
                                    break;
                                }
                            }
                            batch = interceptor_receiver.recv() => {
                                let mut batch = match batch {
                                    Some(batch) => batch,
                                    None => break,
                                };

                                batch.wait_for_previous().await;
                                for event in &batch.events {
                                    match interceptor.write(event).await {
                                        Ok(()) => {},
                                        Err(err) => {
                                            let _ = events_sender.send((id, Err(err))).await;
                                            break 'intercept;
                                        }
                                    }
                                }
                                batch.finish();

                                tracing::trace!(id = %id, "Wrote an event to device");
                            }
                        }
                    }
                });

                let device = &devices[id];

                tracing::info!(
                    id = %id,
                    name = ?device.name,
                    vendor = %device.vendor,
                    product = %device.product,
                    version = %device.version,
                    "Registered new device"
                );
            }
            (id, result) = event => match result {
                Ok(event) => {
                    // A desktop-switching client commonly reconnects before
                    // the old connection notices EOF. Prune the old sender on
                    // the very next physical event so the shortcut cannot
                    // select a stale destination.
                    remove_closed_clients(&mut clients, &mut held_inputs, &mut current);
                    let switch_action = match event {
                        Event::Key(key) => update_switch_chord(
                            key,
                            switch_keys,
                            &mut pressed_keys,
                            &mut changed,
                        ),
                        _ => SwitchAction::Unrelated,
                    };
                    let press = switch_action != SwitchAction::Unrelated;

                    // Who to send this event to.
                    let mut idx = current;
                    let mut switched_from = None;

                    match switch_action {
                        SwitchAction::Activate => {
                            current = next_destination(current, &clients);

                            previous = idx;
                            if current != idx {
                                switched_from = Some(idx);
                            }

                            if current != 0 {
                                tracing::info!(idx = %current, addr = %clients[current - 1].1, "Switched client");
                            } else {
                                tracing::info!(idx = %current, "Switched client");
                            }
                        }
                        SwitchAction::RoutePrevious => {
                            idx = previous;
                        }
                        SwitchAction::Unrelated | SwitchAction::ForwardCurrent => {}
                    }

                    let events = if press && !propagate_switch_keys {
                        Vec::new()
                    } else {
                        [event].into_iter()
                            .chain(press.then_some(Event::Sync(SyncEvent::All))).collect()
                    };

                    if let Some(destination) = switched_from {
                        // Do this even when the switch shortcut is suppressed.
                        // The old peer remains connected, so Drop alone cannot
                        // stop a held letter from repeating indefinitely there.
                        let mut shortcut_done = None;
                        for (device, events) in held_inputs.finish_switch(destination, id, events) {
                            let mut batch = InputBatch::new(events);
                            if destination == 0 {
                                batch.after_shortcut(&mut shortcut_done);
                            }
                            forward(destination, device, batch, &devices, &mut clients,
                                &mut held_inputs, &mut current).await?;
                        }
                    } else if !events.is_empty() {
                        forward(idx, id, InputBatch::new(events), &devices, &mut clients,
                            &mut held_inputs, &mut current).await?;
                    }
                }
                Err(err) if err.kind() == ErrorKind::BrokenPipe => {
                    for (_, (sender, _)) in &clients {
                        let _ = sender.send(Update::DestroyDevice { id }).await;
                    }
                    devices.remove(id);
                    held_inputs.remove_device(id);

                    tracing::info!(id = %id, "Destroyed device");
                }
                Err(err) => return Err(Error::Input(err)),
            }
        }
    }
}

fn remove_closed_clients(
    clients: &mut Slab<ClientEntry>,
    held: &mut HeldInputs,
    current: &mut usize,
) {
    clients.retain(|id, (client, _)| {
        if client.is_closed() {
            held.remove_destination(id + 1);
            false
        } else {
            true
        }
    });
    if *current != 0 && !clients.contains(*current - 1) {
        *current = 0;
    }
}

fn next_destination(current: usize, clients: &Slab<ClientEntry>) -> usize {
    clients
        .iter()
        .map(|(id, _)| id + 1)
        .find(|destination| *destination > current)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SwitchAction {
    Unrelated,
    ForwardCurrent,
    Activate,
    RoutePrevious,
}

fn update_switch_chord(
    event: KeyEvent,
    switch_keys: &HashSet<Key>,
    pressed_keys: &mut HashSet<Key>,
    changed: &mut bool,
) -> SwitchAction {
    if !switch_keys.contains(&event.key) {
        return SwitchAction::Unrelated;
    }

    if event.down {
        pressed_keys.insert(event.key);
    } else {
        pressed_keys.remove(&event.key);
    }

    if *changed {
        if pressed_keys.is_empty() {
            *changed = false;
        }
        // Repeats and releases still belong to the destination where the
        // shortcut began. In particular, an EV_KEY value of 2 must never
        // cycle through destinations while the chord remains held.
        SwitchAction::RoutePrevious
    } else if pressed_keys.len() == switch_keys.len() {
        *changed = true;
        SwitchAction::Activate
    } else {
        SwitchAction::ForwardCurrent
    }
}

struct Device {
    name: CString,
    vendor: u16,
    product: u16,
    version: u16,
    rel: HashSet<RelAxis>,
    abs: HashMap<AbsAxis, AbsInfo>,
    keys: HashSet<Key>,
    delay: Option<i32>,
    period: Option<i32>,
    sender: Sender<InputBatch>,
}

#[derive(Debug)]
struct InputBatch {
    events: Vec<Event>,
    previous: Option<watch::Receiver<bool>>,
    finished: Option<watch::Sender<bool>>,
}

impl InputBatch {
    fn new(events: Vec<Event>) -> Self {
        Self {
            events,
            previous: None,
            finished: None,
        }
    }

    fn after_shortcut(&mut self, shortcut_done: &mut Option<watch::Receiver<bool>>) {
        if let Some(shortcut_done) = shortcut_done {
            // Every cleanup waits directly for the first (shortcut) batch.
            // Removing an intermediate device must not bypass this dependency.
            self.previous = Some(shortcut_done.clone());
        } else {
            let (finished, next) = watch::channel(false);
            self.finished = Some(finished);
            *shortcut_done = Some(next);
        }
    }

    async fn wait_for_previous(&mut self) {
        if let Some(mut previous) = self.previous.take() {
            // A failed/removed first device also unlocks remaining cleanup.
            while !*previous.borrow_and_update() {
                if previous.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    fn finish(mut self) {
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(true);
        }
    }
}

async fn forward(
    destination: usize,
    device: usize,
    batch: InputBatch,
    devices: &Slab<Device>,
    clients: &mut Slab<ClientEntry>,
    held: &mut HeldInputs,
    current: &mut usize,
) -> Result<(), Error> {
    let keys = batch
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Key(key) => Some(*key),
            _ => None,
        })
        .collect::<Vec<_>>();
    if destination == 0 {
        // The interceptor can itself be waiting to send into the event queue;
        // awaiting its input channel here would cause a circular wait.
        let Some(device) = devices.get(device) else {
            return Ok(());
        };
        // A switch releases an arbitrary number of held keys. One batch uses
        // one queue slot, rather than overflowing the 32-slot input queue.
        match device.sender.try_send(batch) {
            Ok(()) => {}
            Err(TrySendError::Closed(_)) => return Ok(()),
            Err(TrySendError::Full(_)) => return Err(Error::Overflow),
        }
    } else {
        let Some((sender, _)) = clients.get(destination - 1) else {
            return Ok(());
        };
        for event in batch.events {
            if sender
                .send(Update::Event { id: device, event })
                .await
                .is_err()
            {
                clients.remove(destination - 1);
                held.remove_destination(destination);
                if *current == destination {
                    *current = 0;
                }
                return Ok(());
            }
        }
    }
    for key in keys {
        held.record(destination, device, key);
    }
    Ok(())
}

#[cfg(test)]
mod forwarding_tests {
    use super::*;
    use rkvm_input::key::Keyboard;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::oneshot;

    #[test]
    fn held_switch_chord_activates_only_once_until_fully_released() {
        let left_ctrl = Key::Key(Keyboard::LeftCtrl);
        let left_alt = Key::Key(Keyboard::LeftAlt);
        let switch_keys = HashSet::from([left_ctrl, left_alt]);
        let mut pressed = HashSet::new();
        let mut changed = false;
        let event = |key, down| KeyEvent { key, down };

        assert_eq!(
            update_switch_chord(
                event(left_ctrl, true),
                &switch_keys,
                &mut pressed,
                &mut changed,
            ),
            SwitchAction::ForwardCurrent
        );
        assert_eq!(
            update_switch_chord(
                event(left_alt, true),
                &switch_keys,
                &mut pressed,
                &mut changed,
            ),
            SwitchAction::Activate
        );
        for repeated in [left_ctrl, left_alt, left_ctrl] {
            assert_eq!(
                update_switch_chord(
                    event(repeated, true),
                    &switch_keys,
                    &mut pressed,
                    &mut changed,
                ),
                SwitchAction::RoutePrevious
            );
        }
        assert_eq!(
            update_switch_chord(
                event(left_alt, false),
                &switch_keys,
                &mut pressed,
                &mut changed,
            ),
            SwitchAction::RoutePrevious
        );
        assert!(changed);
        assert_eq!(
            update_switch_chord(
                event(left_ctrl, false),
                &switch_keys,
                &mut pressed,
                &mut changed,
            ),
            SwitchAction::RoutePrevious
        );
        assert!(!changed);

        assert_eq!(
            update_switch_chord(
                event(left_ctrl, true),
                &switch_keys,
                &mut pressed,
                &mut changed,
            ),
            SwitchAction::ForwardCurrent
        );
        assert_eq!(
            update_switch_chord(
                event(left_alt, true),
                &switch_keys,
                &mut pressed,
                &mut changed,
            ),
            SwitchAction::Activate
        );
    }

    #[test]
    fn sparse_client_ids_remain_reachable() {
        let (first_sender, _first_receiver) = mpsc::channel(1);
        let (second_sender, _second_receiver) = mpsc::channel(1);
        let mut clients = Slab::new();
        let first = clients.insert((first_sender, "127.0.0.1:1".parse().unwrap()));
        let second = clients.insert((second_sender, "127.0.0.1:2".parse().unwrap()));
        clients.remove(first);

        assert_eq!(next_destination(0, &clients), second + 1);
        assert_eq!(next_destination(second + 1, &clients), 0);
    }

    #[test]
    fn closed_client_is_pruned_before_switching() {
        let (dead_sender, dead_receiver) = mpsc::channel(1);
        let (live_sender, _live_receiver) = mpsc::channel(1);
        let mut clients = Slab::new();
        let dead = clients.insert((dead_sender, "127.0.0.1:1".parse().unwrap()));
        let live = clients.insert((live_sender, "127.0.0.1:2".parse().unwrap()));
        drop(dead_receiver);

        let destination = dead + 1;
        let mut held = HeldInputs::default();
        held.record(
            destination,
            7,
            KeyEvent {
                key: Key::Key(Keyboard::A),
                down: true,
            },
        );
        let mut current = destination;
        remove_closed_clients(&mut clients, &mut held, &mut current);

        assert!(!clients.contains(dead));
        assert!(clients.contains(live));
        assert_eq!(current, 0);
        assert!(held.release(destination).is_empty());
        assert_eq!(next_destination(current, &clients), live + 1);
    }

    #[tokio::test]
    async fn large_local_switch_cleanup_uses_one_queue_slot() {
        use rkvm_input::key::Keyboard::*;
        for propagate in [false, true] {
            let (sender, mut receiver) = mpsc::channel(32);
            // Leave just one free slot, even while the interceptor cannot drain it.
            for _ in 0..31 {
                sender
                    .try_send(InputBatch::new(vec![Event::Sync(SyncEvent::All)]))
                    .unwrap();
            }
            let mut devices = Slab::new();
            let id = devices.insert(Device {
                name: CString::new("recording test device").unwrap(),
                version: 0,
                vendor: 0,
                product: 0,
                rel: HashSet::new(),
                abs: HashMap::new(),
                keys: HashSet::new(),
                delay: None,
                period: None,
                sender,
            });
            let mut held = HeldInputs::default();
            // Ordinary held keys are released even with switch-key propagation off.
            let keys = [
                A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z, F1,
                F2, F3, F4, F5, F6, F7, F8, F9, F10,
            ];
            for key in keys {
                held.record(
                    0,
                    id,
                    KeyEvent {
                        key: Key::Key(key),
                        down: true,
                    },
                );
            }
            let mut clients = Slab::new();
            let mut current = 1;
            let leading = if propagate {
                vec![
                    Event::Key(KeyEvent {
                        key: Key::Key(LeftAlt),
                        down: true,
                    }),
                    Event::Sync(SyncEvent::All),
                ]
            } else {
                Vec::new()
            };
            for (device, events) in held.finish_switch(0, id, leading) {
                forward(
                    0,
                    device,
                    InputBatch::new(events),
                    &devices,
                    &mut clients,
                    &mut held,
                    &mut current,
                )
                .await
                .unwrap();
            }
            for _ in 0..31 {
                receiver.try_recv().unwrap();
            }
            let releases = receiver.try_recv().unwrap().events;
            assert_eq!(releases.len(), keys.len() + if propagate { 4 } else { 1 });
            let cleanup_start = if propagate {
                assert!(matches!(
                    releases[0],
                    Event::Key(KeyEvent {
                        key: Key::Key(LeftAlt),
                        down: true
                    })
                ));
                assert!(matches!(releases[1], Event::Sync(SyncEvent::All)));
                2
            } else {
                0
            };
            assert!(releases[cleanup_start..releases.len() - 1]
                .iter()
                .all(|event| matches!(event, Event::Key(KeyEvent { down: false, .. }))));
            assert!(matches!(releases.last(), Some(Event::Sync(SyncEvent::All))));
            assert!(held.release(0).is_empty());
        }
    }

    #[tokio::test]
    async fn switching_remote_peer_sends_release_without_disconnect() {
        let (sender, mut receiver) = mpsc::channel(8);
        let mut clients = Slab::new();
        clients.insert((sender, "127.0.0.1:1".parse().unwrap()));
        let devices = Slab::new();
        let mut held = HeldInputs::default();
        let mut current = 1;
        forward(
            1,
            7,
            InputBatch::new(vec![Event::Key(KeyEvent {
                key: Key::Key(Keyboard::A),
                down: true,
            })]),
            &devices,
            &mut clients,
            &mut held,
            &mut current,
        )
        .await
        .unwrap();
        current = 0;
        for (device, events) in held.release(1) {
            forward(
                1,
                device,
                InputBatch::new(events),
                &devices,
                &mut clients,
                &mut held,
                &mut current,
            )
            .await
            .unwrap();
        }
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Update::Event {
                event: Event::Key(KeyEvent { down: true, .. }),
                ..
            }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Update::Event {
                id: 7,
                event: Event::Key(KeyEvent {
                    key: Key::Key(Keyboard::A),
                    down: false
                })
            }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Update::Event {
                id: 7,
                event: Event::Sync(SyncEvent::All)
            }
        ));
        assert_eq!(clients.len(), 1);
        assert_eq!(current, 0);
    }

    #[tokio::test]
    async fn local_cleanup_waits_for_the_previous_device_to_apply_its_batch() {
        let mut first = InputBatch::new(Vec::new());
        let mut shortcut_done = None;
        first.after_shortcut(&mut shortcut_done);
        let mut second = InputBatch::new(Vec::new());
        second.after_shortcut(&mut shortcut_done);
        let (attempted, attempt) = oneshot::channel();
        let (output, mut applied) = mpsc::channel(2);
        let second_output = output.clone();
        let consumer = tokio::spawn(async move {
            attempted.send(()).unwrap();
            second.wait_for_previous().await;
            second_output.send("second device").await.unwrap();
            second.finish();
        });
        attempt.await.unwrap();
        assert!(matches!(applied.try_recv(), Err(TryRecvError::Empty)));
        // Model the first device's consumer being delayed while the second
        // device has already received its cleanup batch and tried to apply it.
        first.wait_for_previous().await;
        output.send("first device").await.unwrap();
        first.finish();
        assert_eq!(applied.recv().await, Some("first device"));
        assert_eq!(applied.recv().await, Some("second device"));
        consumer.await.unwrap();
    }

    #[tokio::test]
    async fn a_removed_preceding_device_does_not_block_remaining_cleanup() {
        let mut first = InputBatch::new(Vec::new());
        let mut shortcut_done = None;
        first.after_shortcut(&mut shortcut_done);
        let mut second = InputBatch::new(Vec::new());
        second.after_shortcut(&mut shortcut_done);
        drop(first);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            second.wait_for_previous(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn removing_a_middle_device_cannot_bypass_the_shortcut_batch() {
        let mut shortcut_done = None;
        let mut first = InputBatch::new(Vec::new());
        first.after_shortcut(&mut shortcut_done);
        let mut middle = InputBatch::new(Vec::new());
        middle.after_shortcut(&mut shortcut_done);
        let mut last = InputBatch::new(Vec::new());
        last.after_shortcut(&mut shortcut_done);
        drop(middle);
        let (attempted, attempt) = oneshot::channel();
        let (applied, mut application) = oneshot::channel();
        let consumer = tokio::spawn(async move {
            attempted.send(()).unwrap();
            last.wait_for_previous().await;
            applied.send(()).unwrap();
        });
        attempt.await.unwrap();
        assert!(matches!(
            application.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        first.finish();
        application.await.unwrap();
        consumer.await.unwrap();
    }
}

#[derive(Error, Debug)]
enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Incompatible client version (got {client}, expected {server})")]
    Version { server: Version, client: Version },
    #[error("Invalid password")]
    Auth,
    #[error(transparent)]
    Rand(#[from] rand::Error),
}

async fn client(
    mut init_updates: VecDeque<Update>,
    mut receiver: Receiver<Update>,
    stream: TcpStream,
    acceptor: TlsAcceptor,
    password: &str,
) -> Result<(), ClientError> {
    let stream = rkvm_net::timeout(rkvm_net::TLS_TIMEOUT, acceptor.accept(stream)).await?;
    tracing::info!("TLS connected");

    let mut stream = BufStream::with_capacity(1024, 1024, stream);

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        Version::CURRENT.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await?;

    let version = rkvm_net::timeout(rkvm_net::READ_TIMEOUT, Version::decode(&mut stream)).await?;
    if version != Version::CURRENT {
        return Err(ClientError::Version {
            server: Version::CURRENT,
            client: version,
        });
    }

    let challenge = AuthChallenge::generate().await?;

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        challenge.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await?;

    let response =
        rkvm_net::timeout(rkvm_net::READ_TIMEOUT, AuthResponse::decode(&mut stream)).await?;
    let status = match response.verify(&challenge, password) {
        true => AuthStatus::Passed,
        false => AuthStatus::Failed,
    };

    rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
        status.encode(&mut stream).await?;
        stream.flush().await?;

        Ok(())
    })
    .await?;

    if status == AuthStatus::Failed {
        return Err(ClientError::Auth);
    }

    tracing::info!("Authenticated successfully");

    let mut interval = time::interval(rkvm_net::PING_INTERVAL);

    loop {
        let recv = async {
            match init_updates.pop_front() {
                Some(update) => Some(update),
                None => receiver.recv().await,
            }
        };

        let update = tokio::select! {
            // Make sure pings have priority.
            // The client could time out otherwise.
            biased;

            _ = interval.tick() => Some(Update::Ping),
            recv = recv => recv,
        };

        let update = match update {
            Some(update) => update,
            None => break,
        };

        let start = Instant::now();
        rkvm_net::timeout(rkvm_net::WRITE_TIMEOUT, async {
            update.encode(&mut stream).await?;
            stream.flush().await?;

            Ok(())
        })
        .await?;
        let duration = start.elapsed();

        if let Update::Ping = update {
            // Keeping these as debug because it's not as frequent as other updates.
            tracing::debug!(duration = ?duration, "Sent ping");

            let start = Instant::now();
            rkvm_net::timeout(rkvm_net::READ_TIMEOUT, Pong::decode(&mut stream)).await?;
            let duration = start.elapsed();

            tracing::debug!(duration = ?duration, "Received pong");
        }

        tracing::trace!("Wrote an update");
    }

    Ok(())
}
