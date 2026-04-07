//! `accro` — CLI for accroître, a high-speed file copier.

use clap::{Parser, Subcommand};

/// Accroître — high-speed file copier with deduplication and SSH streaming.
#[derive(Parser)]
#[command(name = "accro", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show version information including git SHA.
    Version,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Version) | None => {
            let version = env!("CARGO_PKG_VERSION");
            let sha = env!("ACCRO_GIT_SHA");
            println!("accro {version} ({sha})");
        }
    }
}
