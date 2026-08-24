// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Colors with pango's 16-bit-per-channel representation.
//!
//! Mirrors `pango_color_parse_with_alpha` (pango-color.c). Hex digits are
//! expanded to 16 bits by bit replication, named colors come from the CSS
//! table shared with `subparse-formats` (8-bit values, `* 257` is exactly the
//! replication result).

use subparse_formats::ir::NAMED_COLORS;

/// An sRGB color with pango's 16-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub red: u16,
    pub green: u16,
    pub blue: u16,
}

impl Color {
    /// `pango_color_to_string` form, `#rrrrggggbbbb`.
    pub fn to_hex_string(self) -> String {
        format!("#{:04x}{:04x}{:04x}", self.red, self.green, self.blue)
    }

    /// Collapse to 8-bit channels (top byte of each).
    pub fn to_rgb8(self) -> (u8, u8, u8) {
        (
            (self.red >> 8) as u8,
            (self.green >> 8) as u8,
            (self.blue >> 8) as u8,
        )
    }
}

fn hex_channel(s: &[u8]) -> Option<u32> {
    let mut v = 0u32;
    for &b in s {
        let d = (b as char).to_digit(16)?;
        v = (v << 4) | d;
    }
    Some(v)
}

/// Replicate an n-bit channel value to 16 bits, as pango does.
fn replicate(mut v: u32, mut bits: u32) -> u16 {
    v <<= 16 - bits;
    while bits < 16 {
        v |= v >> bits;
        bits *= 2;
    }
    v as u16
}

/// `pango_color_parse_with_alpha`. `want_alpha` corresponds to passing a
/// non-NULL alpha out param, without it the alpha hex forms are rejected.
/// Returns `(color, alpha)`, alpha `0xffff` when unspecified.
pub fn parse_with_alpha(spec: &str, want_alpha: bool) -> Option<(Color, u16)> {
    if let Some(hex) = spec.strip_prefix('#') {
        let bytes = hex.as_bytes();
        let (per, has_alpha) = match bytes.len() {
            3 | 6 | 9 | 12 => (bytes.len() / 3, false),
            4 | 8 | 16 if want_alpha => (bytes.len() / 4, true),
            _ => return None,
        };
        let r = hex_channel(&bytes[..per])?;
        let g = hex_channel(&bytes[per..per * 2])?;
        let b = hex_channel(&bytes[per * 2..per * 3])?;
        let bits = (per * 4) as u32;
        let alpha = if has_alpha {
            replicate(hex_channel(&bytes[per * 3..])?, bits)
        } else {
            0xffff
        };
        Some((
            Color {
                red: replicate(r, bits),
                green: replicate(g, bits),
                blue: replicate(b, bits),
            },
            alpha,
        ))
    } else {
        // Named lookup skips spaces and matches case-insensitively, like
        // pango's compare_xcolor_entries.
        let name: String = spec
            .chars()
            .filter(|c| *c != ' ')
            .map(|c| c.to_ascii_lowercase())
            .collect();
        NAMED_COLORS
            .binary_search_by(|(n, _)| n.cmp(&name.as_str()))
            .ok()
            .map(|i| {
                let v = NAMED_COLORS[i].1;
                let chan = |x: u32| (x as u16) * 257;
                (
                    Color {
                        red: chan((v >> 16) & 0xff),
                        green: chan((v >> 8) & 0xff),
                        blue: chan(v & 0xff),
                    },
                    0xffff,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_forms_replicate() {
        let (c, a) = parse_with_alpha("#fff", false).unwrap();
        assert_eq!(
            (c.red, c.green, c.blue, a),
            (0xffff, 0xffff, 0xffff, 0xffff)
        );
        let (c, _) = parse_with_alpha("#8000ff", false).unwrap();
        assert_eq!((c.red, c.green, c.blue), (0x8080, 0x0000, 0xffff));
        let (c, _) = parse_with_alpha("#123456789abc", false).unwrap();
        assert_eq!((c.red, c.green, c.blue), (0x1234, 0x5678, 0x9abc));
    }

    #[test]
    fn alpha_forms_need_want_alpha() {
        assert!(parse_with_alpha("#f00f", false).is_none());
        let (c, a) = parse_with_alpha("#f00f", true).unwrap();
        assert_eq!((c.red, a), (0xffff, 0xffff));
        let (_, a) = parse_with_alpha("#ff000080", true).unwrap();
        assert_eq!(a, 0x8080);
    }

    #[test]
    fn named_colors_skip_spaces() {
        let (c, _) = parse_with_alpha("light blue", false).unwrap();
        assert_eq!(
            (c.red, c.green, c.blue),
            (0xad * 257, 0xd8 * 257, 0xe6 * 257)
        );
        assert!(parse_with_alpha("no such color", false).is_none());
    }

    #[test]
    fn to_string_is_16bit_hex() {
        let (c, _) = parse_with_alpha("blue", false).unwrap();
        assert_eq!(c.to_hex_string(), "#00000000ffff");
    }
}
