//! Scan throughput benchmarks over a synthetic directory tree.
//!
//! Run with: `cargo bench --bench scan`

// Bench code is a special case where `.expect()` is idiomatic — a benchmark
// that errors during setup is broken and needs to fail loud.
#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;

use accroitre::{NullProgress, ScanConfig, scan_tree};
use criterion::{Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

/// Build a synthetic tree of N regular files under `dir`, each containing
/// `file_size` bytes of zeros.
fn build_tree(dir: &Path, num_files: usize, file_size: usize) -> std::io::Result<()> {
    for i in 0..num_files {
        let path = dir.join(format!("file_{i:04}.bin"));
        let data = vec![0u8; file_size];
        fs::write(&path, &data)?;
    }
    Ok(())
}

fn bench_scan(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let mut group = c.benchmark_group("scan");
    group.sample_size(10);

    for (label, num_files, file_size) in [
        ("100x4KiB", 100, 4 * 1024),
        ("1kx4KiB", 1_000, 4 * 1024),
        ("10kx4KiB", 10_000, 4 * 1024),
    ] {
        group.bench_function(label, |b| {
            b.iter(|| {
                let tmp = TempDir::new().expect("tempdir");
                build_tree(tmp.path(), num_files, file_size).expect("write synthetic file");
                let scan_result = rt.block_on(async {
                    scan_tree(tmp.path(), &ScanConfig::default(), &NullProgress).await
                });
                assert!(scan_result.is_ok(), "scan failed: {scan_result:?}");
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_scan);
criterion_main!(benches);
