// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Pango attributes, attribute lists and the attribute iterator.
//!
//! Faithful ports of the pieces of pango-attributes.c the markup parser and
//! its conformance dump need: `pango_attr_list_insert` ordering,
//! `pango_attr_list_change` merging, `PangoAttrIterator` and the
//! `pango_attr_list_to_string` per-attribute serialization.

use crate::color::Color;
use crate::fontdesc::FontDescription;

/// `PANGO_SCALE`, pango units per point.
pub const SCALE: i32 = 1024;

/// Range end used for "until the end", `G_MAXUINT`.
pub const END_MAX: u32 = u32::MAX;

/// Attribute type discriminant, `PangoAttrType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Language,
    Family,
    Style,
    Weight,
    Variant,
    Stretch,
    Size,
    FontDesc,
    Foreground,
    Background,
    Underline,
    Strikethrough,
    Rise,
    Scale,
    Fallback,
    LetterSpacing,
    UnderlineColor,
    StrikethroughColor,
    Gravity,
    GravityHint,
    FontFeatures,
    ForegroundAlpha,
    BackgroundAlpha,
    AllowBreaks,
    Show,
    InsertHyphens,
    Overline,
    OverlineColor,
    LineHeight,
    AbsoluteLineHeight,
    TextTransform,
    Word,
    Sentence,
    BaselineShift,
    FontScale,
    Width,
}

impl Ty {
    /// GLib enum value nick of the `PangoAttrType` value.
    pub fn nick(self) -> &'static str {
        match self {
            Ty::Language => "language",
            Ty::Family => "family",
            Ty::Style => "style",
            Ty::Weight => "weight",
            Ty::Variant => "variant",
            Ty::Stretch => "stretch",
            Ty::Size => "size",
            Ty::FontDesc => "font-desc",
            Ty::Foreground => "foreground",
            Ty::Background => "background",
            Ty::Underline => "underline",
            Ty::Strikethrough => "strikethrough",
            Ty::Rise => "rise",
            Ty::Scale => "scale",
            Ty::Fallback => "fallback",
            Ty::LetterSpacing => "letter-spacing",
            Ty::UnderlineColor => "underline-color",
            Ty::StrikethroughColor => "strikethrough-color",
            Ty::Gravity => "gravity",
            Ty::GravityHint => "gravity-hint",
            Ty::FontFeatures => "font-features",
            Ty::ForegroundAlpha => "foreground-alpha",
            Ty::BackgroundAlpha => "background-alpha",
            Ty::AllowBreaks => "allow-breaks",
            Ty::Show => "show",
            Ty::InsertHyphens => "insert-hyphens",
            Ty::Overline => "overline",
            Ty::OverlineColor => "overline-color",
            Ty::LineHeight => "line-height",
            Ty::AbsoluteLineHeight => "absolute-line-height",
            Ty::TextTransform => "text-transform",
            Ty::Word => "word",
            Ty::Sentence => "sentence",
            Ty::BaselineShift => "baseline-shift",
            Ty::FontScale => "font-scale",
            Ty::Width => "width",
        }
    }
}

/// Nick tables for the enum-valued attributes. Values that are not in the
/// table serialize as their number, exactly like GLib enum lookup failing.
pub mod nicks {
    pub const STYLE: &[(i32, &str)] = &[(0, "normal"), (1, "oblique"), (2, "italic")];
    pub const VARIANT: &[(i32, &str)] = &[
        (0, "normal"),
        (1, "small-caps"),
        (2, "all-small-caps"),
        (3, "petite-caps"),
        (4, "all-petite-caps"),
        (5, "unicase"),
        (6, "title-caps"),
    ];
    pub const WEIGHT: &[(i32, &str)] = &[
        (100, "thin"),
        (200, "ultralight"),
        (300, "light"),
        (350, "semilight"),
        (380, "book"),
        (400, "normal"),
        (500, "medium"),
        (600, "semibold"),
        (700, "bold"),
        (800, "ultrabold"),
        (900, "heavy"),
        (1000, "ultraheavy"),
    ];
    pub const STRETCH: &[(i32, &str)] = &[
        (0, "ultra-condensed"),
        (1, "extra-condensed"),
        (2, "condensed"),
        (3, "semi-condensed"),
        (4, "normal"),
        (5, "semi-expanded"),
        (6, "expanded"),
        (7, "extra-expanded"),
        (8, "ultra-expanded"),
    ];
    pub const WIDTH: &[(i32, &str)] = &[
        (500, "ultra-condensed"),
        (625, "extra-condensed"),
        (750, "condensed"),
        (875, "semi-condensed"),
        (1000, "normal"),
        (1125, "semi-expanded"),
        (1250, "expanded"),
        (1500, "extra-expanded"),
        (2000, "ultra-expanded"),
    ];
    pub const UNDERLINE: &[(i32, &str)] = &[
        (0, "none"),
        (1, "single"),
        (2, "double"),
        (3, "low"),
        (4, "error"),
        (5, "single-line"),
        (6, "double-line"),
        (7, "error-line"),
    ];
    pub const OVERLINE: &[(i32, &str)] = &[(0, "none"), (1, "single")];
    pub const GRAVITY: &[(i32, &str)] = &[
        (0, "south"),
        (1, "east"),
        (2, "north"),
        (3, "west"),
        (4, "auto"),
    ];
    pub const GRAVITY_HINT: &[(i32, &str)] = &[(0, "natural"), (1, "strong"), (2, "line")];
    pub const TEXT_TRANSFORM: &[(i32, &str)] = &[
        (0, "none"),
        (1, "lowercase"),
        (2, "uppercase"),
        (3, "capitalize"),
    ];
    pub const BASELINE_SHIFT: &[(i32, &str)] = &[(0, "none"), (1, "superscript"), (2, "subscript")];
    pub const FONT_SCALE: &[(i32, &str)] = &[
        (0, "none"),
        (1, "superscript"),
        (2, "subscript"),
        (3, "small-caps"),
    ];
    pub const SHOW: &[(i32, &str)] = &[
        (0, "none"),
        (1, "spaces"),
        (2, "line-breaks"),
        (4, "ignorables"),
    ];

    pub fn value_to_nick(table: &[(i32, &'static str)], v: i32) -> Option<&'static str> {
        table.iter().find(|(x, _)| *x == v).map(|(_, n)| *n)
    }

    /// `g_enum_get_value_by_nick`, an exact (case-sensitive) match.
    pub fn nick_to_value(table: &[(i32, &str)], nick: &str) -> Option<i32> {
        table.iter().find(|(_, n)| *n == nick).map(|(v, _)| *v)
    }
}

/// One attribute value. Enum-typed pango values are plain `i32` so numeric
/// (non-nick) values round-trip like they do through GLib.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrKind {
    Language(String),
    Family(String),
    Style(i32),
    Weight(i32),
    Variant(i32),
    Stretch(i32),
    Width(i32),
    /// Font size in pango units.
    Size(i32),
    FontDesc(FontDescription),
    Foreground(Color),
    Background(Color),
    Underline(i32),
    Overline(i32),
    Strikethrough(bool),
    Fallback(bool),
    AllowBreaks(bool),
    InsertHyphens(bool),
    Rise(i32),
    LetterSpacing(i32),
    Scale(f64),
    LineHeight(f64),
    AbsoluteLineHeight(i32),
    UnderlineColor(Color),
    StrikethroughColor(Color),
    OverlineColor(Color),
    ForegroundAlpha(u16),
    BackgroundAlpha(u16),
    FontFeatures(String),
    Show(i32),
    TextTransform(i32),
    BaselineShift(i32),
    FontScale(i32),
    Gravity(i32),
    GravityHint(i32),
    Word,
    Sentence,
}

impl AttrKind {
    pub fn ty(&self) -> Ty {
        match self {
            AttrKind::Language(_) => Ty::Language,
            AttrKind::Family(_) => Ty::Family,
            AttrKind::Style(_) => Ty::Style,
            AttrKind::Weight(_) => Ty::Weight,
            AttrKind::Variant(_) => Ty::Variant,
            AttrKind::Stretch(_) => Ty::Stretch,
            AttrKind::Width(_) => Ty::Width,
            AttrKind::Size(_) => Ty::Size,
            AttrKind::FontDesc(_) => Ty::FontDesc,
            AttrKind::Foreground(_) => Ty::Foreground,
            AttrKind::Background(_) => Ty::Background,
            AttrKind::Underline(_) => Ty::Underline,
            AttrKind::Overline(_) => Ty::Overline,
            AttrKind::Strikethrough(_) => Ty::Strikethrough,
            AttrKind::Fallback(_) => Ty::Fallback,
            AttrKind::AllowBreaks(_) => Ty::AllowBreaks,
            AttrKind::InsertHyphens(_) => Ty::InsertHyphens,
            AttrKind::Rise(_) => Ty::Rise,
            AttrKind::LetterSpacing(_) => Ty::LetterSpacing,
            AttrKind::Scale(_) => Ty::Scale,
            AttrKind::LineHeight(_) => Ty::LineHeight,
            AttrKind::AbsoluteLineHeight(_) => Ty::AbsoluteLineHeight,
            AttrKind::UnderlineColor(_) => Ty::UnderlineColor,
            AttrKind::StrikethroughColor(_) => Ty::StrikethroughColor,
            AttrKind::OverlineColor(_) => Ty::OverlineColor,
            AttrKind::ForegroundAlpha(_) => Ty::ForegroundAlpha,
            AttrKind::BackgroundAlpha(_) => Ty::BackgroundAlpha,
            AttrKind::FontFeatures(_) => Ty::FontFeatures,
            AttrKind::Show(_) => Ty::Show,
            AttrKind::TextTransform(_) => Ty::TextTransform,
            AttrKind::BaselineShift(_) => Ty::BaselineShift,
            AttrKind::FontScale(_) => Ty::FontScale,
            AttrKind::Gravity(_) => Ty::Gravity,
            AttrKind::GravityHint(_) => Ty::GravityHint,
            AttrKind::Word => Ty::Word,
            AttrKind::Sentence => Ty::Sentence,
        }
    }
}

/// One attribute over a byte range of the flattened text.
#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub start: u32,
    pub end: u32,
    pub kind: AttrKind,
}

/// `PangoAttrList`, attrs ordered by non-decreasing start index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AttrList {
    pub attrs: Vec<Attr>,
}

impl AttrList {
    pub fn new() -> AttrList {
        AttrList::default()
    }

    fn insert_internal(&mut self, attr: Attr, before: bool) {
        if self.attrs.is_empty() {
            self.attrs.push(attr);
            return;
        }
        let start = attr.start;
        let last = self.attrs.last().unwrap();
        if last.start < start || (!before && last.start == start) {
            self.attrs.push(attr);
            return;
        }
        for i in 0..self.attrs.len() {
            let cur = &self.attrs[i];
            if cur.start > start || (before && cur.start == start) {
                self.attrs.insert(i, attr);
                return;
            }
        }
        // Unreachable per the checks above, matches the C fallthrough.
        self.attrs.push(attr);
    }

    /// `pango_attr_list_insert`, after all attrs with the same start.
    pub fn insert(&mut self, attr: Attr) {
        self.insert_internal(attr, false);
    }

    /// `pango_attr_list_insert_before`.
    pub fn insert_before(&mut self, attr: Attr) {
        self.insert_internal(attr, true);
    }

    /// `pango_attr_list_change`, replacing same-type attrs on the segment and
    /// merging adjoining identical ones.
    pub fn change(&mut self, attr: Attr) {
        let start_index = attr.start;
        let end_index = attr.end;

        if start_index == end_index {
            return;
        }
        if self.attrs.is_empty() {
            self.insert(attr);
            return;
        }

        let mut attr = attr;
        let mut inserted = false;
        // Index the placed attr lives at, tracked in place of the C pointer.
        let mut attr_idx = usize::MAX;
        let mut i = 0;
        let p = self.attrs.len();
        while i < p {
            let tmp = self.attrs[i].clone();

            if tmp.start > start_index {
                self.attrs.insert(i, attr.clone());
                inserted = true;
                attr_idx = i;
                break;
            }

            if tmp.kind.ty() != attr.kind.ty() {
                i += 1;
                continue;
            }

            if tmp.end < start_index {
                i += 1;
                continue;
            }

            if tmp.kind == attr.kind {
                // Merge with the existing identical attribute.
                if tmp.end >= end_index {
                    return;
                }
                self.attrs[i].end = end_index;
                attr = self.attrs[i].clone();
                inserted = true;
                attr_idx = i;
                break;
            } else {
                // Split, truncate, or remove the old attribute.
                if tmp.end > end_index {
                    let mut end_attr = tmp.clone();
                    end_attr.start = end_index;
                    self.insert(end_attr);
                }
                if tmp.start == start_index {
                    self.attrs.remove(i);
                    break;
                } else {
                    self.attrs[i].end = start_index;
                    i += 1;
                }
            }
        }

        if !inserted {
            attr_idx = self.insert_indexed(attr.clone());
        }

        // Fix up the remainder. The C resumes after the loop position, not
        // from the list head.
        let mut i = i + 1;
        while i < self.attrs.len() {
            let tmp = self.attrs[i].clone();

            if tmp.start > end_index {
                break;
            }
            if tmp.kind.ty() != self.attrs[attr_idx].kind.ty() {
                i += 1;
                continue;
            }
            if i == attr_idx {
                i += 1;
                continue;
            }

            let cur = self.attrs[attr_idx].clone();
            if tmp.end <= cur.end || tmp.kind == cur.kind {
                // Merge.
                self.attrs[attr_idx].end = end_index.max(tmp.end);
                self.attrs.remove(i);
                if i < attr_idx {
                    attr_idx -= 1;
                }
                continue;
            } else {
                // Trim the start and bubble it right to keep start ordering.
                self.attrs[i].start = cur.end;
                let mut k = i + 1;
                while k < self.attrs.len() {
                    if self.attrs[k].start >= self.attrs[k - 1].start {
                        break;
                    }
                    self.attrs.swap(k - 1, k);
                    if attr_idx == k {
                        attr_idx = k - 1;
                    }
                    k += 1;
                }
                i += 1;
            }
        }
    }

    /// Insert and report where the attr landed.
    fn insert_indexed(&mut self, attr: Attr) -> usize {
        let start = attr.start;
        if self.attrs.is_empty() {
            self.attrs.push(attr);
            return 0;
        }
        let last = self.attrs.last().unwrap();
        if last.start <= start {
            self.attrs.push(attr);
            return self.attrs.len() - 1;
        }
        for i in 0..self.attrs.len() {
            if self.attrs[i].start > start {
                self.attrs.insert(i, attr);
                return i;
            }
        }
        self.attrs.push(attr);
        self.attrs.len() - 1
    }

    pub fn iter_ranges(&self) -> AttrIterator<'_> {
        AttrIterator::new(&self.attrs)
    }
}

/// `PangoAttrIterator`, walking the ranges where the applicable attribute set
/// is constant.
pub struct AttrIterator<'a> {
    attrs: &'a [Attr],
    /// Indices of attrs covering the current range, in list order.
    stack: Vec<usize>,
    attr_index: usize,
    pub start: u32,
    pub end: u32,
}

impl<'a> AttrIterator<'a> {
    pub fn new(attrs: &'a [Attr]) -> AttrIterator<'a> {
        let mut it = AttrIterator {
            attrs,
            stack: Vec::new(),
            attr_index: 0,
            start: 0,
            end: 0,
        };
        if !it.next_range() {
            it.end = END_MAX;
        }
        it
    }

    /// `pango_attr_iterator_next`. Returns false at the end of the list.
    pub fn next_range(&mut self) -> bool {
        if self.attr_index >= self.attrs.len() && self.stack.is_empty() {
            return false;
        }

        self.start = self.end;
        self.end = END_MAX;

        let start = self.start;
        self.stack.retain(|&idx| self.attrs[idx].end != start);
        for &idx in &self.stack {
            self.end = self.end.min(self.attrs[idx].end);
        }

        loop {
            if self.attr_index >= self.attrs.len() {
                break;
            }
            let attr = &self.attrs[self.attr_index];
            if attr.start != self.start {
                break;
            }
            if attr.end > self.start {
                self.stack.push(self.attr_index);
                self.end = self.end.min(attr.end);
            }
            self.attr_index += 1;
        }

        if self.attr_index < self.attrs.len() {
            self.end = self.end.min(self.attrs[self.attr_index].start);
        }

        true
    }

    /// `pango_attr_iterator_range`, clamped to `G_MAXINT` like the C API.
    pub fn range(&self) -> (i32, i32) {
        (
            self.start.min(i32::MAX as u32) as i32,
            self.end.min(i32::MAX as u32) as i32,
        )
    }

    /// `pango_attr_iterator_get_attrs`: the current range's attrs, one per
    /// type (highest priority wins), except font-desc / baseline-shift /
    /// font-scale which are never deduplicated.
    pub fn get_attrs(&self) -> Vec<&'a Attr> {
        let mut kept: Vec<&Attr> = Vec::new();
        for &idx in self.stack.iter().rev() {
            let attr = &self.attrs[idx];
            let ty = attr.kind.ty();
            let dedup = !matches!(ty, Ty::FontDesc | Ty::BaselineShift | Ty::FontScale);
            if dedup && kept.iter().any(|a| a.kind.ty() == ty) {
                continue;
            }
            kept.push(attr);
        }
        kept.reverse();
        kept
    }

    /// The raw covering-attr stack from bottom to top, undeduplicated, for
    /// `pango_attr_iterator_get_font`.
    pub fn stack_attrs(&self) -> Vec<&'a Attr> {
        self.stack.iter().map(|&idx| &self.attrs[idx]).collect()
    }
}

/// `g_strescape`: backslash-escape `"` and `\`, C escapes for control chars,
/// octal for everything else outside printable ASCII (UTF-8 bytes included).
pub fn strescape(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'\x08' => out.push_str("\\b"),
            b'\x0c' => out.push_str("\\f"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0b => out.push_str("\\v"),
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{:03o}", b)),
        }
    }
    out
}

/// C `%f` (six decimals), what `g_ascii_formatd` produces for the dump.
fn format_f(v: f64) -> String {
    format!("{:.6}", v)
}

/// One attribute in `pango_attr_list_to_string` form,
/// `START END TYPE VALUE`.
pub fn attr_to_string(attr: &Attr) -> String {
    let mut s = format!("{} {} {}", attr.start, attr.end, attr.kind.ty().nick());
    let enum_val = |table: &[(i32, &'static str)], v: i32| -> String {
        match nicks::value_to_nick(table, v) {
            Some(n) => format!(" {}", n),
            None => format!(" {}", v),
        }
    };
    let val = match &attr.kind {
        AttrKind::Style(v) => enum_val(nicks::STYLE, *v),
        AttrKind::Weight(v) => enum_val(nicks::WEIGHT, *v),
        AttrKind::Variant(v) => enum_val(nicks::VARIANT, *v),
        AttrKind::Stretch(v) => enum_val(nicks::STRETCH, *v),
        AttrKind::Width(v) => enum_val(nicks::WIDTH, *v),
        AttrKind::Gravity(v) => enum_val(nicks::GRAVITY, *v),
        AttrKind::GravityHint(v) => enum_val(nicks::GRAVITY_HINT, *v),
        AttrKind::Underline(v) => enum_val(nicks::UNDERLINE, *v),
        AttrKind::Overline(v) => enum_val(nicks::OVERLINE, *v),
        AttrKind::BaselineShift(v) => enum_val(nicks::BASELINE_SHIFT, *v),
        AttrKind::FontScale(v) => enum_val(nicks::FONT_SCALE, *v),
        AttrKind::TextTransform(v) => enum_val(nicks::TEXT_TRANSFORM, *v),
        AttrKind::Strikethrough(v)
        | AttrKind::AllowBreaks(v)
        | AttrKind::InsertHyphens(v)
        | AttrKind::Fallback(v) => {
            if *v {
                " true".to_string()
            } else {
                " false".to_string()
            }
        }
        AttrKind::Family(v) => format!(" \"{}\"", strescape(v)),
        AttrKind::Language(v) => format!(" {}", v),
        AttrKind::Rise(v)
        | AttrKind::LetterSpacing(v)
        | AttrKind::AbsoluteLineHeight(v)
        | AttrKind::Show(v) => format!(" {}", v),
        AttrKind::ForegroundAlpha(v) | AttrKind::BackgroundAlpha(v) => format!(" {}", v),
        AttrKind::Word | AttrKind::Sentence => " 1".to_string(),
        AttrKind::Scale(v) | AttrKind::LineHeight(v) => format!(" {}", format_f(*v)),
        AttrKind::FontDesc(d) => format!(" \"{}\"", strescape(&d.to_description_string())),
        AttrKind::Foreground(c)
        | AttrKind::Background(c)
        | AttrKind::UnderlineColor(c)
        | AttrKind::StrikethroughColor(c)
        | AttrKind::OverlineColor(c) => format!(" {}", c.to_hex_string()),
        AttrKind::Size(v) => format!(" {}", v),
        AttrKind::FontFeatures(v) => format!(" \"{}\"", strescape(v)),
    };
    s.push_str(&val);
    s
}
