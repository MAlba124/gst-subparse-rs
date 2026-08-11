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
fn subparse_cue_ir_styles_srt_font_and_an_tags() {
    // The C deletes <font> and shows {\an8} literally; cue-ir mode styles
    // both. The default output below stays exactly the C's.
    let srt = "1\n00:00:01,000 --> 00:00:02,000\n\
               {\\an8}<font color=\"#00ff00\">green</font> plain\n\n";

    let buffers = run_subparse(srt, None);
    assert_eq!(
        text_of(&buffers[0]),
        "{\\an8}green plain",
        "pango parity: font deleted, override block shown literally"
    );

    let buffers = run_subparse(srt, Some("cue-ir"));
    assert_eq!(text_of(&buffers[0]), "green plain");
    let meta = buffers[0].meta::<CueIrMeta>().expect("meta attached");
    let ir = meta.ir();
    assert_eq!(
        ir.layout.anchor,
        Some(subparse_formats::ir::Anchor::TopCenter)
    );
    let spans: Vec<_> = ir.lines.iter().flat_map(|l| l.spans.iter()).collect();
    assert_eq!(spans[0].text, "green");
    assert_eq!(spans[0].style.foreground, Some(Color::rgb(0, 255, 0)));
    assert_eq!(spans[1].text, " plain");
    assert!(spans[1].style.is_plain());
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

#[test]
fn subparse_cue_ir_applies_webvtt_style_blocks() {
    // STYLE-block CSS must reach the IR: ::cue into the base style, selector
    // rules onto matching spans (overriding the default class colors), and
    // the cue identifier feeding ::cue(#id).
    let vtt = "WEBVTT\n\n\
               STYLE\n\
               ::cue { background: rgba(0, 0, 0, 0.8) }\n\
               ::cue(.yellow) { color: #123456 }\n\n\
               STYLE\n\
               ::cue(#greeting) { font-style: italic }\n\n\
               greeting\n\
               00:00:01.000 --> 00:00:02.000\n\
               <c.yellow>Hi</c> there\n\n\
               00:00:03.000 --> 00:00:04.000\n\
               plain\n\n";
    init();

    let mut h = gst_check::Harness::new("rssubparse");
    h.element()
        .unwrap()
        .set_property_from_str("text-format", "cue-ir");
    h.set_src_caps_str("application/x-subtitle-vtt");
    h.push(gst::Buffer::from_slice(vtt.as_bytes().to_vec()))
        .expect("push succeeded");
    h.push_event(gst::event::Eos::new());

    let buffer = h.try_pull().expect("first cue");
    assert_eq!(text_of(&buffer), "Hi there");
    let meta = buffer.meta::<CueIrMeta>().expect("meta attached");
    let ir = meta.ir();
    // ::cue rules land in the base style; #greeting matched this cue's id.
    assert_eq!(ir.base.background, Some(Color::rgba(0, 0, 0, 204)));
    assert_eq!(ir.base.font_style, Some(FontStyle::Italic));
    // The author rule overrides the default .yellow class color.
    let span = &ir.lines[0].spans[0];
    assert_eq!(span.style.foreground, Some(Color::rgb(0x12, 0x34, 0x56)));

    // The second cue has no id: base CSS still applies, #greeting does not.
    let buffer = h.try_pull().expect("second cue");
    let meta = buffer.meta::<CueIrMeta>().expect("meta attached");
    assert_eq!(meta.ir().base.background, Some(Color::rgba(0, 0, 0, 204)));
    assert_eq!(meta.ir().base.font_style, None);
}

#[test]
fn subparse_default_output_ignores_style_blocks() {
    // Parity: with STYLE blocks present, pango-markup output is unchanged
    // (the C ignores them) and no meta appears.
    let vtt = "WEBVTT\n\n\
               STYLE\n\
               ::cue(b) { color: red }\n\n\
               00:00:01.000 --> 00:00:02.000\n\
               <b>One</b>\n\n";
    init();

    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle-vtt");
    h.push(gst::Buffer::from_slice(vtt.as_bytes().to_vec()))
        .expect("push succeeded");
    h.push_event(gst::event::Eos::new());

    let buffer = h.try_pull().expect("one cue");
    assert_eq!(text_of(&buffer), "<b>One</b>");
    assert!(buffer.meta::<CueIrMeta>().is_none());
}

/// One framed SSA dialogue row through `rsssaparse` (the container shape:
/// `codec_data` in caps, one row per timed buffer). `init_section` is the
/// codec_data payload.
fn run_ssaparse_framed_with_init(
    row: &str,
    format: Option<&str>,
    init_section: &str,
) -> gst::Buffer {
    init();

    let mut h = gst_check::Harness::new("rsssaparse");
    if let Some(format) = format {
        h.element()
            .unwrap()
            .set_property_from_str("text-format", format);
    }

    let codec_data = gst::Buffer::from_slice(init_section.as_bytes().to_vec());
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

fn run_ssaparse_framed(row: &str, format: Option<&str>) -> gst::Buffer {
    run_ssaparse_framed_with_init(row, format, "[Script Info]\n")
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
    // The {\i1} the pango path strips reaches the IR as real styling now.
    assert_eq!(
        meta.ir().lines[0].spans[0].style.font_style,
        Some(FontStyle::Italic)
    );
}

#[test]
fn ssaparse_cue_ir_applies_codec_data_styles_and_overrides() {
    let init = "[Script Info]\n\
                PlayResX: 384\n\
                PlayResY: 288\n\n\
                [V4+ Styles]\n\
                Format: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, Bold, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV\n\
                Style: Default,DejaVu Sans,24,&H0000FFFF,&H00000000,-1,1,2,1,2,10,10,20\n";
    let row = "1,0,Default,,0,0,0,,{\\an8\\c&H00FF00&}Top {\\i1}green";
    let buffer = run_ssaparse_framed_with_init(row, Some("cue-ir"), init);
    assert_eq!(text_of(&buffer), "Top green");

    let meta = buffer.meta::<CueIrMeta>().expect("meta attached");
    let ir = meta.ir();
    // The style definition became the cue's base...
    assert_eq!(ir.base.font_family.as_deref(), Some("DejaVu Sans"));
    assert_eq!(ir.base.foreground, Some(Color::rgb(255, 255, 0)));
    assert_eq!(ir.base.font_weight, Some(700));
    assert_eq!(
        ir.base.font_size,
        Some(subparse_formats::ir::FontSize::FrameHeightPercent(
            24.0 / 288.0 * 100.0
        ))
    );
    // ...its alignment was overridden by {\an8}...
    assert_eq!(
        ir.layout.anchor,
        Some(subparse_formats::ir::Anchor::TopCenter)
    );
    // ...and the margins were normalised out of PlayRes space.
    let margins = ir.layout.margins.unwrap();
    assert!((margins.vertical - 20.0 / 288.0 * 100.0).abs() < 1e-4);
    // Span styling from the override tags.
    let spans: Vec<_> = ir.lines.iter().flat_map(|l| l.spans.iter()).collect();
    assert_eq!(spans[0].style.foreground, Some(Color::rgb(0, 255, 0)));
    assert_eq!(spans[0].style.font_style, None);
    assert_eq!(spans[1].style.font_style, Some(FontStyle::Italic));
}

#[test]
fn ssaparse_whole_file_cue_ir_is_styled() {
    // Whole-file mode (no codec_data): the parser collects the header
    // sections itself.
    let body = "[Script Info]\n\
                PlayResY: 288\n\n\
                [V4+ Styles]\n\
                Format: Name, Fontname, PrimaryColour\n\
                Style: Default,Arial,&H000000FF\n\n\
                [Events]\n\
                Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n\
                Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,{\\b1}Bold\\Nsecond\n";
    init();

    let mut h = gst_check::Harness::new("rsssaparse");
    h.element()
        .unwrap()
        .set_property_from_str("text-format", "cue-ir");
    h.set_src_caps_str("application/x-ssa");
    h.push(gst::Buffer::from_slice(body.as_bytes().to_vec()))
        .expect("push succeeded");
    h.push_event(gst::event::Eos::new());

    let buffer = h.try_pull().expect("one cue");
    // Clean line break (the pango path's " \n" quirk is markup-only).
    assert_eq!(text_of(&buffer), "Bold\nsecond");
    let meta = buffer.meta::<CueIrMeta>().expect("meta attached");
    let ir = meta.ir();
    assert_eq!(ir.base.foreground, Some(Color::rgb(255, 0, 0)));
    assert_eq!(ir.base.font_family.as_deref(), Some("Arial"));
    assert_eq!(ir.lines.len(), 2);
    assert_eq!(ir.lines[0].spans[0].style.font_weight, Some(700));
}

#[test]
fn ssaparse_default_output_ignores_styles() {
    // Parity: with a style-bearing init section, pango-markup output is the
    // stripped text with no meta, exactly as before.
    let init = "[Script Info]\n\n[V4+ Styles]\n\
                Format: Name, PrimaryColour\nStyle: Default,&H0000FF&\n";
    let buffer = run_ssaparse_framed_with_init(SSA_ROW, None, init);
    assert_eq!(text_of(&buffer), "Hello &amp; &lt;world&gt;");
    assert!(buffer.meta::<CueIrMeta>().is_none());
}
