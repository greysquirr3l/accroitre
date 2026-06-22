//! Hash throughput benchmarks for xxHash-128 and BLAKE3.
//!
//! Run with: `cargo bench --bench hash`

use std::time::Duration;

use accroitre::{HashAlgorithm, hash_bytes};
use criterion::{Criterion, criterion_group, criterion_main};

fn bench_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash");

    // Three payload sizes covering the typical copier workload.
    for (label, data) in [
        ("4 KiB", vec![0xABu8; 4 * 1024]),
        ("1 MiB", vec![0xCDu8; 1024 * 1024]),
        ("64 MiB", vec![0xEFu8; 64 * 1024 * 1024]),
    ] {
        group.bench_function(format!("xxhash128/{label}"), |b| {
            b.iter(|| hash_bytes(&data, HashAlgorithm::XxHash128));
        });
        group.bench_function(format!("blake3/{label}"), |b| {
            b.iter(|| hash_bytes(&data, HashAlgorithm::Blake3));
        });
    }

    group.measurement_time(Duration::from_secs(5));
    group.finish();
}

criterion_group!(benches, bench_hash);
criterion_main!(benches);
