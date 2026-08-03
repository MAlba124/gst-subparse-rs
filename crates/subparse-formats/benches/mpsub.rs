// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

// Dependency-free micro-benchmark (harness = false).
// Run: cargo bench -p subparse-formats --bench mpsub
use std::time::Instant;
use subparse_formats::{Format, ParseContext, parse_with};

/// Build a representative MPSub body: a `FORMAT=TIME` header plus ~300
/// relatively-timed cues (`<offset> <duration>` + text + blank line).
fn corpus() -> String {
    let mut s = String::with_capacity(24 * 1024);
    s.push_str("FORMAT=TIME\n\n");
    for i in 0..300u32 {
        // Small floating-point offset/duration in seconds.
        let off = 0.5 + (i % 5) as f32 * 0.25;
        let dur = 2.0 + (i % 3) as f32 * 0.5;
        s.push_str(&format!("{off} {dur}\n"));
        s.push_str(&format!("subtitle line {i}\nsecond text line {i}\n\n"));
    }
    s
}

fn main() {
    let body = corpus();
    let ctx = ParseContext::default();
    let iters = 2000;

    let cues = parse_with(Format::MpSub, &body, &ctx).unwrap();
    let per_iter_cues = cues.len();

    let start = Instant::now();
    let mut acc = 0usize;
    for _ in 0..iters {
        let cues = parse_with(Format::MpSub, &body, &ctx).unwrap();
        acc = acc.wrapping_add(cues.len());
    }
    let elapsed = start.elapsed();
    let per_iter_us = elapsed.as_secs_f64() * 1e6 / iters as f64;

    println!(
        "mpsub: {} bytes, {} cues/iter, {iters} iters, {:.3} ms total, {:.3} us/iter [acc={acc}]",
        body.len(),
        per_iter_cues,
        elapsed.as_secs_f64() * 1e3,
        per_iter_us,
    );
}
