// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

// Dependency-free micro-benchmark (harness = false).
// Run: cargo bench -p subparse-formats --bench webvtt
//
// Times the WebVTT parser over a synthetic corpus and reports ns/op and MB/s.
// No external crates: std::time::Instant + std::hint::black_box only.

use std::hint::black_box;
use std::time::Instant;

use subparse_formats::{Format, ParseContext};

/// Build a representative WebVTT body: a header plus a mix of plain, escaped,
/// styled (whitelisted + disallowed tags), settings-bearing, and multi-line
/// cues, repeated to a few hundred KB.
fn corpus() -> String {
    let mut s = String::from("WEBVTT FILE\n\n");
    // One "block" of varied cues. Timings advance so the monotonic guard passes.
    let block = |n: u64| -> String {
        let a = n * 4;
        let fmt = |t: u64| {
            let h = t / 3600;
            let m = (t % 3600) / 60;
            let sec = t % 60;
            format!("{h:02}:{m:02}:{sec:02}.000")
        };
        format!(
            "{cid}\n{s0} --> {e0} L:12% S:35% A:start\nPlain cue number {n}\n\n\
             {s1} --> {e1} D:vertical T:50%\n<v Narrator>Styled &amp; <b>bold</b> line\n\n\
             {s2} --> {e2}\nAngle < brackets > and ampersand & need escaping\n\n\
             {s3} --> {e3}\n<ruby>base<rt>ann</rt></ruby> then\nsecond line with <font>dropped</font>\n\n",
            cid = n,
            s0 = fmt(a),
            e0 = fmt(a + 2),
            s1 = fmt(a + 2),
            e1 = fmt(a + 3),
            s2 = fmt(a + 3),
            e2 = fmt(a + 4),
            s3 = fmt(a + 4),
            e3 = fmt(a + 5),
        )
    };
    let mut n = 0u64;
    while s.len() < 256 * 1024 {
        s.push_str(&block(n));
        n += 1;
    }
    s
}

fn main() {
    let body = corpus();
    let bytes = body.len();
    let ctx = ParseContext::default();

    // Warm up and learn the cue count.
    let mut cue_count = 0;
    for _ in 0..20 {
        let cues = Format::WebVtt.parser().parse(&body, &ctx).unwrap();
        cue_count = cues.len();
        black_box(&cues);
    }

    let iters: u32 = 1000;
    let start = Instant::now();
    let mut sink = 0usize;
    for _ in 0..iters {
        let cues = Format::WebVtt
            .parser()
            .parse(black_box(&body), &ctx)
            .unwrap();
        sink = sink.wrapping_add(black_box(cues.len()));
    }
    let elapsed = start.elapsed();
    black_box(sink);

    let ns_total = elapsed.as_nanos() as f64;
    let ns_per_iter = ns_total / iters as f64;
    let secs = elapsed.as_secs_f64();
    let mbps = (bytes as f64 * iters as f64) / (secs * 1_000_000.0);
    let ns_per_byte = ns_per_iter / bytes as f64;

    println!("webvtt bench");
    println!("  corpus:      {bytes} bytes ({} KiB)", bytes / 1024);
    println!("  cues/parse:  {cue_count}");
    println!("  iterations:  {iters}");
    println!("  total:       {:.3} ms", ns_total / 1e6);
    println!("  per parse:   {ns_per_iter:.0} ns  ({ns_per_byte:.3} ns/byte)");
    println!("  throughput:  {mbps:.1} MB/s");
}
