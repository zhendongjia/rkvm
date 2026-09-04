use super::{native_key, repeatable, NativeKey};
use crate::event::Event;
use crate::key::{Key, KeyEvent};
use crate::rel::{RelAxis, RelEvent};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

// Missing EV_REP metadata uses conservative defaults. Explicit zero disables
// repeat, just as it does for a Linux input device.
#[derive(Clone, Copy)]
pub(super) struct RepeatConfig {
    delay: Duration,
    period: Duration,
}

impl Default for RepeatConfig {
    fn default() -> Self {
        Self {
            delay: Duration::from_millis(500),
            period: Duration::from_millis(33),
        }
    }
}

impl RepeatConfig {
    pub(super) fn delay(&mut self, value: Option<i32>) -> Result<(), Error> {
        if let Some(value) = value {
            self.delay = milliseconds(value)?;
        }
        Ok(())
    }

    pub(super) fn period(&mut self, value: Option<i32>) -> Result<(), Error> {
        if let Some(value) = value {
            self.period = milliseconds(value)?;
        }
        Ok(())
    }

    fn enabled(self) -> bool {
        !self.delay.is_zero() && !self.period.is_zero()
    }
}

fn milliseconds(value: i32) -> Result<Duration, Error> {
    let value = u64::try_from(value)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "Negative key repeat timing"))?;
    Ok(Duration::from_millis(value))
}

// Tests implement this sink without calling SendInput or touching the desktop.
pub(super) trait InputSink {
    fn key(&mut self, key: NativeKey, down: bool) -> Result<(), Error>;
    fn relative(&mut self, axis: RelAxis, value: i32) -> Result<(), Error>;
    fn lock_workstation(&mut self) -> bool {
        false
    }
}

pub(super) struct SharedInput<S> {
    sink: S,
    // Windows has one logical keyboard/mouse state across all remote devices.
    // Refcounts also cover different evdev names for the same Windows key.
    owners: HashMap<NativeKey, usize>,
}

impl<S: InputSink> SharedInput<S> {
    pub(super) fn new(sink: S) -> Self {
        Self {
            sink,
            owners: HashMap::new(),
        }
    }

    fn press(&mut self, key: NativeKey) -> Result<(), Error> {
        if !self.owners.contains_key(&key) {
            self.sink.key(key, true)?;
        }
        *self.owners.entry(key).or_default() += 1;
        Ok(())
    }

    fn release(&mut self, key: NativeKey) -> Result<(), Error> {
        if self.owners.get(&key) == Some(&1) {
            self.sink.key(key, false)?;
        }
        self.forget(key);
        Ok(())
    }

    fn forget(&mut self, key: NativeKey) {
        if let Some(count) = self.owners.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.owners.remove(&key);
            }
        }
    }

    fn try_lock_workstation(&mut self) -> Result<bool, Error> {
        let meta_keys = [
            native_key(Key::Key(crate::key::Keyboard::LeftMeta))
                .expect("left meta has a Windows mapping"),
            native_key(Key::Key(crate::key::Keyboard::RightMeta))
                .expect("right meta has a Windows mapping"),
        ];
        let held_meta = meta_keys
            .into_iter()
            .filter(|key| self.owners.contains_key(key))
            .collect::<Vec<_>>();
        if held_meta.is_empty() {
            return Ok(false);
        }

        // LockWorkStation does not need the Win key itself. Release injected
        // modifiers while the normal desktop is still active so the service's
        // desktop transition cannot strand a logical Win-down on the system.
        for key in &held_meta {
            self.sink.key(*key, false)?;
        }
        if self.sink.lock_workstation() {
            for key in held_meta {
                self.owners.remove(&key);
            }
            return Ok(true);
        }

        // If the API is unavailable (for example, the workstation is already
        // locked), restore the modifier state and fall back to raw forwarding.
        for key in held_meta {
            self.sink.key(key, true)?;
        }
        Ok(false)
    }
}

fn lock<S>(shared: &Mutex<SharedInput<S>>) -> MutexGuard<'_, SharedInput<S>> {
    // Cleanup must still run if a previous operation unwound with the lock held.
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Repeat {
    source: Key,
    native: NativeKey,
    deadline: Instant,
}

pub(super) struct Device<S: InputSink> {
    shared: Arc<Mutex<SharedInput<S>>>,
    held: Vec<(Key, NativeKey)>,
    suppressed: Vec<Key>,
    config: RepeatConfig,
    repeat: Option<Repeat>,
}

impl<S: InputSink> Device<S> {
    pub(super) fn new(shared: Arc<Mutex<SharedInput<S>>>, config: RepeatConfig) -> Self {
        Self {
            shared,
            held: Vec::new(),
            suppressed: Vec::new(),
            config,
            repeat: None,
        }
    }

    pub(super) fn write(&mut self, event: &Event, now: Instant) -> Result<(), Error> {
        match *event {
            Event::Key(KeyEvent { key, down }) => self.key(key, down, now),
            Event::Rel(RelEvent { axis, value }) => lock(&self.shared).sink.relative(axis, value),
            Event::Abs(_) | Event::Sync(_) => Ok(()),
        }
    }

    fn key(&mut self, key: Key, down: bool, now: Instant) -> Result<(), Error> {
        if let Some(index) = self
            .suppressed
            .iter()
            .position(|suppressed| *suppressed == key)
        {
            if !down {
                self.suppressed.remove(index);
            }
            return Ok(());
        }

        let native = match native_key(key) {
            Some(native) => native,
            None => return Ok(()),
        };
        let held = self.held.iter().position(|(source, _)| *source == key);
        if down
            && held.is_none()
            && key == Key::Key(crate::key::Keyboard::L)
            && lock(&self.shared).try_lock_workstation()?
        {
            self.suppressed.push(key);
            return Ok(());
        }
        match (down, held) {
            (true, None) => {
                lock(&self.shared).press(native)?;
                self.held.push((key, native));
                if repeatable(key) && self.config.enabled() {
                    self.repeat = Some(Repeat {
                        source: key,
                        native,
                        deadline: now + self.config.delay,
                    });
                }
            }
            (false, Some(index)) => {
                if self.repeat.as_ref().map(|repeat| repeat.source) == Some(key) {
                    self.repeat = None;
                }
                // Retain ownership on failure so Drop can retry the release.
                lock(&self.shared).release(native)?;
                self.held.remove(index);
            }
            // Duplicate downs must not reset the delay or acquire another
            // reference; unmatched ups must not release somebody else's key.
            _ => {}
        }
        Ok(())
    }

    pub(super) fn next_repeat(&self) -> Option<Instant> {
        self.repeat.as_ref().map(|repeat| repeat.deadline)
    }

    pub(super) fn repeat(&mut self, now: Instant) -> Result<(), Error> {
        if let Some(repeat) = self.repeat.as_mut() {
            if now >= repeat.deadline {
                lock(&self.shared).sink.key(repeat.native, true)?;
                // Skip missed ticks, rather than flooding the desktop after
                // the process was stalled or the computer resumed from sleep.
                repeat.deadline = now + self.config.period;
            }
        }
        Ok(())
    }
}

impl<S: InputSink> Drop for Device<S> {
    fn drop(&mut self) {
        // There is no detached repeat task: cleanup and all key-downs run
        // synchronously, so no repeat can race a final key-up.
        self.repeat = None;
        let mut shared = lock(&self.shared);
        for (_, native) in self.held.drain(..).rev() {
            if let Err(err) = shared.release(native) {
                // Continue releasing the other keys even when Windows rejects
                // one event. Do not leave stale ownership in the next session.
                shared.forget(native);
                tracing::warn!("Could not release remote input {:?}: {}", native, err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{Button, Keyboard};
    use std::future::{self, Future};
    use std::task::{Context, Poll, Waker};

    #[derive(Default)]
    struct RecordingSink {
        events: Vec<(NativeKey, bool)>,
        fail_next: bool,
        lock_requests: usize,
        lock_succeeds: bool,
    }

    impl InputSink for RecordingSink {
        fn key(&mut self, key: NativeKey, down: bool) -> Result<(), Error> {
            if std::mem::take(&mut self.fail_next) {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    "test injection failure",
                ));
            }
            self.events.push((key, down));
            Ok(())
        }

        fn relative(&mut self, _axis: RelAxis, _value: i32) -> Result<(), Error> {
            Ok(())
        }

        fn lock_workstation(&mut self) -> bool {
            self.lock_requests += 1;
            self.lock_succeeds
        }
    }

    fn setup() -> (
        Device<RecordingSink>,
        Arc<Mutex<SharedInput<RecordingSink>>>,
    ) {
        let shared = Arc::new(Mutex::new(SharedInput::new(RecordingSink::default())));
        let mut config = RepeatConfig::default();
        config.delay(Some(100)).unwrap();
        config.period(Some(20)).unwrap();
        (Device::new(shared.clone(), config), shared)
    }

    fn event(key: Key, down: bool) -> Event {
        Event::Key(KeyEvent { key, down })
    }

    fn keyboard(key: Keyboard, down: bool) -> Event {
        event(Key::Key(key), down)
    }

    fn native(key: Keyboard) -> NativeKey {
        native_key(Key::Key(key)).unwrap()
    }

    #[test]
    fn repeat_obeys_delay_period_and_stops_on_release() {
        let (mut device, shared) = setup();
        let now = Instant::now();
        device.write(&keyboard(Keyboard::A, true), now).unwrap();
        assert_eq!(device.next_repeat(), Some(now + Duration::from_millis(100)));
        device.repeat(now + Duration::from_millis(99)).unwrap();
        assert_eq!(lock(&shared).sink.events.len(), 1);
        device.repeat(now + Duration::from_millis(100)).unwrap();
        device.repeat(now + Duration::from_millis(119)).unwrap();
        assert_eq!(lock(&shared).sink.events.len(), 2);
        device.repeat(now + Duration::from_millis(120)).unwrap();
        device.write(&keyboard(Keyboard::A, false), now).unwrap();
        assert_eq!(device.next_repeat(), None);
        device.repeat(now + Duration::from_secs(1)).unwrap();
        drop(device);
        assert_eq!(
            lock(&shared).sink.events,
            vec![(native(Keyboard::A), true); 3]
                .into_iter()
                .chain([(native(Keyboard::A), false)])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn missed_ticks_do_not_burst_and_duplicate_down_does_not_delay_repeat() {
        let (mut device, shared) = setup();
        let now = Instant::now();
        device
            .write(&keyboard(Keyboard::Backspace, true), now)
            .unwrap();
        device
            .write(
                &keyboard(Keyboard::Backspace, true),
                now + Duration::from_millis(90),
            )
            .unwrap();
        assert_eq!(device.next_repeat(), Some(now + Duration::from_millis(100)));
        device.repeat(now + Duration::from_secs(10)).unwrap();
        device.repeat(now + Duration::from_secs(10)).unwrap();
        assert_eq!(lock(&shared).sink.events.len(), 2);
        assert_eq!(
            device.next_repeat(),
            Some(now + Duration::from_millis(10020))
        );
    }

    #[test]
    fn newest_repeatable_key_wins_and_modifiers_do_not_interrupt_it() {
        let (mut device, shared) = setup();
        let now = Instant::now();
        device.write(&keyboard(Keyboard::A, true), now).unwrap();
        device.write(&keyboard(Keyboard::B, true), now).unwrap();
        device
            .write(&keyboard(Keyboard::LeftShift, true), now)
            .unwrap();
        device.repeat(now + Duration::from_millis(100)).unwrap();
        assert_eq!(
            lock(&shared).sink.events.last(),
            Some(&(native(Keyboard::B), true))
        );
        device.write(&keyboard(Keyboard::A, false), now).unwrap();
        assert!(device.next_repeat().is_some());
        device.write(&keyboard(Keyboard::B, false), now).unwrap();
        assert!(device.next_repeat().is_none());
    }

    #[test]
    fn modifiers_locks_buttons_and_unsupported_keys_do_not_repeat() {
        let (mut device, _) = setup();
        for key in [
            Key::Key(Keyboard::LeftCtrl),
            Key::Key(Keyboard::RightAlt),
            Key::Key(Keyboard::LeftShift),
            Key::Key(Keyboard::RightMeta),
            Key::Key(Keyboard::CapsLock),
            Key::Key(Keyboard::NumLock),
            Key::Key(Keyboard::ScrollLock),
            Key::Key(Keyboard::Pause),
            Key::Button(Button::Left),
        ] {
            device.write(&event(key, true), Instant::now()).unwrap();
            assert!(device.next_repeat().is_none(), "{key:?}");
        }
    }

    #[test]
    fn zero_disables_repeat_and_negative_timings_are_rejected() {
        for zero_delay in [true, false] {
            let (mut device, _) = setup();
            if zero_delay {
                device.config.delay(Some(0)).unwrap();
            } else {
                device.config.period(Some(0)).unwrap();
            }
            device
                .write(&keyboard(Keyboard::A, true), Instant::now())
                .unwrap();
            assert!(device.next_repeat().is_none());
        }
        let mut config = RepeatConfig::default();
        assert_eq!(
            config.delay(Some(-1)).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert_eq!(
            config.period(Some(-1)).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        config.delay(None).unwrap();
        config.period(None).unwrap();
        assert_eq!(config.delay, Duration::from_millis(500));
        assert_eq!(config.period, Duration::from_millis(33));
    }

    #[test]
    fn dropping_device_releases_only_its_held_keys_and_buttons_in_reverse_order() {
        let (mut device, shared) = setup();
        let now = Instant::now();
        device
            .write(&keyboard(Keyboard::LeftCtrl, true), now)
            .unwrap();
        device.write(&keyboard(Keyboard::A, true), now).unwrap();
        device
            .write(&event(Key::Button(Button::Left), true), now)
            .unwrap();
        // A spurious up must never release a key owned by the local keyboard.
        device.write(&keyboard(Keyboard::B, false), now).unwrap();
        drop(device);
        let shared = lock(&shared);
        let mouse = native_key(Key::Button(Button::Left)).unwrap();
        assert_eq!(
            &shared.sink.events[3..],
            &[
                (mouse, false),
                (native(Keyboard::A), false),
                (native(Keyboard::LeftCtrl), false)
            ]
        );
        assert!(shared.owners.is_empty());
    }

    #[test]
    fn one_device_cannot_release_a_key_still_held_by_another() {
        let (mut first, shared) = setup();
        let mut second = Device::new(shared.clone(), RepeatConfig::default());
        first
            .write(&keyboard(Keyboard::LeftCtrl, true), Instant::now())
            .unwrap();
        second
            .write(&keyboard(Keyboard::LeftCtrl, true), Instant::now())
            .unwrap();
        drop(first);
        assert_eq!(
            lock(&shared).sink.events,
            vec![(native(Keyboard::LeftCtrl), true)]
        );
        drop(second);
        assert_eq!(
            lock(&shared).sink.events.last(),
            Some(&(native(Keyboard::LeftCtrl), false))
        );
        assert!(lock(&shared).owners.is_empty());
    }

    #[test]
    fn win_l_uses_system_lock_and_neutralizes_the_injected_meta_key() {
        let (mut device, shared) = setup();
        let now = Instant::now();
        lock(&shared).sink.lock_succeeds = true;

        device
            .write(&keyboard(Keyboard::LeftMeta, true), now)
            .unwrap();
        device.write(&keyboard(Keyboard::L, true), now).unwrap();
        device.write(&keyboard(Keyboard::L, true), now).unwrap();
        device.write(&keyboard(Keyboard::L, false), now).unwrap();
        device
            .write(&keyboard(Keyboard::LeftMeta, false), now)
            .unwrap();

        let shared = lock(&shared);
        assert_eq!(shared.sink.lock_requests, 1);
        assert_eq!(
            shared.sink.events,
            vec![
                (native(Keyboard::LeftMeta), true),
                (native(Keyboard::LeftMeta), false),
            ]
        );
        assert!(shared.owners.is_empty());
    }

    #[test]
    fn failed_system_lock_restores_meta_and_forwards_l_normally() {
        let (mut device, shared) = setup();
        let now = Instant::now();

        device
            .write(&keyboard(Keyboard::LeftMeta, true), now)
            .unwrap();
        device.write(&keyboard(Keyboard::L, true), now).unwrap();
        device.write(&keyboard(Keyboard::L, false), now).unwrap();
        device
            .write(&keyboard(Keyboard::LeftMeta, false), now)
            .unwrap();

        let shared = lock(&shared);
        assert_eq!(shared.sink.lock_requests, 1);
        assert_eq!(
            shared.sink.events,
            vec![
                (native(Keyboard::LeftMeta), true),
                (native(Keyboard::LeftMeta), false),
                (native(Keyboard::LeftMeta), true),
                (native(Keyboard::L), true),
                (native(Keyboard::L), false),
                (native(Keyboard::LeftMeta), false),
            ]
        );
        assert!(shared.owners.is_empty());
    }

    #[test]
    fn mouse_aliases_share_ownership_until_both_sources_are_released() {
        let (mut device, shared) = setup();
        for button in [Button::Side, Button::Back] {
            device
                .write(&event(Key::Button(button), true), Instant::now())
                .unwrap();
        }
        device
            .write(&event(Key::Button(Button::Side), false), Instant::now())
            .unwrap();
        assert_eq!(lock(&shared).sink.events.len(), 1);
        drop(device);
        assert_eq!(lock(&shared).sink.events.len(), 2);
        assert!(lock(&shared).owners.is_empty());
    }

    #[test]
    fn failed_press_is_not_owned_and_failed_release_is_retried_on_drop() {
        let (mut device, shared) = setup();
        lock(&shared).sink.fail_next = true;
        assert!(device
            .write(&keyboard(Keyboard::A, true), Instant::now())
            .is_err());
        assert!(device.held.is_empty());
        device
            .write(&keyboard(Keyboard::B, true), Instant::now())
            .unwrap();
        lock(&shared).sink.fail_next = true;
        assert!(device
            .write(&keyboard(Keyboard::B, false), Instant::now())
            .is_err());
        drop(device);
        assert_eq!(
            lock(&shared).sink.events,
            vec![(native(Keyboard::B), true), (native(Keyboard::B), false)]
        );
    }

    #[test]
    fn cleanup_continues_after_a_release_failure() {
        let (mut device, shared) = setup();
        device
            .write(&keyboard(Keyboard::LeftCtrl, true), Instant::now())
            .unwrap();
        device
            .write(&keyboard(Keyboard::A, true), Instant::now())
            .unwrap();
        lock(&shared).sink.fail_next = true;
        drop(device);
        assert_eq!(
            lock(&shared).sink.events.last(),
            Some(&(native(Keyboard::LeftCtrl), false))
        );
        assert!(lock(&shared).owners.is_empty());
    }

    #[test]
    fn device_removal_releases_held_inputs() {
        let (mut device, shared) = setup();
        device
            .write(&keyboard(Keyboard::LeftCtrl, true), Instant::now())
            .unwrap();
        let mut devices = HashMap::from([(7, device)]);
        devices.remove(&7);
        assert_eq!(
            lock(&shared).sink.events.last(),
            Some(&(native(Keyboard::LeftCtrl), false))
        );
    }

    #[test]
    fn cancelling_a_connection_future_releases_held_inputs() {
        let (mut device, shared) = setup();
        device
            .write(&keyboard(Keyboard::LeftCtrl, true), Instant::now())
            .unwrap();
        let mut connection = Box::pin(async move {
            let _device = device;
            future::pending::<()>().await;
        });
        let waker = Waker::noop();
        assert!(matches!(
            connection.as_mut().poll(&mut Context::from_waker(waker)),
            Poll::Pending
        ));
        drop(connection);
        assert_eq!(
            lock(&shared).sink.events.last(),
            Some(&(native(Keyboard::LeftCtrl), false))
        );
        assert!(lock(&shared).owners.is_empty());
    }

    #[test]
    fn a_failed_repeat_still_releases_the_original_press_on_drop() {
        let (mut device, shared) = setup();
        let now = Instant::now();
        device.write(&keyboard(Keyboard::A, true), now).unwrap();
        lock(&shared).sink.fail_next = true;
        assert!(device.repeat(now + Duration::from_secs(1)).is_err());
        drop(device);
        assert_eq!(
            lock(&shared).sink.events,
            vec![(native(Keyboard::A), true), (native(Keyboard::A), false)]
        );
        assert!(lock(&shared).owners.is_empty());
    }
}
