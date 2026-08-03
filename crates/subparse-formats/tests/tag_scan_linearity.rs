// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Regression tests for the *cost* of the SubRip/WebVTT markup pipeline.
//!
//! `subrip_remove_unhandled_tags` in the C searches for the closing `&gt;` from
//! every escaped `<` it walks past, i.e. it rescans to the end of the buffer per
//! `&lt;`. That is quadratic in the length of one cue, and the length of one cue
//! is not bounded by anything: a cue ends at a blank line, so a body without one
//! is a single cue as long as the stream. A 32 KB cue already cost seconds; a
//! multi-megabyte one never finishes and hangs the element.
//!
//! The port therefore scans once with a forward-only cursor over the `&gt;`
//! positions. These tests are deliberately generous with time: they separate
//! "linear" from "quadratic", they are not benchmarks. The quadratic shape needs
//! hours for the bodies below, so a failure means it is back, not that the
//! machine was busy.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use subparse_formats::formats::{SubRip, WebVtt};
use subparse_formats::{ParseContext, SubtitleFormat};

/// One megabyte of cue text, unterminated (no blank line), so the whole body is
/// a single cue and the whole markup pipeline sees it in one piece.
///
/// `unit` is repeated to fill the cue. `tail` is appended once at the very end,
/// which is where the interesting case lives: a lone `>` a megabyte away is the
/// only closing marker the tag scan can find, and the C would look for it again
/// from every `<` in between.
fn one_huge_cue(timing: &str, unit: &str, tail: &str) -> String {
    const TARGET: usize = 1024 * 1024;

    let mut body = String::with_capacity(TARGET + timing.len() + 64);
    body.push_str(timing);
    while body.len() < TARGET {
        body.push_str(unit);
    }
    body.push_str(tail);
    body
}

/// Run `f` on a worker thread and fail if it has not finished within `budget`.
///
/// A quadratic scan of a megabyte does not finish in any bounded time, so the
/// test has to give up rather than wait for it. The worker is left running (it
/// holds nothing this process needs) and the test binary tears it down on exit.
fn within(budget: Duration, what: &str, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        f();
        // A send failure just means the receiver already gave up.
        let _ = tx.send(started.elapsed());
    });

    match rx.recv_timeout(budget) {
        Ok(elapsed) => eprintln!("{what} finished in {elapsed:?} (budget {budget:?})"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "{what} did not finish within {budget:?}; the tag scan is not linear \
             in the length of a cue"
        ),
        // The sender was dropped without sending: the worker panicked and its
        // own message is already on stderr. Distinguished from a timeout so a
        // correctness failure is never reported as a performance one.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("{what} panicked; see the worker's assertion above")
        }
    }
}

const BUDGET: Duration = Duration::from_secs(30);

/// `<5` is not a tag (a digit cannot start a tag name), so every escaped `&lt;`
/// survives the scan and the scan has to walk past all of them.
#[test]
fn subrip_one_huge_cue_with_a_distant_closing_marker_is_linear() {
    let body = one_huge_cue("1\n00:00:01,000 --> 00:00:02,000\n", "<5", ">");

    within(BUDGET, "1 MB SubRip cue of \"<5\"", move || {
        let cues = SubRip::default()
            .parse(&body, &ParseContext::default())
            .expect("subrip parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(
            cues[0].text.starts_with("&lt;5&lt;5"),
            "text starts {:?}",
            &cues[0].text[..16.min(cues[0].text.len())]
        );
        assert!(cues[0].text.ends_with("&gt;"), "the lone '>' must survive");
    });
}

/// The same body with no closing marker at all. The C searches (and fails) from
/// every `&lt;`, which is just as quadratic as finding one.
#[test]
fn subrip_one_huge_cue_without_any_closing_marker_is_linear() {
    let body = one_huge_cue("1\n00:00:01,000 --> 00:00:02,000\n", "<font ", "");

    within(BUDGET, "1 MB SubRip cue of \"<font \"", move || {
        let cues = SubRip::default()
            .parse(&body, &ParseContext::default())
            .expect("subrip parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        // Nothing is dropped: an unhandled tag needs its closing "&gt;".
        assert!(cues[0].text.starts_with("&lt;font &lt;font "));
    });
}

#[test]
fn webvtt_one_huge_cue_with_a_distant_closing_marker_is_linear() {
    let body = one_huge_cue("WEBVTT FILE\n\n00:00:01.000 --> 00:00:02.000\n", "<5", ">");

    within(BUDGET, "1 MB WebVTT cue of \"<5\"", move || {
        let cues = WebVtt::default()
            .parse(&body, &ParseContext::default())
            .expect("webvtt parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(cues[0].text.starts_with("&lt;5&lt;5"));
        assert!(cues[0].text.ends_with("&gt;"), "the lone '>' must survive");
    });
}

#[test]
fn webvtt_one_huge_cue_without_any_closing_marker_is_linear() {
    let body = one_huge_cue(
        "WEBVTT FILE\n\n00:00:01.000 --> 00:00:02.000\n",
        "<font ",
        "",
    );

    within(BUDGET, "1 MB WebVTT cue of \"<font \"", move || {
        let cues = WebVtt::default()
            .parse(&body, &ParseContext::default())
            .expect("webvtt parse is infallible");
        assert_eq!(cues.len(), 1, "the body is one unterminated cue");
        assert!(cues[0].text.starts_with("&lt;font &lt;font "));
    });
}
