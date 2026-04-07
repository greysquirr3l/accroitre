//! Accroître — high-speed, cross-platform file copier with deduplication and SSH streaming.
//!
//! This crate provides the library core: domain types, port traits, engine logic,
//! and adapter implementations. The binary CLI is in the `accroitre-cli` crate.

pub mod domain;
pub mod ports;
