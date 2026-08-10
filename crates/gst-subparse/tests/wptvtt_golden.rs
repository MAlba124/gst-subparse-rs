// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Golden-file snapshots of the WPT WebVTT corpus.
//!
//! Every `tests/corpus/wptvtt/*` file is pushed through `rssubparse` twice —
//! once in the default pango-markup mode, once with `text-format=cue-ir` —
//! and the result (timing, pango text, and the full [`CueIr`] with only its
//! non-default fields) is rendered into a compact text form and compared
//! against `tests/golden/wptvtt/<name>.txt`.
//!
//! The expectations are *generated, then reviewed*, never hand-written:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p gst-subparse --test wptvtt_golden
//! git diff crates/gst-subparse/tests/golden   # review, then commit
//! ```
//!
//! A word on what these pin: this parser deliberately implements the C
//! `subparse` **subset** of WebVTT, so the goldens record *our intended
//! behaviour* (C parity for cue text/timing, plus the cue-ir styling layer),
//! not W3C-spec conformance. WPT files exercising spec features the C subset
//! rejects (hour-less overflow forms, region blocks, ...) legitimately show
//! fewer or different cues here than a browser would produce.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Once;

use gst::prelude::*;
use gstrssubparse::cueir::CueIrMeta;
use subparse_formats::ir::{
    Anchor, Color, CueIr, FontSize, FontStyle, Layout, LineAlign, LinePosition, PositionAlign,
    RubyPosition, Span, SpanStyle, TextAlign, WritingMode,
};

const WPTVTT: &str = "tests/corpus/wptvtt";
const GOLDEN: &str = "tests/golden/wptvtt";

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Decoding must not depend on the ambient environment.
        // SAFETY: `Once` blocks every other test until this returns, so
        // nothing is decoding or reading the environment yet.
        unsafe {
            std::env::remove_var("GST_SUBTITLE_ENCODING");
        }
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// Push one file through the element and drain the buffers.
fn run_file(path: &Path, cue_ir: bool) -> Vec<gst::Buffer> {
    init();
    let bytes = std::fs::read(path).unwrap();
    let mut h = gst_check::Harness::new("rssubparse");
    if cue_ir {
        h.element()
            .unwrap()
            .set_property_from_str("text-format", "cue-ir");
    }
    h.set_src_caps_str("application/x-subtitle-vtt");
    assert_eq!(
        h.push(gst::Buffer::from_slice(bytes)),
        Ok(gst::FlowSuccess::Ok),
        "push failed for {}",
        path.display(),
    );
    h.push_event(gst::event::Eos::new());

    let mut buffers = Vec::new();
    while let Some(buf) = h.try_pull() {
        buffers.push(buf);
    }
    buffers
}

fn text_of(buffer: &gst::Buffer) -> String {
    let map = buffer.map_readable().unwrap();
    String::from_utf8(map.as_slice().to_vec()).unwrap()
}

fn fmt_time(t: Option<gst::ClockTime>) -> String {
    match t {
        Some(t) => {
            let ns = t.nseconds();
            format!("{}.{:03}", ns / 1_000_000_000, (ns / 1_000_000) % 1000)
        }
        None => "none".to_owned(),
    }
}

// -- compact IR rendering ----------------------------------------------------

fn fmt_color(c: Color) -> String {
    if c.a == 0xff {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a)
    }
}

fn fmt_size(s: FontSize) -> String {
    match s {
        FontSize::Points(p) => format!("{p}pt"),
        FontSize::Scale(f) => format!("scale({f})"),
        FontSize::FrameHeightPercent(p) => format!("{p}%fh"),
    }
}

/// `name=value` parts for every non-`None` field of a [`SpanStyle`].
fn style_parts(s: &SpanStyle) -> Vec<String> {
    let mut p = Vec::new();
    if let Some(f) = &s.font_family {
        p.push(format!("family={f:?}"));
    }
    if let Some(v) = s.font_size {
        p.push(format!("size={}", fmt_size(v)));
    }
    if let Some(v) = s.font_style {
        let v = match v {
            FontStyle::Normal => "normal",
            FontStyle::Italic => "italic",
            FontStyle::Oblique => "oblique",
        };
        p.push(format!("style={v}"));
    }
    if let Some(v) = s.font_weight {
        p.push(format!("weight={v}"));
    }
    if let Some(v) = s.underline {
        p.push(format!("underline={v}"));
    }
    if let Some(v) = s.strikethrough {
        p.push(format!("strikethrough={v}"));
    }
    if let Some(v) = s.foreground {
        p.push(format!("fg={}", fmt_color(v)));
    }
    if let Some(v) = s.background {
        p.push(format!("bg={}", fmt_color(v)));
    }
    if let Some(o) = s.outline {
        p.push(format!("outline={}/{}", fmt_color(o.color), o.width));
    }
    if let Some(sh) = s.shadow {
        p.push(format!(
            "shadow={}/{},{},{}",
            fmt_color(sh.color),
            sh.dx,
            sh.dy,
            sh.blur
        ));
    }
    if let Some(v) = s.letter_spacing {
        p.push(format!("letter-spacing={v}"));
    }
    if let Some(v) = s.baseline_shift {
        p.push(format!("baseline-shift={v}"));
    }
    if let Some((x, y)) = s.scale {
        p.push(format!("glyph-scale={x},{y}"));
    }
    if let Some(l) = &s.language {
        p.push(format!("lang={l}"));
    }
    p
}

fn layout_parts(l: &Layout) -> Vec<String> {
    let mut p = Vec::new();
    match l.writing_mode {
        WritingMode::HorizontalTb => {}
        WritingMode::VerticalRl => p.push("vertical-rl".to_owned()),
        WritingMode::VerticalLr => p.push("vertical-lr".to_owned()),
    }
    if let Some(v) = l.line {
        match v {
            LinePosition::Percent(pc) => p.push(format!("line={pc}%")),
            LinePosition::Line(n) => p.push(format!("line={n}")),
        }
    }
    if let Some(v) = l.line_align {
        let v = match v {
            LineAlign::Start => "start",
            LineAlign::Center => "center",
            LineAlign::End => "end",
        };
        p.push(format!("line-align={v}"));
    }
    if let Some(v) = l.position {
        p.push(format!("position={v}"));
    }
    if let Some(v) = l.position_align {
        let v = match v {
            PositionAlign::Auto => "auto",
            PositionAlign::LineLeft => "line-left",
            PositionAlign::Center => "center",
            PositionAlign::LineRight => "line-right",
        };
        p.push(format!("position-align={v}"));
    }
    if let Some(v) = l.size {
        p.push(format!("size={v}"));
    }
    if let Some(v) = l.align {
        let v = match v {
            TextAlign::Start => "start",
            TextAlign::Center => "center",
            TextAlign::End => "end",
            TextAlign::Left => "left",
            TextAlign::Right => "right",
        };
        p.push(format!("align={v}"));
    }
    if let Some(v) = l.anchor {
        p.push(format!("anchor={v:?}"));
        // Anchor's Debug form is stable and readable (e.g. BottomCenter).
        let _: Anchor = v;
    }
    if let Some((x, y)) = l.origin {
        p.push(format!("origin={x},{y}"));
    }
    if let Some(m) = l.margins {
        p.push(format!("margins={},{},{}", m.left, m.right, m.vertical));
    }
    if let Some(c) = l.background {
        p.push(format!("bg={}", fmt_color(c)));
    }
    p
}

fn push_span(out: &mut String, span: &Span) {
    write!(out, "    span {:?}", span.text).unwrap();
    if let Some(v) = &span.voice {
        write!(out, " voice={v:?}").unwrap();
    }
    if !span.classes.is_empty() {
        write!(out, " classes={}", span.classes.join(".")).unwrap();
    }
    if let Some(r) = &span.ruby {
        let pos = match r.position {
            RubyPosition::Over => "over",
            RubyPosition::Under => "under",
        };
        write!(out, " ruby={:?}/{pos}", r.text).unwrap();
    }
    if let Some(ns) = span.reveal_ns {
        write!(out, " reveal={ns}ns").unwrap();
    }
    let style = style_parts(&span.style);
    if !style.is_empty() {
        write!(out, " [{}]", style.join(" ")).unwrap();
    }
    out.push('\n');
}

fn render_ir(out: &mut String, ir: &CueIr) {
    let layout = layout_parts(&ir.layout);
    if !layout.is_empty() {
        writeln!(out, "  layout: {}", layout.join(" ")).unwrap();
    }
    let base = style_parts(&ir.base);
    if !base.is_empty() {
        writeln!(out, "  base: {}", base.join(" ")).unwrap();
    }
    for line in &ir.lines {
        writeln!(out, "  line:").unwrap();
        for span in &line.spans {
            push_span(out, span);
        }
    }
}

/// The whole snapshot for one corpus file.
fn snapshot(path: &Path) -> String {
    let pango = run_file(path, false);
    let ir = run_file(path, true);
    assert_eq!(
        pango.len(),
        ir.len(),
        "pango-markup and cue-ir modes disagree on the cue count for {}",
        path.display()
    );

    if pango.is_empty() {
        return "(no cues)\n".to_owned();
    }
    let mut out = String::new();
    for (i, (pbuf, ibuf)) in pango.iter().zip(&ir).enumerate() {
        assert_eq!(pbuf.pts(), ibuf.pts());
        writeln!(
            out,
            "cue {i}: {} +{}",
            fmt_time(pbuf.pts()),
            fmt_time(pbuf.duration())
        )
        .unwrap();
        writeln!(out, "  pango: {:?}", text_of(pbuf)).unwrap();
        let meta = ibuf.meta::<CueIrMeta>().expect("cue-ir buffers carry meta");
        // The buffer payload is by construction the IR's own plain text.
        assert_eq!(text_of(ibuf), meta.ir().plain_text());
        writeln!(out, "  text: {:?}", text_of(ibuf)).unwrap();
        render_ir(&mut out, meta.ir());
    }
    out
}

#[test]
fn wptvtt_matches_goldens() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(WPTVTT)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "vtt" || e == "webvtt"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus files under {WPTVTT}");

    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    if update {
        std::fs::create_dir_all(GOLDEN).unwrap();
    }

    let mut failures = Vec::new();
    for path in &files {
        let got = snapshot(path);
        let golden_path = Path::new(GOLDEN).join(format!(
            "{}.txt",
            path.file_name().unwrap().to_string_lossy()
        ));
        if update {
            std::fs::write(&golden_path, &got).unwrap();
            continue;
        }
        match std::fs::read_to_string(&golden_path) {
            Ok(want) if want == got => {}
            Ok(want) => failures.push(format!(
                "{}: snapshot changed.\n--- golden\n{want}\n--- current\n{got}",
                path.display()
            )),
            Err(_) => failures.push(format!(
                "{}: no golden file at {} (run with UPDATE_GOLDEN=1 to generate)",
                path.display(),
                golden_path.display()
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} golden mismatches:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
