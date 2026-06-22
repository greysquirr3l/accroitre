//! Resumable copy: detects an interrupted previous run and skips completed files.
//!
//! Demonstrates the manifest API: a `.accroitre-manifest.json` is read from the
//! destination if present, and per-file completion status is honoured. When the
//! run finishes cleanly, the manifest is removed.
//!
//! Run with: `cargo run --example resume_copy -- /tmp/src /tmp/dst`

use std::path::PathBuf;
use std::process::ExitCode;

use accroitre::{
    CopyConfig, CopyManifest, HashAlgorithm, NullProgress, ScanConfig, VerifyConfig,
    dedup_with_hashing, execute_copy_plan_resumable, scan_tree, verify_plan,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(source) = args.next() else {
        eprintln!("usage: resume_copy <source> <destination>");
        return ExitCode::from(2);
    };
    let Some(destination) = args.next() else {
        eprintln!("usage: resume_copy <source> <destination>");
        return ExitCode::from(2);
    };

    let source = PathBuf::from(source);
    let destination = PathBuf::from(destination);
    let progress = NullProgress;

    // Load existing manifest if present (this is how resume works).
    let mut manifest = CopyManifest::load(&destination)
        .ok()
        .flatten()
        .unwrap_or_else(|| CopyManifest::new(&source, &destination, Some("xxhash128")));
    let already_completed = manifest.completed_count();
    if already_completed > 0 {
        println!("manifest found: {already_completed} files already completed (will skip)");
    } else {
        println!("no existing manifest — starting fresh");
    }

    // Scan, hash, dedup — same as basic_copy.
    let scan = match scan_tree(&source, &ScanConfig::default(), &progress).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scan failed: {e}");
            return ExitCode::from(1);
        }
    };
    let (plan, stats) = dedup_with_hashing(
        scan.entries,
        &source,
        &destination,
        HashAlgorithm::XxHash128,
    );
    println!(
        "plan: {} entries, {} duplicates",
        plan.entries.len(),
        stats.duplicate_files
    );

    // Copy with resume support — completed files in the manifest are skipped.
    let copy_result = match execute_copy_plan_resumable(
        &plan,
        &CopyConfig::default(),
        &progress,
        Some(&mut manifest),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("copy failed: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "copied {} files (this run), linked {}",
        copy_result.files_copied, copy_result.files_linked
    );

    // Save manifest mid-flight so an interruption can resume from here.
    if let Err(e) = manifest.save(&destination) {
        eprintln!("failed to save manifest: {e}");
    }

    // Verify.
    let verify_result = verify_plan(&plan, &VerifyConfig::default(), &progress);
    if !verify_result.failures.is_empty() {
        eprintln!(
            "verification failed ({} mismatches)",
            verify_result.failures.len()
        );
        return ExitCode::from(1);
    }

    // On clean finish, remove the manifest so the next plain copy doesn't
    // see stale "already completed" entries.
    if let Err(e) = CopyManifest::remove(&destination) {
        eprintln!("warning: could not remove manifest: {e}");
    }

    ExitCode::SUCCESS
}
