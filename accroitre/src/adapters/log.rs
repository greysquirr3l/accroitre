//! Structured JSON logging adapter for all copy operations.
//!
//! Implements `ProgressPort` and writes JSON to a file. Each event is a
//! single JSON line; a summary object is appended when `finish()` is called.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::ports::{ProgressPort, ProgressUpdate};

/// An individual log entry written as a JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// Action: copied, linked, skipped, error, progress, or phase-complete.
    pub action: String,
    /// File path (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// File size in bytes (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Method used ("direct", "tar", "ssh").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Hard-link target (if action is "linked").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target: Option<PathBuf>,
    /// Error message (if action is "error").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Phase name (if action is phase-complete or progress).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Progress details (nested).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ProgressDetails>,
}

/// Progress counter details embedded in a log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_done: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_done: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
}

/// Summary statistics written at the end of a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogSummary {
    /// RFC 3339 timestamp at completion.
    pub timestamp: String,
    /// Always "summary".
    pub action: String,
    /// Source root path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// Destination root path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<PathBuf>,
    /// Transfer mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Number of files copied.
    pub files_copied: u64,
    /// Number of files hard-linked (dedup).
    pub files_linked: u64,
    /// Number of files skipped.
    pub files_skipped: u64,
    /// Number of file errors.
    pub files_errored: u64,
    /// Total bytes written.
    pub bytes_written: u64,
}

/// Internal mutable state protected by a mutex.
struct LogState {
    /// Output file handle.
    writer: File,
    /// Running counters.
    summary: LogSummary,
}

/// JSON log adapter implementing `ProgressPort`.
pub struct JsonLog {
    state: Mutex<LogState>,
}

impl JsonLog {
    /// Create a new JSON log writing to the given file path.
    ///
    /// # Errors
    ///
    /// Returns an `io::Error` if the file cannot be created.
    pub fn new(log_path: &Path) -> std::io::Result<Self> {
        let writer = File::create(log_path)?;
        Ok(Self {
            state: Mutex::new(LogState {
                writer,
                summary: LogSummary {
                    action: "summary".to_owned(),
                    ..LogSummary::default()
                },
            }),
        })
    }

    /// Set the source and destination paths for the summary.
    pub fn set_paths(&self, source: &Path, destination: &Path) {
        if let Ok(mut state) = self.state.lock() {
            state.summary.source = Some(source.to_path_buf());
            state.summary.destination = Some(destination.to_path_buf());
        }
    }

    /// Set the transfer mode for the summary.
    pub fn set_mode(&self, mode: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.summary.mode = Some(mode.to_owned());
        }
    }

    /// Record a file copy event.
    pub fn log_copied(&self, path: &Path, size: u64, method: &str) {
        self.write_entry(&LogEntry {
            timestamp: now_rfc3339(),
            action: "copied".to_owned(),
            path: Some(path.to_path_buf()),
            size: Some(size),
            method: Some(method.to_owned()),
            link_target: None,
            error: None,
            phase: None,
            progress: None,
        });
        if let Ok(mut state) = self.state.lock() {
            state.summary.files_copied += 1;
            state.summary.bytes_written += size;
        }
    }

    /// Record a hard-link event.
    pub fn log_linked(&self, path: &Path, target: &Path, size: u64) {
        self.write_entry(&LogEntry {
            timestamp: now_rfc3339(),
            action: "linked".to_owned(),
            path: Some(path.to_path_buf()),
            size: Some(size),
            method: None,
            link_target: Some(target.to_path_buf()),
            error: None,
            phase: None,
            progress: None,
        });
        if let Ok(mut state) = self.state.lock() {
            state.summary.files_linked += 1;
        }
    }

    /// Record a skipped file event.
    pub fn log_skipped(&self, path: &Path) {
        self.write_entry(&LogEntry {
            timestamp: now_rfc3339(),
            action: "skipped".to_owned(),
            path: Some(path.to_path_buf()),
            size: None,
            method: None,
            link_target: None,
            error: None,
            phase: None,
            progress: None,
        });
        if let Ok(mut state) = self.state.lock() {
            state.summary.files_skipped += 1;
        }
    }

    /// Record a file error event.
    pub fn log_error(&self, path: &Path, message: &str) {
        self.write_entry(&LogEntry {
            timestamp: now_rfc3339(),
            action: "error".to_owned(),
            path: Some(path.to_path_buf()),
            size: None,
            method: None,
            link_target: None,
            error: Some(message.to_owned()),
            phase: None,
            progress: None,
        });
        if let Ok(mut state) = self.state.lock() {
            state.summary.files_errored += 1;
        }
    }

    /// Write a single log entry as a JSON line.
    fn write_entry(&self, entry: &LogEntry) {
        if let Ok(mut state) = self.state.lock() {
            // Best-effort — don't propagate write errors from logging.
            if let Ok(json) = serde_json::to_string(&entry) {
                let _ = writeln!(state.writer, "{json}");
            }
        }
    }
    /// Build a progress log entry.
    fn progress_entry(phase: &str, path: Option<PathBuf>, details: ProgressDetails) -> LogEntry {
        LogEntry {
            timestamp: now_rfc3339(),
            action: "progress".to_owned(),
            path,
            size: None,
            method: None,
            link_target: None,
            error: None,
            phase: Some(phase.to_owned()),
            progress: Some(details),
        }
    }
}

impl ProgressPort for JsonLog {
    fn update(&self, event: &ProgressUpdate<'_>) {
        match event {
            ProgressUpdate::ScanProgress {
                files_found,
                current_dir,
            } => {
                self.write_entry(&Self::progress_entry(
                    "scan",
                    Some(current_dir.to_path_buf()),
                    ProgressDetails {
                        files_done: Some(*files_found),
                        files_total: None,
                        bytes_done: None,
                        bytes_total: None,
                    },
                ));
            }
            ProgressUpdate::HashProgress {
                files_hashed,
                files_total,
                bytes_hashed,
                bytes_total,
            } => {
                self.write_entry(&Self::progress_entry(
                    "hash",
                    None,
                    ProgressDetails {
                        files_done: Some(*files_hashed),
                        files_total: Some(*files_total),
                        bytes_done: Some(*bytes_hashed),
                        bytes_total: Some(*bytes_total),
                    },
                ));
            }
            ProgressUpdate::CopyProgress {
                files_copied,
                files_total,
                bytes_copied,
                bytes_total,
            } => {
                self.write_entry(&Self::progress_entry(
                    "copy",
                    None,
                    ProgressDetails {
                        files_done: Some(*files_copied),
                        files_total: Some(*files_total),
                        bytes_done: Some(*bytes_copied),
                        bytes_total: Some(*bytes_total),
                    },
                ));
            }
            ProgressUpdate::VerifyProgress {
                files_verified,
                files_total,
            } => {
                self.write_entry(&Self::progress_entry(
                    "verify",
                    None,
                    ProgressDetails {
                        files_done: Some(*files_verified),
                        files_total: Some(*files_total),
                        bytes_done: None,
                        bytes_total: None,
                    },
                ));
            }
            ProgressUpdate::PhaseComplete { phase } => {
                self.write_entry(&LogEntry {
                    timestamp: now_rfc3339(),
                    action: "phase_complete".to_owned(),
                    path: None,
                    size: None,
                    method: None,
                    link_target: None,
                    error: None,
                    phase: Some((*phase).to_owned()),
                    progress: None,
                });
            }
            ProgressUpdate::FileError { path, message } => {
                self.log_error(path, message);
            }
        }
    }

    fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.summary.timestamp = now_rfc3339();
            if let Ok(json) = serde_json::to_string(&state.summary) {
                let _ = writeln!(state.writer, "{json}");
            }
            let _ = state.writer.flush();
        }
    }
}

/// Current UTC time in RFC 3339 format.
fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::NamedTempFile;

    #[test]
    fn json_output_is_valid_and_deserializable() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let log_path = tmp.path().to_path_buf();

        let log = JsonLog::new(&log_path)?;
        log.log_copied(Path::new("/src/a.txt"), 1024, "direct");
        log.log_linked(Path::new("/src/b.txt"), Path::new("/dest/a.txt"), 1024);
        log.log_skipped(Path::new("/src/c.txt"));
        log.log_error(Path::new("/src/d.txt"), "permission denied");
        log.finish();

        drop(log);

        let mut content = String::new();
        File::open(&log_path)?.read_to_string(&mut content)?;

        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 5); // 4 entries + 1 summary

        // Every line should be valid JSON.
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line)?;
            assert!(parsed.is_object());
        }
        Ok(())
    }

    #[test]
    fn all_action_types_appear() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let log_path = tmp.path().to_path_buf();

        let log = JsonLog::new(&log_path)?;
        log.log_copied(Path::new("/a"), 10, "tar");
        log.log_linked(Path::new("/b"), Path::new("/a"), 10);
        log.log_skipped(Path::new("/c"));
        log.log_error(Path::new("/d"), "err");
        log.finish();

        drop(log);

        let mut content = String::new();
        File::open(&log_path)?.read_to_string(&mut content)?;

        let entries: Vec<LogEntry> = content
            .lines()
            .filter_map(|l| serde_json::from_str::<LogEntry>(l).ok())
            .collect();

        let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
        assert!(actions.contains(&"copied"));
        assert!(actions.contains(&"linked"));
        assert!(actions.contains(&"skipped"));
        assert!(actions.contains(&"error"));
        Ok(())
    }

    #[test]
    fn summary_stats_are_accurate() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let log_path = tmp.path().to_path_buf();

        let log = JsonLog::new(&log_path)?;
        log.set_paths(Path::new("/src"), Path::new("/dest"));
        log.set_mode("local");

        log.log_copied(Path::new("/a"), 100, "direct");
        log.log_copied(Path::new("/b"), 200, "direct");
        log.log_linked(Path::new("/c"), Path::new("/a"), 100);
        log.log_skipped(Path::new("/d"));
        log.log_error(Path::new("/e"), "fail");
        log.finish();

        drop(log);

        let mut content = String::new();
        File::open(&log_path)?.read_to_string(&mut content)?;

        // Last line is the summary.
        let last_line = content.lines().last().ok_or("empty content")?;
        let summary: LogSummary = serde_json::from_str(last_line)?;

        assert_eq!(summary.action, "summary");
        assert_eq!(summary.files_copied, 2);
        assert_eq!(summary.files_linked, 1);
        assert_eq!(summary.files_skipped, 1);
        assert_eq!(summary.files_errored, 1);
        assert_eq!(summary.bytes_written, 300);
        assert_eq!(summary.source.as_deref(), Some(Path::new("/src")));
        assert_eq!(summary.destination.as_deref(), Some(Path::new("/dest")));
        assert_eq!(summary.mode.as_deref(), Some("local"));
        Ok(())
    }

    #[test]
    fn progress_events_serialized() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = NamedTempFile::new()?;
        let log_path = tmp.path().to_path_buf();

        let log = JsonLog::new(&log_path)?;
        log.update(&ProgressUpdate::HashProgress {
            files_hashed: 5,
            files_total: 10,
            bytes_hashed: 5000,
            bytes_total: 10000,
        });
        log.update(&ProgressUpdate::PhaseComplete { phase: "hash" });
        log.finish();

        drop(log);

        let mut content = String::new();
        File::open(&log_path)?.read_to_string(&mut content)?;

        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3); // progress + phase_complete + summary

        let first = lines.first().ok_or("expected first line")?;
        let entry: LogEntry = serde_json::from_str(first)?;
        assert_eq!(entry.action, "progress");
        assert_eq!(entry.phase.as_deref(), Some("hash"));

        let details = entry.progress.ok_or("expected progress details")?;
        assert_eq!(details.files_done, Some(5));
        assert_eq!(details.files_total, Some(10));
        Ok(())
    }
}
