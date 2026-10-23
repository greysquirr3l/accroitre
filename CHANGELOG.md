# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Remote transfers over SSH** — `accro copy user@host:/path` and `accro copy /local user@host:/path` now work end to end, with no SFTP server required on the remote:
  - `accroitre::adapters::ssh` — the `SshAdapter` connection: key, agent and password authentication, `known_hosts` verification and the `--ssh-strict-host-key` policy.
  - `accroitre::adapters::ssh_tar` — tar-over-exec streaming. Small files are packed into one in-memory tar archive per batch and unpacked with `tar xf -` on the remote; duplicates are materialised as hard-links with `ln -f`.
  - `accroitre::adapters::remote_fs` — remote listing via `find` + `stat`, so no directory metadata crosses the wire.
  - `accroitre::adapters::remote_dedup` — remote-side hashing, so duplicates are detected where the data already is instead of being downloaded first.
  - `accroitre-cli::remote_pipeline` — the local ⇄ SSH orchestration, including host-to-host relays that never route the content through the local machine.
  - `accro hash --json --algorithm <xxhash128|blake3>` — the helper the remote side runs for deduplication. Install `accroitre-cli` on the remote host to get it.
- **`accroitre-cli` gained a library target** — the crate now exposes `accroitre_cli::remote_pipeline` and friends as a library alongside the `accro` binary, which lets the copy pipelines be driven — and tested — from outside the binary.

### Fixed

- **Remote deduplication found no duplicates** — the `xxhsum` and `sha256sum` fallbacks key their hash map by the relative name the tool was given, while the caller looks the hashes up by the full path of the entry. Every lookup missed, so no file was ever recognised as a duplicate. The parsed names are now resolved back to the full entry paths.
- **A broken remote hash tool failed the whole transfer** — `accro hash` on a remote that runs an older `accro` without the `hash` subcommand returned a non-zero exit status that was propagated as a fatal error. The hashing methods are now tried in order of preference, a method that hashes fewer files than requested is passed over, and streaming the content for local hashing remains the guaranteed last resort.

## [0.3.0] - 2026-10-23

### Changed

- **MSRV raised to Rust 1.99.1** — this is a breaking change for downstream consumers, since `rust-version` is part of the published package metadata. The workspace `rust-version`, `clippy.toml` MSRV, `rust-toolchain.toml` pin, and every CI toolchain pin move from 1.96.0 to 1.99.1 (current stable).
- **Toolchain is now pinned via `rust-toolchain.toml`** — new file. Without it a newer local stable raises clippy lints that CI's pinned toolchain does not have, so `cargo clippy` failed locally while CI stayed green. That drift is what hid the audit failure described under *Fixed* below.

### Fixed

- **Clippy lints that only appear on Rust 1.99+** — the new toolchain enables `clippy::assert_is_empty` and `clippy::unused_async_trait_impl`, which the CI lint profile treats as errors:
  - `ssh`: the `check_server_key` handler now returns `impl Future` directly instead of an `async fn` with no `.await` in it.
  - `remote_dedup`, `delta`, `macos_io`, `main`, `update`: empty-collection assertions changed from `assert!(x.is_empty())` to `assert_eq!`/`assert_ne!` so a failure prints the offending value.
- **CI audit step no longer drifts from `deny.toml`** — the two advisory-ignore lists are now identical, and the five stale entries whose advisories are no longer reachable were removed. Only `RUSTSEC-2023-0071` (`rsa`, via `russh` → `ssh-key`, no fixed pre-release published) and `RUSTSEC-2025-0119` (`number_prefix`, via `indicatif`) remain ignored.
- **`main` CI was failing on the dependency audit** — the `Audit dependencies` step errored on every push to `main` because the advisory set had grown past the inline ignore list. The `Cargo.lock` update below is what actually fixes it; adding more ignore entries would only have hidden the finding.

### Security

- **Dependency updates clear 13 outstanding advisories** — the `Cargo.lock` was refreshed to pull the fixes that were available upstream, taking `cargo audit` from 13 vulnerabilities and 4 warnings to zero:
  - `rustls` `0.23.37` → `0.23.46` and `rustls-webpki` `0.103.13` → `0.103.16` — fixes [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285), [RUSTSEC-2026-0346](https://rustsec.org/advisories/RUSTSEC-2026-0346), [RUSTSEC-2026-0348](https://rustsec.org/advisories/RUSTSEC-2026-0348), [RUSTSEC-2026-0349](https://rustsec.org/advisories/RUSTSEC-2026-0349) and [RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382).
  - `webpki-roots` `1.0.6` → `1.0.11` and `ctutils` `0.4.2` → `0.4.5` — fixes [RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381) and [RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385).
  - `quinn-proto` `0.11.15` → `0.11.19` — fixes [RUSTSEC-2026-0338](https://rustsec.org/advisories/RUSTSEC-2026-0338), [RUSTSEC-2026-0339](https://rustsec.org/advisories/RUSTSEC-2026-0339), [RUSTSEC-2026-0340](https://rustsec.org/advisories/RUSTSEC-2026-0340), [RUSTSEC-2026-0341](https://rustsec.org/advisories/RUSTSEC-2026-0341) and [RUSTSEC-2026-0342](https://rustsec.org/advisories/RUSTSEC-2026-0342).
  - `crossbeam-epoch` `0.9.18` → `0.9.21` — fixes [RUSTSEC-2026-0204](https://rustsec.org/advisories/RUSTSEC-2026-0204).
  - `anyhow` `1.0.102` → `1.0.104` — fixes [RUSTSEC-2026-0190](https://rustsec.org/advisories/RUSTSEC-2026-0190).
  - `rand` `0.9.2` was dropped from the dependency graph entirely, fixing [RUSTSEC-2026-0097](https://rustsec.org/advisories/RUSTSEC-2026-0097) and removing `rand_chacha`/`rand_core`.
  - `chacha20` `0.10.1` → `0.10.2` and `der` `0.8.0` → `0.8.2` — clears the two yanked-crate warnings.
  - `tokio` `1.51.0` → `1.54.1` — fixes [RUSTSEC-2026-0386](https://rustsec.org/advisories/RUSTSEC-2026-0386) (`AsyncFd::new` and related constructors violate I/O safety). accroitre does not use `AsyncFd`, so the unsound path was not reachable, but the fixed release is taken anyway.

## [0.2.4] - 2026-07-10

### Changed

- **CI: enable `accroitre-cli` in crates.io publish workflow** — the release workflow now publishes both `accroitre` (library) and `accroitre-cli` (binary) to crates.io, with a 30-second delay between publishes to allow registry synchronization.

### Fixed

- **Add missing crate metadata to `accroitre-cli`** — added `keywords` and `categories` to `accroitre-cli/Cargo.toml` to satisfy clippy's `cargo_common_metadata` lint for publishable crates.

## [0.2.3] - 2026-07-10

### Added

- **Publish `accroitre-cli` to crates.io** — the CLI binary crate is now published alongside the library crate, enabling `cargo install accroitre-cli` for direct installation of the `accro` command-line tool.

## [0.2.2] - 2026-06-25

### Fixed

- **CI: resolve cross-platform clippy errors** — three Windows/Linux clippy violations introduced with the 0.2.x feature work:
  - `linux_io`: added backticks around `CoW` in doc comment (`doc_markdown`).
  - `linux_io`: hoisted `FICLONE` constant before statements (`items_after_statements`).
  - `copy`: split `attempt_symlink_fallback` into two `#[cfg]`-gated functions; the non-Unix variant is now `const fn`, satisfying `missing_const_for_fn` on Windows.
  - `main`: boxed `Commands::Copy(Box<CopyArgs>)` to satisfy `large_enum_variant` on Windows where `CopyArgs` is wider due to `PathBuf` encoding.

## [0.2.1] - 2026-06-25

### Fixed

- **Remove redundant `russh-keys` dependency** — `russh-keys` was absorbed into `russh` at 0.60; the now-redundant workspace dep has been removed from `accroitre/Cargo.toml`.

## [0.2.0] - 2026-06-25

### Added

- **`--link-strategy` flag** — control how duplicate files are linked at the destination. `hardlink` (default) creates same-inode hard-links for the lowest disk use. `clone` attempts a platform-native CoW clone first (`clonefile(2)` on APFS, `FICLONE` ioctl on btrfs/XFS, `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on ReFS) and falls back to hard-link when CoW is unavailable on the destination filesystem. Adds a `files_cloned` counter to `CopyResult`.

### Fixed

- **Cross-device duplicate links** — hard-link failures due to `EXDEV` (canonical and duplicate on different mounts in the destination tree) now degrade gracefully per-link instead of silently dropping the file. Fallback chain: `fs::hard_link` → relative symlink with resolution verification (Unix only; Windows skips to copy) → full `fs::copy` as last resort. Adds `files_symlinked` and `files_fallback_copied` counters to `CopyResult`.
- **russh 0.61 API compatibility** — SSH adapter updated to the new `russh` 0.61 interface: `AuthResult` enum replaces bare `bool` for authentication results, `PrivateKeyWithHashAlg::new(Arc::new(key), None)` replaces `Arc<PrivateKey>` in `authenticate_publickey`, and `#[async_trait]` removed from the `Handler` impl (russh 0.61 uses native `impl Future` return types in the trait definition).

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

[Unreleased]: https://github.com/greysquirr3l/accroitre/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/greysquirr3l/accroitre/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/greysquirr3l/accroitre/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/greysquirr3l/accroitre/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/greysquirr3l/accroitre/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/greysquirr3l/accroitre/releases/tag/v0.1.0
