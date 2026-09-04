//! Keys actually forwarded to each destination, not physical key state.
use rkvm_input::event::Event;
use rkvm_input::key::{Key, KeyEvent};
use rkvm_input::sync::SyncEvent;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct HeldInputs {
    destinations: HashMap<usize, HashMap<usize, Vec<Key>>>,
}

impl HeldInputs {
    pub(crate) fn record(&mut self, destination: usize, device: usize, event: KeyEvent) {
        let devices = self.destinations.entry(destination).or_default();
        let keys = devices.entry(device).or_default();
        if event.down {
            if !keys.contains(&event.key) {
                keys.push(event.key);
            }
        } else {
            keys.retain(|key| *key != event.key);
        }
        if keys.is_empty() {
            devices.remove(&device);
        }
        if devices.is_empty() {
            self.destinations.remove(&destination);
        }
    }

    pub(crate) fn release(&mut self, destination: usize) -> Vec<(usize, Vec<Event>)> {
        let mut batches = Vec::new();
        for (device, keys) in self.destinations.remove(&destination).unwrap_or_default() {
            let mut events = Vec::with_capacity(keys.len() + 1);
            // Finish the old destination's held inputs before it stops receiving
            // physical events. Existing protocol messages suffice for all clients.
            for key in keys.into_iter().rev() {
                events.push(Event::Key(KeyEvent { key, down: false }));
            }
            events.push(Event::Sync(SyncEvent::All));
            batches.push((device, events));
        }
        batches
    }

    pub(crate) fn remove_destination(&mut self, destination: usize) {
        self.destinations.remove(&destination);
    }

    pub(crate) fn finish_switch(
        &mut self,
        destination: usize,
        device: usize,
        mut leading: Vec<Event>,
    ) -> Vec<(usize, Vec<Event>)> {
        // Plan the final shortcut events and their releases together. No await
        // occurs between staging these keys and draining the destination; the
        // forwarding path records only batches it actually delivers.
        for event in &leading {
            if let Event::Key(key) = event {
                self.record(destination, device, *key);
            }
        }
        let mut batches = self.release(destination);
        if let Some(index) = batches.iter().position(|(id, _)| *id == device) {
            let (_, mut releases) = batches.swap_remove(index);
            leading.append(&mut releases);
        }
        if !leading.is_empty() {
            // The final shortcut down/Sync must precede releases from other
            // devices too (Ctrl and Alt may originate on different keyboards).
            batches.insert(0, (device, leading));
        }
        batches
    }

    pub(crate) fn remove_device(&mut self, device: usize) {
        self.destinations.retain(|_, devices| {
            devices.remove(&device);
            !devices.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkvm_input::key::{Button, Keyboard};

    #[test]
    fn switching_away_releases_a_held_letter_and_drag_before_sync() {
        let mut held = HeldInputs::default();
        let key = Key::Key(Keyboard::A);
        let button = Key::Button(Button::Left);
        held.record(1, 7, KeyEvent { key, down: true });
        held.record(
            1,
            7,
            KeyEvent {
                key: button,
                down: true,
            },
        );
        let releases = held.release(1);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].0, 7);
        let releases = &releases[0].1;
        assert_eq!(releases.len(), 3);
        assert!(
            matches!(&releases[0], Event::Key(KeyEvent { key, down: false }) if *key == button)
        );
        assert!(
            matches!(&releases[1], Event::Key(KeyEvent { key: released, down: false }) if *released == key)
        );
        assert!(matches!(&releases[2], Event::Sync(SyncEvent::All)));
        assert!(held.release(1).is_empty());
    }

    #[test]
    fn duplicates_and_other_destinations_are_not_released() {
        let mut held = HeldInputs::default();
        let key = Key::Key(Keyboard::LeftCtrl);
        for _ in 0..2 {
            held.record(0, 7, KeyEvent { key, down: true });
        }
        held.record(1, 7, KeyEvent { key, down: true });
        assert_eq!(held.release(0)[0].1.len(), 2);
        assert_eq!(held.release(1)[0].1.len(), 2);
    }

    #[test]
    fn released_or_removed_keys_are_not_released_again() {
        let mut held = HeldInputs::default();
        let key = Key::Key(Keyboard::A);
        held.record(1, 7, KeyEvent { key, down: true });
        held.record(1, 7, KeyEvent { key, down: false });
        assert!(held.release(1).is_empty());
        held.record(1, 8, KeyEvent { key, down: true });
        held.remove_device(8);
        assert!(held.release(1).is_empty());
        held.record(1, 7, KeyEvent { key, down: true });
        held.remove_destination(1);
        assert!(held.release(1).is_empty());
    }

    #[test]
    fn final_shortcut_is_batched_before_other_device_releases() {
        let mut held = HeldInputs::default();
        held.record(
            0,
            3,
            KeyEvent {
                key: Key::Key(Keyboard::LeftCtrl),
                down: true,
            },
        );
        let batches = held.finish_switch(
            0,
            7,
            vec![
                Event::Key(KeyEvent {
                    key: Key::Key(Keyboard::LeftAlt),
                    down: true,
                }),
                Event::Sync(SyncEvent::All),
            ],
        );
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].0, 7);
        assert_eq!(batches[0].1.len(), 4);
        assert!(matches!(
            batches[0].1[0],
            Event::Key(KeyEvent { down: true, .. })
        ));
        assert!(matches!(
            batches[0].1[2],
            Event::Key(KeyEvent { down: false, .. })
        ));
        assert_eq!(batches[1].0, 3);
        assert!(held.release(0).is_empty());
    }
}
