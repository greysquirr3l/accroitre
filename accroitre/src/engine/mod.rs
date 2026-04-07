//! Engine layer — concrete implementations of core operations.

pub mod copy;
pub mod dedup;
pub mod delta;
pub mod hash;
#[cfg(target_os = "linux")]
pub mod linux_io;
#[cfg(target_os = "macos")]
pub mod macos_io;
pub mod scan;
pub mod verify;
#[cfg(target_os = "windows")]
pub mod windows_io;
