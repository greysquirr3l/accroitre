# Accroître

> High-speed, cross-platform file copier with content-aware deduplication and SSH streaming.

Accroître reads files in physical disk order to eliminate random seeks on spinning disks, deduplicates content via xxHash-128 or BLAKE3 (copy once, hard-link the rest), batches small files through tar pipes to amortise syscall overhead, and transfers over SSH without SFTP. Designed to be the obvious choice for local copies, USB/NAS backups, and remote SSH transfers — hands-down faster than `rsync`, `scp`, and `cp` on real-world workloads.

- **Physical-order reads** — files are sorted by their on-disk block offset, so HDDs see one continuous sweep instead of thrashing across cylinders.
- **Content-aware dedup** — files with matching size and hash become hard-links. A 1 TiB copy of a 2 TiB dataset with 50 % duplicates writes 1 TiB and zero additional inodes for the shared half.
- **Tar-batched small files** — sub-`small_file_threshold` files are packed into a single tar archive in memory and unpacked at the destination, dropping per-file `open`/`write`/`close` overhead by 1–2 orders of magnitude on file-heavy trees.
- **SSH without SFTP** — uses `russh` for async, multiplexed channels and pipes tar archives directly over the SSH exec channel. No `sftp-server` requirement on the remote.
- **Resumable copies** — a `.accroitre-manifest.json` at the destination records per-file completion; an interrupted run picks up where it left off.
- **Delta sync** — only files whose size or mtime changed since the last run are copied.
- **Cross-process safety** — a destination-root exclusive lock prevents two `accro` runs from corrupting each other's manifest or SQLite cache.
- **Self-update** — `accro update` pulls a signed release from GitHub and atomically swaps the binary in place.

## Status

| | |
|---|---|
| Crate | [`accroitre`](https://crates.io/crates/accroitre) |
| Binary | `accro` |
| License | MIT OR Apache-2.0 |
| MSRV | Rust 1.96.0 (edition 2024) |
| Platforms | Linux, macOS, Windows |

## Installation

### From crates.io

```bash
cargo install accroitre-cli
```

This installs the `accro` binary into `~/.cargo/bin/`.

### From source

```bash
git clone https://github.com/greysquirr3l/accroitre
cd accroitre
cargo install --path accroitre-cli --locked
```

Requires a stable Rust toolchain ≥ 1.96.0 (`rustup toolchain install stable`).

### Pre-built binaries

Pre-built binaries for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, and `x86_64-pc-windows-msvc` are attached to each [GitHub release](https://github.com/greysquirr3l/accroitre/releases). SHA-256 checksums are published alongside.

```bash
# Linux x86_64
curl -L https://github.com/greysquirr3l/accroitre/releases/latest/download/accro-linux-amd64 \
  -o accro
chmod +x accro
./accro version
```

## Quick start

```bash
# Local → local copy
accro copy /mnt/source /mnt/backup

# Local → SSH (no SFTP server required on the remote)
accro copy /mnt/data user@nas.example.com:/mnt/backup

# SSH → local
accro copy admin@server.example.com:/var/log ./local-logs

# Resume an interrupted copy
accro copy --resume /mnt/source /mnt/backup

# Incremental sync (only changed files)
accro copy --delta /mnt/source /mnt/backup

# Mirror (delete destination files not in source)
accro copy --delete /mnt/source /mnt/backup

# Show what would be copied without writing
accro copy --dry-run /mnt/source /mnt/backup

# Pipe-friendly JSON log for orchestration
accro copy --log-file run.jsonl /mnt/source /mnt/backup

# Compute a hash without copying
accro hash --algorithm blake3 ./large-file.bin

# Self-update
accro update
```

Run `accro --help` or `accro copy --help` for the full flag reference.

## How it works

### 1. Scan (`accroitre::engine::scan`)

Walks the source tree, collects `FileEntry` records (path, size, mtime, permissions), and **resolves each file's physical block offset** via platform-specific ioctls:

- **macOS**: `fcntl(F_LOG2PHYS)`
- **Linux**: `FS_IOC_FIEMAP`
- **Windows**: `FSCTL_GET_NTFS_VOLUME_DATA` + per-file extent query

Files without resolvable offsets (network mounts, exotic filesystems) sort last. Sorting entries by physical offset converts what would be millions of random seeks into one continuous read sweep — typically a 3–10× speedup on HDDs for cold-disk workloads.

### 2. Hash & dedup (`accroitre::engine::hash`, `::dedup`)

A two-pass strategy minimises I/O:

1. **Group by size** — files with unique sizes cannot be duplicates; they skip hashing entirely.
2. **Hash size-collision groups** in parallel via `rayon`. Choose between xxHash-128 (extremely fast, ~25 GB/s/core) and BLAKE3 (cryptographic, ~5 GB/s/core).

The dedup engine then groups by hash. Files sharing a hash become hard-links — Linux/macOS `link(2)`, Windows `CreateHardLinkW` — so duplicate bytes aren't physically re-written.

### 3. Copy (`accroitre::engine::copy`)

Two-phase execution:

- **Phase 1** copies canonical files in disk order. Large files use platform-optimal syscalls:
  - **macOS APFS**: `clonefile(2)` — instant CoW clone, zero bytes written for the duplicate-extent case.
  - **Linux**: `copy_file_range(2)` → `splice(2)` → buffered fallback (in priority order).
  - **Windows**: `FSCTL_DUPLICATE_EXTENTS_TO_FILE` for ReFS block cloning; `CopyFileExW` with callbacks elsewhere.
  - Small files are batched into a tar archive in memory and unpacked at the destination.
- **Phase 2** hard-links duplicates.

A pre-flight `statvfs`/`GetDiskFreeSpaceExW` check guards against running out of disk mid-copy.

### 4. Verify (`accroitre::engine::verify`)

Re-hashes the destination files and compares against source hashes (sizes first as a cheap filter). Reports `VerifyError::HashMismatch` or `SizeMismatch` per failure with the offending path.

### 5. Persist (`accroitre::adapters::manifest`, `::cache`)

- **`.accroitre-manifest.json`** — per-file completion status (path, size, source hash, status). Atomically written via temp-file + rename. Survives crashes and `Ctrl-C`.
- **`.accroitre-cache.db`** — SQLite cache (WAL mode) of `(path, size, mtime, hash, algorithm)` keyed by path. Stale entries are invalidated on size or mtime mismatch. Cross-run dedup skips re-hashing unchanged files entirely.

## Performance

### Component throughput (measured, reproducible)

Micro-benchmarks under `accroitre/benches/` — run `cargo bench` to verify on your hardware.

Measured on Apple M3 Pro (5P+6E cores, 36 GB RAM), Rust 1.96.0, single-thread:

| Operation | Payload | Throughput |
|---|---|---|
| `xxhash-rust` xxh3-128 | 4 KiB | **38.0 GiB/s** |
| `xxhash-rust` xxh3-128 | 1 MiB | **38.4 GiB/s** |
| `xxhash-rust` xxh3-128 | 64 MiB | **37.5 GiB/s** |
| `blake3` | 4 KiB | **1.90 GiB/s** |
| `blake3` | 1 MiB | **2.00 GiB/s** |
| `blake3` | 64 MiB | **1.97 GiB/s** |
| `scan_tree` (synthetic 4 KiB files) | 100 files | 6,690 files/s |
| `scan_tree` (synthetic 4 KiB files) | 1,000 files | 6,840 files/s |
| `scan_tree` (synthetic 4 KiB files) | 10,000 files | 6,020 files/s |

**xxHash-128 is ~19× faster than BLAKE3** at the cost of being non-cryptographic. Use xxHash for hot-path dedup; switch to BLAKE3 when you need cryptographic collision resistance.

### End-to-end workload projections

The micro-benchmarks above are upper bounds. Real workloads are dominated by I/O, dedup ratio, and the platform-optimal syscall (clonefile / copy_file_range / CoW clone). Numbers below are illustrative projections derived from the dominant mechanism:

| Workload | Mechanism | Expected speedup vs `cp -a` |
|---|---|---|
| Cold HDD write, 1M small files | Tar-batched writes amortise per-file `open`/`write`/`close` | **~5–10×** |
| Cold HDD write, 50 % duplicates | Hard-links skip 50 % of physical writes | **~2×** |
| NVMe → NVMe, single large file | `copy_file_range` (Linux) / `clonefile` (macOS APFS) | **~2×** (kernel copy vs userspace copy) |
| Local → SSH | Async russh channel + tar over exec | **~2×** (no per-file SFTP protocol overhead) |

The dominant wins are: physical-order reads (cold HDD), hard-link dedup (re-copies), and tar-batched small files (file-heavy trees).

For end-to-end benchmarks specific to your workload, write a [`criterion`](https://github.com/bheisler/criterion.rs) bench under `accroitre/benches/` that materialises a representative tree and times the full pipeline.

## Architecture

Collapsed hexagonal (DDD-lite). The workspace has two crates:

```
accroitre/
├── accroitre/          # Library — published to crates.io
│   ├── src/
│   │   ├── domain/     # Pure types: FileEntry, Hash, CopyPlan, DedupGroup
│   │   ├── ports/      # Trait interfaces: FileSystemPort, HasherPort,
│   │   │               #   CopierPort, SshPort, CachePort, ProgressPort
│   │   ├── engine/     # Domain logic: scan, hash, dedup, copy, verify,
│   │   │               #   delta + platform-optimal I/O
│   │   └── adapters/   # Concrete implementations of the port traits
│   │                   #   (SqliteCache, CopyManifest, JsonLog, SshAdapter,
│   │                   #    DestLock, etc.)
│   └── Cargo.toml
└── accroitre-cli/      # Binary — produces `accro`
    └── src/
        ├── main.rs     # Clap derive CLI; subcommands: copy, hash, version, update
        ├── pipeline.rs # End-to-end orchestration: scan → hash → dedup →
        │               #   copy → verify → manifest
        ├── tui.rs      # Braille spinners + multi-bar progress via indicatif
        └── update.rs   # GitHub releases fetch + SHA-256 verification + atomic swap
```

The library crate is the only crate published to crates.io; the CLI crate is the consumer. The library depends on `tokio`, `rusqlite`, `russh`, and `indicatif` (an I/O-framework exception), but the `domain` module is pure Rust with zero framework dependencies — usable in `no_std`-adjacent contexts if the optional I/O adapters are excluded.

Port traits are defined in the handler/use-case module (here: `accroitre::engine`), not in the adapter crate. Each concrete adapter struct implements the trait; nothing is re-exported. This mirrors the consumer-owned-interface pattern from Go's idiomatic ports & adapters.

## Platform support

| Platform | Status | Notes |
|---|---|---|
| Linux x86_64 / aarch64 | Full | `copy_file_range`, `splice`, `sendfile`, optional io_uring on 5.1+ |
| macOS aarch64 (Apple Silicon) | Full | `clonefile` (APFS CoW), `fcopyfile` fallback |
| macOS x86_64 | Best-effort | Apple has begun deprecating x86_64 support; Tier 2 in Rust 1.90+ |
| Windows x86_64 | Full | Long-path support (`\\?\`), ReFS block cloning via `FSCTL_DUPLICATE_EXTENTS_TO_FILE` |
| Windows aarch64 | Compiles | Not regularly tested |

## Development

The workspace enforces a strict clippy profile (`all`, `pedantic`, `nursery`, `cargo`, `perf`) with project-wide allows/denies via `[workspace.lints.clippy]`. The full canonical lint command is exposed as `cargo l`:

```bash
cargo l              # strict lint (all groups + deny unwrap/expect/panic/indexing_slicing)
cargo test --workspace
cargo build --workspace
```

Tests return `Result<(), Box<dyn std::error::Error>>` and use `?` exclusively — no `.unwrap()`, `.expect()`, or `panic!()` in test code (memory file `nick.md`, lines 60–66).

## Contributing

Issues and pull requests welcome on [GitHub](https://github.com/greysquirr3l/accroitre). For larger changes, open an issue first to discuss the design. All commits follow [Conventional Commits](https://www.conventionalcommits.org/).

## Acknowledgements

- [`fast-copy`](https://github.com/gekap/fast-copy) original inspiration for accroitre.
- [`xxhash-rust`](https://crates.io/crates/xxhash-rust) and [`blake3`](https://crates.io/crates/blake3) for the hash functions.
- [`russh`](https://crates.io/crates/russh) for async SSH.
- [`indicatif`](https://crates.io/crates/indicatif) for the TUI progress display.
- [`rusqlite`](https://crates.io/crates/rusqlite) for the persistent hash cache.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
