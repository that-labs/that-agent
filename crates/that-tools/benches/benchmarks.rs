//! Benchmarks for core that-tools operations.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_token_counting(c: &mut Criterion) {
    // Token counting is used on every output path
    let short_text = "Hello, world! This is a short string.";
    let medium_text = "fn main() {\n".repeat(100);
    let long_text = "x".repeat(10000);

    c.bench_function("count_tokens_short", |b| {
        b.iter(|| {
            // We can't directly call that-tools' count_tokens since it's not a library,
            // but we benchmark the tiktoken tokenizer directly
            let bpe = tiktoken_rs::cl100k_base().unwrap();
            bpe.encode_ordinary(black_box(short_text)).len()
        })
    });

    c.bench_function("count_tokens_medium", |b| {
        b.iter(|| {
            let bpe = tiktoken_rs::cl100k_base().unwrap();
            bpe.encode_ordinary(black_box(&medium_text)).len()
        })
    });

    c.bench_function("count_tokens_long", |b| {
        b.iter(|| {
            let bpe = tiktoken_rs::cl100k_base().unwrap();
            bpe.encode_ordinary(black_box(&long_text)).len()
        })
    });
}

fn bench_json_serialization(c: &mut Criterion) {
    let data: Vec<serde_json::Value> = (0..100)
        .map(|i| {
            serde_json::json!({
                "name": format!("symbol_{}", i),
                "kind": "function",
                "line_start": i * 10,
                "line_end": i * 10 + 5,
            })
        })
        .collect();

    c.bench_function("serialize_100_symbols", |b| {
        b.iter(|| serde_json::to_string(black_box(&data)).unwrap())
    });

    c.bench_function("serialize_100_symbols_pretty", |b| {
        b.iter(|| serde_json::to_string_pretty(black_box(&data)).unwrap())
    });
}

criterion_group!(benches, bench_token_counting, bench_json_serialization);
criterion_main!(benches);
