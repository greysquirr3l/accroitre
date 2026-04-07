//! Port traits — hexagonal boundaries that adapters implement.
//!
//! All traits reference only domain types. No I/O framework types leak here.

mod cache;
mod copier;
mod filesystem;
mod hasher;
mod progress;
mod ssh;
mod verifier;

pub use cache::CachePort;
pub use copier::CopierPort;
pub use filesystem::FileSystemPort;
pub use hasher::HasherPort;
pub use progress::{NullProgress, ProgressPort, ProgressUpdate};
pub use ssh::SshPort;
pub use verifier::VerifierPort;
