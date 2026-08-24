// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Recovering scanner for markup from the wild.
//!
//! Where the strict GMarkup machine rejects the whole string, this scanner
//! degrades locally: a `<` that does not open a plausible tag stays literal
//! text, unknown entities keep their ampersand, a stray close tag is
//! ignored, a crossing close tag implicitly closes what is nested inside it,
//! and end of input closes everything still open. It emits a balanced
//! [`Events`] stream, the semantics layer runs in tolerant mode on top.

use crate::xml::Events;

/// Decode one entity at `s` (which starts with `&`). Returns the decoded
/// char and bytes consumed, or `None` to keep the `&` literal.
fn decode_entity(s: &str) -> Option<(char, usize)> {
    let rest = &s[1..];
    for (name, c) in [
        ("lt;", '<'),
        ("gt;", '>'),
        ("amp;", '&'),
        ("quot;", '"'),
        ("apos;", '\''),
    ] {
        if rest.starts_with(name) {
            return Some((c, 1 + name.len()));
        }
    }
    let body = rest.strip_prefix('#')?;
    let (digits, radix, prefix) = match body.strip_prefix(['x', 'X']) {
        Some(h) => (h, 16, 3),
        None => (body, 10, 2),
    };
    let len = digits
        .bytes()
        .take_while(|b| (*b as char).is_digit(radix))
        .count();
    if len == 0 || len > 7 || !digits[len..].starts_with(';') {
        return None;
    }
    let value = u32::from_str_radix(&digits[..len], radix).ok()?;
    char::from_u32(value).map(|c| (c, prefix + len + 1))
}

/// Push text with entity decoding and newline normalization.
fn flush_text<E: Events>(events: &mut E, raw: &str) {
    if raw.is_empty() {
        return;
    }
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'&' => match decode_entity(&raw[i..]) {
                Some((c, used)) => {
                    out.push(c);
                    i += used;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            },
            b'\r' => {
                out.push('\n');
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b'&' | b'\r') {
                    i += 1;
                }
                out.push_str(&raw[start..i]);
            }
        }
    }
    // The sink never errors in tolerant mode.
    let _ = events.text(&out);
}

/// Decode an attribute value: entities plus GMarkup's whitespace
/// normalization (tab, newline, return all become spaces).
fn decode_attr_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'&' => match decode_entity(&raw[i..]) {
                Some((c, used)) => {
                    out.push(c);
                    i += used;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            },
            b'\t' | b'\n' => {
                out.push(' ');
                i += 1;
            }
            b'\r' => {
                out.push(' ');
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b'&' | b'\t' | b'\n' | b'\r') {
                    i += 1;
                }
                out.push_str(&raw[start..i]);
            }
        }
    }
    out
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':')
}

/// A parsed tag body, `<...>` exclusive of the angle brackets.
enum Tag {
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_close: bool,
    },
    Close(String),
    /// Not a tag after all, keep it literal.
    Literal,
}

fn parse_tag_body(body: &str) -> Tag {
    let bytes = body.as_bytes();
    let (closing, mut i) = match bytes.first() {
        Some(b'/') => (true, 1usize),
        _ => (false, 0usize),
    };
    // Comments and PIs are skipped silently by the strict layer; treat any
    // <!... or <?... likewise (never literal text, never a tag).
    if matches!(bytes.first(), Some(b'!') | Some(b'?')) {
        return Tag::Close(String::new());
    }
    if i >= bytes.len() || !is_name_start(bytes[i]) {
        return Tag::Literal;
    }
    let name_start = i;
    while i < bytes.len() && is_name_byte(bytes[i]) {
        i += 1;
    }
    let name = body[name_start..i].to_string();
    if closing {
        // Anything after the name is ignored.
        return Tag::Close(name);
    }

    let mut attrs = Vec::new();
    let mut self_close = false;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'/' && i == bytes.len() - 1 {
            self_close = true;
            break;
        }
        if !is_name_start(bytes[i]) {
            // Junk where an attribute should start, drop the rest.
            break;
        }
        let astart = i;
        while i < bytes.len() && is_name_byte(bytes[i]) {
            i += 1;
        }
        let aname = body[astart..i].to_string();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            // Valueless attribute, drop it and the rest.
            break;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let value = if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            let vstart = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let v = &body[vstart..i];
            if i < bytes.len() {
                i += 1;
            }
            v
        } else {
            // Unquoted value, HTML-style leniency: up to whitespace.
            let vstart = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            &body[vstart..i]
        };
        attrs.push((aname, decode_attr_value(value)));
    }

    Tag::Open {
        name,
        attrs,
        self_close,
    }
}

/// Scan `input`, emitting a balanced event stream. Never fails.
pub fn parse<E: Events>(input: &str, events: &mut E) {
    let bytes = input.as_bytes();
    let mut open: Vec<String> = Vec::new();
    let mut pos = 0usize;
    let mut text_start = 0usize;

    while pos < bytes.len() {
        if bytes[pos] != b'<' {
            pos += 1;
            continue;
        }
        // Comments and PIs may contain '>', scan for their real terminator.
        for (open_mark, close_mark) in [("<!--", "-->"), ("<?", "?>")] {
            if input[pos..].starts_with(open_mark) {
                flush_text(events, &input[text_start..pos]);
                let after = pos + open_mark.len();
                pos = match input[after..].find(close_mark) {
                    Some(r) => after + r + close_mark.len(),
                    None => bytes.len(),
                };
                text_start = pos;
            }
        }
        if pos >= bytes.len() || bytes[pos] != b'<' {
            continue;
        }
        let Some(rel) = input[pos + 1..].find('>') else {
            break;
        };
        let body = &input[pos + 1..pos + 1 + rel];
        match parse_tag_body(body) {
            Tag::Literal => {
                pos += 1;
                continue;
            }
            tag => {
                flush_text(events, &input[text_start..pos]);
                match tag {
                    Tag::Open {
                        name,
                        attrs,
                        self_close,
                    } => {
                        let _ = events.start_element(&name, &attrs, 1);
                        if self_close {
                            let _ = events.end_element(&name);
                        } else {
                            open.push(name);
                        }
                    }
                    Tag::Close(name) => {
                        if !name.is_empty()
                            && let Some(at) = open.iter().rposition(|n| *n == name)
                        {
                            while open.len() > at {
                                let n = open.pop().unwrap();
                                let _ = events.end_element(&n);
                            }
                        }
                        // Stray closes, comments and PIs vanish.
                    }
                    Tag::Literal => unreachable!(),
                }
                pos += rel + 2;
                text_start = pos;
            }
        }
    }

    flush_text(events, &input[text_start..]);
    while let Some(n) = open.pop() {
        let _ = events.end_element(&n);
    }
}

#[cfg(test)]
mod tests {
    use crate::markup::parse_markup_tolerant;

    #[test]
    fn plain_survives() {
        let p = parse_markup_tolerant("hello <3 world & such");
        assert_eq!(p.text, "hello <3 world & such");
        assert!(p.attrs.attrs.is_empty());
    }

    #[test]
    fn unclosed_and_stray_tags() {
        let p = parse_markup_tolerant("<i>seven");
        assert_eq!(p.text, "seven");
        assert_eq!(
            crate::attr::attr_to_string(&p.attrs.attrs[0]),
            "0 5 style italic"
        );

        let p = parse_markup_tolerant("</b>text");
        assert_eq!(p.text, "text");
        assert!(p.attrs.attrs.is_empty());
    }

    #[test]
    fn crossing_tags_close_inner() {
        let p = parse_markup_tolerant("<b>a<i>b</b>c</i>");
        assert_eq!(p.text, "abc");
        let dump: Vec<String> = p
            .attrs
            .attrs
            .iter()
            .map(crate::attr::attr_to_string)
            .collect();
        assert_eq!(dump, vec!["0 2 weight bold", "1 2 style italic"]);
    }

    #[test]
    fn unknown_tags_are_transparent() {
        let p = parse_markup_tolerant("a<c.yellow>b</c>c");
        assert_eq!(p.text, "abc");
        assert!(p.attrs.attrs.is_empty());
    }

    #[test]
    fn bad_values_dropped_good_kept() {
        let p = parse_markup_tolerant("<span foreground=\"bogus\" weight=\"bold\">x</span>");
        assert_eq!(p.text, "x");
        let dump: Vec<String> = p
            .attrs
            .attrs
            .iter()
            .map(crate::attr::attr_to_string)
            .collect();
        assert_eq!(dump, vec!["0 1 weight bold"]);
    }

    #[test]
    fn unquoted_values_and_entities() {
        let p = parse_markup_tolerant("<span foreground=red>r &amp; b &bogus;</span>");
        assert_eq!(p.text, "r & b &bogus;");
        assert_eq!(
            crate::attr::attr_to_string(&p.attrs.attrs[0]),
            "0 13 foreground #ffff00000000"
        );
    }

    #[test]
    fn comments_vanish() {
        let p = parse_markup_tolerant("a<!-- x -->b");
        assert_eq!(p.text, "ab");
    }
}
