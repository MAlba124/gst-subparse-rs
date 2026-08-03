// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

// Dependency-free micro-benchmark (harness = false).
// Run: cargo bench -p subparse-formats --bench tmplayer
//
// Times the whole-file `TmPlayer::parse` over a synthetic corpus using only
// std::time::Instant. The corpus exercises the buffering state machine: single
// and multiline timestamp scanning, `|`->newline, duration deduction from the
// next timestamp (with the 5 s clamp), and the end-of-stream flush.

use std::time::Instant;

use subparse_formats::formats::TmPlayer;
use subparse_formats::{ParseContext, SubtitleFormat};

/// Build a TMPlayer body with `n` timestamped text lines (single-line and
/// multiline varieties interleaved, each closed by the next timestamp).
fn make_corpus(n: usize) -> String {
    let mut s = String::with_capacity(n * 48);
    for i in 0..n {
        let sec = i * 3;
        let (h, m, r) = (sec / 3600, (sec / 60) % 60, sec % 60);
        if i % 2 == 0 {
            s.push_str(&format!(
                "{h}:{m:02}:{r:02}:Single line {i}|with a second part\n"
            ));
        } else {
            s.push_str(&format!("{h}:{m:02}:{r:02},1=Multiline {i} part one\n"));
            s.push_str(&format!("{h}:{m:02}:{r:02},2=part two\n"));
        }
    }
    s
}

fn bench<F: FnMut() -> usize>(label: &str, iters: u32, units: usize, mut f: F) {
    let mut sink = 0usize;
    for _ in 0..(iters / 10).max(1) {
        sink = sink.wrapping_add(f());
    }
    let t = Instant::now();
    for _ in 0..iters {
        sink = sink.wrapping_add(f());
    }
    let elapsed = t.elapsed().as_nanos() as f64;
    let per_call = elapsed / iters as f64;
    let per_unit = per_call / units as f64;
    println!(
        "{label:<28} {per_call:>10.0} ns/op  {per_unit:>8.1} ns/unit  (iters={iters}, sink={})",
        sink & 0xff
    );
}

fn main() {
    let ctx = ParseContext::default();
    let n = 500usize;
    let corpus = make_corpus(n);

    bench("tmplayer parse (full file)", 3_000, n, || {
        let mut p = TmPlayer::default();
        p.parse(&corpus, &ctx).map(|c| c.len()).unwrap_or(0)
    });
}
