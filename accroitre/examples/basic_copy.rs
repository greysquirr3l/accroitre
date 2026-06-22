//! End-to-end programmatic copy using the accroitre library.
//!
//! Demonstrates the canonical pipeline: scan → hash → dedup → copy → verify.
//!
//! Run with: `cargo run --example basic_copy -- /tmp/src /tmp/dst`

use std::path::PathBuf;
use std::process::ExitCode;

use accroitre::{
    CopyConfig, HashAlgorithm, NullProgress, ScanConfig, VerifyConfig, dedup_with_hashing,
    execute_copy_plan, scan_tree, verify_plan,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(source) = args.next() else {
        eprintln!("usage: basic_copy <source> <destination>");
        return ExitCode::from(2);
    };
    let Some(destination) = args.next() else {
        eprintln!("usage: basic_copy <source> <destination>");
        return ExitCode::from(2);
    };

    let source = PathBuf::from(source);
    let destination = PathBuf::from(destination);

    let progress = NullProgress;

    // Phase 1: scan the source tree.
    let scan = match scan_tree(&source, &ScanConfig::default(), &progress).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan failed: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "scanned {} entries ({} bytes)",
        scan.entries.len(),
        scan.entries.iter().map(|e| e.size).sum::<u64>()
    );

    // Phase 2: hash + dedup.
    let (plan, stats) = dedup_with_hashing(
        scan.entries,
        &source,
        &destination,
        HashAlgorithm::XxHash128,
    );
    println!(
        "dedup: {} unique, {} duplicates, {} bytes saved",
        stats.unique_files, stats.duplicate_files, stats.bytes_saved
    );

    // Phase 3: copy.
    let copy_result = match execute_copy_plan(&plan, &CopyConfig::default(), &progress) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("copy failed: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "copied {} files, linked {} ({} bytes)",
        copy_result.files_copied, copy_result.files_linked, copy_result.bytes_copied
    );
    if !copy_result.errors.is_empty() {
        eprintln!("{} non-fatal copy errors", copy_result.errors.len());
    }

    // Phase 4: verify.
    let verify_result = verify_plan(&plan, &VerifyConfig::default(), &progress);
    println!(
        "verified {} ({} failed)",
        verify_result.files_verified, verify_result.files_failed
    );
    if !verify_result.failures.is_empty() {
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}
