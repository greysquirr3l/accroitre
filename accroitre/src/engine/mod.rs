//! Engine layer — concrete implementations of core operations.

pub mod copy;
pub mod dedup;
pub mod hash;
#[cfg(target_os = "linux")]
pub mod linux_io;
pub mod scan;
pub mod verify;
