// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! End-to-end demo of the cue-IR path:
//!
//! ```text
//! appsrc ! rssubparse text-format=cue-ir ! appsink
//! ```
//!
//! Each buffer out of `rssubparse` carries a `CueIrMeta`; the demo reads the
//! POD `CueIr` off it and renders the styled cue with parley (layout) +
//! vello_cpu (raster) onto a synthetic 720p "video frame", one PNG per cue.
//! [`renderer`] is the part meant to be lifted into the fcast receiver.
//!
//! Usage:
//!
//! ```text
//! cargo run --release [subtitle-file] [output-dir]
//! ```
//!
//! With no arguments it renders an embedded WebVTT sample into
//! `cueir-demo-output/`. Any format `rssubparse` autodetects works (SRT,
//! WebVTT, SAMI, MicroDVD, ...).

mod renderer;

// The cue engine written for the fcast receiver (a drop-in replacement for
// fcast-video/src/cue.rs, developed and tested here — see its module docs).
// Nothing in the demo binary calls it; it exists to compile and be tested.
#[allow(dead_code)]
mod fcast_cue;

use gst::prelude::*;
use peniko::Color;
use vello_cpu::{Pixmap, RenderContext, kurbo::Rect};

use gstrssubparse::cueir::CueIrMeta;
use renderer::CueRenderer;

const FRAME_W: u16 = 1280;
const FRAME_H: u16 = 720;

/// Exercises italics/bold/underline, voice, color classes, ruby, and cue
/// settings (line, position, size, align). WebVTT `bg_*` background classes
/// are not in here: the markup pass mirrors the C, whose attribute whitelist
/// drops the `_`, so they don't reach the IR yet — span backgrounds do work
/// (e.g. QTtext `bgcolor`), see the renderer's `CueBrush::bg`.
const DEMO_VTT: &str = "\
WEBVTT

00:00.000 --> 00:02.000 L:8% A:start
<v Narrator><i>Once upon a time…</i>

00:02.000 --> 00:04.000
Plain, <b>bold</b>, <i>italic</i>, <u>underline</u>,
and some <c.yellow>yellow</c> text

00:04.000 --> 00:06.000 T:50% S:60%
<c.cyan>cyan</c> and <ruby>ruby<rt>annotated</rt></ruby> text
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    gst::init()?;
    gstrssubparse::plugin_register_static()?;

    let mut args = std::env::args().skip(1);
    let input = args.next();
    let out_dir = std::path::PathBuf::from(
        args.next()
            .unwrap_or_else(|| "cueir-demo-output".to_owned()),
    );
    std::fs::create_dir_all(&out_dir)?;

    let bytes = match &input {
        Some(path) => std::fs::read(path)?,
        None => DEMO_VTT.as_bytes().to_vec(),
    };

    // SSA/ASS is the separate ssaparse element (as upstream); everything else
    // goes to rssubparse, which detects the actual format from the content —
    // the caps only get the stream linked.
    let is_ssa = input.as_deref().is_some_and(|p| {
        std::path::Path::new(p)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ass") || e.eq_ignore_ascii_case("ssa"))
    });
    let launch = if is_ssa {
        "appsrc name=src caps=application/x-ssa \
         ! rsssaparse text-format=cue-ir \
         ! appsink name=sink sync=false"
    } else {
        "appsrc name=src caps=application/x-subtitle \
         ! rssubparse text-format=cue-ir \
         ! appsink name=sink sync=false"
    };
    let pipeline = gst::parse::launch(launch)?
        .downcast::<gst::Pipeline>()
        .expect("a launch line is a pipeline");
    let src = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    pipeline.set_state(gst::State::Playing)?;
    src.push_buffer(gst::Buffer::from_slice(bytes))
        .map_err(|e| format!("pushing the subtitle data failed: {e:?}"))?;
    src.end_of_stream()
        .map_err(|e| format!("finishing the stream failed: {e:?}"))?;

    let mut renderer = CueRenderer::new();
    let mut index = 0usize;
    // pull_sample() errors once the stream is at EOS, ending the loop.
    while let Ok(sample) = sink.pull_sample() {
        let Some(buffer) = sample.buffer() else {
            continue;
        };

        let Some(meta) = buffer.meta::<CueIrMeta>() else {
            eprintln!("cue {index}: buffer without CueIrMeta, skipping");
            continue;
        };
        let ir = meta.ir();

        // A synthetic dark "video frame" with the cue composited on top. In
        // the receiver, the render target is the video overlay surface and
        // this block is the per-frame draw.
        let mut rc = RenderContext::new(FRAME_W, FRAME_H);
        rc.set_paint(Color::from_rgb8(28, 30, 34));
        rc.fill_rect(&Rect::new(0.0, 0.0, FRAME_W as f64, FRAME_H as f64));
        renderer.draw(ir, &mut rc, FRAME_W, FRAME_H);
        rc.flush();
        let mut pixmap = Pixmap::new(FRAME_W, FRAME_H);
        rc.render_to_pixmap(&mut pixmap);

        let pts_ms = buffer.pts().map(|t| t.mseconds()).unwrap_or(0);
        let end_ms = pts_ms + buffer.duration().map(|d| d.mseconds()).unwrap_or(0);
        let path = out_dir.join(format!("cue-{index:02}-{pts_ms:05}ms.png"));
        std::fs::write(&path, pixmap.into_png()?)?;
        println!(
            "cue {index:02} [{pts_ms:>6} ms -> {end_ms:>6} ms] {:>2} line(s) -> {}",
            ir.lines.len(),
            path.display()
        );
        index += 1;
    }

    pipeline.set_state(gst::State::Null)?;
    println!("rendered {index} cue(s) into {}/", out_dir.display());
    Ok(())
}
