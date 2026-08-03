// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

// Dependency-free micro-benchmark (harness = false), timed with std::time.
// Run: cargo bench -p subparse-formats --bench qttext
//
// Parses a synthetic QTtext corpus many times and reports throughput. No
// external crates (no criterion). This is a plain binary per the crate policy.

use std::time::Instant;

use subparse_formats::formats::QtText;
use subparse_formats::{ParseContext, SubtitleFormat};

/// Build a representative QTtext body: a styled header plus `n` timestamped
/// cues cycling through plain / bold / italic / colored / multi-line blocks.
fn corpus(n: usize) -> String {
    let mut s = String::with_capacity(n * 96);
    s.push_str("{QTtext}{font:Sans}{size:14}{timescale:1000}\n");
    s.push_str("[00:00:00.00]\n");
    for i in 0..n {
        match i % 5 {
            0 => s.push_str("{plain}A plain caption line\n"),
            1 => s.push_str("{bold}A bold caption line\n"),
            2 => s.push_str("{italic}An italic caption line\n"),
            3 => {
                s.push_str("{textColor:65535,32768,0}{backColor:0,0,32768}");
                s.push_str("A colored caption line\n");
            }
            _ => s.push_str("First line of two\nSecond line of two\n"),
        }
        // Each cue is ~2 seconds long. Emit the closing timestamp.
        let total_cs = (i as u64 + 1) * 200; // centiseconds at timescale 1000 -> use ms
        let ms = total_cs * 10;
        let sec = ms / 1000;
        let dec = ms % 1000;
        let h = sec / 3600;
        let m = (sec % 3600) / 60;
        let sc = sec % 60;
        s.push_str(&format!("[{h:02}:{m:02}:{sc:02}.{dec:03}]\n"));
    }
    s
}

fn main() {
    let n_cues = 2_000usize;
    let body = corpus(n_cues);
    let bytes = body.len();
    let ctx = ParseContext::default();

    // Warm up.
    let warm = QtText::default().parse(&body, &ctx).expect("parse ok");
    let cues_per_run = warm.len();

    let iters = 200usize;
    let mut sink = 0usize;
    let start = Instant::now();
    for _ in 0..iters {
        let cues = QtText::default().parse(&body, &ctx).expect("parse ok");
        sink = sink.wrapping_add(cues.len());
    }
    let elapsed = start.elapsed();

    let total_bytes = (bytes * iters) as f64;
    let secs = elapsed.as_secs_f64();
    let mb_s = total_bytes / secs / (1024.0 * 1024.0);
    let per_run_us = elapsed.as_micros() as f64 / iters as f64;

    println!("qttext bench");
    println!("  corpus:      {bytes} bytes, {cues_per_run} cues");
    println!("  iterations:  {iters}");
    println!("  total time:  {elapsed:?}");
    println!("  per run:     {per_run_us:.1} us");
    println!("  throughput:  {mb_s:.1} MiB/s");
    // Keep the optimizer honest.
    std::hint::black_box(sink);
}
