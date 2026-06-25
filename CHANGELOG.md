# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Cross-device duplicate links** — hard-link failures due to EXDEV (canonical and duplicate on different mounts in the destination tree) now degrade gracefully per-link instead of silently dropping the file. Fallback chain: `fs::hard_link` → relative symlink with resolution verification (Unix only; Windows skips to copy) → full `fs::copy` as last resort. Adds `files_symlinked` and `files_fallback_copied` counters to `CopyResult`.

## [0.1.1] - 2026-06-24

### Security

- **quinn-proto** upgraded `0.11.14` → `0.11.15` to resolve [RUSTSEC-2026-0185](https://rustsec.org/advisories/RUSTSEC-2026-0185): remote memory exhaustion via unbounded out-of-order stream reassembly (CVSS 7.5 high). Transitive dependency via `reqwest → quinn → quinn-proto`.

## [0.1.0] - 2026-06-22

### Added

- **Physical-order scanning** — `scan_tree` resolves per-file on-disk block offsets (macOS `F_LOG2PHYS`, Linux `FS_IOC_FIEMAP`, Windows NTFS extent query) and sorts entries by offset to eliminate random seeks on HDDs.
- **Content-aware deduplication** — two-pass strategy: group by size (cheap pre-filter) then hash size-collision groups with xxHash-128 or BLAKE3 in parallel via rayon. Duplicates become hard-links (`link(2)` / `CreateHardLinkW`).
- **Tar-batched small files** — files under `small_file_threshold` are packed into a single in-memory tar archive and unpacked at the destination, dropping per-file `open`/`write`/`close` overhead by 1–2 orders of magnitude on file-heavy trees.
- **Platform-optimal large-file copies**:
  - macOS APFS: `clonefile(2)` (CoW) → `fcopyfile` fallback.
  - Linux: `copy_file_range(2)` → `splice(2)` → `sendfile` → buffered copy, in priority order. `io_uring` opt-in on kernels 5.1+.
  - Windows: `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS block clone) → `CopyFileExW` → buffered copy.
- **SSH streaming without SFTP** — async, multiplexed SSH via `russh`. Pipes tar archives directly over the SSH exec channel — no `sftp-server` requirement on the remote. Supports key file, key file + passphrase, agent, and password auth.
- **Resumable copies** — `.accroitre-manifest.json` at the destination records per-file completion status (path, size, source hash, status). Atomic write-temp-then-rename. An interrupted run picks up where it left off.
- **Cross-process safety** — `DestLock` (File::try_lock on `.accroitre.lock` in the destination root) prevents two concurrent `accro` runs from clobbering each other's manifest temp file. Second invocation fails fast with a clear `LockError::Busy`.
- **SQLite hash cache** — `.accroitre-cache.db` in the destination root with WAL mode. Stale entries invalidated by size/mtime mismatch. Cross-run dedup skips re-hashing unchanged files.
- **Delta sync** — `compute_delta` filters source entries to those whose size or mtime changed since the last run. `find_orphans` / `delete_orphans` enable mirror-mode cleanup.
- **JSON structured logging** — `.accroitre-{ts}.jsonl` records every copied, linked, skipped, errored event with summary stats. Pipe-friendly for orchestration.
- **Self-update** — `accro update` checks GitHub releases for a newer version, verifies SHA-256 checksums, downloads the platform-matching asset, and atomically swaps the binary in place. Skips prereleases.
- **Rich TUI** — `indicatif`-powered multi-bar progress with braille spinners. `PhaseComplete` events mark phase transitions; `NullProgress` provided for non-interactive use.
- **Examples** — `cargo run --example basic_copy`, `cargo run --example resume_copy`, `cargo run --example ssh_config` demonstrate the programmatic API.
- **Library API at crate root** — `use accroitre::{scan_tree, execute_copy_plan, ...}` works without reaching into nested modules.
- **Strict CI** — GitHub Actions runs fmt + `cargo l` (full pedantic + nursery + cargo + perf profile with project-wide allow/deny set) + build + test across `ubuntu-latest`, `macos-latest`, `windows-latest`, plus `cargo audit` on Ubuntu, plus `cargo doc` with `-D warnings`, plus `cargo deny`, plus an MSRV job gated at 1.96.0.
- **crates.io publish workflow** — `.github/workflows/release.yml` resolves tag → verifies Cargo.toml version matches → publishes the library crate on `v*` push. Idempotent on re-run.

### Platform support

- Linux x86_64 / aarch64 (full: io_uring, copy_file_range, splice, sendfile)
- macOS aarch64 (full: clonefile, fcopyfile)
- macOS x86_64 (best-effort; Apple has begun deprecating x86_64)
- Windows x86_64 / aarch64 (full: long-path support, ReFS block cloning)

### Notes

- This is the initial public release. The API may change before 1.0.0.
- The library crate (`accroitre`) is the only crate published to crates.io. The CLI (`accroitre-cli`) is installed via `cargo install accroitre-cli`.
- MSRV: Rust 1.96.0 (edition 2024). Stable toolchain only; no nightly features.
- License: dual MIT OR Apache-2.0.

[Unreleased]: https://github.com/greysquirr3l/accroitre/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/greysquirr3l/accroitre/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/greysquirr3l/accroitre/releases/tag/v0.1.0
