// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! SAMI (Synchronized Accessible Media Interchange) parser.
//!
//! SAMI is an SGML/HTML-like subtitle format. Real-world SAMI files are messy
//! and non-conformant, so this parser deliberately mirrors the tolerant behavior
//! of the upstream GStreamer C parser
//! (`gst-plugins-base/gst/subparse/samiparse.c`) rather than any strict grammar.
//! See `specs/sami.md`.
//!
//! Shape: **lex -> parse**.
//!
//! * The lexer ([`SamiContext::feed`]) splits an (already entity-unescaped) line
//!   into three token kinds (start/self-close/end **tags** `<...>` and **text**
//!   runs), buffering an unterminated tag across lines exactly like the C
//!   `html_context_parse`.
//! * The parser is a small state machine driven by a **tag stack**
//!   ([`SamiContext::state`]). Nesting is handled by pushing a marker per open
//!   `<font>/<i>/<ruby>/<rt>` tag and, on `<sync>`/close, popping and emitting the
//!   matching pango-markup closers (this is the forgiving equivalent of a Pratt
//!   parser for a grammar whose real inputs almost never nest cleanly).
//!
//! Output is **pango-markup**. Entities are resolved first. XML entities are
//! re-escaped so they survive as markup, HTML/numeric entities become UTF-8, and
//! a bare `&` becomes `&amp;`.

use crate::cue::{Cue, CueSettings, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

/// Nanoseconds per millisecond (SAMI `Start=` attributes are milliseconds).
const NS_PER_MS: u64 = 1_000_000;

/// Sentinel for "no end time", mirroring `GST_CLOCK_TIME_NONE`. Surfaces as
/// `Cue::end_ns == None`.
const TIME_NONE: u64 = u64::MAX;

// Tag-stack markers (one byte per open element), matching the C `#define`s.
const ITALIC_TAG: u8 = b'i';
const SPAN_TAG: u8 = b's';
const RUBY_TAG: u8 = b'r';
const RT_TAG: u8 = b't';
/// Never pushed. `pop_state(CLEAR_TAG)` closes *every* open tag.
const CLEAR_TAG: u8 = b'0';

/// Max tag-stack depth before we bail out (DoS guard, matches the C's `> 64`).
const MAX_NESTING: usize = 64;

/// Parser for the SAMI subtitle format. Emits pango-markup.
///
/// Streaming: SAMI is tag-structured rather than record-structured, so
/// "incremental" here means a streaming tag scan. [`SamiContext`] already *was*
/// one (it carries a partial tag in `lexbuf` across line boundaries and emits a
/// cue when a `<sync>` closes the previous block); the only change is that the
/// context now lives for the whole stream instead of one `parse` call.
#[derive(Debug, Default)]
pub struct Sami {
    lines: LineScanner,
    ctx: SamiContext,
}

impl SubtitleFormat for Sami {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, ctx } = self;

        // The C element feeds the parser one '\n'-terminated line at a time and
        // leaves any unterminated trailing remainder in its buffer (SAMI is not
        // in the EOS force-flush list), so we process only complete lines.
        let mut cues = Vec::new();
        let mut consumed = lines.feed(body, |line| {
            if let Some(cue) = ctx.parse_line(line) {
                cues.push(cue);
            }
        });

        if at_eos {
            // The unterminated remainder is dropped, matching `get_next_line`.
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }
}

/// The streaming SAMI state machine. One instance parses a whole body.
#[derive(Debug, Default)]
struct SamiContext {
    /// Content of the current `<sync>` block being assembled (pango-markup).
    buf: String,
    /// Ruby annotation content, prepended to the result on flush.
    rubybuf: String,
    /// Finished-but-not-yet-emitted content (moved here when the next `<sync>`
    /// opens, so following content does not get appended to it).
    resultbuf: String,
    /// Tag stack, one marker byte per open `<font>/<i>/<ruby>/<rt>`.
    state: Vec<u8>,
    /// Lexer carry-over, an unterminated tag `<...` awaiting its `>`.
    lexbuf: String,
    /// A finished cue is ready in `resultbuf`.
    has_result: bool,
    /// Inside a `<sync>` element. Only then is text captured.
    in_sync: bool,
    /// Previous `<sync>` start (cue start), nanoseconds.
    time1: u64,
    /// Current `<sync>` start (cue end), nanoseconds, or `TIME_NONE`.
    time2: u64,
}

impl SamiContext {
    /// Feed one raw line. Returns a cue if one just completed.
    fn parse_line(&mut self, line: &str) -> Option<Cue> {
        let unescaped = unescape_string(line);
        if !self.feed(&unescaped) {
            // Malformed markup. Drop accumulated state and continue, like the C.
            self.reset();
            return None;
        }

        if self.has_result {
            if !self.rubybuf.is_empty() {
                self.rubybuf.push('\n');
                let mut merged = std::mem::take(&mut self.rubybuf);
                merged.push_str(&self.resultbuf);
                self.resultbuf = merged;
            }
            let text = std::mem::take(&mut self.resultbuf);
            let start_ns = self.time1;
            let end_ns = if self.time2 == TIME_NONE {
                None
            } else {
                Some(self.time2)
            };
            self.has_result = false;
            return Some(Cue {
                start_ns,
                end_ns,
                text,
                settings: CueSettings::default(),
                id: None,
                ssa: None,
            });
        }
        None
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.rubybuf.clear();
        self.resultbuf.clear();
        self.state.clear();
        self.lexbuf.clear();
        self.has_result = false;
        self.in_sync = false;
        self.time1 = 0;
        self.time2 = 0;
    }

    /// Lexer. Split `text` (appended to any carried-over partial tag) into tag
    /// and text tokens and dispatch them. Returns `false` on malformed markup.
    ///
    /// Faithfully reproduces `html_context_parse`, including its quirk of
    /// retaining the whole buffer (and thus re-emitting already-seen text) when a
    /// tag is left unterminated at end of line.
    fn feed(&mut self, text: &str) -> bool {
        let mut buf = std::mem::take(&mut self.lexbuf);
        buf.push_str(text);
        let mut pos = 0usize;
        loop {
            let rest = &buf[pos..];
            if rest.as_bytes().first() == Some(&b'<') {
                match rest.find('>') {
                    None => {
                        // Unterminated tag. Keep the buffer, resume next line.
                        self.lexbuf = buf;
                        return true;
                    }
                    Some(gt) => {
                        let element = &rest[..gt];
                        if !self.dispatch_element(element) {
                            return false; // lexbuf stays empty (already taken)
                        }
                        pos += gt + 1;
                    }
                }
            } else if let Some(lt) = rest.find('<') {
                let tok = ascii_strip(&rest[..lt]);
                self.text(tok);
                pos += lt;
            } else {
                let tok = ascii_strip(rest);
                self.text(tok);
                return true; // lexbuf cleared (already taken)
            }
        }
    }

    /// Classify a `<...>` token (comment / self-close / end / start) and run it.
    fn dispatch_element(&mut self, element: &str) -> bool {
        if element.starts_with("<!") {
            // Comment, DOCTYPE, CDATA, etc. Skipped.
            true
        } else if element.ends_with('/') {
            // `<blah/>`
            let name = &element[1..element.len() - 1];
            is_valid_element_name(name) && self.handle_element(name, true)
        } else if element.as_bytes().get(1) == Some(&b'/') {
            // `</blah>`
            let name = &element[2..];
            is_valid_element_name(name) && self.end_element(name)
        } else {
            // `<blah>`
            let name = &element[1..];
            is_valid_element_name(name) && self.handle_element(name, false)
        }
    }

    /// Parse an element's name + attributes and fire start (and, if self-closing,
    /// end). Mirrors `html_context_handle_element`, including its crude, bug-for-
    /// bug attribute counting.
    fn handle_element(&mut self, string: &str, must_close: bool) -> bool {
        let (name, after) = match string.find(' ') {
            Some(sp) => (&string[..sp], Some(&string[sp..])),
            None => (string, None),
        };

        let mut attrs: Vec<(String, String)> = Vec::new();
        if let Some(after) = after {
            // Count of `=` after the first space bounds the (flat-indexed) loop.
            let count = after[1..].matches('=').count();
            let mut i = 0usize;
            let mut cursor = &after[1..];
            while i < count {
                let eq = match cursor.find('=') {
                    Some(e) => e,
                    None => break,
                };
                let attr_name = &cursor[..eq];
                if !is_valid_attribute_name(attr_name) {
                    return false;
                }
                let after_eq = &cursor[eq + 1..];
                let (val_raw, adv) = match after_eq.find(' ') {
                    Some(sp) => (&after_eq[..sp], Some(sp)),
                    None => (after_eq, None),
                };
                attrs.push((attr_name.to_string(), strip_quotes(val_raw)));
                i += 2;
                match adv {
                    Some(sp) => cursor = &after_eq[sp + 1..],
                    None => break,
                }
            }
        }

        if !self.start_element(name, &attrs) {
            return false;
        }
        if must_close && !self.end_element(name) {
            return false;
        }
        true
    }

    fn start_element(&mut self, name: &str, attrs: &[(String, String)]) -> bool {
        // DoS guard. Refuse absurd nesting (resets the context upstream).
        if self.state.len() > MAX_NESTING {
            return false;
        }

        if name.eq_ignore_ascii_case("sync") {
            self.handle_start_sync(attrs);
            self.in_sync = true;
        } else if name.eq_ignore_ascii_case("font") {
            self.handle_start_font(attrs);
        } else if name.eq_ignore_ascii_case("ruby") {
            self.state.push(RUBY_TAG);
        } else if name.eq_ignore_ascii_case("br") {
            self.buf.push('\n');
        } else if name.eq_ignore_ascii_case("rt") {
            if self.state.contains(&ITALIC_TAG) {
                self.rubybuf.push_str("<i>");
            }
            self.rubybuf.push_str("<span size='xx-small' rise='-100'>");
            self.state.push(RT_TAG);
        } else if name.eq_ignore_ascii_case("i") {
            self.buf.push_str("<i>");
            self.state.push(ITALIC_TAG);
        }
        // "p" and anything else is a no-op.
        true
    }

    fn end_element(&mut self, name: &str) -> bool {
        if name.eq_ignore_ascii_case("sync") {
            self.in_sync = false;
        } else if name.eq_ignore_ascii_case("body") || name.eq_ignore_ascii_case("sami") {
            // Flush the final block. The last cue has no following <sync>.
            if !self.buf.is_empty() {
                if self.resultbuf.is_empty() {
                    self.time1 = self.time2;
                }
                self.time2 = TIME_NONE;
                let buf = std::mem::take(&mut self.buf);
                self.resultbuf.push_str(&buf);
                self.has_result = !self.resultbuf.is_empty();
            }
        } else if name.eq_ignore_ascii_case("font") {
            self.pop_state(SPAN_TAG);
        } else if name.eq_ignore_ascii_case("ruby") {
            self.pop_state(RUBY_TAG);
        } else if name.eq_ignore_ascii_case("i") {
            self.pop_state(ITALIC_TAG);
        }
        true
    }

    fn text(&mut self, text: &str) {
        if !self.in_sync {
            return;
        }
        if self.state.contains(&RT_TAG) {
            self.rubybuf.push(' ');
            self.rubybuf.push_str(text);
            self.rubybuf.push(' ');
        } else {
            self.buf.push_str(text);
        }
    }

    fn handle_start_sync(&mut self, attrs: &[(String, String)]) {
        self.pop_state(CLEAR_TAG);
        for (key, value) in attrs {
            if key.eq_ignore_ascii_case("start") {
                // Only advance the start time if nothing is pending.
                if self.resultbuf.is_empty() {
                    self.time1 = self.time2;
                }
                let ms = atoi(value);
                self.time2 = (ms as u64).wrapping_mul(NS_PER_MS).max(self.time1);
                let buf = std::mem::take(&mut self.buf);
                self.resultbuf.push_str(&buf);
                self.has_result = !self.resultbuf.is_empty();
            }
        }
    }

    fn handle_start_font(&mut self, attrs: &[(String, String)]) {
        self.pop_state(SPAN_TAG);
        // The C always emits a span for <font> (its attr array is never NULL).
        self.buf.push_str("<span");
        for (key, value) in attrs {
            if key.eq_ignore_ascii_case("color") {
                // Fix up hex colours that forgot their '#', and map the handful
                // of X11 names pango does not know.
                let mut sharp = "";
                let is_hashed7 = value.starts_with('#') && value.len() == 7;
                if !is_hashed7 && looks_like_hex6(value) {
                    sharp = "#";
                }
                let mapped = map_named_color(value).unwrap_or(value);
                self.buf.push_str(" foreground=\"");
                self.buf.push_str(sharp);
                self.buf.push_str(mapped);
                self.buf.push('"');
            } else if key.eq_ignore_ascii_case("face") {
                self.buf.push_str(" font_family=\"");
                self.buf.push_str(value);
                self.buf.push('"');
            }
        }
        self.buf.push('>');
        self.state.push(SPAN_TAG);
    }

    /// Walk the tag stack from the top, emitting pango closers, until `target` is
    /// found (then truncate above it). For `CLEAR_TAG`, close everything instead.
    /// Mirrors `sami_context_pop_state`.
    ///
    /// Deliberate deviation from the C for the "target is not open" case. The C
    /// walks the stack first and discards the `</i>`/`</span>` closers it
    /// collected once the target turns out never to have been opened, but its
    /// `<rt>` arm appends to `rubybuf` as it walks, so that append survives the
    /// discarded walk. A stray `</font>` inside a `<ruby>` therefore leaves an
    /// extra `</span>` behind and pango rejects the whole cue as invalid markup.
    /// We locate the target up front instead and do nothing at all when it is not
    /// open, which keeps the output balanced. The found case (and `CLEAR_TAG`) is
    /// unchanged. See `specs/sami.md`.
    fn pop_state(&mut self, target: u8) {
        let stop = if target == CLEAR_TAG {
            // CLEAR_TAG is never pushed. It means "close everything".
            0
        } else {
            match self.state.iter().rposition(|&tag| tag == target) {
                Some(i) => i,
                None => return,
            }
        };

        let has_italic = self.state.contains(&ITALIC_TAG);
        let mut closers = String::new();
        for i in (stop..self.state.len()).rev() {
            match self.state[i] {
                ITALIC_TAG => closers.push_str("</i>"),
                SPAN_TAG => closers.push_str("</span>"),
                RT_TAG => {
                    self.rubybuf.push_str("</span>");
                    if has_italic {
                        self.rubybuf.push_str("</i>");
                    }
                }
                // RUBY_TAG has no pango equivalent, so it closes silently.
                _ => {}
            }
        }
        self.buf.push_str(&closers);
        self.state.truncate(stop);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (the "lexer" primitives) for entity unescaping, name validation,
// attribute-value cleanup, and small C-runtime look-alikes.
// ---------------------------------------------------------------------------

/// Resolve entities and collapse whitespace in a single line, matching
/// `unescape_string`:
/// * `&nbsp`/`&nbsp;` -> U+00A0 (case-insensitive, semicolon optional)
/// * XML entities (`quot amp apos lt gt`) are re-escaped to canonical
///   `&name;` (case-insensitive, semicolon required) so pango keeps them literal
/// * HTML entities -> UTF-8 (case-sensitive, semicolon required)
/// * `&#dd;` / `&#xhh;` numeric refs -> UTF-8 (semicolon optional)
/// * any other `&` -> `&amp;`
/// * every run of ASCII whitespace -> a single space
fn unescape_string(text: &str) -> String {
    let b = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'&' {
            i += 1;

            // &nbsp / &nbsp;
            if b.len() - i >= 4 && ascii_eq_ci(&b[i..i + 4], b"nbsp") {
                push_unichar(&mut out, 0xA0);
                i += 4;
                if i < b.len() && b[i] == b';' {
                    i += 1;
                }
                continue;
            }

            // XML entities: pass through re-escaped for pango.
            if let Some(name) = match_entity(&b[i..], XML_ENTITIES, true) {
                out.push(b'&');
                out.extend_from_slice(name.as_bytes());
                out.push(b';');
                i += name.len() + 1;
                continue;
            }

            // HTML entities: resolve to UTF-8.
            if let Some((cp, name)) = match_html_entity(&b[i..]) {
                push_unichar(&mut out, cp);
                i += name.len() + 1;
                continue;
            }

            // Numeric character references.
            if i < b.len() && b[i] == b'#' {
                i += 1;
                let is_hex = i < b.len() && b[i] == b'x';
                if is_hex {
                    i += 1;
                }
                let radix: u64 = if is_hex { 16 } else { 10 };
                let digits = i;
                let mut val: u64 = 0;
                let mut erange = false;
                while i < b.len() {
                    let d = if is_hex {
                        (b[i] as char).to_digit(16)
                    } else if b[i].is_ascii_digit() {
                        Some((b[i] - b'0') as u32)
                    } else {
                        None
                    };
                    match d {
                        Some(d) => {
                            val = match val
                                .checked_mul(radix)
                                .and_then(|v| v.checked_add(u64::from(d)))
                            {
                                Some(v) => v,
                                None => {
                                    // strtoul sets ERANGE past ULONG_MAX.
                                    erange = true;
                                    u64::MAX
                                }
                            };
                            i += 1;
                        }
                        None => break,
                    }
                }
                if i == digits || erange {
                    // `text == end` (no digits) or `errno != 0`. The C passes the
                    // reference on without consuming it, so only the "&#"/"&#x"
                    // is dropped and the digits stay as literal text.
                    i = digits;
                    continue;
                }
                push_unichar(&mut out, val as u32);
                if i < b.len() && b[i] == b';' {
                    i += 1;
                }
                continue;
            }

            // Bare '&'.
            out.extend_from_slice(b"&amp;");
        } else if is_ascii_space(b[i]) {
            out.push(b' ');
            i += 1;
            while i < b.len() && is_ascii_space(b[i]) {
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    // Only valid UTF-8 was ever pushed (source bytes + entity expansions).
    String::from_utf8(out).unwrap_or_default()
}

/// Case-insensitive match of a `&name;` from a table (semicolon required).
/// Returns the canonical name on success.
fn match_entity(rest: &[u8], table: &[(u32, &'static str)], ci: bool) -> Option<&'static str> {
    for &(_, name) in table {
        let nl = name.len();
        let same = if ci {
            rest.len() > nl && ascii_eq_ci(&rest[..nl], name.as_bytes())
        } else {
            rest.len() > nl && &rest[..nl] == name.as_bytes()
        };
        if same && rest[nl] == b';' {
            return Some(name);
        }
    }
    None
}

/// Case-sensitive HTML entity match (semicolon required). Returns codepoint+name.
fn match_html_entity(rest: &[u8]) -> Option<(u32, &'static str)> {
    for &(cp, name) in HTML_ENTITIES {
        let nl = name.len();
        if rest.len() > nl && &rest[..nl] == name.as_bytes() && rest[nl] == b';' {
            return Some((cp, name));
        }
    }
    None
}

/// Encode a codepoint as UTF-8 into `out` (invalid codepoints are dropped, as
/// pango/glib would reject them).
fn push_unichar(out: &mut Vec<u8>, cp: u32) {
    if let Some(c) = char::from_u32(cp) {
        let mut tmp = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
    }
}

/// ASCII whitespace per `g_ascii_isspace`.
fn is_ascii_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn ascii_eq_ci(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Strip leading/trailing ASCII whitespace, matching `g_strstrip`.
fn ascii_strip(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r'))
}

/// Element names must be all-alnum up to the first space.
fn is_valid_element_name(name: &str) -> bool {
    for &b in name.as_bytes() {
        if b == b' ' {
            break;
        }
        if !b.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

/// Attribute names must be all-alnum up to the `=`.
fn is_valid_attribute_name(name: &str) -> bool {
    for &b in name.as_bytes() {
        if b == b'=' {
            break;
        }
        if !b.is_ascii_alphanumeric() {
            return false;
        }
    }
    true
}

/// Drop one leading and/or one trailing quote (`"` or `'`) from an attribute
/// value, matching the C's unconditional (mismatch-tolerant) stripping.
fn strip_quotes(v: &str) -> String {
    let mut s = v;
    if s.starts_with('"') || s.starts_with('\'') {
        s = &s[1..];
    }
    if !s.is_empty() {
        let last = s.as_bytes()[s.len() - 1];
        if last == b'"' || last == b'\'' {
            s = &s[..s.len() - 1];
        }
    }
    s.to_string()
}

/// `atoi`. Optional leading ASCII whitespace, optional sign, then decimal digits.
fn atoi(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_ascii_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut val: i64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        val = val.saturating_mul(10).saturating_add((b[i] - b'0') as i64);
        i += 1;
    }
    if neg { -val } else { val }
}

/// True for exactly six ASCII hex digits (a `#`-less colour like `ff0000`).
fn looks_like_hex6(v: &str) -> bool {
    v.len() == 6 && v.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Map the X11 colour names pango's parser lacks. Case-insensitive.
fn map_named_color(v: &str) -> Option<&'static str> {
    const NAMED: &[(&str, &str)] = &[
        ("aqua", "#00ffff"),
        ("crimson", "#dc143c"),
        ("fuchsia", "#ff00ff"),
        ("indigo", "#4b0082"),
        ("lime", "#00ff00"),
        ("olive", "#808000"),
        ("silver", "#c0c0c0"),
        ("teal", "#008080"),
    ];
    NAMED
        .iter()
        .find(|(k, _)| v.eq_ignore_ascii_case(k))
        .map(|(_, hex)| *hex)
}

// ---------------------------------------------------------------------------
// Entity tables (transcribed verbatim from samiparse.c). XML entities are
// re-escaped. HTML entities resolve to the listed Unicode codepoint.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const XML_ENTITIES: &[(u32, &str)] = &[
    (34, "quot"), (38, "amp"), (39, "apos"), (60, "lt"), (62, "gt"),
];

#[rustfmt::skip]
const HTML_ENTITIES: &[(u32, &str)] = &[
    (161, "iexcl"), (162, "cent"), (163, "pound"), (164, "curren"), (165, "yen"),
    (166, "brvbar"), (167, "sect"), (168, "uml"), (169, "copy"), (170, "ordf"), (171, "laquo"),
    (172, "not"), (173, "shy"), (174, "reg"), (175, "macr"), (176, "deg"), (177, "plusmn"),
    (178, "sup2"), (179, "sup3"), (180, "acute"), (181, "micro"), (182, "para"),
    (183, "middot"), (184, "cedil"), (185, "sup1"), (186, "ordm"), (187, "raquo"),
    (188, "frac14"), (189, "frac12"), (190, "frac34"), (191, "iquest"), (192, "Agrave"),
    (193, "Aacute"), (194, "Acirc"), (195, "Atilde"), (196, "Auml"), (197, "Aring"),
    (198, "AElig"), (199, "Ccedil"), (200, "Egrave"), (201, "Eacute"), (202, "Ecirc"),
    (203, "Euml"), (204, "Igrave"), (205, "Iacute"), (206, "Icirc"), (207, "Iuml"),
    (208, "ETH"), (209, "Ntilde"), (210, "Ograve"), (211, "Oacute"), (212, "Ocirc"),
    (213, "Otilde"), (214, "Ouml"), (215, "times"), (216, "Oslash"), (217, "Ugrave"),
    (218, "Uacute"), (219, "Ucirc"), (220, "Uuml"), (221, "Yacute"), (222, "THORN"),
    (223, "szlig"), (224, "agrave"), (225, "aacute"), (226, "acirc"), (227, "atilde"),
    (228, "auml"), (229, "aring"), (230, "aelig"), (231, "ccedil"), (232, "egrave"),
    (233, "eacute"), (234, "ecirc"), (235, "euml"), (236, "igrave"), (237, "iacute"),
    (238, "icirc"), (239, "iuml"), (240, "eth"), (241, "ntilde"), (242, "ograve"),
    (243, "oacute"), (244, "ocirc"), (245, "otilde"), (246, "ouml"), (247, "divide"),
    (248, "oslash"), (249, "ugrave"), (250, "uacute"), (251, "ucirc"), (252, "uuml"),
    (253, "yacute"), (254, "thorn"), (255, "yuml"), (338, "OElig"), (339, "oelig"),
    (352, "Scaron"), (353, "scaron"), (376, "Yuml"), (402, "fnof"), (710, "circ"),
    (732, "tilde"), (913, "Alpha"), (914, "Beta"), (915, "Gamma"), (916, "Delta"),
    (917, "Epsilon"), (918, "Zeta"), (919, "Eta"), (920, "Theta"), (921, "Iota"),
    (922, "Kappa"), (923, "Lambda"), (924, "Mu"), (925, "Nu"), (926, "Xi"), (927, "Omicron"),
    (928, "Pi"), (929, "Rho"), (931, "Sigma"), (932, "Tau"), (933, "Upsilon"), (934, "Phi"),
    (935, "Chi"), (936, "Psi"), (937, "Omega"), (945, "alpha"), (946, "beta"), (947, "gamma"),
    (948, "delta"), (949, "epsilon"), (950, "zeta"), (951, "eta"), (952, "theta"),
    (953, "iota"), (954, "kappa"), (955, "lambda"), (956, "mu"), (957, "nu"), (958, "xi"),
    (959, "omicron"), (960, "pi"), (961, "rho"), (962, "sigmaf"), (963, "sigma"), (964, "tau"),
    (965, "upsilon"), (966, "phi"), (967, "chi"), (968, "psi"), (969, "omega"),
    (977, "thetasym"), (978, "upsih"), (982, "piv"), (8194, "ensp"), (8195, "emsp"),
    (8201, "thinsp"), (8204, "zwnj"), (8205, "zwj"), (8206, "lrm"), (8207, "rlm"),
    (8211, "ndash"), (8212, "mdash"), (8216, "lsquo"), (8217, "rsquo"), (8218, "sbquo"),
    (8220, "ldquo"), (8221, "rdquo"), (8222, "bdquo"), (8224, "dagger"), (8225, "Dagger"),
    (8226, "bull"), (8230, "hellip"), (8240, "permil"), (8242, "prime"), (8243, "Prime"),
    (8249, "lsaquo"), (8250, "rsaquo"), (8254, "oline"), (8260, "frasl"), (8364, "euro"),
    (8465, "image"), (8472, "weierp"), (8476, "real"), (8482, "trade"), (8501, "alefsym"),
    (8592, "larr"), (8593, "uarr"), (8594, "rarr"), (8595, "darr"), (8596, "harr"),
    (8629, "crarr"), (8656, "lArr"), (8657, "uArr"), (8658, "rArr"), (8659, "dArr"),
    (8660, "hArr"), (8704, "forall"), (8706, "part"), (8707, "exist"), (8709, "empty"),
    (8711, "nabla"), (8712, "isin"), (8713, "notin"), (8715, "ni"), (8719, "prod"),
    (8721, "sum"), (8722, "minus"), (8727, "lowast"), (8730, "radic"), (8733, "prop"),
    (8734, "infin"), (8736, "ang"), (8743, "and"), (8744, "or"), (8745, "cap"), (8746, "cup"),
    (8747, "int"), (8756, "there4"), (8764, "sim"), (8773, "cong"), (8776, "asymp"),
    (8800, "ne"), (8801, "equiv"), (8804, "le"), (8805, "ge"), (8834, "sub"), (8835, "sup"),
    (8836, "nsub"), (8838, "sube"), (8839, "supe"), (8853, "oplus"), (8855, "otimes"),
    (8869, "perp"), (8901, "sdot"), (8968, "lceil"), (8969, "rceil"), (8970, "lfloor"),
    (8971, "rfloor"), (9001, "lang"), (9002, "rang"), (9674, "loz"), (9824, "spades"),
    (9827, "clubs"), (9829, "hearts"), (9830, "diams"),
];

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = NS_PER_MS;

    fn parse(body: &str) -> Vec<Cue> {
        Sami::default()
            .parse(body, &ParseContext::default())
            .expect("sami parse")
    }

    /// Assert a cue's timing (in ms) and text.
    fn check(cue: &Cue, start_ms: u64, end_ms: Option<u64>, text: &str) {
        assert_eq!(cue.start_ns, start_ms * MS, "start mismatch");
        assert_eq!(cue.end_ns, end_ms.map(|e| e * MS), "end mismatch");
        assert_eq!(cue.text, text, "text mismatch");
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(Sami::default().output_format(), OutputFormat::PangoMarkup);
    }

    // --- Ported from the C suite (subparse.c) ------------------------------

    /// `test_sami`: HEAD/STYLE/comment skipping, <br>, two syncs, body close.
    #[test]
    fn c_test_sami() {
        let body = "<SAMI>\n\
<HEAD>\n\
    <TITLE>Subtitle</TITLE>\n\
    <STYLE TYPE=\"text/css\">\n\
    <!--\n\
        P {margin-left:8pt; margin-right:8pt; margin-bottom:2pt; margin-top:2pt; text-align:center; font-size:12pt; font-weight:normal; color:black;}\n\
        .CC {Name:English; lang:en-AU; SAMIType:CC;}\n\
        #STDPrn {Name:Standard Print;}\n\
        #LargePrn {Name:Large Print; font-size:24pt;}\n\
        #SmallPrn {Name:Small Print; font-size:16pt;}\n\
    -->\n\
    </Style>\n\
</HEAD>\n\
<BODY>\n\
    <SYNC Start=1000>\n\
        <P Class=CC>\n\
            This is a comment.<br>\n\
            This is a second comment.\n\
    <SYNC Start=2000>\n\
        <P Class=CC>\n\
            This is a third comment.<br>\n\
            This is a fourth comment.\n\
</BODY>\n\
</SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "This is a comment.\nThis is a second comment.",
        );
        check(
            &cues[1],
            2000,
            None,
            "This is a third comment.\nThis is a fourth comment.",
        );
    }

    /// `test_sami_xml_entities`: XML entities survive re-escaped for pango.
    #[test]
    fn c_test_sami_xml_entities() {
        let body = "<SAMI>\n\
<BODY>\n\
    <SYNC Start=1000>\n\
        <P Class=CC>\n\
            &lt;Hello&gt; &amp;\n\
    <SYNC Start=2000>\n\
        <P Class=CC>\n\
            &quot;World&apos;\n\
</BODY>\n\
</SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "&lt;Hello&gt; &amp;");
        check(&cues[1], 2000, None, "&quot;World&apos;");
    }

    /// `test_sami_html_entities`: named + numeric HTML entities -> UTF-8.
    #[test]
    fn c_test_sami_html_entities() {
        let body = "<SAMI>\n\
<BODY>\n\
    <SYNC Start=1000>\n\
        <P Class=CC>\n\
            &nbsp; &plusmn; &acute;\n\
    <SYNC Start=2000>\n\
        <P Class=CC>\n\
            &Alpha; &omega;\n\
    <SYNC Start=3000>\n\
        <P Class=CC>\n\
            &#xa0; &#177; &#180;\n\
</BODY>\n\
</SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 3, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "\u{a0} \u{b1} \u{b4}");
        check(&cues[1], 2000, Some(3000), "\u{391} \u{3c9}");
        check(&cues[2], 3000, None, "\u{a0} \u{b1} \u{b4}");
    }

    /// `test_sami_bad_entities`: missing semicolons, lone `&`.
    #[test]
    fn c_test_sami_bad_entities() {
        let body = "<SAMI>\n\
<BODY>\n\
    <SYNC Start=1000>\n\
        <P Class=CC>\n\
            &nbsp &\n\
    <SYNC Start=2000>\n\
        <P Class=CC>\n\
            &#xa0 &#177 &#180;\n\
</BODY>\n\
</SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "\u{a0} &amp;");
        check(&cues[1], 2000, None, "\u{a0} \u{b1} \u{b4}");
    }

    /// `test_sami_comment`: a top-level multi-line comment + a quoted `Class`
    /// value containing the comment delimiter.
    #[test]
    fn c_test_sami_comment() {
        let body = "<SAMI>\n\
<!--\n\
=======\n\
foo bar\n\
=======\n\
-->\n\
<BODY>\n\
    <SYNC Start=1000>\n\
        <P Class=\"C====\">\n\
            &nbsp &\n\
    <SYNC Start=2000>\n\
        <P Class=CC>\n\
            &#xa0 &#177 &#180;\n\
</BODY>\n\
</SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "\u{a0} &amp;");
        check(&cues[1], 2000, None, "\u{a0} \u{b1} \u{b4}");
    }

    /// `test_sami_self_contained_tags`: `<i />` opens and immediately closes.
    #[test]
    fn c_test_sami_self_contained_tags() {
        let body = "<SAMI>\n\
<BODY>\n\
    <SYNC Start=1000>\n\
        <P Class=CC>\n\
            This line has a self-closing format tag<i /> and more.\n\
    <SYNC Start=2000>\n\
        <P Class=CC>\n\
            This is a third comment.<br>\n\
            This is a fourth comment.\n\
</BODY>\n\
</SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "This line has a self-closing format tag<i></i>and more.",
        );
        check(
            &cues[1],
            2000,
            None,
            "This is a third comment.\nThis is a fourth comment.",
        );
    }

    // --- Additional edge cases --------------------------------------------

    #[test]
    fn empty_and_non_sami_bodies() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n\n").is_empty());
        // No <sync>: text is never captured.
        assert!(parse("<SAMI>\n<BODY>\nloose text\n</BODY>\n</SAMI>\n").is_empty());
    }

    #[test]
    fn italic_is_auto_closed_on_next_sync() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><i>Hello\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "<i>Hello</i>");
    }

    #[test]
    fn font_color_named_becomes_span_foreground() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><font color=aqua>Hi\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span foreground=\"#00ffff\">Hi</span>",
        );
    }

    #[test]
    fn font_color_hashless_hex_gets_hash() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><font color=ff0000>Red\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span foreground=\"#ff0000\">Red</span>",
        );
    }

    #[test]
    fn font_color_full_hex_preserved() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><font color=#123456>X\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span foreground=\"#123456\">X</span>",
        );
    }

    #[test]
    fn font_face_becomes_font_family() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><font face=Arial>Hi\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span font_family=\"Arial\">Hi</span>",
        );
    }

    #[test]
    fn explicit_font_close_emits_span_close() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><font color=red>Hi</font>there\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span foreground=\"red\">Hi</span>there",
        );
    }

    /// Quirk parity. Every text token is whitespace-stripped, so text abutting a
    /// tag loses its bordering spaces (matches samiparse.c's `g_strstrip`).
    #[test]
    fn text_adjacent_to_tags_loses_spaces() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC>word <i>emph</i> more\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "word<i>emph</i>more");
    }

    #[test]
    fn br_becomes_newline() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC>a<br>b<br>c\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "a\nb\nc");
    }

    #[test]
    fn ruby_rt_becomes_small_span() {
        // <rt> annotation is prepended (with a trailing newline) to the block.
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC><ruby>base<rt>anno</rt></ruby>\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span size='xx-small' rise='-100'> anno </span>\nbase",
        );
    }

    /// Assert every pango tag this parser emits is closed exactly once.
    fn assert_balanced(text: &str) {
        for tag in ["span", "i"] {
            let open = text.matches(&format!("<{tag}")).count();
            let close = text.matches(&format!("</{tag}>")).count();
            assert_eq!(
                open, close,
                "unbalanced <{tag}> in {text:?} ({open} open, {close} close)"
            );
        }
    }

    #[test]
    fn stray_close_inside_ruby_keeps_markup_balanced() {
        // `</font>` closes a <span> that was never opened. The C walks the tag
        // stack anyway and its <rt> arm appends a `</span>` to the ruby buffer
        // before giving up, so the annotation ends up with two closers for one
        // opener and pango rejects the cue. We bail out before mutating
        // anything, a deliberate deviation documented in `specs/sami.md`.
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P><ruby>base<rt>anno</font></ruby>\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        assert_balanced(&cues[0].text);
        check(
            &cues[0],
            1000,
            Some(2000),
            "<span size='xx-small' rise='-100'> anno </span>\nbase",
        );
    }

    #[test]
    fn stray_close_inside_italic_ruby_keeps_markup_balanced() {
        // Same shape with an open <i>, which the C's <rt> arm duplicates too.
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P><i><ruby>base<rt>anno</font></ruby>\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        assert_balanced(&cues[0].text);
        check(
            &cues[0],
            1000,
            Some(2000),
            "<i><span size='xx-small' rise='-100'> anno </span></i>\n<i>base</i>",
        );
    }

    #[test]
    fn stray_close_without_ruby_is_a_no_op() {
        // A `</font>`/`</i>` with nothing open must not close the other one.
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P><i>a</font>b</i>\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        assert_balanced(&cues[0].text);
        check(&cues[0], 1000, Some(2000), "<i>ab</i>");
    }

    #[test]
    fn lowercase_sami_and_tags() {
        // Tag names are case-insensitive.
        let body = "<sami>\n<body>\n\
    <sync start=1000><p class=cc>hi\n\
    <sync start=2000></body></sami>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "hi");
    }

    #[test]
    fn multi_byte_utf8_passthrough() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC>Un éclair — naïve?\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "Un éclair — naïve?");
    }

    #[test]
    fn empty_syncs_advance_timing_without_cues() {
        // Two empty syncs, then content, then a closing sync. Timing tracks the
        // most recent pair around the content.
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000>\n\
    <SYNC Start=2000><P Class=CC>hello\n\
    <SYNC Start=3000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 2000, Some(3000), "hello");
    }

    #[test]
    fn sync_start_time_never_goes_backwards() {
        // Start=500 < previous 1000, clamped to the previous time (MAX).
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC>a\n\
    <SYNC Start=500><P Class=CC>b\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(1000), "a");
        check(&cues[1], 1000, Some(2000), "b");
    }

    #[test]
    fn multiline_tag_spanning_lines() {
        // A tag split across a line boundary is buffered until its '>'. The
        // indentation on the continued line supplies the name/attr separator
        // (collapsed to a single space by unescaping). Note that there are no
        // `\` line continuations here. Those would eat the leading whitespace.
        let body = "<SAMI>\n<BODY>\n<SYNC\n    Start=1000><P Class=CC>split tag\n<SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "split tag");
    }

    #[test]
    fn invalid_element_name_resets_state() {
        // A tag with a non-alnum name is malformed -> context reset, no cue.
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC>text<a-b>more\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        // The reset drops the pending sync, so the first block never flushes.
        assert!(cues.is_empty(), "cues: {cues:#?}");
    }

    #[test]
    fn deep_nesting_does_not_panic() {
        // >64 nested <i> triggers the DoS guard (reset). Must not panic.
        let mut body = String::from("<SAMI>\n<BODY>\n    <SYNC Start=1000><P Class=CC>");
        for _ in 0..80 {
            body.push_str("<i>");
        }
        body.push_str("deep\n    <SYNC Start=2000></BODY></SAMI>\n");
        let _ = parse(&body); // just must not panic / must terminate
    }

    #[test]
    fn whitespace_runs_collapse_to_single_space() {
        let body = "<SAMI>\n<BODY>\n\
    <SYNC Start=1000><P Class=CC>lots     of\t\tspace\n\
    <SYNC Start=2000></BODY></SAMI>\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        check(&cues[0], 1000, Some(2000), "lots of space");
    }

    // --- Direct helper coverage -------------------------------------------

    #[test]
    fn unescape_basics() {
        assert_eq!(unescape_string("&nbsp;x"), "\u{a0}x");
        assert_eq!(unescape_string("&nbsp x"), "\u{a0} x");
        assert_eq!(unescape_string("&lt;&gt;"), "&lt;&gt;");
        assert_eq!(unescape_string("&LT;"), "&lt;"); // XML is case-insensitive
        assert_eq!(unescape_string("&amp;"), "&amp;"); // XML entity survives
        assert_eq!(unescape_string("&"), "&amp;"); // bare '&' -> &amp;
        assert_eq!(unescape_string("&amp"), "&amp;amp"); // no ';', so bare '&' + "amp"
        assert_eq!(unescape_string("&#65;"), "A");
        assert_eq!(unescape_string("&#x41;"), "A");
        assert_eq!(unescape_string("a   b"), "a b");
    }

    #[test]
    fn numeric_entity_past_strtoul_stays_literal() {
        // 21 digits overflow `strtoul`, which reports ERANGE. The C then passes
        // the reference on *without consuming it*, so only the "&#"/"&#x" is
        // dropped and the digits (and the ';') stay as text. Wrapping the
        // accumulator instead produced an arbitrary character.
        assert_eq!(
            unescape_string("&#123456789012345678901;"),
            "123456789012345678901;"
        );
        assert_eq!(
            unescape_string("&#x123456789012345678901;"),
            "123456789012345678901;"
        );
        // In range is unaffected, including the C's truncation into a gunichar.
        assert_eq!(unescape_string("&#4294967361;"), "A"); // 2^32 + 65
    }

    #[test]
    fn atoi_matches_c() {
        assert_eq!(atoi("1000"), 1000);
        assert_eq!(atoi("  42abc"), 42);
        assert_eq!(atoi("-7"), -7);
        assert_eq!(atoi("abc"), 0);
        assert_eq!(atoi(""), 0);
    }

    #[test]
    fn strip_quotes_cases() {
        assert_eq!(strip_quotes("\"x\""), "x");
        assert_eq!(strip_quotes("'x'"), "x");
        assert_eq!(strip_quotes("x"), "x");
        assert_eq!(strip_quotes("\"x"), "x");
        assert_eq!(strip_quotes("x\""), "x");
        assert_eq!(strip_quotes("\""), "");
    }
}
