// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The markup semantics, a port of pango-markup.c.
//!
//! Tags push [`OpenTag`]s carrying pending attributes; closing a tag stamps
//! the byte range and moves them to the apply list; `big`/`small`/relative
//! sizes accumulate a scale level that materializes as one `scale` or `size`
//! attribute per tag on close. Attribute vocabulary, value parsing and error
//! conditions follow `span_parse_func` and friends.

use std::collections::VecDeque;

use crate::attr::{Attr, AttrKind, AttrList, SCALE, nicks};
use crate::color;
use crate::fontdesc::{self, FontDescription};
use crate::xml::{self, Events, XmlError};

/// The result of a parse: flattened text, attributes, first accelerator.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub text: String,
    pub attrs: AttrList,
    pub accel_char: Option<char>,
}

/// `PANGO_UNDERLINE_LOW`, what accel markers produce.
const UNDERLINE_LOW: i32 = 3;
/// `PANGO_UNDERLINE_SINGLE`.
const UNDERLINE_SINGLE: i32 = 1;
/// `PANGO_STYLE_ITALIC`.
const STYLE_ITALIC: i32 = 2;
/// `PANGO_WEIGHT_BOLD`.
const WEIGHT_BOLD: i32 = 700;
/// `PANGO_FONT_SCALE_SUPERSCRIPT` / `SUBSCRIPT`, `PANGO_BASELINE_SHIFT_*`.
const SCALE_SUPERSCRIPT: i32 = 1;
const SCALE_SUBSCRIPT: i32 = 2;

struct OpenTag {
    attrs: Vec<AttrKind>,
    start_index: u32,
    scale_level: i32,
    scale_level_delta: i32,
    base_scale_factor: f64,
    base_font_size: i32,
    has_base_font_size: bool,
}

impl OpenTag {
    fn set_absolute_font_size(&mut self, font_size: i32) {
        self.base_font_size = font_size;
        self.has_base_font_size = true;
        self.scale_level = 0;
        self.scale_level_delta = 0;
    }

    fn set_absolute_font_scale(&mut self, scale: f64) {
        self.base_scale_factor = scale;
        self.has_base_font_size = false;
        self.scale_level = 0;
        self.scale_level_delta = 0;
    }
}

/// The 1.2 CSS factor between size levels.
fn scale_factor(scale_level: i32, base: f64) -> f64 {
    let mut factor = base;
    let mut i = 0;
    while i < scale_level {
        factor *= 1.2;
        i += 1;
    }
    let mut i = scale_level;
    while i < 0 {
        factor /= 1.2;
        i += 1;
    }
    factor
}

struct MarkupData {
    attr_list: AttrList,
    text: String,
    index: u32,
    tag_stack: Vec<OpenTag>,
    to_apply: VecDeque<Attr>,
    accel_marker: Option<char>,
    accel_char: Option<char>,
    strict: bool,
}

impl MarkupData {
    fn new(accel_marker: Option<char>, strict: bool) -> MarkupData {
        MarkupData {
            attr_list: AttrList::new(),
            text: String::new(),
            index: 0,
            tag_stack: Vec::new(),
            to_apply: VecDeque::new(),
            accel_marker,
            accel_char: None,
            strict,
        }
    }

    fn open_tag(&mut self) -> usize {
        let (bsf, bfs, hbfs, lvl) = match self.tag_stack.last() {
            None => (1.0, 0, false, 0),
            Some(p) => (
                p.base_scale_factor,
                p.base_font_size,
                p.has_base_font_size,
                p.scale_level,
            ),
        };
        self.tag_stack.push(OpenTag {
            attrs: Vec::new(),
            start_index: self.index,
            scale_level: lvl,
            scale_level_delta: 0,
            base_scale_factor: bsf,
            base_font_size: bfs,
            has_base_font_size: hbfs,
        });
        self.tag_stack.len() - 1
    }

    fn close_tag(&mut self) {
        let Some(mut ot) = self.tag_stack.pop() else {
            return;
        };
        // Innermost tags end up first in to_apply, outermost in front of
        // them, so the final insertion order applies outer tags first.
        for kind in ot.attrs.drain(..).rev() {
            self.to_apply.push_front(Attr {
                start: ot.start_index,
                end: self.index,
                kind,
            });
        }
        if ot.scale_level_delta != 0 {
            let kind = if ot.has_base_font_size {
                let size = scale_factor(ot.scale_level, 1.0) * ot.base_font_size as f64;
                AttrKind::Size(size as i32)
            } else {
                AttrKind::Scale(scale_factor(ot.scale_level, ot.base_scale_factor))
            };
            self.to_apply.push_front(Attr {
                start: ot.start_index,
                end: self.index,
                kind,
            });
        }
    }

    fn add_attribute(&mut self, tag_idx: usize, kind: AttrKind) {
        self.tag_stack[tag_idx].attrs.push(kind);
    }

    fn push_text(&mut self, text: &str) {
        self.text.push_str(text);
        self.index += text.len() as u32;
    }

    fn finish(mut self) -> Parsed {
        for attr in std::mem::take(&mut self.to_apply) {
            self.attr_list.insert(attr);
        }
        Parsed {
            text: self.text,
            attrs: self.attr_list,
            accel_char: self.accel_char,
        }
    }

    /// The accel path of `text_handler`.
    fn text_with_accel(&mut self, text: &str, marker: char) {
        let mut range_end: Option<usize> = None;
        let mut range_start = 0usize;
        for (pos, c) in text.char_indices() {
            if let Some(re) = range_end {
                if c == marker {
                    // Escaped marker: emit up to and including the first
                    // marker, restart after the second.
                    let emit_end = re + marker.len_utf8();
                    self.push_text(&text[range_start..emit_end]);
                    range_start = pos + c.len_utf8();
                } else {
                    if self.accel_char.is_none() {
                        self.accel_char = Some(c);
                    }
                    self.push_text(&text[range_start..re]);
                    let uline_start = self.index;
                    let uline_len = c.len_utf8() as u32;
                    self.attr_list.change(Attr {
                        start: uline_start,
                        end: uline_start + uline_len,
                        kind: AttrKind::Underline(UNDERLINE_LOW),
                    });
                    range_start = pos;
                }
                range_end = None;
            } else if c == marker {
                range_end = Some(pos);
            }
        }
        self.push_text(&text[range_start..]);
    }
}

impl Events for MarkupData {
    fn start_element(
        &mut self,
        name: &str,
        attrs: &[(String, String)],
        line: usize,
    ) -> Result<(), String> {
        let tag = self.open_tag();
        let no_attrs = |md: &MarkupData| -> Result<(), String> {
            if let Some((n, _)) = attrs.first()
                && md.strict
            {
                return Err(format!(
                    "Tag '{}' does not support attribute '{}' on line {} char 0",
                    name, n, line
                ));
            }
            Ok(())
        };
        match name {
            "b" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::Weight(WEIGHT_BOLD));
            }
            "big" => {
                no_attrs(self)?;
                let ot = &mut self.tag_stack[tag];
                ot.scale_level_delta += 1;
                ot.scale_level += 1;
            }
            "i" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::Style(STYLE_ITALIC));
            }
            "markup" => {
                no_attrs(self)?;
            }
            "s" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::Strikethrough(true));
            }
            "sub" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::FontScale(SCALE_SUBSCRIPT));
                self.add_attribute(tag, AttrKind::BaselineShift(SCALE_SUBSCRIPT));
            }
            "sup" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::FontScale(SCALE_SUPERSCRIPT));
                self.add_attribute(tag, AttrKind::BaselineShift(SCALE_SUPERSCRIPT));
            }
            "small" => {
                no_attrs(self)?;
                let ot = &mut self.tag_stack[tag];
                ot.scale_level_delta -= 1;
                ot.scale_level -= 1;
            }
            "tt" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::Family("Monospace".to_string()));
            }
            "u" => {
                no_attrs(self)?;
                self.add_attribute(tag, AttrKind::Underline(UNDERLINE_SINGLE));
            }
            "span" => {
                span_parse(self, tag, attrs, line)?;
            }
            _ => {
                if self.strict {
                    return Err(format!("Unknown tag '{}' on line {} char 0", name, line));
                }
                // Tolerant mode: unknown tags are transparent.
            }
        }
        Ok(())
    }

    fn end_element(&mut self, _name: &str) -> Result<(), String> {
        self.close_tag();
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        match self.accel_marker {
            None => self.push_text(text),
            Some(marker) => self.text_with_accel(text, marker),
        }
        Ok(())
    }
}

// -- span attribute values ---------------------------------------------------

/// `attr_strcmp`: equality with '-' and '_' interchangeable.
fn attr_name_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes().zip(b.bytes()).all(|(ca, cb)| {
            let ca = if ca == b'_' { b'-' } else { ca };
            let cb = if cb == b'_' { b'-' } else { cb };
            ca == cb
        })
}

/// `_pango_scan_int`: strtol semantics, result must fit an int. Returns the
/// value and the rest of the string.
fn scan_int(s: &str) -> Option<(i32, &str)> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let len = digits.bytes().take_while(|b| b.is_ascii_digit()).count();
    if len == 0 {
        // strtol parses 0 digits as value 0 with end == start, which
        // _pango_scan_int treats as success (end just does not move).
        return Some((0, s));
    }
    let mag: i64 = digits[..len].parse().ok()?;
    let val = if neg { -mag } else { mag };
    i32::try_from(val).ok().map(|v| (v, &digits[len..]))
}

/// `span_parse_int`: the whole value must be one integer. An empty string is
/// 0, like strtol consuming nothing with end at the terminator.
fn parse_int_full(s: &str) -> Option<i32> {
    let (v, rest) = scan_int(s)?;
    rest.is_empty().then_some(v)
}

/// pango-utils.c `parse_int` (the enum fallback): digits required, whole
/// string, non-negative.
fn parse_enum_int(s: &str) -> Option<i32> {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if neg || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<i64>()
        .ok()
        .and_then(|v| i32::try_from(v).ok())
}

/// Leading float, `g_ascii_strtod` style (trailing junk ignored).
fn strtod_prefix(s: &str) -> (f64, &str) {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let bytes = t.as_bytes();
    let mut end = 0;
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let int_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        end += 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
    }
    if end == int_start || (end == int_start + 1 && bytes[int_start] == b'.') {
        return (0.0, s);
    }
    // Exponent.
    let mantissa_end = end;
    if end < bytes.len() && (bytes[end] | 0x20) == b'e' {
        let mut e = end + 1;
        if e < bytes.len() && (bytes[e] == b'+' || bytes[e] == b'-') {
            e += 1;
        }
        let digits = bytes[e..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 {
            end = e + digits;
        } else {
            end = mantissa_end;
        }
    }
    match t[..end].parse::<f64>() {
        Ok(v) => (v, &t[end..]),
        Err(_) => (0.0, s),
    }
}

/// `parse_length`: bare int in pango units, or a float with a `pt` suffix.
fn parse_length(s: &str) -> Option<i32> {
    if let Some(v) = parse_int_full(s) {
        return Some(v);
    }
    let (val, rest) = strtod_prefix(s);
    if rest == "pt" && rest.len() != s.len() {
        return Some((val * SCALE as f64) as i32);
    }
    None
}

/// `parse_percentage`: float with a `%` suffix, positive.
fn parse_percentage(s: &str) -> Option<f64> {
    let (v, rest) = strtod_prefix(s);
    if rest == "%" && rest.len() != s.len() && v > 0.0 {
        Some(v)
    } else {
        None
    }
}

/// `span_parse_boolean`.
fn parse_boolean(s: &str) -> Option<bool> {
    match s {
        "true" | "yes" | "t" | "y" => Some(true),
        "false" | "no" | "f" | "n" => Some(false),
        _ => None,
    }
}

/// `span_parse_alpha`: 1..=65535, or a percentage (trailing junk after '%'
/// tolerated, as in the C).
fn parse_alpha(s: &str) -> Option<u16> {
    let (v, rest) = scan_int(s)?;
    if rest.is_empty() && v > 0 && v <= 0xffff {
        Some(v as u16)
    } else if rest.starts_with('%') && v > 0 && v <= 100 {
        Some((v * 0xffff / 100) as u16)
    } else {
        None
    }
}

/// `span_parse_enum`: exact nick match, else an integer.
fn parse_enum(table: &[(i32, &str)], s: &str) -> Option<i32> {
    nicks::nick_to_value(table, s).or_else(|| parse_enum_int(s))
}

/// `pango_parse_flags`: nick, integer, or |-combination of nicks.
fn parse_flags(table: &[(i32, &str)], s: &str) -> Option<i32> {
    if let Some(v) = nicks::nick_to_value(table, s) {
        return Some(v);
    }
    if let Some(v) = parse_enum_int(s) {
        return Some(v);
    }
    let mut val = 0;
    for part in s.split('|') {
        let part = part.trim();
        val |= nicks::nick_to_value(table, part)?;
    }
    Some(val)
}

/// `pango_language_from_string` canonical form.
fn language_canonical(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '_' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

/// `parse_absolute_size`: CSS keywords or a percentage. Emits the scale attr
/// itself, returns false when the value is not an absolute size.
fn parse_absolute_size(md: &mut MarkupData, tag: usize, size: &str) -> bool {
    let level = match size {
        "xx-small" => Some(-3),
        "x-small" => Some(-2),
        "small" => Some(-1),
        "medium" => Some(0),
        "large" => Some(1),
        "x-large" => Some(2),
        "xx-large" => Some(3),
        _ => None,
    };
    let factor = match level {
        Some(l) => scale_factor(l, 1.0),
        None => match parse_percentage(size) {
            Some(v) => v / 100.0,
            None => return false,
        },
    };
    md.add_attribute(tag, AttrKind::Scale(factor));
    md.tag_stack[tag].set_absolute_font_scale(factor);
    true
}

/// The `<span>` attribute slots, in `span_parse_func` order.
#[derive(Default)]
struct SpanValues<'a> {
    family: Option<&'a str>,
    size: Option<&'a str>,
    style: Option<&'a str>,
    weight: Option<&'a str>,
    variant: Option<&'a str>,
    stretch: Option<&'a str>,
    width: Option<&'a str>,
    desc: Option<&'a str>,
    foreground: Option<&'a str>,
    background: Option<&'a str>,
    underline: Option<&'a str>,
    underline_color: Option<&'a str>,
    overline: Option<&'a str>,
    overline_color: Option<&'a str>,
    strikethrough: Option<&'a str>,
    strikethrough_color: Option<&'a str>,
    rise: Option<&'a str>,
    baseline_shift: Option<&'a str>,
    letter_spacing: Option<&'a str>,
    lang: Option<&'a str>,
    fallback: Option<&'a str>,
    gravity: Option<&'a str>,
    gravity_hint: Option<&'a str>,
    font_features: Option<&'a str>,
    alpha: Option<&'a str>,
    background_alpha: Option<&'a str>,
    allow_breaks: Option<&'a str>,
    insert_hyphens: Option<&'a str>,
    show: Option<&'a str>,
    line_height: Option<&'a str>,
    text_transform: Option<&'a str>,
    segment: Option<&'a str>,
    font_scale: Option<&'a str>,
}

fn span_parse(
    md: &mut MarkupData,
    tag: usize,
    attrs: &[(String, String)],
    line: usize,
) -> Result<(), String> {
    let strict = md.strict;
    let mut v = SpanValues::default();

    for (name, value) in attrs {
        // Alias table standing in for the CHECK_ATTRIBUTE chains.
        let slot: Option<&mut Option<&str>> = {
            let n = name.as_str();
            macro_rules! m {
                ($($alias:literal => $slot:ident),+ $(,)?) => {
                    $(if attr_name_eq(n, $alias) { Some(&mut v.$slot) } else)+ { None }
                };
            }
            m!(
                "allow_breaks" => allow_breaks,
                "alpha" => alpha,
                "background" => background,
                "bgcolor" => background,
                "background_alpha" => background_alpha,
                "bgalpha" => background_alpha,
                "baseline_shift" => baseline_shift,
                "color" => foreground,
                "fallback" => fallback,
                "font" => desc,
                "font_desc" => desc,
                "face" => family,
                "font_family" => family,
                "font_size" => size,
                "font_stretch" => stretch,
                "font_width" => width,
                "font_style" => style,
                "font_variant" => variant,
                "font_weight" => weight,
                "font_scale" => font_scale,
                "foreground" => foreground,
                "fgcolor" => foreground,
                "fgalpha" => alpha,
                "font_features" => font_features,
                "show" => show,
                "size" => size,
                "stretch" => stretch,
                "strikethrough" => strikethrough,
                "strikethrough_color" => strikethrough_color,
                "style" => style,
                "segment" => segment,
                "text_transform" => text_transform,
                "gravity" => gravity,
                "gravity_hint" => gravity_hint,
                "insert_hyphens" => insert_hyphens,
                "lang" => lang,
                "letter_spacing" => letter_spacing,
                "line_height" => line_height,
                "overline" => overline,
                "overline_color" => overline_color,
                "underline" => underline,
                "underline_color" => underline_color,
                "rise" => rise,
                "variant" => variant,
                "weight" => weight,
                "width" => width,
            )
        };
        match slot {
            Some(slot) => {
                if slot.is_some() {
                    if strict {
                        return Err(format!(
                            "Attribute '{}' occurs twice on <span> tag on line {} char 0, \
                             may only occur once",
                            name, line
                        ));
                    }
                    continue;
                }
                *slot = Some(value.as_str());
            }
            None => {
                if strict {
                    return Err(format!(
                        "Attribute '{}' is not allowed on the <span> tag on line {} char 0",
                        name, line
                    ));
                }
            }
        }
    }

    // Bad values: strict errors out of the whole parse, tolerant just drops
    // the attribute.
    macro_rules! bad {
        ($msg:expr) => {{
            if strict {
                return Err($msg);
            }
        }};
    }

    // Desc first, the other font attributes modify it.
    if let Some(desc) = v.desc {
        let parsed = FontDescription::from_description_string(desc);
        let size = parsed.size;
        md.add_attribute(tag, AttrKind::FontDesc(parsed));
        md.tag_stack[tag].set_absolute_font_size(size);
    }

    if let Some(family) = v.family {
        md.add_attribute(tag, AttrKind::Family(family.to_string()));
    }

    if let Some(size) = v.size {
        match parse_length(size) {
            Some(n) if n > 0 => {
                md.add_attribute(tag, AttrKind::Size(n));
                md.tag_stack[tag].set_absolute_font_size(n);
            }
            _ => {
                if size == "smaller" {
                    let ot = &mut md.tag_stack[tag];
                    ot.scale_level_delta -= 1;
                    ot.scale_level -= 1;
                } else if size == "larger" {
                    let ot = &mut md.tag_stack[tag];
                    ot.scale_level_delta += 1;
                    ot.scale_level += 1;
                } else if parse_absolute_size(md, tag, size) {
                    // Done.
                } else {
                    bad!(format!(
                        "Value of 'size' attribute on <span> tag on line {} could not \
                         be parsed; should be an integer, or a string such as 'small', \
                         not '{}'",
                        line, size
                    ));
                }
            }
        }
    }

    if let Some(style) = v.style {
        match fontdesc::parse_style(style) {
            Some(s) => md.add_attribute(tag, AttrKind::Style(s)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'style' attribute on <span> tag, \
                 line {}; valid values are 'normal', 'oblique', 'italic'",
                style, line
            )),
        }
    }

    if let Some(weight) = v.weight {
        match fontdesc::parse_weight(weight) {
            Some(w) => md.add_attribute(tag, AttrKind::Weight(w)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'weight' attribute on <span> tag, \
                 line {}; valid values are for example 'light', 'ultrabold' or a number",
                weight, line
            )),
        }
    }

    if let Some(variant) = v.variant {
        match fontdesc::parse_variant(variant) {
            Some(val) => md.add_attribute(tag, AttrKind::Variant(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'variant' attribute on <span> tag, \
                 line {}; valid values are 'normal', 'smallcaps'",
                variant, line
            )),
        }
    }

    if let Some(stretch) = v.stretch {
        match fontdesc::parse_stretch(stretch) {
            Some(val) => md.add_attribute(tag, AttrKind::Stretch(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'stretch' attribute on <span> tag, \
                 line {}; valid values are for example 'condensed', 'ultraexpanded', \
                 'normal'",
                stretch, line
            )),
        }
    }

    if let Some(width) = v.width {
        match fontdesc::parse_width(width) {
            Some(val) => md.add_attribute(tag, AttrKind::Width(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'width' attribute on <span> tag, \
                 line {}; valid values are for example 'ultra-condensed', \
                 'semi-expanded' or a number",
                width, line
            )),
        }
    }

    if let Some(fg) = v.foreground {
        match color::parse_with_alpha(fg, true) {
            Some((c, alpha)) => {
                md.add_attribute(tag, AttrKind::Foreground(c));
                if alpha != 0xffff {
                    md.add_attribute(tag, AttrKind::ForegroundAlpha(alpha));
                }
            }
            None => bad!(format!(
                "Value of 'foreground' attribute on <span> tag on line {} could not \
                 be parsed; should be a color specification, not '{}'",
                line, fg
            )),
        }
    }

    if let Some(bg) = v.background {
        match color::parse_with_alpha(bg, true) {
            Some((c, alpha)) => {
                md.add_attribute(tag, AttrKind::Background(c));
                if alpha != 0xffff {
                    md.add_attribute(tag, AttrKind::BackgroundAlpha(alpha));
                }
            }
            None => bad!(format!(
                "Value of 'background' attribute on <span> tag on line {} could not \
                 be parsed; should be a color specification, not '{}'",
                line, bg
            )),
        }
    }

    if let Some(alpha) = v.alpha {
        match parse_alpha(alpha) {
            Some(val) => md.add_attribute(tag, AttrKind::ForegroundAlpha(val)),
            None => bad!(format!(
                "Value of 'alpha' attribute on <span> tag on line {} could not be \
                 parsed; should be between 0 and 65536 or a percentage, not '{}'",
                line, alpha
            )),
        }
    }

    if let Some(alpha) = v.background_alpha {
        match parse_alpha(alpha) {
            Some(val) => md.add_attribute(tag, AttrKind::BackgroundAlpha(val)),
            None => bad!(format!(
                "Value of 'background_alpha' attribute on <span> tag on line {} could \
                 not be parsed; should be between 0 and 65536 or a percentage, not '{}'",
                line, alpha
            )),
        }
    }

    if let Some(underline) = v.underline {
        match parse_enum(nicks::UNDERLINE, underline) {
            Some(val) => md.add_attribute(tag, AttrKind::Underline(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'underline' attribute on <span> \
                 tag, line {}",
                underline, line
            )),
        }
    }

    if let Some(c) = v.underline_color {
        match color::parse_with_alpha(c, false) {
            Some((col, _)) => md.add_attribute(tag, AttrKind::UnderlineColor(col)),
            None => bad!(format!(
                "Value of 'underline_color' attribute on <span> tag on line {} could \
                 not be parsed; should be a color specification, not '{}'",
                line, c
            )),
        }
    }

    if let Some(overline) = v.overline {
        match parse_enum(nicks::OVERLINE, overline) {
            Some(val) => md.add_attribute(tag, AttrKind::Overline(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'overline' attribute on <span> \
                 tag, line {}",
                overline, line
            )),
        }
    }

    if let Some(c) = v.overline_color {
        match color::parse_with_alpha(c, false) {
            Some((col, _)) => md.add_attribute(tag, AttrKind::OverlineColor(col)),
            None => bad!(format!(
                "Value of 'overline_color' attribute on <span> tag on line {} could \
                 not be parsed; should be a color specification, not '{}'",
                line, c
            )),
        }
    }

    if let Some(gravity) = v.gravity {
        match parse_enum(nicks::GRAVITY, gravity) {
            Some(4) | None => bad!(format!(
                "'{}' is not a valid value for the 'gravity' attribute on <span> tag, \
                 line {}; valid values are for example 'south', 'east', 'north', 'west'",
                gravity, line
            )),
            Some(val) => md.add_attribute(tag, AttrKind::Gravity(val)),
        }
    }

    if let Some(hint) = v.gravity_hint {
        match parse_enum(nicks::GRAVITY_HINT, hint) {
            Some(val) => md.add_attribute(tag, AttrKind::GravityHint(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'gravity_hint' attribute on <span> \
                 tag, line {}",
                hint, line
            )),
        }
    }

    if let Some(st) = v.strikethrough {
        match parse_boolean(st) {
            Some(b) => md.add_attribute(tag, AttrKind::Strikethrough(b)),
            None => bad!(format!(
                "Value of 'strikethrough' attribute on <span> tag line {} should have \
                 one of 'true/yes/t/y' or 'false/no/f/n': '{}' is not valid",
                line, st
            )),
        }
    }

    if let Some(c) = v.strikethrough_color {
        match color::parse_with_alpha(c, false) {
            Some((col, _)) => md.add_attribute(tag, AttrKind::StrikethroughColor(col)),
            None => bad!(format!(
                "Value of 'strikethrough_color' attribute on <span> tag on line {} \
                 could not be parsed; should be a color specification, not '{}'",
                line, c
            )),
        }
    }

    if let Some(fb) = v.fallback {
        match parse_boolean(fb) {
            Some(b) => md.add_attribute(tag, AttrKind::Fallback(b)),
            None => bad!(format!(
                "Value of 'fallback' attribute on <span> tag line {} should have one \
                 of 'true/yes/t/y' or 'false/no/f/n': '{}' is not valid",
                line, fb
            )),
        }
    }

    if let Some(show) = v.show {
        match parse_flags(nicks::SHOW, show) {
            Some(val) => md.add_attribute(tag, AttrKind::Show(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'show' attribute on <span> tag, \
                 line {}; valid values are 'none', 'spaces', 'line-breaks', \
                 'ignorables' or combinations with |",
                show, line
            )),
        }
    }

    if let Some(tf) = v.text_transform {
        match parse_enum(nicks::TEXT_TRANSFORM, tf) {
            Some(val) => md.add_attribute(tag, AttrKind::TextTransform(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'text_transform' attribute on \
                 <span> tag, line {}",
                tf, line
            )),
        }
    }

    if let Some(rise) = v.rise {
        match parse_length(rise) {
            Some(n) => md.add_attribute(tag, AttrKind::Rise(n)),
            None => bad!(format!(
                "Value of 'rise' attribute on <span> tag on line {} could not be \
                 parsed; should be an integer, or a string such as '5.5pt', not '{}'",
                line, rise
            )),
        }
    }

    if let Some(shift) = v.baseline_shift {
        if let Some(val) = parse_enum(nicks::BASELINE_SHIFT, shift) {
            md.add_attribute(tag, AttrKind::BaselineShift(val));
        } else if let Some(n) = parse_length(shift)
            && !(-1024..=1024).contains(&n)
        {
            md.add_attribute(tag, AttrKind::BaselineShift(n));
        } else {
            bad!(format!(
                "Value of 'baseline_shift' attribute on <span> tag on line {} could \
                 not be parsed; should be 'superscript' or 'subscript' or an integer, \
                 or a string such as '5.5pt', not '{}'",
                line, shift
            ));
        }
    }

    if let Some(fs) = v.font_scale {
        match parse_enum(nicks::FONT_SCALE, fs) {
            Some(val) => md.add_attribute(tag, AttrKind::FontScale(val)),
            None => bad!(format!(
                "'{}' is not a valid value for the 'font_scale' attribute on <span> \
                 tag, line {}",
                fs, line
            )),
        }
    }

    if let Some(ls) = v.letter_spacing {
        match parse_int_full(ls) {
            Some(n) => md.add_attribute(tag, AttrKind::LetterSpacing(n)),
            None => bad!(format!(
                "Value of 'letter_spacing' attribute on <span> tag on line {} could \
                 not be parsed; should be an integer, not '{}'",
                line, ls
            )),
        }
    }

    if let Some(lh) = v.line_height {
        // span_parse_float never fails on junk, it reads the leading float.
        let (f, _) = strtod_prefix(lh);
        if f > 1024.0 && !lh.contains('.') {
            md.add_attribute(tag, AttrKind::AbsoluteLineHeight(f as i32));
        } else {
            md.add_attribute(tag, AttrKind::LineHeight(f));
        }
    }

    if let Some(lang) = v.lang {
        md.add_attribute(tag, AttrKind::Language(language_canonical(lang)));
    }

    if let Some(ff) = v.font_features {
        md.add_attribute(tag, AttrKind::FontFeatures(ff.to_string()));
    }

    if let Some(ab) = v.allow_breaks {
        match parse_boolean(ab) {
            Some(b) => md.add_attribute(tag, AttrKind::AllowBreaks(b)),
            None => bad!(format!(
                "Value of 'allow_breaks' attribute on <span> tag line {} should have \
                 one of 'true/yes/t/y' or 'false/no/f/n': '{}' is not valid",
                line, ab
            )),
        }
    }

    if let Some(ih) = v.insert_hyphens {
        match parse_boolean(ih) {
            Some(b) => md.add_attribute(tag, AttrKind::InsertHyphens(b)),
            None => bad!(format!(
                "Value of 'insert_hyphens' attribute on <span> tag line {} should \
                 have one of 'true/yes/t/y' or 'false/no/f/n': '{}' is not valid",
                line, ih
            )),
        }
    }

    if let Some(seg) = v.segment {
        match seg {
            "word" => md.add_attribute(tag, AttrKind::Word),
            "sentence" => md.add_attribute(tag, AttrKind::Sentence),
            _ => bad!(format!(
                "Value of 'segment' attribute on <span> tag on line {} could not be \
                 parsed; should be one of 'word' or 'sentence', not '{}'",
                line, seg
            )),
        }
    }

    Ok(())
}

// -- entry points ------------------------------------------------------------

/// `pango_parse_markup`: strict, whole-input-or-error, pango parity.
pub fn parse_markup(input: &str, accel_marker: Option<char>) -> Result<Parsed, XmlError> {
    let mut md = MarkupData::new(accel_marker, true);
    xml::parse_wrapped(input, &mut md)?;
    Ok(md.finish())
}

/// Tolerant parse for real-world subtitle streams: never fails. Unknown tags
/// are transparent, bad attribute values are dropped, malformed syntax stays
/// literal text, unclosed tags close at the end of input. The tolerant
/// scanner emits a balanced event stream, so nothing stays open here.
pub fn parse_markup_tolerant(input: &str) -> Parsed {
    let mut md = MarkupData::new(None, false);
    crate::tolerant::parse(input, &mut md);
    md.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump(p: &Parsed) -> Vec<String> {
        p.attrs
            .attrs
            .iter()
            .map(crate::attr::attr_to_string)
            .collect()
    }

    #[test]
    fn simple_tags() {
        let p = parse_markup("plain <b>bold</b> <i>italic</i>", None).unwrap();
        assert_eq!(p.text, "plain bold italic");
        assert_eq!(dump(&p), vec!["6 10 weight bold", "11 17 style italic"]);
    }

    #[test]
    fn nested_scale_merges_per_tag() {
        let p = parse_markup("<big><big>x</big>y</big>", None).unwrap();
        // Outer big covers both chars at 1.2, inner big alone re-scales, and
        // pango emits absolute cumulative factors per tag.
        assert_eq!(dump(&p), vec!["0 2 scale 1.200000", "0 1 scale 1.440000"]);
    }

    #[test]
    fn span_size_absolute_resets_scale() {
        let p = parse_markup("<span size=\"10240\"><big>x</big></span>", None).unwrap();
        assert_eq!(dump(&p), vec!["0 1 size 10240", "0 1 size 12288"]);
    }

    #[test]
    fn strict_rejects_unknown() {
        assert!(parse_markup("<bogus>x</bogus>", None).is_err());
        assert!(parse_markup("<b weight=\"1\">x</b>", None).is_err());
        assert!(parse_markup("<span nope=\"1\">x</span>", None).is_err());
        assert!(parse_markup("<span size=\"1\" size=\"2\">x</span>", None).is_err());
        assert!(parse_markup("<span size=\"bogus\">x</span>", None).is_err());
    }

    #[test]
    fn accel_marker_low_underlines() {
        let p = parse_markup("_File __and", Some('_')).unwrap();
        assert_eq!(p.text, "File _and");
        assert_eq!(p.accel_char, Some('F'));
        assert_eq!(dump(&p), vec!["0 1 underline low"]);
    }

    #[test]
    fn foreground_alpha_forms() {
        let p = parse_markup("<span foreground=\"#ff000080\">x</span>", None).unwrap();
        assert_eq!(
            dump(&p),
            vec!["0 1 foreground #ffff00000000", "0 1 foreground-alpha 32896"]
        );
    }
}
