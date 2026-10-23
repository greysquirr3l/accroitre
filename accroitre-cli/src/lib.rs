//! accroître command line interface.
//!
//! The crate is split into a library and a thin binary so that the copy
//! pipelines can be exercised by integration tests.

pub mod cli;
pub mod hash_cmd;
pub mod pipeline;
pub mod remote_pipeline;
pub mod tui;
pub mod update;

pub use cli::{Cli, Commands, CopyArgs, HashArgs, Location, UpdateArgs, parse_location};
