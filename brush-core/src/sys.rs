//! Platform abstraction facilities

#![allow(unused)]

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(unix)]
pub(crate) use unix as platform;

#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows as platform;

#[cfg(target_family = "wasm")]
pub(crate) mod wasm;
#[cfg(target_family = "wasm")]
pub(crate) use wasm as platform;

#[cfg(not(unix))]
pub(crate) mod stubs;

#[cfg(any(unix, windows))]
pub(crate) mod hostname;
#[cfg(any(unix, windows))]
pub mod tokio_process;

pub mod fs;

pub use platform::commands;
pub use platform::fd;
pub use platform::input;
pub(crate) use platform::network;
pub use platform::poll;
pub use platform::process;
pub use platform::resource;
pub use platform::signal;
pub use platform::terminal;
pub(crate) use platform::users;

pub use platform::PlatformError;

/// Returns the current system time, using JS Date on WASM where
/// `std::time::SystemTime::now()` is unsupported.
#[cfg(target_family = "wasm")]
pub(crate) fn system_time_now() -> std::time::SystemTime {
    let millis = js_sys::Date::now() as u64;
    std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis)
}

/// Returns the current system time via the standard library.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn system_time_now() -> std::time::SystemTime {
    std::time::SystemTime::now()
}
