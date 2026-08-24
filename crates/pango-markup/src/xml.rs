// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The GMarkup XML subset (glib gmarkup.c), single-shot.
//!
//! Pango parses markup by feeding `<markup>` + input + `</markup>` through a
//! GMarkup context. This module reproduces that machine over a complete
//! input: elements with quoted attributes, the five named entities plus
//! numeric character references, comments / PIs / CDATA / DOCTYPE skipped as
//! passthrough. No DTDs, no namespaces. Errors match GMarkup's conditions
//! (messages are similar, not byte-identical).

/// A strict parse error, condition-compatible with `G_MARKUP_ERROR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Error on line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for XmlError {}

/// Event sink for the parser. Handlers may return an error string to abort
/// (pango's tag handlers do), it is wrapped into an [`XmlError`].
pub trait Events {
    fn start_element(
        &mut self,
        name: &str,
        attrs: &[(String, String)],
        line: usize,
    ) -> Result<(), String>;
    fn end_element(&mut self, name: &str) -> Result<(), String>;
    fn text(&mut self, text: &str) -> Result<(), String>;
}

fn xml_isspace(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

/// gmarkup `IS_COMMON_NAME_END_CHAR`.
fn is_name_end(b: u8) -> bool {
    b == b'=' || b == b'/' || b == b'>' || b == b' '
}

/// gmarkup name validation: alpha `_` `:` start, then alnum `.` `-` `_` `:`,
/// unicode letters allowed throughout.
fn validate_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':' || c.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| {
        c.is_ascii_alphanumeric()
            || c == '.'
            || c == '-'
            || c == '_'
            || c == ':'
            || c.is_alphabetic()
    })
}

/// gmarkup `unescape_gstring_inplace`. In attribute mode tabs and newlines
/// normalize to spaces; in text mode `\r`(`\n`) normalizes to `\n`.
pub fn unescape(s: &str, attribute: bool, line: usize) -> Result<String, XmlError> {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let err = |msg: &str| XmlError {
        line,
        message: msg.to_string(),
    };
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\t' | b'\n' if attribute => {
                out.push(' ');
                i += 1;
            }
            b'\r' => {
                out.push(if attribute { ' ' } else { '\n' });
                i += 1;
                if i < bytes.len() && bytes[i] == b'\n' {
                    i += 1;
                }
            }
            b'&' => {
                i += 1;
                let rest = &s[i..];
                if let Some(body) = rest.strip_prefix('#') {
                    let (digits, radix) = match body.strip_prefix('x') {
                        Some(h) => (h, 16),
                        None => (body, 10),
                    };
                    let len = digits
                        .bytes()
                        .take_while(|b| (*b as char).is_digit(radix))
                        .count();
                    if len == 0 {
                        return Err(err("Failed to parse character reference, expected a digit"));
                    }
                    let value = u64::from_str_radix(&digits[..len], radix)
                        .map_err(|_| err("Character reference digit is too large"))?;
                    if !digits[len..].starts_with(';') {
                        return Err(err("Character reference did not end with a semicolon"));
                    }
                    let permitted = (1..=0xd7ff).contains(&value)
                        || (0xe000..=0xfffd).contains(&value)
                        || (0x10000..=0x10ffff).contains(&value);
                    let c = permitted
                        .then(|| char::from_u32(value as u32))
                        .flatten()
                        .ok_or_else(|| {
                            err("Character reference does not encode a permitted character")
                        })?;
                    out.push(c);
                    i += (if radix == 16 { 2 } else { 1 }) + len + 1;
                } else if rest.starts_with("lt;") {
                    out.push('<');
                    i += 3;
                } else if rest.starts_with("gt;") {
                    out.push('>');
                    i += 3;
                } else if rest.starts_with("amp;") {
                    out.push('&');
                    i += 4;
                } else if rest.starts_with("quot;") {
                    out.push('"');
                    i += 5;
                } else if rest.starts_with("apos;") {
                    out.push('\'');
                    i += 5;
                } else {
                    return Err(err(
                        "Entity did not end with a semicolon, or unknown entity; \
                         escape ampersand as &amp;",
                    ));
                }
            }
            _ => {
                // Copy one whole UTF-8 sequence.
                let ch_len = utf8_len(b);
                out.push_str(&s[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    Ok(out)
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

struct Parser<'a, E: Events> {
    bytes: &'a [u8],
    input: &'a str,
    pos: usize,
    line: usize,
    tag_stack: Vec<String>,
    events: &'a mut E,
}

/// Parse pango-style wrapped markup strictly: the synthetic `<markup>` root
/// is fed around `input`, exactly like `pango_markup_parser_new_internal` +
/// `pango_markup_parser_finish`.
pub fn parse_wrapped<E: Events>(input: &str, events: &mut E) -> Result<(), XmlError> {
    let wrapped = format!("<markup>{}</markup>", input);
    let mut p = Parser {
        bytes: wrapped.as_bytes(),
        input: &wrapped,
        pos: 0,
        line: 1,
        tag_stack: Vec::new(),
        events,
    };
    p.run()
}

impl<'a, E: Events> Parser<'a, E> {
    fn err(&self, message: String) -> XmlError {
        XmlError {
            line: self.line,
            message,
        }
    }

    fn advance(&mut self) {
        if self.bytes[self.pos] == b'\n' {
            self.line += 1;
        }
        self.pos += 1;
    }

    fn skip_spaces(&mut self) {
        while self.pos < self.bytes.len() && xml_isspace(self.bytes[self.pos]) {
            self.advance();
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn read_name(&mut self) -> String {
        let start = self.pos;
        while !self.at_end() {
            let b = self.bytes[self.pos];
            if is_name_end(b) || xml_isspace(b) {
                break;
            }
            self.advance();
        }
        self.input[start..self.pos].to_string()
    }

    fn run(&mut self) -> Result<(), XmlError> {
        // STATE_START
        loop {
            self.skip_spaces();
            if self.at_end() {
                break;
            }
            if self.bytes[self.pos] != b'<' {
                return Err(
                    self.err("Document must begin with an element (e.g. <book>)".to_string())
                );
            }
            self.advance();
            self.after_open_angle()?;
            // Content loop while elements are open.
            while !self.tag_stack.is_empty() {
                if self.at_end() {
                    return Err(self.err(format!(
                        "Document ended unexpectedly with elements still open, \
                         '{}' was the last element opened",
                        self.tag_stack.last().unwrap()
                    )));
                }
                self.inside_text()?;
            }
        }
        if !self.tag_stack.is_empty() {
            return Err(self.err("Document ended unexpectedly".to_string()));
        }
        Ok(())
    }

    /// STATE_INSIDE_TEXT up to the next tag, then dispatch it.
    fn inside_text(&mut self) -> Result<(), XmlError> {
        let start = self.pos;
        let start_line = self.line;
        while !self.at_end() && self.bytes[self.pos] != b'<' {
            self.advance();
        }
        if self.at_end() {
            // Raw text at EOF with open elements, caller reports.
            return Ok(());
        }
        let text = &self.input[start..self.pos];
        if !text.is_empty() {
            let unescaped = unescape(text, false, start_line)?;
            self.events.text(&unescaped).map_err(|m| self.err(m))?;
        } else {
            // GMarkup still emits empty-adjacent text only when non-empty,
            // nothing to do.
        }
        self.advance();
        self.after_open_angle()
    }

    /// STATE_AFTER_OPEN_ANGLE.
    fn after_open_angle(&mut self) -> Result<(), XmlError> {
        if self.at_end() {
            return Err(self.err(
                "Document ended unexpectedly just after an open angle bracket '<'".to_string(),
            ));
        }
        match self.bytes[self.pos] {
            b'?' | b'!' => self.passthrough(),
            b'/' => {
                self.advance();
                self.close_tag()
            }
            b if !is_name_end(b) => self.open_tag(),
            _ => Err(self.err(
                "Invalid character following a '<' character, it may not begin an \
                 element name"
                    .to_string(),
            )),
        }
    }

    /// STATE_INSIDE_OPEN_TAG_NAME then the attribute loop.
    fn open_tag(&mut self) -> Result<(), XmlError> {
        let name = self.read_name();
        let mut attrs: Vec<(String, String)> = Vec::new();
        loop {
            // STATE_BETWEEN_ATTRIBUTES
            self.skip_spaces();
            if self.at_end() {
                return Err(self.err(format!(
                    "Document ended unexpectedly inside the start tag of element '{}'",
                    name
                )));
            }
            match self.bytes[self.pos] {
                b'/' => {
                    // STATE_AFTER_ELISION_SLASH
                    self.advance();
                    if self.at_end() || self.bytes[self.pos] != b'>' {
                        return Err(self.err(format!(
                            "Odd character, expected a '>' character to end the \
                             empty-element tag '{}'",
                            name
                        )));
                    }
                    self.advance();
                    self.emit_start(&name, &attrs)?;
                    self.events.end_element(&name).map_err(|m| self.err(m))?;
                    return Ok(());
                }
                b'>' => {
                    self.advance();
                    self.emit_start(&name, &attrs)?;
                    self.tag_stack.push(name);
                    return Ok(());
                }
                b if !is_name_end(b) => {
                    // STATE_INSIDE_ATTRIBUTE_NAME / AFTER_ATTRIBUTE_NAME
                    let attr_name = self.read_name();
                    self.skip_spaces();
                    if !validate_name(&attr_name) {
                        return Err(self.err(format!("'{}' is not a valid name", attr_name)));
                    }
                    if self.at_end() || self.bytes[self.pos] != b'=' {
                        return Err(self.err(format!(
                            "Odd character, expected a '=' after attribute name '{}' \
                             of element '{}'",
                            attr_name, name
                        )));
                    }
                    self.advance();
                    // STATE_AFTER_ATTRIBUTE_EQUALS_SIGN
                    self.skip_spaces();
                    if self.at_end() {
                        return Err(self.err("Document ended unexpectedly".to_string()));
                    }
                    let delim = self.bytes[self.pos];
                    if delim != b'"' && delim != b'\'' {
                        return Err(self.err(format!(
                            "Odd character, expected an open quote mark after the \
                             equals sign when giving value for attribute '{}' of \
                             element '{}'",
                            attr_name, name
                        )));
                    }
                    self.advance();
                    let vstart = self.pos;
                    let vline = self.line;
                    while !self.at_end() && self.bytes[self.pos] != delim {
                        self.advance();
                    }
                    if self.at_end() {
                        return Err(self.err(format!(
                            "Document ended unexpectedly while inside an attribute \
                             value of element '{}'",
                            name
                        )));
                    }
                    let raw = &self.input[vstart..self.pos];
                    self.advance();
                    let value = unescape(raw, true, vline)?;
                    attrs.push((attr_name, value));
                }
                _ => {
                    return Err(self.err(format!(
                        "Odd character, expected a '>' or '/' character to end the \
                         start tag of element '{}', or optionally an attribute",
                        name
                    )));
                }
            }
        }
    }

    fn emit_start(&mut self, name: &str, attrs: &[(String, String)]) -> Result<(), XmlError> {
        if !validate_name(name) {
            return Err(self.err(format!("'{}' is not a valid name", name)));
        }
        let line = self.line;
        self.events
            .start_element(name, attrs, line)
            .map_err(|m| self.err(m))
    }

    /// STATE_AFTER_CLOSE_TAG_SLASH onwards.
    fn close_tag(&mut self) -> Result<(), XmlError> {
        if self.at_end() || is_name_end(self.bytes[self.pos]) {
            return Err(self.err(
                "Invalid character following the characters '</', may not begin an \
                 element name"
                    .to_string(),
            ));
        }
        let name = self.read_name();
        self.skip_spaces();
        if self.at_end() {
            return Err(self.err("Document ended unexpectedly".to_string()));
        }
        if self.bytes[self.pos] != b'>' {
            return Err(self.err(format!(
                "Invalid character following the close element name '{}', the allowed \
                 character is '>'",
                name
            )));
        }
        match self.tag_stack.last() {
            None => Err(self.err(format!(
                "Element '{}' was closed, no element is currently open",
                name
            ))),
            Some(open) if *open != name => Err(self.err(format!(
                "Element '{}' was closed, but the currently open element is '{}'",
                name, open
            ))),
            Some(_) => {
                self.advance();
                self.events.end_element(&name).map_err(|m| self.err(m))?;
                self.tag_stack.pop();
                Ok(())
            }
        }
    }

    /// STATE_INSIDE_PASSTHROUGH, entered at the '?' or '!'. The open angle
    /// was already consumed. Comments, PIs, CDATA and DOCTYPE are skipped
    /// (pango installs no passthrough handler).
    fn passthrough(&mut self) -> Result<(), XmlError> {
        let start = self.pos - 1;
        let mut balance = 1i32;
        while !self.at_end() {
            let b = self.bytes[self.pos];
            if b == b'<' {
                balance += 1;
            }
            if b == b'>' {
                balance -= 1;
                let chunk = &self.bytes[start..self.pos];
                let done = (chunk.len() >= 2 && chunk[1] == b'?' && chunk[chunk.len() - 1] == b'?')
                    || (chunk.starts_with(b"<!--") && chunk.ends_with(b"--"))
                    || (chunk.starts_with(b"<![CDATA[") && chunk.ends_with(b"]]"))
                    || (chunk.starts_with(b"<!DOCTYPE") && balance == 0);
                if done {
                    self.advance();
                    return Ok(());
                }
            }
            self.advance();
        }
        Err(self.err(
            "Document ended unexpectedly inside a comment or processing instruction".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl Events for Log {
        fn start_element(
            &mut self,
            name: &str,
            attrs: &[(String, String)],
            _line: usize,
        ) -> Result<(), String> {
            let attrs: Vec<String> = attrs.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            self.0.push(format!("<{} {}>", name, attrs.join(",")));
            Ok(())
        }
        fn end_element(&mut self, name: &str) -> Result<(), String> {
            self.0.push(format!("</{}>", name));
            Ok(())
        }
        fn text(&mut self, text: &str) -> Result<(), String> {
            self.0.push(format!("T:{}", text));
            Ok(())
        }
    }

    fn events(input: &str) -> Result<Vec<String>, XmlError> {
        let mut log = Log::default();
        parse_wrapped(input, &mut log).map(|_| log.0)
    }

    #[test]
    fn parses_nested_tags_and_entities() {
        let ev = events("a<b>x &amp; &#65;</b>c").unwrap();
        assert_eq!(
            ev,
            vec![
                "<markup >",
                "T:a",
                "<b >",
                "T:x & A",
                "</b>",
                "T:c",
                "</markup>",
            ]
        );
    }

    #[test]
    fn attribute_values_normalize_whitespace() {
        let ev = events("<span size=\"lar\nge\">x</span>").unwrap();
        assert_eq!(ev[1], "<span size=lar ge>");
    }

    #[test]
    fn self_close_and_quotes() {
        let ev = events("<span foreground='red'/>").unwrap();
        assert_eq!(ev[1], "<span foreground=red>");
        assert_eq!(ev[2], "</span>");
    }

    #[test]
    fn comments_and_pi_skipped() {
        let ev = events("a<!-- <x> -->b<?pi ?>c").unwrap();
        assert_eq!(ev[1..4], ["T:a", "T:b", "T:c"]);
    }

    #[test]
    fn errors() {
        assert!(events("<i>unclosed").is_err());
        assert!(events("</b>stray").is_err());
        assert!(events("<i>x</b>").is_err());
        assert!(events("a & b").is_err());
        assert!(events("<span size=big>x</span>").is_err());
        assert!(events("&#xD800;").is_err());
        assert!(events("&#0;").is_err());
    }

    #[test]
    fn char_ref_edge_values() {
        assert!(events("&#x10FFFF;").is_ok());
        assert!(events("&#xFFFE;").is_err());
        assert!(events("&#55295;").is_ok());
    }
}
