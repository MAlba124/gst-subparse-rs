// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Element-level tests for `text-format=cue-ir`: both elements must push
//! plain-utf8 buffers carrying a `CueIrMeta` whose IR matches the styling the
//! default pango-markup mode would have emitted inline — and the default mode
//! must stay byte-identical to the C (no meta, markup in the text).

use std::sync::Once;

use gst::prelude::*;

use gstrssubparse::cueir::CueIrMeta;
use subparse_formats::ir::{Color, FontStyle, LinePosition, TextAlign};

const S: u64 = 1_000_000_000; // one second, in nanoseconds

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// Push `input` through `rssubparse` (with `text-format` set to `format` when
/// given) and return the pulled buffers.
fn run_subparse(input: &str, format: Option<&str>) -> Vec<gst::Buffer> {
    init();

    let mut h = gst_check::Harness::new("rssubparse");
    if let Some(format) = format {
        h.element()
            .unwrap()
            .set_property_from_str("text-format", format);
    }
    h.set_src_caps_str("application/x-subtitle");

    h.push(gst::Buffer::from_slice(input.as_bytes().to_vec()))
        .expect("push succeeded");
    h.push_event(gst::event::Eos::new());

    let mut buffers = Vec::new();
    while let Some(buffer) = h.try_pull() {
        buffers.push(buffer);
    }
    buffers
}

fn text_of(buffer: &gst::Buffer) -> String {
    let map = buffer.map_readable().unwrap();
    String::from_utf8(map.as_slice().to_vec()).unwrap()
}

const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\n<i>Hello</i> &\n\n\
                   2\n00:00:03,000 --> 00:00:04,000\nPlain\n\n";

#[test]
fn subparse_default_stays_pango_markup_without_meta() {
    let buffers = run_subparse(SRT, None);
    assert_eq!(buffers.len(), 2);
    assert_eq!(text_of(&buffers[0]), "<i>Hello</i> &amp;");
    assert!(
        buffers[0].meta::<CueIrMeta>().is_none(),
        "pango-markup mode must not attach a CueIrMeta"
    );
}

#[test]
fn subparse_cue_ir_pushes_plain_text_with_meta() {
    let buffers = run_subparse(SRT, Some("cue-ir"));
    assert_eq!(buffers.len(), 2);

    // Payload is plain text (markup gone, entities decoded)...
    assert_eq!(text_of(&buffers[0]), "Hello &");
    assert_eq!(buffers[0].pts(), Some(gst::ClockTime::from_nseconds(S)));
    assert_eq!(
        buffers[0].duration(),
        Some(gst::ClockTime::from_nseconds(S))
    );

    // ...and the styling is in the meta.
    let meta = buffers[0].meta::<CueIrMeta>().expect("meta attached");
    let ir = meta.ir();
    assert_eq!(ir.plain_text(), "Hello &");
    let spans: Vec<_> = ir.lines.iter().flat_map(|l| l.spans.iter()).collect();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].text, "Hello");
    assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
    assert_eq!(spans[1].text, " &");
    assert!(spans[1].style.is_plain());

    // The unstyled cue still gets a (trivial) meta.
    let meta = buffers[1].meta::<CueIrMeta>().expect("meta attached");
    assert_eq!(meta.ir().plain_text(), "Plain");
}

#[test]
fn subparse_cue_ir_negotiates_utf8_caps() {
    init();

    let mut h = gst_check::Harness::new("rssubparse");
    h.element()
        .unwrap()
        .set_property_from_str("text-format", "cue-ir");
    h.set_src_caps_str("application/x-subtitle");
    h.push(gst::Buffer::from_slice(SRT.as_bytes().to_vec()))
        .expect("push succeeded");

    let caps = h
        .sinkpad()
        .expect("harness sinkpad")
        .current_caps()
        .unwrap();
    let s = caps.structure(0).unwrap();
    assert_eq!(s.name(), "text/x-raw");
    assert_eq!(s.get::<&str>("format"), Ok("utf8"));
}

#[test]
fn subparse_cue_ir_carries_webvtt_settings_and_classes() {
    // WebVTT with cue settings and styled voice/class content.
    let vtt = "WEBVTT\n\n\
               00:00:01.000 --> 00:00:02.000 L:10% T:50% A:end\n\
               <v Fred><c.yellow>Hi</c></v>\n\n";
    init();

    let mut h = gst_check::Harness::new("rssubparse");
    h.element()
        .unwrap()
        .set_property_from_str("text-format", "cue-ir");
    h.set_src_caps_str("application/x-subtitle-vtt");
    h.push(gst::Buffer::from_slice(vtt.as_bytes().to_vec()))
        .expect("push succeeded");
    h.push_event(gst::event::Eos::new());

    let buffer = h.try_pull().expect("one cue");
    assert_eq!(text_of(&buffer), "Hi");

    let meta = buffer.meta::<CueIrMeta>().expect("meta attached");
    let ir = meta.ir();
    assert_eq!(ir.layout.line, Some(LinePosition::Percent(10.0)));
    assert_eq!(ir.layout.position, Some(50.0));
    assert_eq!(ir.layout.align, Some(TextAlign::End));

    let span = &ir.lines[0].spans[0];
    assert_eq!(span.text, "Hi");
    assert_eq!(span.voice.as_deref(), Some("Fred"));
    assert_eq!(span.classes, vec!["yellow"]);
    assert_eq!(span.style.foreground, Some(Color::rgb(0xff, 0xff, 0x00)));
}

/// One framed SSA dialogue row through `rsssaparse` (the container shape:
/// `codec_data` in caps, one row per timed buffer).
fn run_ssaparse_framed(row: &str, format: Option<&str>) -> gst::Buffer {
    init();

    let mut h = gst_check::Harness::new("rsssaparse");
    if let Some(format) = format {
        h.element()
            .unwrap()
            .set_property_from_str("text-format", format);
    }

    let codec_data = gst::Buffer::from_slice("[Script Info]\n".as_bytes().to_vec());
    let caps = gst::Caps::builder("application/x-ssa")
        .field("codec_data", codec_data)
        .build();
    h.set_src_caps(caps);

    let mut buffer = gst::Buffer::from_slice(row.as_bytes().to_vec());
    {
        let buf = buffer.get_mut().unwrap();
        buf.set_pts(gst::ClockTime::from_nseconds(S));
        buf.set_duration(gst::ClockTime::from_nseconds(S));
    }
    h.push(buffer).expect("push succeeded");
    h.try_pull().expect("one cue")
}

const SSA_ROW: &str = "1,0,Default,,0,0,0,,{\\i1}Hello & <world>";

#[test]
fn ssaparse_default_stays_pango_markup_without_meta() {
    let buffer = run_ssaparse_framed(SSA_ROW, None);
    // Override blocks stripped, text markup-escaped, no meta.
    assert_eq!(text_of(&buffer), "Hello &amp; &lt;world&gt;");
    assert!(buffer.meta::<CueIrMeta>().is_none());
}

#[test]
fn ssaparse_cue_ir_pushes_plain_text_with_meta() {
    let buffer = run_ssaparse_framed(SSA_ROW, Some("cue-ir"));
    assert_eq!(text_of(&buffer), "Hello & <world>");
    assert_eq!(buffer.pts(), Some(gst::ClockTime::from_nseconds(S)));

    let meta = buffer.meta::<CueIrMeta>().expect("meta attached");
    assert_eq!(meta.ir().plain_text(), "Hello & <world>");
}
