//! Criterion benches for [`shelf::template::Template`]: parse and render
//! against two representative templates — a short photo-library style path
//! and a longer one with metadata + hash tokens.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use shelf::metadata::{DateSource, Metadata};
use shelf::template::{RenderContext, Template};

const SHORT: &str = "{yyyy}/{mm}/{seq:05}";
const LONG: &str = "{yyyy}/{mm}_{camera}/{kind}_{hh}{min}{ss}_{hash:8}";

fn fake_metadata() -> Metadata {
    let mut md = Metadata::minimal(
        NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(14, 22, 10)
            .unwrap(),
        DateSource::Exif,
        "photo".to_string(),
    );
    md.camera = Some("Canon EOS R5".to_string());
    md.lens = Some("RF 24-70mm F2.8 L IS USM".to_string());
    md.width = Some(8192);
    md.height = Some(5464);
    md
}

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("Template::parse");
    for (name, src) in &[("short", SHORT), ("long", LONG)] {
        group.bench_with_input(BenchmarkId::from_parameter(name), src, |b, s| {
            b.iter(|| Template::parse(s).expect("parse"));
        });
    }
    group.finish();
}

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("Template::render");
    let md = fake_metadata();
    let fallbacks: BTreeMap<String, String> = BTreeMap::new();
    let hash = "deadbeefcafebabedeadbeefcafebabedeadbeefcafebabedeadbeefcafebabe";

    for (name, src) in &[("short", SHORT), ("long", LONG)] {
        let tpl = Template::parse(src).expect("parse");
        let when = md.taken_at;
        group.bench_with_input(BenchmarkId::from_parameter(name), &tpl, |b, t| {
            b.iter(|| {
                let ctx = RenderContext {
                    taken_at: &when,
                    metadata: &md,
                    canonical_ext: Some("jpg"),
                    sha256_hex: hash,
                    seq: Some(42),
                    fallbacks: &fallbacks,
                };
                t.render(&ctx).expect("render")
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse, bench_render);
criterion_main!(benches);
