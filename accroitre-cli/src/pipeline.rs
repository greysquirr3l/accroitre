//! Pipeline — wires CLI arguments to engine stages and runs the full copy flow.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use accroitre::adapters::log::JsonLog;
use accroitre::domain::HashAlgorithm;
use accroitre::engine::copy::{execute_copy_plan, CopyConfig, CopyResult};
use accroitre::engine::dedup::{build_dedup_plan, DedupStats};
use accroitre::engine::hash::HashConfig;
use accroitre::engine::scan::{scan_tree, ScanConfig};
use accroitre::engine::verify::{verify_plan, VerifyConfig, VerifyResult};
use accroitre::ports::ProgressPort;
use anyhow::{Context, Result, bail};
use glob::Pattern;

use crate::CopyArgs;
use crate::tui::TuiProgress;

// ── Exit codes ────────────────────────────────────────────────────────────────

/// Everything completed without errors.
pub const EXIT_SUCCESS: i32 = 0;
/// Some files errored but the run completed.
pub const EXIT_PARTIAL: i32 = 1;
/// Fatal error — the pipeline could not complete.
pub const EXIT_FAILURE: i32 = 2;

// ── Pipeline result ───────────────────────────────────────────────────────────

/// Outcome of a completed pipeline run.
pub struct PipelineResult {
    pub copy_result: CopyResult,
    pub dedup_stats: DedupStats,
    pub verify_result: Option<VerifyResult>,
    pub cancelled: bool,
}

impl PipelineResult {
    /// Determine the appropriate exit code.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.cancelled {
            return EXIT_PARTIAL;
        }
        let copy_errors = self.copy_result.errors.len();
        let verify_failures = self
            .verify_result
            .as_ref()
            .map_or(0, |v| v.failures.len());
        if copy_errors > 0 || verify_failures > 0 {
            EXIT_PARTIAL
        } else {
            EXIT_SUCCESS
        }
    }
}

// ── Pipeline execution ────────────────────────────────────────────────────────

/// Run the full local-to-local copy pipeline.
///
/// # Errors
///
/// Returns an error if the source path doesn't exist, patterns are invalid,
/// the log file can't be created, scanning fails, or copying fails fatally.
pub async fn run_local_pipeline(
    args: &CopyArgs,
    tui: &TuiProgress,
    cancelled: Arc<AtomicBool>,
) -> Result<PipelineResult> {
    let source = PathBuf::from(&args.source);
    let destination = PathBuf::from(&args.destination);

    if !source.exists() {
        bail!("source path does not exist: {}", source.display());
    }

    let exclude_patterns = parse_excludes(&args.exclude)?;
    let json_log = open_json_log(args, &source, &destination)?;

    #[allow(clippy::cast_possible_truncation)]
    let buffer_size = args.buffer as usize * 1024 * 1024;
    let algorithm = HashAlgorithm::XxHash128;

    // 1. Scan
    if cancelled.load(Ordering::Relaxed) {
        return Ok(cancelled_result());
    }
    let scan_result = run_scan(&source, exclude_patterns, tui).await?;

    if cancelled.load(Ordering::Relaxed) {
        return Ok(cancelled_result());
    }

    // 2. Hash + Dedup
    let (plan, dedup_stats) =
        run_dedup(args, scan_result, &source, &destination, algorithm, buffer_size, tui);

    if cancelled.load(Ordering::Relaxed) {
        return Ok(cancelled_result());
    }

    // 3. Dry-run gate
    if args.dry_run {
        return Ok(dry_run_result(&plan, dedup_stats));
    }

    // 4. Copy
    let copy_result = run_copy(&plan, buffer_size, tui)?;
    if let Some(ref log) = json_log {
        log_copy_results(log, &copy_result, &plan);
    }

    if cancelled.load(Ordering::Relaxed) {
        return Ok(PipelineResult { copy_result, dedup_stats, verify_result: None, cancelled: true });
    }

    // 5. Verify
    let verify_result = run_verify(args, &plan, algorithm, buffer_size, tui);
    if let Some(ref log) = json_log {
        log.finish();
    }

    Ok(PipelineResult {
        copy_result,
        dedup_stats,
        verify_result,
        cancelled: cancelled.load(Ordering::Relaxed),
    })
}

// ── Pipeline stage helpers ────────────────────────────────────────────────────

fn parse_excludes(patterns: &[String]) -> Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|s| Pattern::new(s).with_context(|| format!("invalid glob pattern: {s}")))
        .collect()
}

fn open_json_log(args: &CopyArgs, source: &Path, destination: &Path) -> Result<Option<JsonLog>> {
    if let Some(log_path) = &args.log_file {
        let log = JsonLog::new(log_path)
            .with_context(|| format!("cannot open log file: {}", log_path.display()))?;
        log.set_paths(source, destination);
        log.set_mode("local-to-local");
        Ok(Some(log))
    } else {
        Ok(None)
    }
}

async fn run_scan(
    source: &Path,
    exclude_patterns: Vec<Pattern>,
    tui: &TuiProgress,
) -> Result<accroitre::engine::scan::ScanResult> {
    let scan_config = ScanConfig {
        exclude_patterns,
        follow_symlinks: false,
    };
    let result = scan_tree(source, &scan_config, tui)
        .await
        .context("scan failed")?;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "scan" });
    Ok(result)
}

fn run_dedup(
    args: &CopyArgs,
    scan_result: accroitre::engine::scan::ScanResult,
    source: &Path,
    destination: &Path,
    algorithm: HashAlgorithm,
    buffer_size: usize,
    tui: &TuiProgress,
) -> (accroitre::domain::CopyPlan, DedupStats) {
    let hash_config = HashConfig {
        algorithm,
        buffer_size,
        thread_count: args.threads,
    };

    if args.no_dedup {
        let mut plan = accroitre::domain::CopyPlan::new(
            source.to_path_buf(),
            destination.to_path_buf(),
        );
        plan.entries = scan_result.entries;
        let file_count: u64 = plan.entries.len().try_into().unwrap_or(u64::MAX);
        let stats = DedupStats {
            total_files: file_count,
            unique_files: file_count,
            duplicate_files: 0,
            bytes_saved: 0,
        };
        (plan, stats)
    } else {
        let (plan, stats) = build_dedup_plan(
            scan_result.entries,
            source,
            destination,
            &hash_config,
            tui,
        );
        tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "hash" });
        (plan, stats)
    }
}

fn dry_run_result(plan: &accroitre::domain::CopyPlan, dedup_stats: DedupStats) -> PipelineResult {
    eprintln!(
        "Dry run: would copy {} files ({} bytes), link {} duplicates (saving {} bytes)",
        plan.entries.len(),
        plan.total_bytes(),
        plan.link_count(),
        dedup_stats.bytes_saved,
    );
    PipelineResult {
        copy_result: CopyResult {
            files_copied: 0,
            files_linked: 0,
            bytes_copied: 0,
            errors: Vec::new(),
        },
        dedup_stats,
        verify_result: None,
        cancelled: false,
    }
}

fn run_copy(
    plan: &accroitre::domain::CopyPlan,
    buffer_size: usize,
    tui: &TuiProgress,
) -> Result<CopyResult> {
    let copy_config = CopyConfig {
        buffer_size,
        small_file_threshold: 32 * 1024,
        try_clonefile: true,
    };
    let result = execute_copy_plan(plan, &copy_config, tui).context("copy failed")?;
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "copy" });
    Ok(result)
}

fn run_verify(
    args: &CopyArgs,
    plan: &accroitre::domain::CopyPlan,
    algorithm: HashAlgorithm,
    buffer_size: usize,
    tui: &TuiProgress,
) -> Option<VerifyResult> {
    if args.no_verify {
        return None;
    }
    let verify_config = VerifyConfig {
        algorithm,
        buffer_size,
        thread_count: args.threads,
    };
    let result = verify_plan(plan, &verify_config, tui);
    tui.update(&accroitre::ports::ProgressUpdate::PhaseComplete { phase: "verify" });
    Some(result)
}
// ── Helpers ───────────────────────────────────────────────────────────────────

fn cancelled_result() -> PipelineResult {
    PipelineResult {
        copy_result: CopyResult {
            files_copied: 0,
            files_linked: 0,
            bytes_copied: 0,
            errors: Vec::new(),
        },
        dedup_stats: DedupStats {
            total_files: 0,
            unique_files: 0,
            duplicate_files: 0,
            bytes_saved: 0,
        },
        verify_result: None,
        cancelled: true,
    }
}

fn log_copy_results(log: &JsonLog, result: &CopyResult, plan: &accroitre::domain::CopyPlan) {
    // Log individual file results from dedup groups.
    for group in &plan.dedup_groups {
        if let Some(canonical_entry) = plan.entries.get(group.canonical) {
            log.log_copied(&canonical_entry.path, canonical_entry.size, "copy");
            for &dup_idx in &group.duplicates {
                if let Some(dup_entry) = plan.entries.get(dup_idx) {
                    log.log_linked(&dup_entry.path, &canonical_entry.path, dup_entry.size);
                }
            }
        }
    }

    for err in &result.errors {
        log.log_error(Path::new(""), &err.to_string());
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_copy_args(src: &str, dst: &str) -> CopyArgs {
        CopyArgs {
            source: src.to_owned(),
            destination: dst.to_owned(),
            buffer: 1,
            threads: Some(1),
            dry_run: false,
            no_verify: false,
            no_dedup: false,
            no_cache: false,
            force: false,
            overwrite: false,
            exclude: Vec::new(),
            log_file: None,
            quiet: true,
            ssh_src_port: 22,
            ssh_src_key: None,
            ssh_src_password: None,
            ssh_dst_port: 22,
            ssh_dst_key: None,
            ssh_dst_password: None,
            compress: false,
        }
    }

    #[tokio::test]
    async fn end_to_end_local_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("hello.txt"), "hello world").unwrap();
        fs::write(src.join("data.bin"), vec![42u8; 1024]).unwrap();

        let args = make_copy_args(
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
        );
        let tui = TuiProgress::new(true);
        let cancelled = Arc::new(AtomicBool::new(false));

        let result = run_local_pipeline(&args, &tui, cancelled).await.unwrap();
        assert!(!result.cancelled);
        assert_eq!(result.exit_code(), EXIT_SUCCESS);
        assert!(result.copy_result.errors.is_empty());
        assert!(dst.join("hello.txt").exists());
        assert!(dst.join("data.bin").exists());
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn dry_run_does_not_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("file.txt"), "content").unwrap();

        let mut args = make_copy_args(
            src.to_str().unwrap(),
            dst.to_str().unwrap(),
        );
        args.dry_run = true;

        let tui = TuiProgress::new(true);
        let cancelled = Arc::new(AtomicBool::new(false));

        let result = run_local_pipeline(&args, &tui, cancelled).await.unwrap();
        assert_eq!(result.copy_result.files_copied, 0);
        assert!(!dst.exists());
    }

    #[tokio::test]
    async fn nonexistent_source_returns_error() {
        let args = make_copy_args("/tmp/does-not-exist-accro-test", "/tmp/dst");
        let tui = TuiProgress::new(true);
        let cancelled = Arc::new(AtomicBool::new(false));

        let result = run_local_pipeline(&args, &tui, cancelled).await;
        assert!(result.is_err());
    }

    #[test]
    fn exit_codes_correct() {
        let success = PipelineResult {
            copy_result: CopyResult {
                files_copied: 10,
                files_linked: 0,
                bytes_copied: 1000,
                errors: Vec::new(),
            },
            dedup_stats: DedupStats {
                total_files: 10,
                unique_files: 10,
                duplicate_files: 0,
                bytes_saved: 0,
            },
            verify_result: None,
            cancelled: false,
        };
        assert_eq!(success.exit_code(), EXIT_SUCCESS);

        let cancelled = PipelineResult {
            cancelled: true,
            ..success
        };
        assert_eq!(cancelled.exit_code(), EXIT_PARTIAL);
    }
}
