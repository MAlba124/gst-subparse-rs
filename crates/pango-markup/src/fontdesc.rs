// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! `PangoFontDescription` and its string round-trip, ported from fonts.c.
//!
//! The markup parser needs `from_string` for the `font`/`font_desc` span
//! attribute and the field parsers (`pango_parse_style` and friends) for the
//! plain style attributes. The conformance dump additionally needs
//! `to_string` and the `pango_attr_iterator_get_font` merge behavior.

use crate::attr::SCALE;

pub const MASK_FAMILY: u32 = 1 << 0;
pub const MASK_STYLE: u32 = 1 << 1;
pub const MASK_VARIANT: u32 = 1 << 2;
pub const MASK_WEIGHT: u32 = 1 << 3;
pub const MASK_WIDTH: u32 = 1 << 4;
pub const MASK_SIZE: u32 = 1 << 5;
pub const MASK_GRAVITY: u32 = 1 << 6;
pub const MASK_VARIATIONS: u32 = 1 << 7;
pub const MASK_FEATURES: u32 = 1 << 8;
pub const MASK_COLOR: u32 = 1 << 9;

/// The `FieldMap`s from fonts.c. Map strings match case-insensitively with
/// every '-' optional. Empty strings are the "Normal" defaults, skipped when
/// matching and when serializing.
const STYLE_MAP: &[(i32, &str)] = &[(0, ""), (0, "Roman"), (1, "Oblique"), (2, "Italic")];
const VARIANT_MAP: &[(i32, &str)] = &[
    (0, ""),
    (1, "Small-Caps"),
    (2, "All-Small-Caps"),
    (3, "Petite-Caps"),
    (4, "All-Petite-Caps"),
    (5, "Unicase"),
    (6, "Title-Caps"),
];
const WEIGHT_MAP: &[(i32, &str)] = &[
    (100, "Thin"),
    (200, "Ultra-Light"),
    (200, "Extra-Light"),
    (300, "Light"),
    (350, "Semi-Light"),
    (350, "Demi-Light"),
    (380, "Book"),
    (400, ""),
    (400, "Regular"),
    (500, "Medium"),
    (600, "Semi-Bold"),
    (600, "Demi-Bold"),
    (700, "Bold"),
    (800, "Ultra-Bold"),
    (800, "Extra-Bold"),
    (900, "Heavy"),
    (900, "Black"),
    (1000, "Ultra-Heavy"),
    (1000, "Extra-Heavy"),
    (1000, "Ultra-Black"),
    (1000, "Extra-Black"),
];
const WIDTH_MAP: &[(i32, &str)] = &[
    (500, "Ultra-Condensed"),
    (625, "Extra-Condensed"),
    (750, "Condensed"),
    (875, "Semi-Condensed"),
    (1000, ""),
    (1125, "Semi-Expanded"),
    (1250, "Expanded"),
    (1500, "Extra-Expanded"),
    (2000, "Ultra-Expanded"),
];
const GRAVITY_MAP: &[(i32, &str)] = &[
    (0, "Not-Rotated"),
    (0, "South"),
    (2, "Upside-Down"),
    (2, "North"),
    (1, "Rotated-Left"),
    (1, "East"),
    (3, "Rotated-Right"),
    (3, "West"),
];
const COLOR_MAP: &[(i32, &str)] = &[(1, "With-Color"), (2, "Without-Color")];

/// fonts.c `field_matches`: `s1` is the map entry, `s2` the input limited to
/// `n` bytes. Case-insensitive, dashes in the map entry are optional.
fn field_matches(s1: &str, s2: &[u8]) -> bool {
    let mut a = s1.as_bytes();
    let mut b = s2;
    while !b.is_empty() && !a.is_empty() {
        let c1 = a[0].to_ascii_lowercase();
        let c2 = b[0].to_ascii_lowercase();
        if c1 != c2 {
            if c1 == b'-' {
                a = &a[1..];
                continue;
            }
            return false;
        }
        a = &a[1..];
        b = &b[1..];
    }
    b.is_empty() && a.is_empty()
}

/// fonts.c `parse_int`: strtol that must consume the whole word, non-negative.
fn parse_int_word(word: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(word).ok()?;
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let t = t.strip_prefix('+').unwrap_or(t);
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let val: i64 = t.parse().ok()?;
    i32::try_from(val).ok()
}

/// fonts.c `find_field`. With `what` set, a `what=<int>` prefix form is
/// allowed; with `what` unset a bare integer is.
fn find_field(what: Option<&str>, map: &[(i32, &str)], input: &[u8]) -> Option<i32> {
    let mut s = input;
    let mut had_prefix = false;
    if let Some(w) = what
        && s.len() > w.len()
        && s[..w.len()].eq_ignore_ascii_case(w.as_bytes())
        && s[w.len()] == b'='
    {
        s = &s[w.len() + 1..];
        had_prefix = true;
    }
    for (v, name) in map {
        if !name.is_empty() && field_matches(name, s) {
            return Some(*v);
        }
    }
    if what.is_none() || had_prefix {
        return parse_int_word(s);
    }
    None
}

/// fonts.c `parse_field`, the guts of `pango_parse_style` etc.
fn parse_field(map: &[(i32, &str)], str_: &str) -> Option<i32> {
    if str_.is_empty() {
        return None;
    }
    if field_matches("Normal", str_.as_bytes()) {
        return Some(
            map.iter()
                .find(|(_, n)| n.is_empty())
                .map_or(0, |(v, _)| *v),
        );
    }
    find_field(None, map, str_.as_bytes())
}

pub fn parse_style(s: &str) -> Option<i32> {
    parse_field(STYLE_MAP, s)
}
pub fn parse_variant(s: &str) -> Option<i32> {
    parse_field(VARIANT_MAP, s)
}
pub fn parse_weight(s: &str) -> Option<i32> {
    parse_field(WEIGHT_MAP, s)
}
pub fn parse_width(s: &str) -> Option<i32> {
    parse_field(WIDTH_MAP, s)
}

/// `pango_parse_stretch`: parsed via the width map, then collapsed onto the
/// stretch enum with fonts.c `width_to_stretch`.
pub fn parse_stretch(s: &str) -> Option<i32> {
    parse_width(s).map(width_to_stretch)
}

pub fn width_to_stretch(width: i32) -> i32 {
    let w = width * 2;
    if w < 500 + 625 {
        0
    } else if w < 625 + 750 {
        1
    } else if w < 750 + 875 {
        2
    } else if w < 875 + 1000 {
        3
    } else if w < 1000 + 1125 {
        4
    } else if w < 1125 + 1250 {
        5
    } else if w < 1250 + 1500 {
        6
    } else if w < 1500 + 2000 {
        7
    } else {
        8
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontDescription {
    pub family: Option<String>,
    pub style: i32,
    pub variant: i32,
    pub weight: i32,
    /// The width (nee stretch) field, `PangoWidth` values.
    pub width: i32,
    pub gravity: i32,
    pub color: i32,
    /// Size in pango units.
    pub size: i32,
    pub size_is_absolute: bool,
    pub variations: Option<String>,
    pub features: Option<String>,
    pub mask: u32,
}

impl Default for FontDescription {
    fn default() -> FontDescription {
        FontDescription {
            family: None,
            style: 0,
            variant: 0,
            weight: 400,
            width: 1000,
            gravity: 0,
            color: 0,
            size: 0,
            size_is_absolute: false,
            variations: None,
            features: None,
            mask: 0,
        }
    }
}

/// fonts.c `getword`: scan one word backwards from `last`, stopping at
/// whitespace or a stop character. Returns (word_start, word_len).
fn getword(str_: &[u8], last: usize, stop: &[u8]) -> (usize, usize) {
    let mut last = last;
    while last > 0 && str_[last - 1].is_ascii_whitespace() {
        last -= 1;
    }
    let mut result = last;
    while result > 0 && !str_[result - 1].is_ascii_whitespace() && !stop.contains(&str_[result - 1])
    {
        result -= 1;
    }
    (result, last - result)
}

/// fonts.c `parse_size`: float, optional `px` suffix, in `[0, 1000000]`.
fn parse_size_word(word: &[u8]) -> Option<(i32, bool)> {
    let s = std::str::from_utf8(word).ok()?;
    let (num, absolute) = match s.strip_suffix("px") {
        Some(n) => (n, true),
        None => (s, false),
    };
    if num.is_empty() {
        return None;
    }
    // g_ascii_strtod, but the word must be fully consumed.
    let size: f64 = num.parse().ok()?;
    if !(0.0..=1000000.0).contains(&size) {
        return None;
    }
    Some(((size * SCALE as f64 + 0.5) as i32, absolute))
}

impl FontDescription {
    /// `pango_font_description_from_string`. Infallible like the C, an
    /// unparsable trailer just lands in the family list.
    pub fn from_description_string(str_: &str) -> FontDescription {
        let mut desc = FontDescription {
            mask: MASK_STYLE | MASK_WEIGHT | MASK_VARIANT | MASK_WIDTH,
            ..FontDescription::default()
        };
        let bytes = str_.as_bytes();
        let mut last = bytes.len();

        // Variations (@...) or features (#...) at the end, at most one of
        // each. The first scan takes a whole space-separated word (commas
        // included), the second stops at commas, exactly like the C.
        for stop in [b"" as &[u8], b","] {
            let (p, wordlen) = getword(bytes, last, stop);
            if wordlen == 0 {
                continue;
            }
            let word = &bytes[p..p + wordlen];
            if word[0] == b'#' && desc.mask & MASK_FEATURES == 0 {
                desc.features = Some(String::from_utf8_lossy(&word[1..]).into_owned());
                desc.mask |= MASK_FEATURES;
                last = p;
            } else if word[0] == b'@' && desc.mask & MASK_VARIATIONS == 0 {
                desc.variations = Some(String::from_utf8_lossy(&word[1..]).into_owned());
                desc.mask |= MASK_VARIATIONS;
                last = p;
            }
        }

        // Size.
        let (p, wordlen) = getword(bytes, last, b",");
        if wordlen != 0
            && let Some((size, absolute)) = parse_size_word(&bytes[p..p + wordlen])
        {
            desc.size = size;
            desc.size_is_absolute = absolute;
            desc.mask |= MASK_SIZE;
            last = p;
        }

        // Style words.
        loop {
            let (p, wordlen) = getword(bytes, last, b",");
            if wordlen == 0 {
                break;
            }
            if !desc.find_field_any(&bytes[p..p + wordlen]) {
                break;
            }
            last = p;
        }

        // Remainder is the family list.
        let mut start = 0usize;
        while last > start && bytes[last - 1].is_ascii_whitespace() {
            last -= 1;
        }
        if last > start && bytes[last - 1] == b',' {
            last -= 1;
        }
        while last > start && bytes[last - 1].is_ascii_whitespace() {
            last -= 1;
        }
        while last > start && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        if start != last {
            let fam = String::from_utf8_lossy(&bytes[start..last]).into_owned();
            let fam = fam.split(',').map(str::trim).collect::<Vec<_>>().join(",");
            desc.family = Some(fam);
            desc.mask |= MASK_FAMILY;
        }

        desc
    }

    /// fonts.c `find_field_any`, one style word into the right field.
    fn find_field_any(&mut self, word: &[u8]) -> bool {
        if field_matches("Normal", word) {
            return true;
        }
        macro_rules! field {
            ($what:literal, $map:expr, $slot:expr, $mask:expr) => {
                if let Some(v) = find_field(Some($what), $map, word) {
                    $slot = v;
                    self.mask |= $mask;
                    return true;
                }
            };
        }
        field!("weight", WEIGHT_MAP, self.weight, MASK_WEIGHT);
        field!("style", STYLE_MAP, self.style, MASK_STYLE);
        field!("width", WIDTH_MAP, self.width, MASK_WIDTH);
        field!("variant", VARIANT_MAP, self.variant, MASK_VARIANT);
        field!("gravity", GRAVITY_MAP, self.gravity, MASK_GRAVITY);
        field!("color", COLOR_MAP, self.color, MASK_COLOR);
        false
    }

    /// Whether a word would be consumed as a style word, for the trailing
    /// comma rule in `to_description_string`.
    fn word_is_style(word: &[u8]) -> bool {
        if field_matches("Normal", word) {
            return true;
        }
        for (what, map) in [
            ("weight", WEIGHT_MAP),
            ("style", STYLE_MAP),
            ("width", WIDTH_MAP),
            ("variant", VARIANT_MAP),
            ("gravity", GRAVITY_MAP),
            ("color", COLOR_MAP),
        ] {
            if find_field(Some(what), map, word).is_some() {
                return true;
            }
        }
        false
    }

    /// `pango_font_description_to_string`.
    pub fn to_description_string(&self) -> String {
        let mut result = String::new();

        if let Some(family) = &self.family
            && self.mask & MASK_FAMILY != 0
        {
            result.push_str(family);
            let bytes = family.as_bytes();
            let (p, wordlen) = getword(bytes, bytes.len(), b",");
            if wordlen != 0 {
                let word = &bytes[p..p + wordlen];
                let looks_like_size = parse_size_word(word).is_some()
                    && self.weight == 400
                    && self.style == 0
                    && self.width == 1000
                    && self.variant == 0
                    && self.mask & (MASK_GRAVITY | MASK_SIZE) == 0;
                if Self::word_is_style(word) || looks_like_size {
                    result.push(',');
                }
            }
        }

        let append_field = |result: &mut String, what: &str, map: &[(i32, &str)], val: i32| {
            for (v, name) in map {
                if *v != val {
                    continue;
                }
                if !name.is_empty() {
                    if !result.is_empty() && !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.push_str(name);
                }
                return;
            }
            if !result.is_empty() && !result.ends_with(' ') {
                result.push(' ');
            }
            result.push_str(&format!("{}={}", what, val));
        };

        append_field(&mut result, "weight", WEIGHT_MAP, self.weight);
        append_field(&mut result, "style", STYLE_MAP, self.style);
        append_field(&mut result, "width", WIDTH_MAP, self.width);
        append_field(&mut result, "variant", VARIANT_MAP, self.variant);
        if self.mask & MASK_GRAVITY != 0 {
            append_field(&mut result, "gravity", GRAVITY_MAP, self.gravity);
        }
        if self.mask & MASK_COLOR != 0 {
            append_field(&mut result, "color", COLOR_MAP, self.color);
        }

        if result.is_empty() {
            result.push_str("Normal");
        }

        if self.mask & MASK_SIZE != 0 {
            if !result.ends_with(' ') {
                result.push(' ');
            }
            result.push_str(&format_nearest_multiple(
                self.size as f64 / SCALE as f64,
                1.0 / SCALE as f64,
            ));
            if self.size_is_absolute {
                result.push_str("px");
            }
        }

        if let Some(v) = &self.variations
            && self.mask & MASK_VARIATIONS != 0
            && !v.is_empty()
        {
            result.push_str(" @");
            result.push_str(v);
        }
        if let Some(f) = &self.features
            && self.mask & MASK_FEATURES != 0
            && !f.is_empty()
        {
            result.push_str(" #");
            result.push_str(f);
        }

        result
    }

    /// `pango_font_description_merge` with `replace_existing`.
    pub fn merge(&mut self, other: &FontDescription, replace: bool) {
        let new_mask = if replace {
            other.mask
        } else {
            other.mask & !self.mask
        };
        if new_mask & MASK_FAMILY != 0 {
            self.family = other.family.clone();
        }
        if new_mask & MASK_STYLE != 0 {
            self.style = other.style;
        }
        if new_mask & MASK_VARIANT != 0 {
            self.variant = other.variant;
        }
        if new_mask & MASK_WEIGHT != 0 {
            self.weight = other.weight;
        }
        if new_mask & MASK_WIDTH != 0 {
            self.width = other.width;
        }
        if new_mask & MASK_SIZE != 0 {
            self.size = other.size;
            self.size_is_absolute = other.size_is_absolute;
        }
        if new_mask & MASK_GRAVITY != 0 {
            self.gravity = other.gravity;
        }
        if new_mask & MASK_COLOR != 0 {
            self.color = other.color;
        }
        if new_mask & MASK_VARIATIONS != 0 {
            self.variations = other.variations.clone();
        }
        if new_mask & MASK_FEATURES != 0 {
            self.features = other.features.clone();
        }
        self.mask |= new_mask;
    }

    /// `pango_font_description_unset_fields`.
    pub fn unset_fields(&mut self, mask: u32) {
        let defaults = FontDescription::default();
        if mask & MASK_FAMILY != 0 {
            self.family = None;
        }
        if mask & MASK_STYLE != 0 {
            self.style = defaults.style;
        }
        if mask & MASK_VARIANT != 0 {
            self.variant = defaults.variant;
        }
        if mask & MASK_WEIGHT != 0 {
            self.weight = defaults.weight;
        }
        if mask & MASK_WIDTH != 0 {
            self.width = defaults.width;
        }
        if mask & MASK_SIZE != 0 {
            self.size = defaults.size;
            self.size_is_absolute = defaults.size_is_absolute;
        }
        if mask & MASK_GRAVITY != 0 {
            self.gravity = defaults.gravity;
        }
        if mask & MASK_COLOR != 0 {
            self.color = defaults.color;
        }
        if mask & MASK_VARIATIONS != 0 {
            self.variations = None;
        }
        if mask & MASK_FEATURES != 0 {
            self.features = None;
        }
        self.mask &= !mask;
    }

    /// `pango_font_description_set_size` (sets the mask bit, like the C).
    pub fn set_size(&mut self, size: i32) {
        self.size = size;
        self.size_is_absolute = false;
        self.mask |= MASK_SIZE;
    }

    pub fn set_absolute_size(&mut self, size: i32) {
        self.size = size;
        self.size_is_absolute = true;
        self.mask |= MASK_SIZE;
    }
}

/// fonts.c `g_ascii_format_nearest_multiple`: print `value` with just enough
/// decimals to distinguish it at `factor` granularity.
fn format_nearest_multiple(value: f64, factor: f64) -> String {
    let value = (value / factor).round() * factor;
    let eps = 0.5 * factor;
    let lo = value - eps;
    let hi = value + eps;

    if lo.floor() != hi.floor() {
        return format!("{}", value.round() as i64);
    }

    let buf1 = format!("{:.8}", lo);
    let buf2 = format!("{:.8}", hi);
    debug_assert_eq!(buf1.len(), buf2.len());
    let mut i = 0;
    let (b1, b2) = (buf1.as_bytes(), buf2.as_bytes());
    while i < b1.len() && b1[i] == b2[i] {
        i += 1;
    }
    let j = buf1.find('.').unwrap_or(0);
    format!("{:.*}", i.saturating_sub(j), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_description() {
        let d = FontDescription::from_description_string("Sans Bold Italic 12.5");
        assert_eq!(d.family.as_deref(), Some("Sans"));
        assert_eq!(d.weight, 700);
        assert_eq!(d.style, 2);
        assert_eq!(d.size, (12.5 * 1024.0 + 0.5) as i32);
        assert!(d.mask & MASK_SIZE != 0);
    }

    #[test]
    fn roundtrips_description() {
        let d = FontDescription::from_description_string("Monospace 12");
        assert_eq!(d.to_description_string(), "Monospace 12");
        let d = FontDescription::from_description_string("Cantarell Ultra-Bold Condensed 11px");
        assert_eq!(
            d.to_description_string(),
            "Cantarell Ultra-Bold Condensed 11px"
        );
    }

    #[test]
    fn field_words_match_dashless() {
        assert_eq!(parse_weight("semibold"), Some(600));
        assert_eq!(parse_weight("Semi-Bold"), Some(600));
        assert_eq!(parse_weight("550"), Some(550));
        assert_eq!(parse_weight("bogus"), None);
        assert_eq!(parse_style("ITALIC"), Some(2));
        assert_eq!(parse_stretch("expanded"), Some(6));
    }

    #[test]
    fn empty_description_is_normal() {
        let d = FontDescription::from_description_string("");
        assert_eq!(d.to_description_string(), "Normal");
    }

    #[test]
    fn size_formats_minimal_decimals() {
        assert_eq!(format_nearest_multiple(12.0, 1.0 / 1024.0), "12");
        assert_eq!(format_nearest_multiple(12.5, 1.0 / 1024.0), "12.5");
    }
}
