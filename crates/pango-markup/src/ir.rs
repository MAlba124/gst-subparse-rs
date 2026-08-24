// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Mapping parsed markup onto `subparse_formats::ir::CueIr`.
//!
//! The attribute iterator walks the ranges of constant styling; each range
//! becomes one styled span per line it touches. Font-ish attributes resolve
//! with `pango_attr_iterator_get_font` precedence (innermost wins, a
//! `font_desc` fills what nothing claimed), the rest through the iterator's
//! deduplicated attr list. Attributes `CueIr` cannot express (overline,
//! gravity, show, text-transform, font features, line height, word/sentence
//! segmentation, enum baseline shifts) are dropped.

use subparse_formats::ir::{Color as IrColor, CueIr, FontSize, FontStyle, Line, Span, SpanStyle};

use crate::attr::{AttrKind, SCALE};
use crate::fontdesc::{self, FontDescription};
use crate::markup::{Parsed, parse_markup_tolerant};

/// The factor sub/superscript glyphs shrink by when no explicit size is in
/// play (pango takes it from font metrics, ~0.6).
const FONT_SCALE_FACTOR: f32 = 0.62;

fn style_for_range(attrs: &[&crate::attr::Attr]) -> SpanStyle {
    let mut style = SpanStyle::default();

    // Font resolution, get_font precedence: walk top of stack down, first
    // claim per field wins.
    let mut desc = FontDescription {
        mask: 0,
        ..FontDescription::default()
    };
    let mut mask = 0u32;
    let mut scale: Option<f64> = None;
    let mut font_scale_shrink = false;
    for attr in attrs.iter().rev() {
        match &attr.kind {
            AttrKind::FontDesc(d) => {
                let new_mask = d.mask & !mask;
                mask |= new_mask;
                desc.unset_fields(new_mask);
                let mut masked = d.clone();
                masked.mask = new_mask;
                desc.merge(&masked, false);
            }
            AttrKind::Family(f) => {
                if mask & fontdesc::MASK_FAMILY == 0 {
                    mask |= fontdesc::MASK_FAMILY;
                    desc.family = Some(f.clone());
                    desc.mask |= fontdesc::MASK_FAMILY;
                }
            }
            AttrKind::Style(v) => {
                if mask & fontdesc::MASK_STYLE == 0 {
                    mask |= fontdesc::MASK_STYLE;
                    desc.style = *v;
                    desc.mask |= fontdesc::MASK_STYLE;
                }
            }
            AttrKind::Weight(v) => {
                if mask & fontdesc::MASK_WEIGHT == 0 {
                    mask |= fontdesc::MASK_WEIGHT;
                    desc.weight = *v;
                    desc.mask |= fontdesc::MASK_WEIGHT;
                }
            }
            AttrKind::Size(v) => {
                if mask & fontdesc::MASK_SIZE == 0 {
                    mask |= fontdesc::MASK_SIZE;
                    desc.set_size(*v);
                }
            }
            AttrKind::Scale(v) => {
                if scale.is_none() {
                    scale = Some(*v);
                }
            }
            AttrKind::FontScale(v) if *v == 1 || *v == 2 => font_scale_shrink = true,
            _ => {}
        }
    }

    if desc.mask & fontdesc::MASK_FAMILY != 0 {
        style.font_family = desc.family.clone();
    }
    if desc.mask & fontdesc::MASK_STYLE != 0 {
        style.font_style = Some(match desc.style {
            1 => FontStyle::Oblique,
            2 => FontStyle::Italic,
            _ => FontStyle::Normal,
        });
    }
    if desc.mask & fontdesc::MASK_WEIGHT != 0 {
        style.font_weight = Some(desc.weight.clamp(1, 1000) as u16);
    }
    if desc.mask & fontdesc::MASK_SIZE != 0 && desc.size > 0 {
        let mut pts = desc.size as f32 / SCALE as f32;
        if let Some(s) = scale {
            pts *= s as f32;
        }
        style.font_size = Some(FontSize::Points(pts));
    } else if let Some(s) = scale {
        style.font_size = Some(FontSize::Scale(s as f32));
    } else if font_scale_shrink {
        style.font_size = Some(FontSize::Scale(FONT_SCALE_FACTOR));
    }

    // Non-font attributes, one per type with pango's priority.
    let mut fg_alpha: Option<u16> = None;
    let mut bg_alpha: Option<u16> = None;
    let mut seen = Vec::new();
    for attr in attrs.iter().rev() {
        let ty = attr.kind.ty();
        if seen.contains(&ty) {
            continue;
        }
        seen.push(ty);
        match &attr.kind {
            AttrKind::Underline(v) => style.underline = Some(*v != 0),
            AttrKind::Strikethrough(v) => style.strikethrough = Some(*v),
            AttrKind::Foreground(c) => {
                let (r, g, b) = c.to_rgb8();
                style.foreground = Some(IrColor::rgb(r, g, b));
            }
            AttrKind::Background(c) => {
                let (r, g, b) = c.to_rgb8();
                style.background = Some(IrColor::rgb(r, g, b));
            }
            AttrKind::ForegroundAlpha(a) => fg_alpha = Some(*a),
            AttrKind::BackgroundAlpha(a) => bg_alpha = Some(*a),
            AttrKind::Rise(v) => {
                style.baseline_shift = Some(*v as f32 / SCALE as f32);
            }
            AttrKind::BaselineShift(v) if v.abs() > 1024 => {
                style.baseline_shift = Some(*v as f32 / SCALE as f32);
            }
            AttrKind::LetterSpacing(v) => {
                style.letter_spacing = Some(*v as f32 / SCALE as f32);
            }
            AttrKind::Language(l) => style.language = Some(l.clone()),
            _ => {}
        }
    }
    if let (Some(a), Some(c)) = (fg_alpha, &mut style.foreground) {
        c.a = (a >> 8) as u8;
    }
    if let (Some(a), Some(c)) = (bg_alpha, &mut style.background) {
        c.a = (a >> 8) as u8;
    }

    style
}

/// Turn a parse result into a [`CueIr`].
pub fn to_cue_ir(parsed: &Parsed) -> CueIr {
    let text = parsed.text.as_str();
    let mut lines: Vec<Line> = vec![Line::default()];

    let mut it = parsed.attrs.iter_ranges();
    loop {
        let start = (it.start as usize).min(text.len());
        let end = (it.end as usize).min(text.len());
        if start < end {
            let style = style_for_range(&it.stack_attrs());
            for (i, part) in text[start..end].split('\n').enumerate() {
                if i > 0 {
                    lines.push(Line::default());
                }
                if !part.is_empty() {
                    lines.last_mut().unwrap().spans.push(Span {
                        text: part.to_string(),
                        style: style.clone(),
                        ..Span::default()
                    });
                }
            }
        }
        if end >= text.len() || !it.next_range() {
            break;
        }
    }

    CueIr {
        lines,
        ..CueIr::default()
    }
}

/// Tolerantly parse pango markup straight into a [`CueIr`], the one-call
/// path for a renderer fed `text/x-raw, format=pango-markup` buffers.
pub fn markup_to_cue_ir(markup: &str) -> CueIr {
    to_cue_ir(&parse_markup_tolerant(markup))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styled_spans_and_lines() {
        let ir = markup_to_cue_ir("<i>a\nb</i> c");
        assert_eq!(ir.lines.len(), 2);
        assert_eq!(ir.lines[0].spans[0].text, "a");
        assert_eq!(
            ir.lines[0].spans[0].style.font_style,
            Some(FontStyle::Italic)
        );
        assert_eq!(ir.lines[1].spans[0].text, "b");
        assert_eq!(ir.lines[1].spans[1].text, " c");
        assert!(ir.lines[1].spans[1].style.is_plain());
    }

    #[test]
    fn font_and_color_resolution() {
        let ir = markup_to_cue_ir(
            "<span font=\"Sans Bold 12\" foreground=\"#ff0000\" alpha=\"50%\">x</span>",
        );
        let s = &ir.lines[0].spans[0].style;
        assert_eq!(s.font_family.as_deref(), Some("Sans"));
        assert_eq!(s.font_weight, Some(700));
        assert_eq!(s.font_size, Some(FontSize::Points(12.0)));
        // 50% is 50 * 0xffff / 100 = 0x7fff, top byte 0x7f.
        assert_eq!(s.foreground, Some(IrColor::rgba(0xff, 0, 0, 0x7f)));
    }

    #[test]
    fn inner_attrs_win() {
        let ir = markup_to_cue_ir(
            "<span weight=\"bold\" font_family=\"A\"><span weight=\"100\">x</span></span>",
        );
        let s = &ir.lines[0].spans[0].style;
        assert_eq!(s.font_weight, Some(100));
        assert_eq!(s.font_family.as_deref(), Some("A"));
    }

    #[test]
    fn scale_without_size_stays_relative() {
        let ir = markup_to_cue_ir("<big>x</big>");
        assert_eq!(
            ir.lines[0].spans[0].style.font_size,
            Some(FontSize::Scale(1.2))
        );
    }

    #[test]
    fn plain_text_passthrough() {
        let ir = markup_to_cue_ir("just text\n");
        assert_eq!(ir.lines.len(), 2);
        assert_eq!(ir.plain_text(), "just text\n");
    }
}
