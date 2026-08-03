// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

// Dependency-free micro-benchmark (harness = false).
// Run: cargo bench -p subparse-formats --bench mpl2
//
// Times the whole-file `Mpl2::parse` over a synthetic corpus using only
// std::time::Instant. The corpus exercises `[start][end]` decisecond parsing,
// the `|` line split, leading-`/` italics (`<i>...</i>`), Pango escaping and
// the final whitespace strip.

use std::time::Instant;

use subparse_formats::formats::Mpl2;
use subparse_formats::{ParseContext, SubtitleFormat};

/// Build an MPL2 body with `n` cues.
fn make_corpus(n: usize) -> String {
    let mut s = String::with_capacity(n * 48);
    for i in 0..n {
        let a = 100 + i * 30;
        let b = a + 25;
        match i % 3 {
            0 => s.push_str(&format!("[{a}][{b}] Plain line {i}|second half\n")),
            1 => s.push_str(&format!("[{a}][{b}]/Italic {i}|Normal & <plain>\n")),
            _ => s.push_str(&format!("[{a}][{b}] It's line {i}, \"quoted\"   \n")),
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

    bench("mpl2 parse (full file)", 3_000, n, || {
        let mut p = Mpl2::default();
        p.parse(&corpus, &ctx).map(|c| c.len()).unwrap_or(0)
    });
}
