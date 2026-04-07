//! Rich TUI with braille spinners, progress bars, and final summary.
//!
//! Implements [`ProgressPort`] to bridge engine events to the terminal display.

use std::sync::Mutex;
use std::time::Instant;

use accroitre::ports::{ProgressPort, ProgressUpdate};
use console::Term;
use indicatif::{
    HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};

// ── Braille spinner frames ────────────────────────────────────────────────────

const BRAILLE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ── Current phase ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Scanning,
    Hashing,
    Copying,
    Verifying,
}

// ── Summary statistics ────────────────────────────────────────────────────────

/// Final run summary.
#[derive(Debug, Default)]
pub struct RunSummary {
    pub files_total: u64,
    pub files_copied: u64,
    pub files_linked: u64,
    pub files_skipped: u64,
    pub files_errored: u64,
    pub bytes_total: u64,
    pub bytes_copied: u64,
    pub dedup_bytes_saved: u64,
}

// ── Internal state ────────────────────────────────────────────────────────────

struct TuiState {
    phase: Phase,
    scanner_bar: Option<ProgressBar>,
    overall_bar: Option<ProgressBar>,
    started_at: Instant,
    summary: RunSummary,
}

// ── Public TUI adapter ────────────────────────────────────────────────────────

/// A rich terminal UI that implements [`ProgressPort`].
pub struct TuiProgress {
    mp: MultiProgress,
    state: Mutex<TuiState>,
    quiet: bool,
}

impl TuiProgress {
    /// Create a new TUI progress display.
    ///
    /// When `quiet` is true the [`MultiProgress`] target is set to hidden so
    /// nothing is drawn; only the final summary is printed to stderr.
    #[must_use]
    pub fn new(quiet: bool) -> Self {
        let mp = MultiProgress::new();
        if quiet {
            mp.set_draw_target(ProgressDrawTarget::hidden());
        }
        Self {
            mp,
            state: Mutex::new(TuiState {
                phase: Phase::Idle,
                scanner_bar: None,
                overall_bar: None,
                started_at: Instant::now(),
                summary: RunSummary::default(),
            }),
            quiet,
        }
    }

    /// Whether in quiet mode (no TUI output, only final summary).
    pub fn is_quiet(&self) -> bool {
        self.quiet
    }
    ///
    /// The caller must hold no other lock on `self.state`.
    pub fn summary(&self) -> RunSummary {
        if let Ok(state) = self.state.lock() {
            RunSummary {
                files_total: state.summary.files_total,
                files_copied: state.summary.files_copied,
                files_linked: state.summary.files_linked,
                files_skipped: state.summary.files_skipped,
                files_errored: state.summary.files_errored,
                bytes_total: state.summary.bytes_total,
                bytes_copied: state.summary.bytes_copied,
                dedup_bytes_saved: state.summary.dedup_bytes_saved,
            }
        } else {
            RunSummary::default()
        }
    }

    // ── Phase transitions ─────────────────────────────────────────────────

    fn enter_scanning(&self, state: &mut TuiState) {
        let spinner = self.mp.add(ProgressBar::new_spinner());
        spinner.set_style(
            ProgressStyle::with_template("{spinner} {prefix} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_strings(BRAILLE_FRAMES),
        );
        spinner.set_prefix("Scanning");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));
        state.scanner_bar = Some(spinner);
        state.phase = Phase::Scanning;
    }

    fn enter_hash_or_copy_or_verify(
        &self,
        state: &mut TuiState,
        phase: Phase,
        label: &str,
        total_bytes: u64,
    ) {
        // Finish previous bar.
        if let Some(bar) = state.scanner_bar.take() {
            bar.finish_and_clear();
        }
        if let Some(bar) = state.overall_bar.take() {
            bar.finish_and_clear();
        }

        let bar = self.mp.add(ProgressBar::new(total_bytes));
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner} {prefix} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .tick_strings(BRAILLE_FRAMES)
            .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        bar.set_prefix(label.to_owned());
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        state.overall_bar = Some(bar);
        state.phase = phase;
    }

    fn enter_verify(
        &self,
        state: &mut TuiState,
        total_files: u64,
    ) {
        if let Some(bar) = state.scanner_bar.take() {
            bar.finish_and_clear();
        }
        if let Some(bar) = state.overall_bar.take() {
            bar.finish_and_clear();
        }

        let bar = self.mp.add(ProgressBar::new(total_files));
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner} {prefix} [{bar:30.cyan/blue}] {pos}/{len} files ({per_sec}, ETA {eta})",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .tick_strings(BRAILLE_FRAMES)
            .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        bar.set_prefix("Verifying".to_owned());
        bar.enable_steady_tick(std::time::Duration::from_millis(100));
        state.overall_bar = Some(bar);
        state.phase = Phase::Verifying;
    }
}

/// Format the final summary as a multi-line string.
#[must_use]
pub fn format_summary(summary: &RunSummary, elapsed: std::time::Duration) -> String {
    let speed = if elapsed.as_secs_f64() > 0.0 {
        #[allow(clippy::cast_precision_loss)]
        let bps = summary.bytes_copied as f64 / elapsed.as_secs_f64();
        // bps is guaranteed non-negative; truncation toward zero is acceptable.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bps_u64 = bps as u64;
        format!("{}/s", HumanBytes(bps_u64))
    } else {
        "N/A".to_owned()
    };

    format!(
        "\n{title}\n  Files:   {total} total, {copied} copied, {linked} linked, {skipped} skipped, {errored} errored\n  Bytes:   {bytes_total} total, {bytes_copied} copied\n  Dedup:   {dedup_saved} saved by hard-linking\n  Speed:   {speed}\n  Elapsed: {elapsed}\n",
        title = "── Summary ──",
        total = summary.files_total,
        copied = summary.files_copied,
        linked = summary.files_linked,
        skipped = summary.files_skipped,
        errored = summary.files_errored,
        bytes_total = HumanBytes(summary.bytes_total),
        bytes_copied = HumanBytes(summary.bytes_copied),
        dedup_saved = HumanBytes(summary.dedup_bytes_saved),
        elapsed = HumanDuration(elapsed),
    )
}

// ── ProgressPort implementation ───────────────────────────────────────────────

impl ProgressPort for TuiProgress {
    fn update(&self, event: &ProgressUpdate<'_>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        match event {
            ProgressUpdate::ScanProgress {
                files_found,
                current_dir,
            } => {
                if state.phase != Phase::Scanning {
                    self.enter_scanning(&mut state);
                }
                if let Some(bar) = &state.scanner_bar {
                    bar.set_message(format!(
                        "{files_found} files — {}",
                        current_dir.display()
                    ));
                }
                state.summary.files_total = *files_found;
            }

            ProgressUpdate::HashProgress {
                bytes_hashed,
                bytes_total,
                ..
            } => {
                if state.phase != Phase::Hashing {
                    self.enter_hash_or_copy_or_verify(
                        &mut state,
                        Phase::Hashing,
                        "Hashing",
                        *bytes_total,
                    );
                }
                if let Some(bar) = &state.overall_bar {
                    bar.set_position(*bytes_hashed);
                }
            }

            ProgressUpdate::CopyProgress {
                files_copied,
                bytes_copied,
                bytes_total,
                ..
            } => {
                if state.phase != Phase::Copying {
                    self.enter_hash_or_copy_or_verify(
                        &mut state,
                        Phase::Copying,
                        "Copying",
                        *bytes_total,
                    );
                }
                if let Some(bar) = &state.overall_bar {
                    bar.set_position(*bytes_copied);
                }
                state.summary.bytes_copied = *bytes_copied;
                state.summary.files_copied = *files_copied;
            }

            ProgressUpdate::VerifyProgress {
                files_verified,
                files_total,
            } => {
                if state.phase != Phase::Verifying {
                    self.enter_verify(&mut state, *files_total);
                }
                if let Some(bar) = &state.overall_bar {
                    bar.set_position(*files_verified);
                }
            }

            ProgressUpdate::PhaseComplete { phase } => {
                if let Some(bar) = state.scanner_bar.take() {
                    bar.finish_and_clear();
                }
                if let Some(bar) = state.overall_bar.take() {
                    bar.finish_with_message(format!("{phase} complete ✓"));
                }
                state.phase = Phase::Idle;
            }

            ProgressUpdate::FileError { path, message } => {
                state.summary.files_errored += 1;
                if let Some(bar) = &state.overall_bar {
                    bar.suspend(|| {
                        eprintln!("ERROR: {}: {message}", path.display());
                    });
                } else {
                    eprintln!("ERROR: {}: {message}", path.display());
                }
            }
        }
    }

    fn finish(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        // Clear remaining bars.
        if let Some(bar) = state.scanner_bar.take() {
            bar.finish_and_clear();
        }
        if let Some(bar) = state.overall_bar.take() {
            bar.finish_and_clear();
        }

        let elapsed = state.started_at.elapsed();
        let text = format_summary(&state.summary, elapsed);

        // Even in quiet mode, print the summary.
        let term = Term::stderr();
        let _ = term.write_line(&text);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn quiet_mode_receives_events_without_panic() {
        let tui = TuiProgress::new(true);
        tui.update(&ProgressUpdate::ScanProgress {
            files_found: 42,
            current_dir: Path::new("/tmp/test"),
        });
        tui.update(&ProgressUpdate::HashProgress {
            files_hashed: 10,
            files_total: 42,
            bytes_hashed: 1024,
            bytes_total: 4096,
        });
        tui.update(&ProgressUpdate::CopyProgress {
            files_copied: 5,
            files_total: 42,
            bytes_copied: 2048,
            bytes_total: 4096,
        });
        tui.update(&ProgressUpdate::VerifyProgress {
            files_verified: 5,
            files_total: 42,
        });
        tui.update(&ProgressUpdate::PhaseComplete { phase: "copy" });
        tui.update(&ProgressUpdate::FileError {
            path: Path::new("/tmp/bad"),
            message: "permission denied",
        });
        tui.finish();
        let s = tui.summary();
        assert_eq!(s.files_total, 42);
        assert_eq!(s.files_copied, 5);
        assert_eq!(s.files_errored, 1);
    }

    #[test]
    fn format_summary_includes_all_fields() {
        let summary = RunSummary {
            files_total: 100,
            files_copied: 80,
            files_linked: 15,
            files_skipped: 3,
            files_errored: 2,
            bytes_total: 10_000_000,
            bytes_copied: 8_000_000,
            dedup_bytes_saved: 2_000_000,
        };
        let text = format_summary(&summary, std::time::Duration::from_secs(10));
        assert!(text.contains("Summary"));
        assert!(text.contains("100 total"));
        assert!(text.contains("80 copied"));
        assert!(text.contains("15 linked"));
        assert!(text.contains("3 skipped"));
        assert!(text.contains("2 errored"));
    }

    #[test]
    fn format_summary_zero_elapsed() {
        let summary = RunSummary::default();
        let text = format_summary(&summary, std::time::Duration::ZERO);
        assert!(text.contains("N/A"));
    }

    #[test]
    fn phase_transitions_update_state() {
        let tui = TuiProgress::new(true);

        // Scanning
        tui.update(&ProgressUpdate::ScanProgress {
            files_found: 1,
            current_dir: Path::new("/a"),
        });
        {
            let state = tui.state.lock().unwrap();
            assert_eq!(state.phase, Phase::Scanning);
        }

        // Hashing
        tui.update(&ProgressUpdate::HashProgress {
            files_hashed: 0,
            files_total: 10,
            bytes_hashed: 0,
            bytes_total: 1000,
        });
        {
            let state = tui.state.lock().unwrap();
            assert_eq!(state.phase, Phase::Hashing);
        }

        // Copy
        tui.update(&ProgressUpdate::CopyProgress {
            files_copied: 0,
            files_total: 10,
            bytes_copied: 0,
            bytes_total: 1000,
        });
        {
            let state = tui.state.lock().unwrap();
            assert_eq!(state.phase, Phase::Copying);
        }

        // Verify
        tui.update(&ProgressUpdate::VerifyProgress {
            files_verified: 0,
            files_total: 10,
        });
        {
            let state = tui.state.lock().unwrap();
            assert_eq!(state.phase, Phase::Verifying);
        }

        // Phase complete → Idle
        tui.update(&ProgressUpdate::PhaseComplete { phase: "verify" });
        {
            let state = tui.state.lock().unwrap();
            assert_eq!(state.phase, Phase::Idle);
        }
    }
}
