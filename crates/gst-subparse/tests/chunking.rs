// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Buffer-boundary tests for the two elements.
//!
//! `subparse-formats/tests/incremental.rs` drives the same chunk-equivalence
//! property at the parser level, but it can only split on `char` boundaries:
//! `parse_incremental` takes a `&str`. The cases that only exist below the
//! parser — a push buffer that ends *inside* a multi-byte character, or inside
//! the byte-order mark — belong here, because resolving them is the charset
//! decoder's job and the split has to arrive as raw bytes.
//!
//! The property is the same one, stated over the element's output:
//!
//! > the sequence of (pts, duration, text) an element emits does not depend on
//! > how the input bytes were divided into buffers.

use std::sync::Once;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // The charset decision is sensitive to this variable, and these tests
        // compare runs against each other, so pin it off for the whole binary.
        // SAFETY: no other thread of this binary is running yet.
        unsafe {
            std::env::remove_var("GST_SUBTITLE_ENCODING");
        }
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// What one buffer told us. Compared across chunkings.
type Emitted = (Option<u64>, Option<u64>, String);

/// Push `body` through `element` split at `splits` (ascending **byte** offsets,
/// which may land anywhere, including mid-character) and collect the output.
fn run_chunked(element: &str, caps: &str, body: &[u8], splits: &[usize]) -> Vec<Emitted> {
    init();

    let mut h = gst_check::Harness::new(element);
    h.set_src_caps_str(caps);

    let mut prev = 0usize;
    for &end in splits.iter().chain(std::iter::once(&body.len())) {
        assert!(end >= prev && end <= body.len(), "bad split {end}");
        assert_eq!(
            h.push(gst::Buffer::from_slice(body[prev..end].to_vec())),
            Ok(gst::FlowSuccess::Ok),
            "push of bytes {prev}..{end} failed"
        );
        prev = end;
    }
    h.push_event(gst::event::Eos::new());

    let mut out = Vec::new();
    while let Some(buf) = h.try_pull() {
        let text = {
            let map = buf.map_readable().unwrap();
            String::from_utf8_lossy(map.as_slice()).into_owned()
        };
        out.push((
            buf.pts().map(|t| t.nseconds()),
            buf.duration().map(|t| t.nseconds()),
            text,
        ));
    }
    out
}

fn subparse(body: &[u8], splits: &[usize]) -> Vec<Emitted> {
    run_chunked("rssubparse", "application/x-subtitle", body, splits)
}

// ---------------------------------------------------------------------------
// Bodies. Every one carries multi-byte UTF-8 so that "split mid-character" is
// among the offsets the exhaustive walk visits.
// ---------------------------------------------------------------------------

struct Case {
    name: &'static str,
    body: &'static str,
}

// Every entry is `#[cfg]`-gated on its format's feature, which `vec![]` cannot
// express, so the pushes stay.
#[allow(clippy::vec_init_then_push)]
fn cases() -> Vec<Case> {
    let mut v: Vec<Case> = Vec::new();

    #[cfg(feature = "subrip")]
    v.push(Case {
        name: "subrip",
        body: "1\n00:00:01,000 --> 00:00:02,000\nCafé — naïve\nSecond line\n\n\
               2\n00:00:02,500 --> 00:00:04,000\n<i>Two</i> & bold\n\n\
               3\n00:00:05,000 --> 00:00:06,000\nПривет мир 🎬 no blank line after",
    });

    #[cfg(feature = "webvtt")]
    v.push(Case {
        name: "webvtt",
        body: "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHéllo wörld\n\n\
               00:00:03.000 --> 00:00:04.000\n<b>Two</b> 🎬\n\n\
               00:00:05.000 --> 00:00:06.000\nTrailing, no blank line",
    });

    #[cfg(feature = "microdvd")]
    v.push(Case {
        name: "microdvd",
        body: "{1}{1}25.000\n{25}{50}Café|second line\n\
               {100}{200}{y:i}Two 日本語\n{300}{400}Three — trailing",
    });

    #[cfg(feature = "mpl2")]
    v.push(Case {
        name: "mpl2",
        body: "[123][456] This is the Earth|when the dinosaurs roamed\n\
               [1234][5678]a lush and fertile planét.\n\
               [12345][27890] /Italic 🎬|Normal\n",
    });

    #[cfg(feature = "subviewer")]
    v.push(Case {
        name: "subviewer",
        body: "[INFORMATION]\n[TITLE]xxxxxxxxxx\n[END INFORMATION]\n[SUBTITLE]\n\
               00:00:41.00,00:00:44.40\nThe Age of Göds was closing.\nEternity ended.\n\n\
               00:00:55.00,00:00:58.40\nTHERE IS A PLACE[br]ON EARTH 日本語\n\n",
    });

    #[cfg(feature = "sami")]
    v.push(Case {
        name: "sami",
        body: "<SAMI>\n<BODY>\n\
               <SYNC Start=1000><P>Hello wörld\n\
               <SYNC Start=2000><P>Second <i>italic</i> 🎬\n\
               <SYNC Start=3000><P>Third\n\
               </BODY>\n</SAMI>\n",
    });

    #[cfg(feature = "tmplayer")]
    v.push(Case {
        name: "tmplayer",
        body: "00:00:10,1=This is the Earth at a time\n\
               00:00:10,2=when the dinosaurs roaméd...\n\
               00:00:13,1=\n\
               00:00:14,1=a lush and fertile planet 日本語\n\
               00:00:16,1=\n",
    });

    #[cfg(feature = "mpsub")]
    v.push(Case {
        name: "mpsub",
        body: "FORMAT=TIME\n\n2.0 3.0\nHello wörld\n\n1.5 2.0\nSecond 🎬\nLine two\n\n",
    });

    #[cfg(feature = "qttext")]
    v.push(Case {
        name: "qttext",
        body: "{QTtext}{font:Sans}{size:18}\n[00:00:01.00]\nHello wörld\n\
               [00:00:03.00]\nSecond 日本語\n[00:00:05.00]\n",
    });

    #[cfg(feature = "lrc")]
    v.push(Case {
        name: "lrc",
        body: "[ti:Title]\n[00:01.00]first lyric\n[00:02.34]sécond lyric 🎬\n\
               [00:03.00]third\n",
    });

    #[cfg(feature = "dks")]
    v.push(Case {
        name: "dks",
        body: "[00:00:07]THERE IS A PLACE ON EARTH[br]WHERE IT IS MÖRNING\n\
               [00:00:12]\n[00:00:13]AND THE GREAT HERDS RUN FREE 日本語\n[00:00:15]\n",
    });

    v
}

/// The reference for one body: everything in a single push buffer.
fn reference(body: &str) -> Vec<Emitted> {
    let out = subparse(body.as_bytes(), &[]);
    assert!(
        out.len() >= 2,
        "body did not autodetect / produce cues, so the comparison would be vacuous:\n{body:?}"
    );
    out
}

// ---------------------------------------------------------------------------
// The property
// ---------------------------------------------------------------------------

/// Split after every single byte offset, including offsets inside multi-byte
/// characters. The charset decoder holds the incomplete tail in `state.pending`
/// (separate from `textbuf`), so the parser must never see a broken character
/// and the output must be byte-identical to the one-buffer run.
#[test]
fn every_byte_split_matches_single_buffer() {
    for case in cases() {
        let expected = reference(case.body);
        let bytes = case.body.as_bytes();
        for k in 0..=bytes.len() {
            let got = subparse(bytes, &[k]);
            assert_eq!(
                got,
                expected,
                "{}: splitting the input after byte {k} (mid-char: {}) changed the output",
                case.name,
                !case.body.is_char_boundary(k)
            );
        }
    }
}

/// One byte per push buffer. Every multi-byte character is split, every record
/// straddles many buffers, and the element makes one `parse_incremental` call
/// per byte.
#[test]
fn one_byte_per_buffer_matches_single_buffer() {
    for case in cases() {
        let expected = reference(case.body);
        let splits: Vec<usize> = (0..case.body.len()).collect();
        let got = subparse(case.body.as_bytes(), &splits);
        assert_eq!(
            got, expected,
            "{}: one byte per buffer changed the output",
            case.name
        );
    }
}

/// Zero-length push buffers interleaved with real ones, plus an empty final
/// buffer before EOS.
#[test]
fn empty_buffers_interleaved_match_single_buffer() {
    for case in cases() {
        let expected = reference(case.body);
        let len = case.body.len();
        for k in 0..=len {
            let splits = [0, 0, k, k, k, len, len];
            let got = subparse(case.body.as_bytes(), &splits);
            assert_eq!(
                got, expected,
                "{}: empty buffers around byte {k} changed the output",
                case.name
            );
        }
    }
}

/// A leading UTF-8 BOM, split at every byte including the two offsets *inside*
/// the three-byte mark. The BOM is what the decoder uses to commit a charset,
/// so a buffer that ends in the middle of it is the one place a split can
/// change the decoding rather than just the parse.
#[test]
fn bom_split_mid_mark_matches_single_buffer() {
    #[cfg(feature = "subrip")]
    {
        let body = "\u{feff}1\n00:00:01,000 --> 00:00:02,000\nJust testing é.\n\n\
                    2\n00:00:03,000 --> 00:00:04,000\nSecond\n\n";
        let expected = reference(body);
        for k in 0..=body.len() {
            let got = subparse(body.as_bytes(), &[k]);
            assert_eq!(
                got, expected,
                "BOM body split after byte {k} changed the output"
            );
        }
        // And byte-at-a-time, which splits the BOM at both interior offsets in
        // the same run.
        let splits: Vec<usize> = (0..body.len()).collect();
        assert_eq!(subparse(body.as_bytes(), &splits), expected);
    }
}

/// Formats whose *autodetection* does not survive CRLF, so a CRLF body of that
/// format yields nothing at all and there is no output to compare chunkings of.
///
/// Only LRC. `matches_lrc` accepts a metadata line such as `[ti:Title]` by
/// requiring it to end with `]`, and under CRLF the line ends with `]\r`. That
/// is a faithful port of the C, which splits on `"\n"` with `g_strsplit` and
/// leaves the `\r` on too, so it is upstream behaviour rather than a defect
/// introduced here, and changing it would be a deliberate divergence from the C
/// that this task has no mandate for. Recorded here so the exemption is visible
/// and so a *new* format cannot quietly join it.
const CRLF_DETECTION_GAPS: &[&str] = &["lrc"];

/// The same body as CRLF, split at every byte. The offset between the `\r` and
/// the `\n` is the one that matters: the line terminator itself is divided
/// across two push buffers.
#[test]
fn crlf_split_between_cr_and_lf_matches_single_buffer() {
    for case in cases() {
        let body = case.body.replace('\n', "\r\n");
        let expected = subparse(body.as_bytes(), &[]);

        if expected.is_empty() {
            assert!(
                CRLF_DETECTION_GAPS.contains(&case.name),
                "{}: CRLF body produced nothing and is not a known detection gap",
                case.name
            );
            continue;
        }
        assert!(
            expected.len() >= 2,
            "{}: CRLF body produced only {} cue(s)",
            case.name,
            expected.len()
        );

        for k in 0..=body.len() {
            let got = subparse(body.as_bytes(), &[k]);
            assert_eq!(
                got, expected,
                "{}: CRLF body split after byte {k} changed the output",
                case.name
            );
        }
    }
}

/// Pseudo-random buffer sizes with a fixed seed, over a body long enough to
/// cross many records.
#[test]
fn random_buffer_sizes_match_single_buffer() {
    const SEED: u64 = 0x0DDB_A11C_0FFE_E123;

    for case in cases() {
        // Repeat the body so a chunking crosses many records. Repeats push some
        // formats' monotonicity guards, which is fine: the property under test
        // is equivalence, not cue count.
        let mut body = String::new();
        for _ in 0..8 {
            body.push_str(case.body);
            if !body.ends_with('\n') {
                body.push('\n');
            }
        }
        let expected = subparse(body.as_bytes(), &[]);
        assert!(!expected.is_empty(), "{}: repeated body empty", case.name);

        let mut state = SEED ^ case.name.len() as u64;
        for round in 0..6 {
            let max_step = [1usize, 3, 7, 31, 128, 1024][round];
            let mut splits = Vec::new();
            let mut pos = 0usize;
            while pos < body.len() && splits.len() < 4 * body.len() + 16 {
                // xorshift64*
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let r = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as usize;
                pos = (pos + r % (max_step + 1)).min(body.len());
                splits.push(pos);
            }
            let got = subparse(body.as_bytes(), &splits);
            assert_eq!(
                got, expected,
                "{}: random buffers round {round} (seed {SEED:#x}, max {max_step}) \
                 changed the output",
                case.name
            );
        }
    }
}

/// EOS with nothing pushed at all must not panic and must emit nothing.
#[test]
fn eos_with_no_data_emits_nothing() {
    assert!(subparse(b"", &[]).is_empty());
}

/// A cue whose start time is the parsers' `GST_CLOCK_TIME_NONE` sentinel
/// (`u64::MAX`) must not blow up the element.
///
/// Several parsers deliberately produce it: SAMI's `TIME_NONE` for "no end
/// time", QTtext's `CLOCK_TIME_NONE` on timestamp overflow, TMPlayer's
/// saturating hour field. SAMI reaches it on ordinary-looking input: `</BODY>`
/// sets `time2 = TIME_NONE`, and the next `<SYNC>` copies `time2` into `time1`,
/// so every cue after a second `<BODY>` starts at `u64::MAX`. Two concatenated
/// SAMI documents in one stream is enough.
///
/// `gst::ClockTime::from_nseconds(u64::MAX)` panics, and while
/// `catch_panic_pad_function` downgrades that to a `FlowError`, the stream is
/// still dead. This predates the incremental work: the conversion is unchanged
/// by it.
#[test]
fn clock_time_none_sentinel_does_not_panic() {
    #[cfg(feature = "sami")]
    {
        let doc = "<SAMI>\n<BODY>\n\
                   <SYNC Start=1000><P>First\n\
                   <SYNC Start=2000><P>Second\n\
                   </BODY>\n</SAMI>\n";
        let body = format!("{doc}{doc}");
        // Must not error the flow, and must still emit the cues it can.
        let out = subparse(body.as_bytes(), &[]);
        assert!(
            out.len() >= 2,
            "concatenated SAMI documents produced {:?}",
            out.len()
        );
        // The cues with an unusable start time get no PTS rather than killing
        // the stream.
        assert!(
            out.iter().any(|(pts, _, _)| pts.is_none()),
            "expected at least one cue with an unset PTS, got {out:?}"
        );
    }
}

/// Format autodetection must not run on a **partial first line**.
///
/// `autodetect::detect` is a pure function of whatever has accumulated, and the
/// element latches its answer for the rest of the stream. A truncated line can
/// legitimately look like a different format: the first six bytes of the MPL2
/// line `[123][456] ...` are `[123][`, which is a perfectly good start for an
/// LRC `[mm:ss.xx]` tag, and `detect` says `Lrc`. The old guard was only
/// "at least six bytes have arrived" (the C's `strlen(textbuf) < 6`), so a
/// six-byte first push buffer silently condemned the whole stream to the wrong
/// parser and no cues at all.
///
/// That predates the incremental work (the guard and `autodetect` are both
/// untouched by it); the C has the same hole and only gets away with it because
/// `filesrc` hands it 4 KB at a time. It surfaced here because chunk
/// equivalence is now actually being tested.
#[test]
fn detection_is_not_fooled_by_a_partial_first_line() {
    #[cfg(all(feature = "mpl2", feature = "lrc"))]
    {
        let body = "[123][456] This is the Earth|when the dinosaurs roamed\n\
                    [1234][5678]a lush and fertile planet.\n";
        let expected = subparse(body.as_bytes(), &[]);
        assert_eq!(expected.len(), 2, "reference run should give two MPL2 cues");

        // Exactly the six bytes the old guard waited for, and no more.
        let got = subparse(body.as_bytes(), &[6]);
        assert_eq!(
            got, expected,
            "a 6-byte first buffer ('[123][') made the element latch the wrong \
             format for the whole stream"
        );
    }
}

/// A single record longer than any plausible buffer, fed one byte at a time.
#[test]
fn record_longer_than_many_buffers() {
    #[cfg(feature = "subrip")]
    {
        let long = "wörd ".repeat(400);
        let body = format!(
            "1\n00:00:01,000 --> 00:00:02,000\n{long}\n\n\
             2\n00:00:03,000 --> 00:00:04,000\nshort\n\n"
        );
        let expected = reference(&body);
        let splits: Vec<usize> = (0..body.len()).step_by(3).collect();
        assert_eq!(
            subparse(body.as_bytes(), &splits),
            expected,
            "a record spanning hundreds of buffers changed the output"
        );
    }
}

// ---------------------------------------------------------------------------
// ssaparse
// ---------------------------------------------------------------------------

#[cfg(feature = "ssa")]
mod ssa {
    use super::*;

    const BODY: &str = "[Script Info]\nTitle: Tëst\n\n\
                        [Events]\n\
                        Format: Layer, Start, End, Style, Name, MarginL, MarginR, \
                        MarginV, Effect, Text\n\
                        Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}wörld{\\i0}\n\
                        Dialogue: 0,0:00:03.50,0:00:05.00,Default,,0,0,0,,Second\\Nline 🎬\n\
                        Comment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,ignored\n\
                        Dialogue: 0,0:00:06.00,0:00:07.00,Default,,0,0,0,,Trailing, no newline";

    fn ssaparse(body: &[u8], splits: &[usize]) -> Vec<Emitted> {
        run_chunked("rsssaparse", "application/x-ssa", body, splits)
    }

    #[test]
    fn every_byte_split_matches_single_buffer() {
        let expected = ssaparse(BODY.as_bytes(), &[]);
        assert!(
            expected.len() >= 3,
            "ssa body produced {:?}",
            expected.len()
        );
        for k in 0..=BODY.len() {
            assert_eq!(
                ssaparse(BODY.as_bytes(), &[k]),
                expected,
                "ssaparse: split after byte {k} (mid-char: {}) changed the output",
                !BODY.is_char_boundary(k)
            );
        }
    }

    #[test]
    fn one_byte_per_buffer_matches_single_buffer() {
        let expected = ssaparse(BODY.as_bytes(), &[]);
        let splits: Vec<usize> = (0..BODY.len()).collect();
        assert_eq!(ssaparse(BODY.as_bytes(), &splits), expected);
    }

    #[test]
    fn eos_with_no_data_emits_nothing() {
        assert!(ssaparse(b"", &[]).is_empty());
    }
}
