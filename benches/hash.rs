//! Criterion benches for [`shelf::hash::sha256_file`].
//!
//! Three sizes (1 MiB, 10 MiB, 100 MiB) covering the realistic span of a
//! photo library: thumbnails / small RAWs, full RAW frames, and video
//! clips. The 100 MiB case is slow to run but useful to verify throughput
//! doesn't degrade nonlinearly.
//!
//! Run with `cargo bench --bench hash`. The fixture files are created in
//! a tempdir per benchmark group and torn down when the group drops.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use shelf::hash::sha256_file;
use tempfile::TempDir;

fn make_fixture(dir: &TempDir, name: &str, size: usize) -> PathBuf {
    let path = dir.path().join(name);
    let mut f = File::create(&path).expect("create fixture");
    // Pseudo-random bytes via a tiny LCG so the file is uncompressible
    // and the hasher can't take any shortcuts on a constant page.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0usize;
    while written < size {
        for byte in buf.iter_mut() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *byte = (state >> 32) as u8;
        }
        let chunk = buf.len().min(size - written);
        f.write_all(&buf[..chunk]).expect("write fixture");
        written += chunk;
    }
    f.sync_all().expect("sync fixture");
    path
}

fn bench_sha256_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("sha256_file");
    let tmp = TempDir::new().expect("tmpdir");

    // 1 MiB and 10 MiB always; 100 MiB only when SHELF_BENCH_BIG=1 is set
    // so CI stays under a minute. The threshold isn't load-bearing — toggle
    // locally when you actually want the larger run.
    let mut cases: Vec<(&str, usize)> = vec![("1MiB", 1024 * 1024), ("10MiB", 10 * 1024 * 1024)];
    if std::env::var_os("SHELF_BENCH_BIG").is_some() {
        cases.push(("100MiB", 100 * 1024 * 1024));
    }

    for (name, size) in cases {
        let path = make_fixture(&tmp, name, size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &path, |b, p| {
            b.iter(|| sha256_file(p).expect("hash"));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_sha256_file);
criterion_main!(benches);
