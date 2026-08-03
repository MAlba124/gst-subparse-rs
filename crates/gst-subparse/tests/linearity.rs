// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Regression tests for the *cost* of streaming, not its correctness.
//!
//! The element used to build a fresh parser and re-parse the whole accumulated
//! body on every chain call, so an N-chunk file cost O(N x size). A 40 KB
//! subtitle hid that completely (ten parses of a tiny buffer); a 20 MB one turns
//! it into tens of gigabytes of parsing and never finishes. Nothing else in the
//! path caps the input size, so the guard has to live here.
//!
//! These tests are deliberately generous with time: they are meant to separate
//! "linear" from "quadratic", not to benchmark. A failure means the quadratic
//! shape is back, not that the machine was busy.

use std::sync::Once;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// Roughly 20 MB of well-formed SubRip, as `(body, cue_count)`.
///
/// Cues are made deliberately fat (a few hundred bytes each) so the buffer
/// count stays in the tens of thousands: the test is about the size of the
/// input, and pushing a million one-line cues downstream would measure the
/// harness instead.
fn big_subrip(target_bytes: usize) -> (String, usize) {
    let filler = "The quick brown fox jumps over the lazy dog, again and again. ";
    let mut body = String::with_capacity(target_bytes + 1024);
    let mut count = 0usize;

    while body.len() < target_bytes {
        let n = count + 1;
        // Two seconds apart, so the parser's monotonicity guard is satisfied
        // and every cue is kept.
        let start = 2 * count as u64;
        let end = start + 2;
        body.push_str(&format!(
            "{n}\n{}:{:02}:{:02},000 --> {}:{:02}:{:02},000\n",
            start / 3600,
            (start / 60) % 60,
            start % 60,
            end / 3600,
            (end / 60) % 60,
            end % 60,
        ));
        // ~360 bytes of payload across two visual lines.
        body.push_str(filler);
        body.push_str(filler);
        body.push_str(&format!("cue {n}\n"));
        body.push_str(filler);
        body.push_str(&format!("cue {n} second line\n"));
        body.push('\n');
        count += 1;
    }

    (body, count)
}

/// Push `body` through the element in `chunk` sized buffers, draining output as
/// it goes, and return how many buffers came out.
fn stream_through_element(body: &str, chunk: usize) -> usize {
    init();

    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");

    let bytes = body.as_bytes();
    let mut pulled = 0usize;

    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + chunk).min(bytes.len());
        let buf = gst::Buffer::from_slice(bytes[offset..end].to_vec());
        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
        offset = end;
        // Drain as we go. Otherwise the harness queue holds every cue of a
        // 20 MB file at once, which measures the harness rather than us.
        while h.try_pull().is_some() {
            pulled += 1;
        }
    }

    h.push_event(gst::event::Eos::new());
    while h.try_pull().is_some() {
        pulled += 1;
    }

    pulled
}

/// Run `f` on a worker thread and fail if it has not finished within `budget`.
///
/// A quadratic parse of a 20 MB file does not finish in any bounded time, so
/// the test has to give up rather than wait for it. The worker is left running
/// (it holds no lock this process needs) and the test binary tears it down on
/// exit.
fn within<T: Send + 'static>(budget: Duration, what: &str, f: impl FnOnce() -> T + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let value = f();
        // A send failure just means the receiver already gave up.
        let _ = tx.send((value, started.elapsed()));
    });

    match rx.recv_timeout(budget) {
        Ok((_, elapsed)) => {
            eprintln!("{what} finished in {elapsed:?} (budget {budget:?})");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "{what} did not finish within {budget:?}; parsing is not linear in the input size"
        ),
        // The sender was dropped without sending: the worker panicked, and its
        // own message is already on stderr. Distinguished from a timeout so a
        // correctness failure is never reported as a performance one.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what} panicked; see the worker's assertion above")
        }
    }
}

/// ~20 MB fed in 4 KB buffers (the `filesrc` default read size). Under the old
/// whole-body-per-chain-call parser this is ~5000 parses over an average 10 MB
/// buffer, i.e. tens of gigabytes of parsing, and does not complete.
#[test]
fn subrip_streamed_in_small_chunks_is_linear() {
    let (body, cues) = big_subrip(20 * 1024 * 1024);
    eprintln!("generated {} bytes / {} cues of SubRip", body.len(), cues);

    within(
        Duration::from_secs(60),
        "20 MB SubRip in 4 KB chunks",
        move || {
            let pulled = stream_through_element(&body, 4096);
            assert_eq!(pulled, cues, "every cue must still be emitted");
        },
    );
}

/// A large body with **no line break in it at all**, streamed in small buffers.
///
/// Nothing can ever be consumed (every format here is line-oriented and no line
/// ever completes), so the retained buffer grows with the stream and the
/// parser is handed a longer and longer body each call. Searching that body for
/// a newline from offset zero every time would be quadratic all over again, for
/// the same reason the old whole-body parse was. `LineScanner` therefore keeps a
/// watermark of how far it has already looked and resumes from there, and this
/// is the test that holds it to that.
///
/// Note what this does *not* claim: memory. A newline-free stream still
/// accumulates, because a partial record has to be retained until it completes.
/// The bound on the retained buffer is the longest line, not a constant.
///
/// The assertion is a *ratio* against the same body in one buffer rather than a
/// wall-clock budget. Without the watermark this body takes ~150x the
/// single-buffer time, but only ~20 s in a release build, which any fixed
/// budget generous enough for a loaded CI machine would happily accept. Both
/// measurements come from the same machine and the same build, so the ratio
/// separates linear from quadratic without depending on either.
#[test]
fn body_with_no_line_breaks_is_still_linear() {
    // MicroDVD, so autodetection commits to a real parser and the parser is
    // actually invoked on every chain call. A body that detects as nothing
    // would return early and prove nothing.
    let mut body = String::with_capacity(20 * 1024 * 1024 + 16);
    body.push_str("{25}{50}");
    while body.len() < 20 * 1024 * 1024 {
        body.push_str("this line never ends and never will, not once, no. ");
    }
    assert!(!body.contains('\n'), "the point of this body is no newlines");
    let len = body.len();

    within(
        Duration::from_secs(120),
        "20 MB single line",
        move || {
            // One cue at EOS (the unterminated line is flushed at `at_eos`).
            let baseline = Instant::now();
            let pulled = stream_through_element(&body, len);
            let baseline = baseline.elapsed();
            assert!(pulled <= 1, "expected at most one cue, got {pulled}");

            let chunked = Instant::now();
            let pulled = stream_through_element(&body, 4096);
            let chunked = chunked.elapsed();
            assert!(pulled <= 1, "expected at most one cue, got {pulled}");

            // 25x the one-buffer cost, floored at a second so a very fast
            // baseline cannot make this trip on scheduling noise. Quadratic
            // rescanning lands two orders of magnitude above it.
            let allowed = (baseline * 25).max(Duration::from_secs(1));
            eprintln!(
                "{len} newline-free bytes: one buffer {baseline:?}, \
                 4 KB buffers {chunked:?}, allowed {allowed:?}"
            );
            assert!(
                chunked <= allowed,
                "streaming a newline-free body cost {chunked:?} against a \
                 {baseline:?} single-buffer baseline (allowed {allowed:?}); \
                 the unparsed remainder is being rescanned from the start on \
                 every chain call"
            );
        },
    );
}

/// The same body in one buffer. This is linear even under the old code (one
/// parse), so it is the control: if this is slow, the parser itself is slow and
/// the chunked test above is not measuring what it claims to.
#[test]
fn subrip_in_a_single_chunk_is_fast() {
    let (body, cues) = big_subrip(20 * 1024 * 1024);
    let len = body.len();

    within(
        Duration::from_secs(60),
        "20 MB SubRip in one chunk",
        move || {
            let pulled = stream_through_element(&body, len);
            assert_eq!(pulled, cues, "every cue must still be emitted");
        },
    );
}
