// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Element-level tests for `rsssaparse`'s **container-framed** mode: the shape
//! the upstream C `ssaparse` accepts, and the only one a demuxer produces.
//!
//! Matroska hands the script header over once in the caps' `codec_data` and then
//! pushes one dialogue *field row* per buffer:
//!
//! ```text
//! 0,0,Default,,0,0,0,,Hello world
//! ```
//!
//! No `[Events]` section, no `Dialogue:` keyword, and the timing on the buffer
//! rather than in the line. The element used to feed exactly this to the
//! whole-file `[Events]` parser, which produced no cues at all for every
//! embedded SSA/ASS track there is.
//!
//! `tests/chunking.rs` covers the whole-file mode's buffer-boundary behaviour;
//! what is repeated here is only that the mode is still reachable, since it is
//! now selected by the absence of `codec_data`.

use std::sync::Once;

use gst::prelude::*;

const MS: u64 = 1_000_000; // one millisecond, in nanoseconds
const S: u64 = 1_000_000_000; // one second, in nanoseconds

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // The charset tests below pin the behaviour with nothing configured, so
        // they have to see an unset variable even when the suite is run with one
        // exported.
        // SAFETY: no other thread of this binary is running a pipeline or
        // reading the environment at this point.
        unsafe {
            std::env::remove_var("GST_SUBTITLE_ENCODING");
        }
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// A realistic Matroska `CodecPrivate` for an ASS track: the script up to and
/// including the `[Events]` `Format:` line, whose `ReadOrder` first column is
/// what makes a framed row's field order differ from a file's `Dialogue:` line.
const INIT_SECTION: &str = "\
[Script Info]
ScriptType: v4.00+
PlayResX: 1920

[V4+ Styles]
Format: Name, Fontname, Fontsize
Style: Default,Arial,48

[Events]
Format: ReadOrder, Layer, Style, Name, MarginL, MarginR, MarginV, Effect, Text
";

/// What one output buffer told us: `(pts, duration, text)`.
type Emitted = (Option<u64>, Option<u64>, String);

/// A harness whose caps carry `codec_data`, i.e. framed mode.
fn framed_harness(init_section: &str) -> gst_check::Harness {
    init();
    let mut h = gst_check::Harness::new("rsssaparse");
    let codec_data = gst::Buffer::from_slice(init_section.as_bytes().to_vec());
    h.set_src_caps(
        gst::Caps::builder("application/x-ass")
            .field("codec_data", &codec_data)
            .build(),
    );
    h
}

/// One framed dialogue row, timed like a demuxer times it.
fn row(line: &[u8], pts: u64, duration: u64) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_slice(line.to_vec());
    {
        let buf = buffer.get_mut().unwrap();
        buf.set_pts(gst::ClockTime::from_nseconds(pts));
        buf.set_duration(gst::ClockTime::from_nseconds(duration));
    }
    buffer
}

fn drain(h: &mut gst_check::Harness) -> Vec<Emitted> {
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

fn emitted(pts: u64, duration: u64, text: &str) -> Emitted {
    (Some(pts), Some(duration), text.to_owned())
}

/// The negotiated caps on the harness's sink pad.
fn src_caps(h: &gst_check::Harness) -> gst::Caps {
    h.sinkpad()
        .and_then(|p| p.current_caps())
        .expect("caps must be negotiated")
}

// ---------------------------------------------------------------------------
// Framed mode, end to end
// ---------------------------------------------------------------------------

/// The whole point: field rows in, timed pango-markup out.
///
/// Every row carries the same transform the C's `gst_ssa_parse_push_line`
/// applies: skip the 8 non-text fields, drop `{...}` override blocks, translate
/// the `\N`/`\n`/`\h` escapes, markup-escape the rest. The timing comes off the
/// buffer, untouched.
#[test]
fn framed_rows_become_timed_cues() {
    let mut h = framed_harness(INIT_SECTION);

    let rows: &[(&str, u64, u64)] = &[
        ("0,0,Default,,0,0,0,,{\\i1}Hello world{\\i0}", S, 2 * S),
        ("1,0,Default,,0,0,0,,Hello, world & <friends>", 4 * S, S),
        ("2,0,Default,,0,0,0,,First\\NSecond", 6 * S, 1500 * MS),
        ("3,0,Default,,0,0,0,,a\\hb 'q' \"d\"", 8 * S, 500 * MS),
    ];
    for (line, pts, duration) in rows {
        assert_eq!(
            h.push(row(line.as_bytes(), *pts, *duration)),
            Ok(gst::FlowSuccess::Ok)
        );
    }

    assert_eq!(
        drain(&mut h),
        vec![
            emitted(S, 2 * S, "Hello world"),
            emitted(4 * S, S, "Hello, world &amp; &lt;friends&gt;"),
            // "\N" is a space then a newline, a quirk of the C we preserve.
            emitted(6 * S, 1500 * MS, "First \nSecond"),
            emitted(8 * S, 500 * MS, "a  b &apos;q&apos; &quot;d&quot;"),
        ]
    );

    let caps = src_caps(&h);
    let s = caps.structure(0).unwrap();
    assert_eq!(s.name(), "text/x-raw");
    assert_eq!(s.get::<String>("format").unwrap(), "pango-markup");
}

/// The text field keeps its commas: only the 8 fields before it are skipped.
#[test]
fn framed_text_field_keeps_its_commas() {
    let mut h = framed_harness(INIT_SECTION);
    assert_eq!(
        h.push(row(
            b"0,0,Default,,0,0,0,,Hello, world, and again",
            S,
            2 * S
        )),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        drain(&mut h),
        vec![emitted(S, 2 * S, "Hello, world, and again")]
    );
}

/// A row ending in `\N` translates to a trailing `" \n"`, and this element never
/// emits a trailing newline (its whole-file path and the C `subparse` both strip
/// them). The space the C's translation puts *before* the newline is text and
/// stays.
#[test]
fn framed_trailing_newline_escape_is_trimmed() {
    let mut h = framed_harness(INIT_SECTION);
    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,Ends here\\N", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    let out = drain(&mut h);
    assert_eq!(out, vec![emitted(S, S, "Ends here ")]);
    assert!(!out[0].2.ends_with('\n'));
}

/// The `subtitle-codec` tag the C sends before its first row.
#[test]
fn framed_mode_sends_the_subtitle_codec_tag() {
    let mut h = framed_harness(INIT_SECTION);
    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,Hi", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    drain(&mut h);

    let mut codec = None;
    while let Some(event) = h.try_pull_event() {
        if let gst::EventView::Tag(tag) = event.view() {
            codec = tag
                .tag()
                .get::<gst::tags::SubtitleCodec>()
                .map(|v| v.get().to_owned());
        }
    }
    assert_eq!(codec.as_deref(), Some("SubStation Alpha"));
}

// ---------------------------------------------------------------------------
// Rows with nothing to show
// ---------------------------------------------------------------------------

/// An empty or whitespace-only result is dropped rather than pushed as an empty
/// buffer, and dropping it must not cost the rows around it.
///
/// This is not a corner case: karaoke rows whose text is nothing but timing
/// override codes strip to the empty string.
#[test]
fn framed_empty_dialogue_is_dropped() {
    let mut h = framed_harness(INIT_SECTION);

    let rows: &[(&[u8], u64)] = &[
        (b"0,0,Default,,0,0,0,,Before", S),
        // Eight commas, no text at all.
        (b"1,0,Default,,0,0,0,,", 2 * S),
        // Only override codes.
        (b"2,0,Default,,0,0,0,,{\\k30}{\\k25}", 3 * S),
        // Whitespace only.
        (b"3,0,Default,,0,0,0,,   ", 4 * S),
        // A zero-length buffer, the C's `empty_text` case.
        (b"", 5 * S),
        (b"4,0,Default,,0,0,0,,After", 6 * S),
    ];
    for (line, pts) in rows {
        assert_eq!(h.push(row(line, *pts, S)), Ok(gst::FlowSuccess::Ok));
    }

    assert_eq!(
        drain(&mut h),
        vec![emitted(S, S, "Before"), emitted(6 * S, S, "After")]
    );
}

/// A row with fewer than 8 commas is not a dialogue row. The C returns
/// `GST_FLOW_ERROR` and then swallows it, so either way nothing is emitted and
/// the stream carries on.
#[test]
fn framed_malformed_row_is_dropped_without_killing_the_stream() {
    let mut h = framed_harness(INIT_SECTION);

    for (line, pts) in [
        (&b"0,0,Default"[..], S),
        (&b"not a dialogue row at all"[..], 2 * S),
        (&b"1,0,Default,,0,0,0,,Fine"[..], 3 * S),
    ] {
        assert_eq!(h.push(row(line, pts, S)), Ok(gst::FlowSuccess::Ok));
    }

    assert_eq!(drain(&mut h), vec![emitted(3 * S, S, "Fine")]);
}

// ---------------------------------------------------------------------------
// codec_data handling
// ---------------------------------------------------------------------------

/// The C rejects the caps when the init section has no `[Script Info]` header.
/// This element only warns: the section is never read back, and a remuxed file
/// with a stripped header still carries perfectly parsable rows.
#[test]
fn framed_mode_survives_an_init_section_without_script_info() {
    let mut h = framed_harness("[Events]\nFormat: ReadOrder, Layer, Style\n");
    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,Still parsed", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(drain(&mut h), vec![emitted(S, S, "Still parsed")]);
}

/// A `codec_data` that is not valid UTF-8 is used up to the bad byte (as the C
/// does) and never reaches the output either way.
#[test]
fn framed_mode_survives_a_non_utf8_init_section() {
    init();
    let mut h = gst_check::Harness::new("rsssaparse");
    let mut init_section = b"[Script Info]\nTitle: caf\xe9\n".to_vec();
    init_section.extend_from_slice(b"[Events]\n");
    let codec_data = gst::Buffer::from_slice(init_section);
    h.set_src_caps(
        gst::Caps::builder("application/x-ssa")
            .field("codec_data", &codec_data)
            .build(),
    );

    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,Fine", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(drain(&mut h), vec![emitted(S, S, "Fine")]);
}

// ---------------------------------------------------------------------------
// Whole-file mode is still reachable
// ---------------------------------------------------------------------------

const FILE_BODY: &str = "\
[Script Info]
Title: Test

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}world{\\i0}
Dialogue: 0,0:00:03.50,0:00:05.00,Default,,0,0,0,,Second\\Nline
";

/// No `codec_data` in the caps means a standalone `.ssa`/`.ass` file body, whose
/// `Dialogue:` lines carry their own timing. That is an extension over the C
/// (which refuses the caps outright) and it must be exactly as it was.
#[test]
fn whole_file_mode_without_codec_data_still_parses() {
    for caps in ["application/x-ssa", "application/x-ass"] {
        init();
        let mut h = gst_check::Harness::new("rsssaparse");
        h.set_src_caps_str(caps);

        assert_eq!(
            h.push(gst::Buffer::from_slice(FILE_BODY.as_bytes().to_vec())),
            Ok(gst::FlowSuccess::Ok)
        );
        h.push_event(gst::event::Eos::new());

        assert_eq!(
            drain(&mut h),
            vec![
                emitted(S, 2 * S, "Hello world"),
                emitted(3500 * MS, 1500 * MS, "Second \nline"),
            ],
            "whole-file mode broke on {caps}"
        );
        assert_eq!(
            src_caps(&h)
                .structure(0)
                .unwrap()
                .get::<String>("format")
                .unwrap(),
            "pango-markup"
        );
    }
}

/// The mode follows the caps, not the content: caps *with* `codec_data` replaced
/// by caps without it puts the element back on the whole-file path.
#[test]
fn dropping_codec_data_returns_to_whole_file_mode() {
    let mut h = framed_harness(INIT_SECTION);
    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,Framed", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(drain(&mut h), vec![emitted(S, S, "Framed")]);

    h.set_src_caps_str("application/x-ass");
    assert_eq!(
        h.push(gst::Buffer::from_slice(FILE_BODY.as_bytes().to_vec())),
        Ok(gst::FlowSuccess::Ok)
    );
    h.push_event(gst::event::Eos::new());

    assert_eq!(
        drain(&mut h),
        vec![
            emitted(S, 2 * S, "Hello world"),
            emitted(3500 * MS, 1500 * MS, "Second \nline"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Stream restart
// ---------------------------------------------------------------------------

/// EOS, then a new stream on the same pad.
///
/// The state left behind by the stream that ended is stale, and one part of it is
/// worse than stale: the charset decoder is a one-shot that was finished at EOS
/// and panics (`debug_assert`) if it is fed again. STREAM_START is where that
/// state is dropped. What must survive is `framed`, which comes from the caps,
/// and the caps are not re-sent for a new stream on the same pad.
#[test]
fn framed_stream_restarts_after_eos_and_stream_start() {
    let mut h = framed_harness(INIT_SECTION);

    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,First stream", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(drain(&mut h), vec![emitted(S, S, "First stream")]);

    assert!(h.push_event(gst::event::Eos::new()));
    assert!(
        h.push_event(
            gst::event::StreamStart::builder("rsssaparse-restart")
                .group_id(gst::GroupId::next())
                .build()
        )
    );
    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );

    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,Second stream", 10 * S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(drain(&mut h), vec![emitted(10 * S, S, "Second stream")]);
}

/// The same restart on the whole-file path, which is where the finished decoder
/// actually gets fed a second time.
#[test]
fn whole_file_stream_restarts_after_eos_and_stream_start() {
    init();
    let mut h = gst_check::Harness::new("rsssaparse");
    h.set_src_caps_str("application/x-ass");

    assert_eq!(
        h.push(gst::Buffer::from_slice(FILE_BODY.as_bytes().to_vec())),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(h.push_event(gst::event::Eos::new()));
    assert_eq!(drain(&mut h).len(), 2);

    assert!(
        h.push_event(
            gst::event::StreamStart::builder("rsssaparse-restart")
                .group_id(gst::GroupId::next())
                .build()
        )
    );
    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );

    assert_eq!(
        h.push(gst::Buffer::from_slice(FILE_BODY.as_bytes().to_vec())),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(h.push_event(gst::event::Eos::new()));
    assert_eq!(
        drain(&mut h),
        vec![
            emitted(S, 2 * S, "Hello world"),
            emitted(3500 * MS, 1500 * MS, "Second \nline"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Charset
// ---------------------------------------------------------------------------

/// A framed row that is not UTF-8 decodes through the decoder's fallback instead
/// of wedging the stream, and doing so must not change how the UTF-8 rows around
/// it decode.
///
/// Each row is decoded on its own (it is a complete unit, so a truncated
/// sequence at its end is damage, not a split character), and the *decision*
/// carries over: `caf\xe9` is cp1252 here, and the row after it is still read as
/// the UTF-8 it is.
#[test]
fn framed_non_utf8_row_decodes_through_the_fallback() {
    let mut h = framed_harness(INIT_SECTION);

    let rows: &[(&[u8], u64)] = &[
        (b"0,0,Default,,0,0,0,,caf\xe9 au lait", S),
        ("1,0,Default,,0,0,0,,café au lait".as_bytes(), 2 * S),
        (b"2,0,Default,,0,0,0,,caf\xe9 noir", 3 * S),
    ];
    for (line, pts) in rows {
        assert_eq!(h.push(row(line, *pts, S)), Ok(gst::FlowSuccess::Ok));
    }

    assert_eq!(
        drain(&mut h),
        vec![
            emitted(S, S, "café au lait"),
            emitted(2 * S, S, "café au lait"),
            emitted(3 * S, S, "café noir"),
        ]
    );
}

/// A multi-byte character split across two *buffers* is damage in framed mode,
/// not something to hold for the next buffer: the two buffers are two rows.
/// Nothing may wedge, and the following row must still be intact.
#[test]
fn framed_row_ending_mid_character_does_not_hold_the_next_row() {
    let mut h = framed_harness(INIT_SECTION);

    // "caf" + the first byte of a two-byte sequence, then a well-formed row.
    assert_eq!(
        h.push(row(b"0,0,Default,,0,0,0,,caf\xc3", S, S)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        h.push(row("1,0,Default,,0,0,0,,naïve".as_bytes(), 2 * S, S)),
        Ok(gst::FlowSuccess::Ok)
    );

    let out = drain(&mut h);
    assert_eq!(out.len(), 2, "got {out:?}");
    assert_eq!(out[0].0, Some(S));
    assert!(
        out[0].2.starts_with("caf"),
        "the row's own text was lost: {:?}",
        out[0].2
    );
    assert_eq!(out[1], emitted(2 * S, S, "naïve"));
}
