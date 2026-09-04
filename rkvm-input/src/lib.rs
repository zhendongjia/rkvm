pub mod abs;
pub mod event;
pub mod key;
pub mod rel;
pub mod sync;
pub mod writer;

#[cfg(target_os = "linux")]
pub mod interceptor;
#[cfg(target_os = "linux")]
pub mod monitor;

#[cfg(target_os = "linux")]
mod convert;
#[cfg(target_os = "linux")]
mod evdev;
#[cfg(target_os = "linux")]
mod glue;
#[cfg(target_os = "linux")]
mod registry;
#[cfg(target_os = "linux")]
mod uinput;
