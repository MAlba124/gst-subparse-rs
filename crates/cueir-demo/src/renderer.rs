// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The reusable piece: turn one [`CueIr`] into pixels with parley + vello_cpu.
//!
//! There is deliberately no GStreamer in this module — it depends only on
//! `subparse_formats::ir` for the cue description, parley for text layout and
//! vello_cpu for rasterizing, so the fcast receiver can lift it as-is and
//! point it at its own render target instead of the demo's PNG pixmap.
//!
//! What is mapped today: per-span font family/size/weight/style, underline,
//! strikethrough, foreground color, span background boxes, letter spacing,
//! and the cue-level layout (`position`/`line`/`size`/`align`). Not mapped
//! yet (the IR carries them, the receiver's renderer will want them):
//! outline, shadow, baseline shift (ruby annotations, sub/superscript),
//! glyph scale, per-span language, vertical writing modes, karaoke
//! `reveal_ns`.

use std::ops::Range;

use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamilyName, FontWeight, GenericFamily, GlyphRun,
    LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty,
};
use peniko::Color;
use subparse_formats::ir::{self, CueIr};
use vello_cpu::{
    Glyph, RenderContext,
    kurbo::{Affine, Rect, Vec2},
};

/// Parley brush for cue text: the glyph fill color plus an optional
/// background box painted behind the span (WebVTT `bg_*` classes, SSA opaque
/// boxes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CueBrush {
    pub fg: Color,
    pub bg: Option<Color>,
}

impl Default for CueBrush {
    fn default() -> Self {
        // Subtitle house style: white text, no box.
        Self {
            fg: Color::WHITE,
            bg: None,
        }
    }
}

/// Cue renderer with reusable font/layout contexts (they cache font data, so
/// keep one instance alive across cues).
pub struct CueRenderer {
    font_cx: FontContext,
    layout_cx: LayoutContext<CueBrush>,
}

impl Default for CueRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl CueRenderer {
    pub fn new() -> Self {
        Self {
            font_cx: FontContext::new(),
            layout_cx: LayoutContext::new(),
        }
    }

    /// Lay out `ir` and draw it into `rc`, positioned for a `width` x
    /// `height` video frame. The caller owns the render target (and any
    /// frame content already in it); this only adds the cue on top.
    pub fn draw(&mut self, ir: &CueIr, rc: &mut RenderContext, width: u16, height: u16) {
        // House style: ~5.3% of the frame height (≈38 px on 720p).
        let base_px = height as f32 * 0.053;
        // Cue box width from the `size` setting, default 90% of the frame.
        let size_pct = ir.layout.size.unwrap_or(90.0).clamp(1.0, 100.0);
        let max_advance = width as f32 * size_pct / 100.0;

        // Flatten the cue into one string plus per-span byte ranges, which is
        // the shape parley's ranged builder styles with.
        let mut text = String::new();
        let mut spans: Vec<(Range<usize>, &ir::Span)> = Vec::new();
        for (i, line) in ir.lines.iter().enumerate() {
            if i != 0 {
                text.push('\n');
            }
            for span in &line.spans {
                let start = text.len();
                text.push_str(&span.text);
                spans.push((start..text.len(), span));
            }
        }
        if text.trim().is_empty() {
            return;
        }

        let mut b = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, &text, 1.0, true);

        // Cue-wide defaults, taking the IR's base style where it sets one.
        let base = &ir.base;
        b.push_default(StyleProperty::Brush(CueBrush {
            fg: base.foreground.map(color).unwrap_or(Color::WHITE),
            bg: base.background.map(color),
        }));
        b.push_default(GenericFamily::SansSerif);
        b.push_default(LineHeight::FontSizeRelative(1.2));
        b.push_default(StyleProperty::FontSize(font_px(base.font_size, base_px)));
        if let Some(family) = base.font_family.as_deref() {
            b.push_default(FontFamilyName::Named(family.into()));
        }
        if let Some(style) = base.font_style {
            b.push_default(font_style(style));
        }
        if let Some(weight) = base.font_weight {
            b.push_default(FontWeight::new(weight as f32));
        }
        if base.underline == Some(true) {
            b.push_default(StyleProperty::Underline(true));
        }
        if base.strikethrough == Some(true) {
            b.push_default(StyleProperty::Strikethrough(true));
        }

        for (range, span) in &spans {
            let s = &span.style;
            if s.foreground.is_some() || s.background.is_some() {
                b.push(
                    StyleProperty::Brush(CueBrush {
                        fg: s
                            .foreground
                            .or(base.foreground)
                            .map(color)
                            .unwrap_or(Color::WHITE),
                        bg: s.background.or(base.background).map(color),
                    }),
                    range.clone(),
                );
            }
            if let Some(style) = s.font_style {
                b.push(font_style(style), range.clone());
            }
            if let Some(weight) = s.font_weight {
                b.push(FontWeight::new(weight as f32), range.clone());
            }
            if let Some(underline) = s.underline {
                b.push(StyleProperty::Underline(underline), range.clone());
            }
            if let Some(strikethrough) = s.strikethrough {
                b.push(StyleProperty::Strikethrough(strikethrough), range.clone());
            }
            if s.font_size.is_some() {
                b.push(
                    StyleProperty::FontSize(font_px(s.font_size, base_px)),
                    range.clone(),
                );
            }
            if let Some(family) = s.font_family.as_deref() {
                b.push(FontFamilyName::Named(family.into()), range.clone());
            }
            if let Some(spacing) = s.letter_spacing {
                b.push(
                    StyleProperty::LetterSpacing(pt_to_px(spacing)),
                    range.clone(),
                );
            }
        }

        let mut layout = b.build(&text);
        layout.break_all_lines(Some(max_advance));
        layout.align(alignment(ir.layout.align), AlignmentOptions::default());

        // Alignment offsets the lines *within* the `max_advance` container,
        // so the frame placement works with the container while the contrast
        // box hugs the aligned ink extents.
        let lh = layout.height();
        let (mut ink_x0, mut ink_x1) = (f32::MAX, 0.0f32);
        for line in layout.lines() {
            let m = line.metrics();
            ink_x0 = ink_x0.min(m.offset);
            ink_x1 = ink_x1.max(m.offset + m.advance - m.trailing_whitespace);
        }
        if ink_x0 >= ink_x1 {
            return;
        }

        // Position the cue box on the frame. `position` and `line` are
        // percentages; the default is the customary bottom-center strip.
        let x = match ir.layout.position {
            Some(p) => width as f32 * p / 100.0 - max_advance / 2.0,
            None => (width as f32 - max_advance) / 2.0,
        }
        .clamp(0.0, (width as f32 - max_advance).max(0.0));
        let y = match ir.layout.line {
            Some(ir::LinePosition::Percent(p)) => height as f32 * p / 100.0,
            // Line numbers and the default both land on the bottom strip.
            _ => height as f32 * 0.94 - lh,
        }
        .clamp(0.0, (height as f32 - lh).max(0.0));

        rc.set_transform(Affine::translate(Vec2::new(x as f64, y as f64)));

        // Contrast box behind the whole cue (the cue-level background when
        // the IR sets one, a translucent scrim otherwise), then per-span
        // boxes, then glyphs.
        let pad = (base_px * 0.35) as f64;
        let scrim = ir
            .layout
            .background
            .map(color)
            .unwrap_or(Color::from_rgba8(0, 0, 0, 144));
        rc.set_paint(scrim);
        rc.fill_rect(&Rect::new(
            ink_x0 as f64 - pad,
            -pad,
            ink_x1 as f64 + pad,
            lh as f64 + pad,
        ));

        for line in layout.lines() {
            let m = line.metrics();
            let (top, bottom) = (
                (m.baseline - m.ascent) as f64,
                (m.baseline + m.descent) as f64,
            );
            // Backgrounds for the whole line first, so a box never paints
            // over a neighbouring run's glyphs.
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                if let Some(bg) = glyph_run.style().brush.bg {
                    rc.set_paint(bg);
                    rc.fill_rect(&Rect::new(
                        glyph_run.offset() as f64,
                        top,
                        (glyph_run.offset() + glyph_run.advance()) as f64,
                        bottom,
                    ));
                }
            }
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                draw_glyph_run(rc, &glyph_run);
            }
        }

        rc.set_transform(Affine::IDENTITY);
    }
}

/// Fill one glyph run and its underline/strikethrough decorations
/// (the vello_cpu_render example's per-run body).
fn draw_glyph_run(rc: &mut RenderContext, glyph_run: &GlyphRun<'_, CueBrush>) {
    let run = glyph_run.run();
    rc.set_paint(glyph_run.style().brush.fg);
    rc.glyph_run(run.font())
        .font_size(run.font_size())
        .hint(true)
        .normalized_coords(run.normalized_coords())
        .fill_glyphs(glyph_run.positioned_glyphs().map(|g| Glyph {
            id: g.id,
            x: g.x,
            y: g.y,
        }));

    let style = glyph_run.style();
    if let Some(decoration) = &style.underline {
        let offset = decoration.offset.unwrap_or(run.metrics().underline_offset);
        let size = decoration.size.unwrap_or(run.metrics().underline_size);
        draw_decoration(rc, glyph_run, decoration.brush.fg, offset, size);
    }
    if let Some(decoration) = &style.strikethrough {
        let offset = decoration
            .offset
            .unwrap_or(run.metrics().strikethrough_offset);
        let size = decoration.size.unwrap_or(run.metrics().strikethrough_size);
        draw_decoration(rc, glyph_run, decoration.brush.fg, offset, size);
    }
}

/// A decoration (underline/strikethrough) is a filled rectangle across the
/// run's advance.
fn draw_decoration(
    rc: &mut RenderContext,
    glyph_run: &GlyphRun<'_, CueBrush>,
    color: Color,
    offset: f32,
    size: f32,
) {
    rc.set_paint(color);
    let y = (glyph_run.baseline() - offset) as f64;
    let x = glyph_run.offset() as f64;
    rc.fill_rect(&Rect::new(
        x,
        y,
        x + glyph_run.advance() as f64,
        y + size as f64,
    ));
}

fn color(c: ir::Color) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn pt_to_px(pt: f32) -> f32 {
    pt * 96.0 / 72.0
}

/// IR font size -> pixels. Absolute point sizes get the CSS pt->px factor;
/// scales are relative to the frame's base cue size.
fn font_px(size: Option<ir::FontSize>, base_px: f32) -> f32 {
    match size {
        Some(ir::FontSize::Points(pt)) => pt_to_px(pt),
        Some(ir::FontSize::Scale(s)) => base_px * s,
        None => base_px,
    }
}

fn font_style(style: ir::FontStyle) -> parley::FontStyle {
    match style {
        ir::FontStyle::Normal => parley::FontStyle::Normal,
        ir::FontStyle::Italic => parley::FontStyle::Italic,
        ir::FontStyle::Oblique => parley::FontStyle::Oblique(None),
    }
}

fn alignment(align: Option<ir::TextAlign>) -> Alignment {
    match align {
        // Subtitles center by default.
        None | Some(ir::TextAlign::Center) => Alignment::Center,
        Some(ir::TextAlign::Start) => Alignment::Start,
        Some(ir::TextAlign::End) => Alignment::End,
        Some(ir::TextAlign::Left) => Alignment::Left,
        Some(ir::TextAlign::Right) => Alignment::Right,
    }
}
