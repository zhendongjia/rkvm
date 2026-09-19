use crate::abs::{AbsAxis, AbsInfo};
use crate::event::Event;
use crate::key::{Button, Key, Keyboard as KeyboardKey};
use crate::rel::RelAxis;

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::io::{Error, ErrorKind};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use winapi::um::winuser::{self, INPUT, KEYBDINPUT, MOUSEINPUT};

mod state;
use state::{Device, InputSink, RepeatConfig, SharedInput};

const WHEEL_DELTA: i32 = 120;
const VIRTUAL_HID_PATH: &str = r"\\.\RkvmVirtualHid";
const KEYBOARD_REPORT_ID: u8 = 1;
const MOUSE_REPORT_ID: u8 = 2;
const CONSUMER_REPORT_ID: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum NativeKey {
    Keyboard {
        scan_code: u16,
        extended: bool,
        hid_usage: u8,
    },
    Consumer {
        virtual_key: u16,
        mask: u8,
    },
    Mouse(Button),
}

fn native_key(key: Key) -> Option<NativeKey> {
    match key {
        Key::Key(key) => {
            let consumer = match key {
                KeyboardKey::Mute => Some((winuser::VK_VOLUME_MUTE, 0x01)),
                KeyboardKey::VolumeDown => Some((winuser::VK_VOLUME_DOWN, 0x02)),
                KeyboardKey::VolumeUp => Some((winuser::VK_VOLUME_UP, 0x04)),
                _ => None,
            };
            if let Some((virtual_key, mask)) = consumer {
                return Some(NativeKey::Consumer {
                    virtual_key: virtual_key as u16,
                    mask,
                });
            }
            let (scan_code, extended) = scan_code(key)?;
            Some(NativeKey::Keyboard {
                scan_code,
                extended,
                hid_usage: hid_usage(key)?,
            })
        }
        Key::Button(button) => Some(NativeKey::Mouse(match button {
            Button::Left | Button::Right | Button::Middle => button,
            Button::Side | Button::Back => Button::Side,
            Button::Extra | Button::Forward => Button::Extra,
            _ => return None,
        })),
    }
}

fn repeatable(key: Key) -> bool {
    use KeyboardKey::*;
    match key {
        Key::Button(_) => false,
        Key::Key(VolumeDown | VolumeUp) => true,
        Key::Key(
            LeftCtrl | RightCtrl | LeftShift | RightShift | LeftAlt | RightAlt | LeftMeta
            | RightMeta | CapsLock | NumLock | ScrollLock | Appselect | ContextMenu | Menu | Mute,
        ) => false,
        Key::Key(key) => scan_code(key).is_some(),
    }
}

struct WindowsSink {
    virtual_hid: Option<VirtualHid>,
}

impl WindowsSink {
    fn new() -> Self {
        match VirtualHid::open() {
            Ok(virtual_hid) => {
                tracing::info!(path = VIRTUAL_HID_PATH, "Using the rkvm virtual HID driver");
                Self {
                    virtual_hid: Some(virtual_hid),
                }
            }
            Err(err) => {
                tracing::info!(
                    path = VIRTUAL_HID_PATH,
                    error = %err,
                    "Virtual HID driver unavailable; using SendInput"
                );
                Self { virtual_hid: None }
            }
        }
    }
}

impl InputSink for WindowsSink {
    fn key(&mut self, key: NativeKey, down: bool) -> Result<(), Error> {
        if let Some(virtual_hid) = self.virtual_hid.as_mut() {
            return virtual_hid.key(key, down);
        }
        let mut input = key_input(key, down);
        write_raw(std::slice::from_mut(&mut input))
    }

    fn relative(&mut self, axis: RelAxis, value: i32) -> Result<(), Error> {
        if let Some(virtual_hid) = self.virtual_hid.as_mut() {
            return virtual_hid.relative(axis, value);
        }
        if let Some(mut input) = relative_input(axis, value) {
            write_raw(std::slice::from_mut(&mut input))?;
        }
        Ok(())
    }

    fn lock_workstation(&mut self) -> bool {
        unsafe { winuser::LockWorkStation() != 0 }
    }
}

struct VirtualHid {
    file: File,
    modifiers: u8,
    keys: [u8; 6],
    buttons: u8,
    consumer_buttons: u8,
}

impl VirtualHid {
    fn open() -> Result<Self, Error> {
        Ok(Self {
            file: OpenOptions::new().write(true).open(VIRTUAL_HID_PATH)?,
            modifiers: 0,
            keys: [0; 6],
            buttons: 0,
            consumer_buttons: 0,
        })
    }

    fn submit(&mut self, report: &[u8]) -> Result<(), Error> {
        self.file.write_all(report)
    }

    fn key(&mut self, key: NativeKey, down: bool) -> Result<(), Error> {
        match key {
            NativeKey::Keyboard { hid_usage, .. } => self.keyboard(hid_usage, down),
            NativeKey::Consumer { mask, .. } => self.consumer(mask, down),
            NativeKey::Mouse(button) => self.mouse_button(button, down),
        }
    }

    fn consumer(&mut self, mask: u8, down: bool) -> Result<(), Error> {
        let released = self.consumer_buttons & !mask;
        // A repeated identical HID state is not another press. Pulse only this
        // control so holding volume repeats without toggling a held mute key.
        if down && self.consumer_buttons & mask != 0 {
            self.submit(&[CONSUMER_REPORT_ID, released])?;
            self.consumer_buttons = released;
        }
        let buttons = if down { released | mask } else { released };
        self.submit(&[CONSUMER_REPORT_ID, buttons])?;
        self.consumer_buttons = buttons;
        Ok(())
    }

    fn keyboard(&mut self, usage: u8, down: bool) -> Result<(), Error> {
        let (modifiers, keys, report) =
            keyboard_transition(self.modifiers, self.keys, usage, down)?;
        self.submit(&report)?;
        self.modifiers = modifiers;
        self.keys = keys;
        Ok(())
    }

    fn mouse_button(&mut self, button: Button, down: bool) -> Result<(), Error> {
        let Some(bit) = mouse_button_bit(button) else {
            return Ok(());
        };
        let mut buttons = self.buttons;
        if down {
            buttons |= 1 << bit;
        } else {
            buttons &= !(1 << bit);
        }

        self.submit(&mouse_report(buttons, None))?;
        self.buttons = buttons;
        Ok(())
    }

    fn relative(&mut self, axis: RelAxis, value: i32) -> Result<(), Error> {
        let index = match axis {
            RelAxis::X => 2,
            RelAxis::Y => 3,
            RelAxis::Wheel => 4,
            RelAxis::HWheel => 5,
            // The server also forwards the matching legacy wheel event, so
            // consuming both high-resolution variants would scroll twice.
            RelAxis::WheelHiRes | RelAxis::HWheelHiRes => return Ok(()),
            RelAxis::Z
            | RelAxis::Rx
            | RelAxis::Ry
            | RelAxis::Rz
            | RelAxis::Dial
            | RelAxis::Misc => return Ok(()),
        };

        let mut remaining = value;
        while remaining != 0 {
            let delta = remaining.clamp(-127, 127) as i8;
            let report = mouse_report(self.buttons, Some((index, delta)));
            self.submit(&report)?;
            remaining -= i32::from(delta);
        }
        Ok(())
    }
}

fn keyboard_transition(
    mut modifiers: u8,
    mut keys: [u8; 6],
    usage: u8,
    down: bool,
) -> Result<(u8, [u8; 6], [u8; 9]), Error> {
    if (0xe0..=0xe7).contains(&usage) {
        let mask = 1 << (usage - 0xe0);
        if down {
            modifiers |= mask;
        } else {
            modifiers &= !mask;
        }
    } else if down {
        if !keys.contains(&usage) {
            let slot = keys.iter_mut().find(|key| **key == 0).ok_or_else(|| {
                Error::new(
                    ErrorKind::WouldBlock,
                    "virtual HID boot keyboard supports at most six simultaneous keys",
                )
            })?;
            *slot = usage;
        }
    } else if let Some(slot) = keys.iter_mut().find(|key| **key == usage) {
        *slot = 0;
    }

    let report = [
        KEYBOARD_REPORT_ID,
        modifiers,
        0,
        keys[0],
        keys[1],
        keys[2],
        keys[3],
        keys[4],
        keys[5],
    ];
    Ok((modifiers, keys, report))
}

fn mouse_button_bit(button: Button) -> Option<u8> {
    Some(match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::Side | Button::Back => 3,
        Button::Extra | Button::Forward => 4,
        _ => return None,
    })
}

fn mouse_report(buttons: u8, delta: Option<(usize, i8)>) -> [u8; 6] {
    let mut report = [MOUSE_REPORT_ID, buttons, 0, 0, 0, 0];
    if let Some((index, value)) = delta {
        report[index] = value as u8;
    }
    report
}

static SHARED_INPUT: OnceLock<Arc<Mutex<SharedInput<WindowsSink>>>> = OnceLock::new();

/// A Windows input writer.
///
/// Windows does not expose a uinput equivalent, so events are injected into
/// the current interactive desktop with SendInput.
pub struct Writer(Device<WindowsSink>);

impl Writer {
    pub fn builder() -> Result<WriterBuilder, Error> {
        Ok(WriterBuilder {
            repeat: RepeatConfig::default(),
        })
    }

    pub async fn write(&mut self, event: &Event) -> Result<(), Error> {
        self.0.write(event, Instant::now())
    }

    /// The client services this deadline in its receive loop; no detached
    /// background task can inject another down after the writer is dropped.
    pub fn next_repeat(&self) -> Option<Instant> {
        self.0.next_repeat()
    }

    pub fn repeat(&mut self, now: Instant) -> Result<(), Error> {
        self.0.repeat(now)
    }
}

fn relative_input(axis: RelAxis, value: i32) -> Option<INPUT> {
    let (dx, dy, mouse_data, flags) = match axis {
        RelAxis::X => (value, 0, 0, winuser::MOUSEEVENTF_MOVE),
        RelAxis::Y => (0, value, 0, winuser::MOUSEEVENTF_MOVE),
        RelAxis::Wheel => (
            0,
            0,
            value.saturating_mul(WHEEL_DELTA) as u32,
            winuser::MOUSEEVENTF_WHEEL,
        ),
        RelAxis::HWheel => (
            0,
            0,
            value.saturating_mul(WHEEL_DELTA) as u32,
            winuser::MOUSEEVENTF_HWHEEL,
        ),
        // Linux may report high-resolution wheel events in addition to the
        // legacy Wheel/HWheel events. Ignoring these avoids scrolling twice.
        RelAxis::WheelHiRes | RelAxis::HWheelHiRes => return None,
        RelAxis::Z | RelAxis::Rx | RelAxis::Ry | RelAxis::Rz | RelAxis::Dial | RelAxis::Misc => {
            return None
        }
    };

    Some(mouse_input(dx, dy, mouse_data, flags))
}

fn key_input(key: NativeKey, down: bool) -> INPUT {
    match key {
        NativeKey::Consumer { virtual_key, .. } => {
            let mut input = unsafe { std::mem::zeroed::<INPUT>() };
            unsafe {
                *input.u.ki_mut() = KEYBDINPUT {
                    wVk: virtual_key,
                    wScan: 0,
                    dwFlags: if down { 0 } else { winuser::KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: 0,
                };
            }
            input.type_ = winuser::INPUT_KEYBOARD;
            input
        }
        NativeKey::Keyboard {
            scan_code,
            extended,
            ..
        } => {
            let mut input = unsafe { std::mem::zeroed::<INPUT>() };
            unsafe {
                *input.u.ki_mut() = KEYBDINPUT {
                    wVk: 0,
                    wScan: scan_code,
                    dwFlags: winuser::KEYEVENTF_SCANCODE
                        | if extended {
                            winuser::KEYEVENTF_EXTENDEDKEY
                        } else {
                            0
                        }
                        | if down { 0 } else { winuser::KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: 0,
                };
            }
            input.type_ = winuser::INPUT_KEYBOARD;
            input
        }
        NativeKey::Mouse(button) => {
            let (flags, mouse_data) = mouse_button(button, down).expect("normalized mouse button");
            mouse_input(0, 0, mouse_data, flags)
        }
    }
}

fn mouse_button(button: Button, down: bool) -> Option<(u32, u32)> {
    let flags = match button {
        Button::Left => {
            if down {
                winuser::MOUSEEVENTF_LEFTDOWN
            } else {
                winuser::MOUSEEVENTF_LEFTUP
            }
        }
        Button::Right => {
            if down {
                winuser::MOUSEEVENTF_RIGHTDOWN
            } else {
                winuser::MOUSEEVENTF_RIGHTUP
            }
        }
        Button::Middle => {
            if down {
                winuser::MOUSEEVENTF_MIDDLEDOWN
            } else {
                winuser::MOUSEEVENTF_MIDDLEUP
            }
        }
        Button::Side | Button::Back => {
            if down {
                winuser::MOUSEEVENTF_XDOWN
            } else {
                winuser::MOUSEEVENTF_XUP
            }
        }
        Button::Extra | Button::Forward => {
            if down {
                winuser::MOUSEEVENTF_XDOWN
            } else {
                winuser::MOUSEEVENTF_XUP
            }
        }
        _ => return None,
    };

    let mouse_data = match button {
        Button::Side | Button::Back => u32::from(winuser::XBUTTON1),
        Button::Extra | Button::Forward => u32::from(winuser::XBUTTON2),
        _ => 0,
    };

    Some((flags, mouse_data))
}

fn mouse_input(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> INPUT {
    let mut input = unsafe { std::mem::zeroed::<INPUT>() };
    unsafe {
        *input.u.mi_mut() = MOUSEINPUT {
            dx,
            dy,
            mouseData: mouse_data,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        };
    }
    input.type_ = winuser::INPUT_MOUSE;
    input
}

fn write_raw(inputs: &mut [INPUT]) -> Result<(), Error> {
    let written = unsafe {
        winuser::SendInput(
            inputs.len() as _,
            inputs.as_mut_ptr(),
            std::mem::size_of::<INPUT>() as _,
        )
    };

    if written as usize == inputs.len() {
        return Ok(());
    }

    if written == 0 {
        let error = Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
        // UIPI can reject SendInput without setting GetLastError. Never report
        // a misleading "operation completed successfully" for a failed write.
        return Err(Error::new(ErrorKind::PermissionDenied,
            "Windows rejected input (check the interactive desktop and application integrity level)"));
    }

    Err(Error::other(format!(
        "Windows accepted {written} of {} input events",
        inputs.len()
    )))
}

pub struct WriterBuilder {
    repeat: RepeatConfig,
}

impl WriterBuilder {
    pub fn name(self, _name: &CStr) -> Self {
        self
    }

    pub fn vendor(self, _value: u16) -> Self {
        self
    }

    pub fn product(self, _value: u16) -> Self {
        self
    }

    pub fn version(self, _value: u16) -> Self {
        self
    }

    pub fn rel<T: IntoIterator<Item = RelAxis>>(self, _items: T) -> Result<Self, Error> {
        Ok(self)
    }

    pub fn abs<T: IntoIterator<Item = (AbsAxis, AbsInfo)>>(self, _items: T) -> Result<Self, Error> {
        Ok(self)
    }

    pub fn key<T: IntoIterator<Item = Key>>(self, _items: T) -> Result<Self, Error> {
        Ok(self)
    }

    pub fn delay(mut self, value: Option<i32>) -> Result<Self, Error> {
        self.repeat.delay(value)?;
        Ok(self)
    }

    pub fn period(mut self, value: Option<i32>) -> Result<Self, Error> {
        self.repeat.period(value)?;
        Ok(self)
    }

    pub async fn build(self) -> Result<Writer, Error> {
        let shared =
            SHARED_INPUT.get_or_init(|| Arc::new(Mutex::new(SharedInput::new(WindowsSink::new()))));
        Ok(Writer(Device::new(shared.clone(), self.repeat)))
    }
}

fn scan_code(key: KeyboardKey) -> Option<(u16, bool)> {
    use KeyboardKey::*;

    let code = match key {
        A => (0x1e, false),
        B => (0x30, false),
        C => (0x2e, false),
        D => (0x20, false),
        E => (0x12, false),
        F => (0x21, false),
        G => (0x22, false),
        H => (0x23, false),
        I => (0x17, false),
        J => (0x24, false),
        K => (0x25, false),
        L => (0x26, false),
        M => (0x32, false),
        N => (0x31, false),
        O => (0x18, false),
        P => (0x19, false),
        Q => (0x10, false),
        R => (0x13, false),
        S => (0x1f, false),
        T => (0x14, false),
        U => (0x16, false),
        V => (0x2f, false),
        W => (0x11, false),
        X => (0x2d, false),
        Y => (0x15, false),
        Z => (0x2c, false),
        N0 => (0x0b, false),
        N1 => (0x02, false),
        N2 => (0x03, false),
        N3 => (0x04, false),
        N4 => (0x05, false),
        N5 => (0x06, false),
        N6 => (0x07, false),
        N7 => (0x08, false),
        N8 => (0x09, false),
        N9 => (0x0a, false),
        Grave => (0x29, false),
        Minus => (0x0c, false),
        Equal => (0x0d, false),
        Backslash => (0x2b, false),
        Backspace => (0x0e, false),
        Space => (0x39, false),
        Tab => (0x0f, false),
        CapsLock => (0x3a, false),
        LeftShift => (0x2a, false),
        LeftCtrl => (0x1d, false),
        LeftMeta => (0x5b, true),
        LeftAlt => (0x38, false),
        RightShift => (0x36, false),
        RightCtrl => (0x1d, true),
        RightMeta => (0x5c, true),
        RightAlt => (0x38, true),
        Appselect | ContextMenu | Menu => (0x5d, true),
        Enter => (0x1c, false),
        Esc => (0x01, false),
        F1 => (0x3b, false),
        F2 => (0x3c, false),
        F3 => (0x3d, false),
        F4 => (0x3e, false),
        F5 => (0x3f, false),
        F6 => (0x40, false),
        F7 => (0x41, false),
        F8 => (0x42, false),
        F9 => (0x43, false),
        F10 => (0x44, false),
        F11 => (0x57, false),
        F12 => (0x58, false),
        ScrollLock => (0x46, false),
        LeftBrace => (0x1a, false),
        Insert => (0x52, true),
        Home => (0x47, true),
        PageUp => (0x49, true),
        Delete => (0x53, true),
        End => (0x4f, true),
        PageDown => (0x51, true),
        Up => (0x48, true),
        Left => (0x4b, true),
        Down => (0x50, true),
        Right => (0x4d, true),
        NumLock => (0x45, false),
        KpSlash => (0x35, true),
        KpAsterisk => (0x37, false),
        KpMinus => (0x4a, false),
        KpPlus => (0x4e, false),
        KpEnter => (0x1c, true),
        KpDot => (0x53, false),
        Kp0 => (0x52, false),
        Kp1 => (0x4f, false),
        Kp2 => (0x50, false),
        Kp3 => (0x51, false),
        Kp4 => (0x4b, false),
        Kp5 => (0x4c, false),
        Kp6 => (0x4d, false),
        Kp7 => (0x47, false),
        Kp8 => (0x48, false),
        Kp9 => (0x49, false),
        RightBrace => (0x1b, false),
        Semicolon => (0x27, false),
        Apostrophe => (0x28, false),
        Comma => (0x33, false),
        Dot => (0x34, false),
        Slash => (0x35, false),
        _ => return None,
    };

    Some(code)
}

fn hid_usage(key: KeyboardKey) -> Option<u8> {
    use KeyboardKey::*;

    Some(match key {
        A => 0x04,
        B => 0x05,
        C => 0x06,
        D => 0x07,
        E => 0x08,
        F => 0x09,
        G => 0x0a,
        H => 0x0b,
        I => 0x0c,
        J => 0x0d,
        K => 0x0e,
        L => 0x0f,
        M => 0x10,
        N => 0x11,
        O => 0x12,
        P => 0x13,
        Q => 0x14,
        R => 0x15,
        S => 0x16,
        T => 0x17,
        U => 0x18,
        V => 0x19,
        W => 0x1a,
        X => 0x1b,
        Y => 0x1c,
        Z => 0x1d,
        N1 => 0x1e,
        N2 => 0x1f,
        N3 => 0x20,
        N4 => 0x21,
        N5 => 0x22,
        N6 => 0x23,
        N7 => 0x24,
        N8 => 0x25,
        N9 => 0x26,
        N0 => 0x27,
        Enter => 0x28,
        Esc => 0x29,
        Backspace => 0x2a,
        Tab => 0x2b,
        Space => 0x2c,
        Minus => 0x2d,
        Equal => 0x2e,
        LeftBrace => 0x2f,
        RightBrace => 0x30,
        Backslash => 0x31,
        Semicolon => 0x33,
        Apostrophe => 0x34,
        Grave => 0x35,
        Comma => 0x36,
        Dot => 0x37,
        Slash => 0x38,
        CapsLock => 0x39,
        F1 => 0x3a,
        F2 => 0x3b,
        F3 => 0x3c,
        F4 => 0x3d,
        F5 => 0x3e,
        F6 => 0x3f,
        F7 => 0x40,
        F8 => 0x41,
        F9 => 0x42,
        F10 => 0x43,
        F11 => 0x44,
        F12 => 0x45,
        ScrollLock => 0x47,
        Insert => 0x49,
        Home => 0x4a,
        PageUp => 0x4b,
        Delete => 0x4c,
        End => 0x4d,
        PageDown => 0x4e,
        Right => 0x4f,
        Left => 0x50,
        Down => 0x51,
        Up => 0x52,
        NumLock => 0x53,
        KpSlash => 0x54,
        KpAsterisk => 0x55,
        KpMinus => 0x56,
        KpPlus => 0x57,
        KpEnter => 0x58,
        Kp1 => 0x59,
        Kp2 => 0x5a,
        Kp3 => 0x5b,
        Kp4 => 0x5c,
        Kp5 => 0x5d,
        Kp6 => 0x5e,
        Kp7 => 0x5f,
        Kp8 => 0x60,
        Kp9 => 0x61,
        Kp0 => 0x62,
        KpDot => 0x63,
        Appselect | ContextMenu | Menu => 0x65,
        LeftCtrl => 0xe0,
        LeftShift => 0xe1,
        LeftAlt => 0xe2,
        LeftMeta => 0xe3,
        RightCtrl => 0xe4,
        RightShift => 0xe5,
        RightAlt => 0xe6,
        RightMeta => 0xe7,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_keys_produce_windows_virtual_key_press_and_release() {
        for (key, expected_vk) in [
            (KeyboardKey::Mute, 0xad),
            (KeyboardKey::VolumeDown, 0xae),
            (KeyboardKey::VolumeUp, 0xaf),
        ] {
            let native = native_key(Key::Key(key)).expect("volume key must not be dropped");
            for (down, expected_flags) in [(true, 0), (false, winuser::KEYEVENTF_KEYUP)] {
                let input = key_input(native, down);
                assert_eq!(input.type_, winuser::INPUT_KEYBOARD);
                let keyboard = unsafe { input.u.ki() };
                assert_eq!(keyboard.wVk, expected_vk);
                assert_eq!(keyboard.wScan, 0);
                assert_eq!(keyboard.dwFlags, expected_flags);
            }
        }
    }

    #[test]
    fn volume_hid_reports_preserve_other_keys_and_pulse_repeats() {
        use std::io::{Read, Seek, SeekFrom};
        let path = std::env::temp_dir().join(format!(
            "rkvm-volume-test-{}-{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let mut hid = VirtualHid {
            file,
            modifiers: 0,
            keys: [0; 6],
            buttons: 0,
            consumer_buttons: 0,
        };
        let up = native_key(Key::Key(KeyboardKey::VolumeUp)).expect("volume up mapping");
        let mute = native_key(Key::Key(KeyboardKey::Mute)).expect("mute mapping");
        hid.key(up, true).unwrap();
        hid.key(mute, true).unwrap();
        hid.key(up, true).unwrap();
        hid.key(up, false).unwrap();
        hid.key(mute, false).unwrap();
        hid.file.seek(SeekFrom::Start(0)).unwrap();
        let mut reports = Vec::new();
        hid.file.read_to_end(&mut reports).unwrap();
        drop(hid);
        std::fs::remove_file(path).unwrap();
        // Consumer report ID 3: mute bit 0, volume-down bit 1, volume-up bit 2.
        assert_eq!(reports, [3, 4, 3, 5, 3, 1, 3, 5, 3, 1, 3, 0]);
    }

    #[test]
    fn maps_standard_keyboard_scancodes() {
        assert_eq!(scan_code(KeyboardKey::A), Some((0x1e, false)));
        assert_eq!(scan_code(KeyboardKey::Enter), Some((0x1c, false)));
        assert_eq!(scan_code(KeyboardKey::Left), Some((0x4b, true)));
        assert_eq!(scan_code(KeyboardKey::RightMeta), Some((0x5c, true)));
        assert_eq!(hid_usage(KeyboardKey::A), Some(0x04));
        assert_eq!(hid_usage(KeyboardKey::Left), Some(0x50));
        assert_eq!(hid_usage(KeyboardKey::RightMeta), Some(0xe7));
    }

    #[test]
    fn maps_mouse_movement_and_wheel() {
        assert!(relative_input(RelAxis::X, 1).is_some());
        assert!(relative_input(RelAxis::Y, -1).is_some());
        assert!(relative_input(RelAxis::Wheel, 1).is_some());
        assert!(relative_input(RelAxis::HWheel, -1).is_some());
        assert!(relative_input(RelAxis::WheelHiRes, 120).is_none());
    }

    #[test]
    fn ignores_unrepresentable_keyboard_events() {
        assert!(scan_code(KeyboardKey::Pause).is_none());
        assert!(native_key(Key::Key(KeyboardKey::Pause)).is_none());
    }

    #[test]
    fn release_preserves_extended_scan_code_and_mouse_button_identity() {
        let key = native_key(Key::Key(KeyboardKey::RightCtrl)).unwrap();
        let release = key_input(key, false);
        let keyboard = unsafe { release.u.ki() };
        assert_eq!(keyboard.wScan, 0x1d);
        assert_eq!(
            keyboard.dwFlags,
            winuser::KEYEVENTF_SCANCODE | winuser::KEYEVENTF_EXTENDEDKEY | winuser::KEYEVENTF_KEYUP
        );
        assert_ne!(key, native_key(Key::Key(KeyboardKey::LeftCtrl)).unwrap());
        let release = key_input(native_key(Key::Button(Button::Back)).unwrap(), false);
        let mouse = unsafe { release.u.mi() };
        assert_eq!(mouse.dwFlags, winuser::MOUSEEVENTF_XUP);
        assert_eq!(mouse.mouseData, u32::from(winuser::XBUTTON1));
    }

    #[test]
    fn virtual_hid_keyboard_reports_full_state_and_enforces_six_key_rollover() {
        let (modifiers, keys, report) =
            keyboard_transition(0, [0; 6], hid_usage(KeyboardKey::LeftCtrl).unwrap(), true)
                .unwrap();
        assert_eq!(modifiers, 0x01);
        assert_eq!(report, [KEYBOARD_REPORT_ID, 0x01, 0, 0, 0, 0, 0, 0, 0]);

        let (modifiers, mut keys, report) =
            keyboard_transition(modifiers, keys, hid_usage(KeyboardKey::A).unwrap(), true).unwrap();
        assert_eq!(report, [KEYBOARD_REPORT_ID, 0x01, 0, 0x04, 0, 0, 0, 0, 0]);
        for usage in 0x05..=0x09 {
            (_, keys, _) = keyboard_transition(modifiers, keys, usage, true).unwrap();
        }
        assert_eq!(
            keyboard_transition(modifiers, keys, 0x0a, true)
                .unwrap_err()
                .kind(),
            ErrorKind::WouldBlock
        );

        let (_, keys, report) = keyboard_transition(modifiers, keys, 0x04, false).unwrap();
        assert!(!keys.contains(&0x04));
        assert_eq!(report[3], 0);
    }

    #[test]
    fn virtual_hid_mouse_reports_buttons_and_signed_deltas() {
        assert_eq!(mouse_button_bit(Button::Left), Some(0));
        assert_eq!(mouse_button_bit(Button::Forward), Some(4));
        assert_eq!(
            mouse_report(0x11, None),
            [MOUSE_REPORT_ID, 0x11, 0, 0, 0, 0]
        );
        assert_eq!(
            mouse_report(0x01, Some((3, -127))),
            [MOUSE_REPORT_ID, 0x01, 0, 129, 0, 0]
        );
    }
}
