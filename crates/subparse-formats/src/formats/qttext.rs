// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! QTtext (QuickTime Text) subtitle parser.
//!
//! Dependency-free port of the upstream GStreamer parser
//! `gst-plugins-base/gst/subparse/qttextparse.c` (`parse_qttext` and the
//! `GstQTTextContext` state machine). See `specs/qttext.md` for the format
//! notes and references.
//!
//! A QTtext body is a sequence of lines. Each line is either:
//!   * one or more `{...}` descriptors (`{QTtext}`, `{font:...}`, `{size:N}`,
//!     `{textColor:r,g,b}`, `{backColor:r,g,b}`, `{plain|bold|italic}`,
//!     `{timescale:N}`, `{timestamps:relative}`), optionally followed by text;
//!   * a `[HH:MM:SS.dec]` timestamp (which flushes any pending text as a cue);
//!   * plain text (leading spaces/tabs are skipped).
//!
//! Text accumulates until the *next* timestamp closes it. A cue therefore spans
//! `[previous_timestamp, this_timestamp)`. Consequently a trailing text block
//! with no following timestamp is never emitted, exactly as upstream (the
//! element's EOS "\n\n" flush is a no-op for QTtext). Output is Pango markup
//! (`text/x-raw, format=pango-markup`), matching upstream.

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;
const MIN_TO_NSEC: u64 = 60 * GST_SECOND;
const HOUR_TO_NSEC: u64 = 60 * MIN_TO_NSEC;
/// Mirror of `GST_CLOCK_TIME_NONE` (used on timestamp overflow).
const CLOCK_TIME_NONE: u64 = u64::MAX;

/// Parser for the QTtext subtitle format.
///
/// Streaming: like TMPlayer, a QTtext cue is closed by the *next* timestamp, so
/// exactly one block of pending text is held at a time. The descriptor state
/// (font, size, colours, timescale, absolute/relative) is long-lived and lives
/// in [`Ctx`], which is now carried across calls rather than rebuilt per parse.
#[derive(Debug, Default)]
pub struct QtText {
    lines: LineScanner,
    ctx: Option<Ctx>,
}

impl SubtitleFormat for QtText {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, ctx } = self;
        let ctx = ctx.get_or_insert_with(Ctx::new);

        // Upstream `get_next_line` splits on '\n' and strips one trailing '\r'.
        // The body is already newline-normalized, but the '\r' strip is kept for
        // fidelity.
        let mut out = Vec::new();
        let mut consumed = lines.feed(body, |line| ctx.parse_line(line.as_bytes(), &mut out));

        if at_eos {
            // The whole-body parser iterated `split('\n')`, which also yields
            // the unterminated remainder. That matters here: a trailing
            // timestamp line with no newline after it still flushes its cue.
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            ctx.parse_line(tail.as_bytes(), &mut out);
            consumed = body.len();
        }

        Ok(Parsed {
            cues: out,
            consumed,
        })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }
}

/// Mirror of `GstQTTextContext` plus the pending-text buffer (`state->buf`).
#[derive(Debug)]
struct Ctx {
    timescale: i32,
    absolute: bool,
    start_time: u64,

    markup_open: bool,
    need_markup: bool,

    font: Option<String>,
    font_size: i32,
    bg_color: Option<String>,
    fg_color: Option<String>,

    bold: bool,
    italic: bool,

    /// Accumulated text for the current (not-yet-flushed) cue.
    buf: Option<String>,
}

impl Ctx {
    /// Mirror of `qttext_context_init`.
    fn new() -> Self {
        Ctx {
            timescale: 1000,
            absolute: true,
            start_time: 0,
            markup_open: false,
            need_markup: false,
            font: None,
            font_size: 12,
            bg_color: None,
            fg_color: None,
            bold: false,
            italic: false,
            buf: None,
        }
    }

    /// Mirror of `parse_qttext`. Process one line, possibly emitting a cue.
    fn parse_line(&mut self, line: &[u8], out: &mut Vec<Cue>) {
        let mut i = 0usize;
        while i < line.len() {
            match line[i] {
                b'{' => {
                    // A descriptor tag. On a malformed tag we abandon the line.
                    if !self.parse_tag(line, &mut i) {
                        break;
                    }
                }
                b'[' => {
                    // A timestamp closes any pending text as a cue.
                    let ts = parse_timestamp(line, i, self.timescale);
                    if self.buf.is_some() {
                        let text = self.get_text();
                        let start = self.start_time;
                        // Absolute: duration = ts - start_time, so the end is
                        // the timestamp itself. A timestamp *before* the pending
                        // block's start (which includes the 0 that a malformed
                        // one reports) underflows in the C into a duration of
                        // some 584 years. We emit an open-ended cue instead:
                        // just as unbounded in practice, and it keeps a cue's
                        // end from preceding its start.
                        let end = if self.absolute {
                            (ts >= start).then_some(ts)
                        } else {
                            Some(start.saturating_add(ts))
                        };
                        out.push(Cue::new(start, end, text));
                    }
                    self.buf = None;
                    // ts == 0 covers both a legit `[00:00:00.00]` and a bad
                    // timestamp. In both cases start_time is left unchanged.
                    if ts != 0 {
                        if self.absolute {
                            self.start_time = ts;
                        } else {
                            self.start_time = self.start_time.wrapping_add(ts);
                        }
                    }
                    // The rest of the line is ignored.
                    break;
                }
                b' ' | b'\t' => {
                    i += 1;
                }
                _ => {
                    // The remainder of the line is text.
                    self.parse_text(line, i);
                    break;
                }
            }
        }
    }

    /// Mirror of `qttext_parse_tag`. Returns false on a malformed (unterminated)
    /// tag, which aborts parsing of the current line.
    fn parse_tag(&mut self, line: &[u8], i: &mut usize) -> bool {
        debug_assert_eq!(line[*i], b'{');
        let close = match line[*i..].iter().position(|&c| c == b'}') {
            Some(p) => *i + p,
            None => return false, // error_out
        };
        let next_index = close + 1;
        *i += 1; // skip '{'
        let idx = *i;
        let tag = &line[idx..];

        if tag.starts_with(b"QTtext") {
            // NOP
        } else if tag.starts_with(b"font") {
            if let Some(s) = read_str(line, idx + 4, close) {
                self.font = Some(s);
                self.need_markup = true;
            }
        } else if tag.starts_with(b"size") {
            let aux = read_int(&line[idx + 4..]);
            self.font_size = if aux == 0 { 12 } else { aux };
            self.need_markup = true;
        } else if tag.starts_with(b"textColor") {
            if let Some((r, g, b)) = read_color(line, idx + 9) {
                self.fg_color = Some(make_color(r, g, b));
            }
            self.need_markup = true;
        } else if tag.starts_with(b"backColor") {
            match read_color(line, idx + 9) {
                Some((r, g, b)) => self.bg_color = Some(make_color(r, g, b)),
                None => self.bg_color = None, // failure disables the background
            }
            self.need_markup = true;
        } else if tag.starts_with(b"plain") {
            self.bold = false;
            self.italic = false;
            self.need_markup = true;
        } else if tag.starts_with(b"bold") {
            self.bold = true;
            self.italic = false;
            self.need_markup = true;
        } else if tag.starts_with(b"italic") {
            self.bold = false;
            self.italic = true;
            self.need_markup = true;
        } else if tag.starts_with(b"timescale") {
            let aux = read_int(&line[idx + 9..]);
            self.timescale = if aux == 0 { 1000 } else { aux };
        } else if tag.starts_with(b"timestamps") {
            // Mirror of `string_match`. Upstream returns true when "relative" is
            // NOT found before the closing brace (its `strstr` may return NULL,
            // which compares below `upto`), so anything that is not explicitly
            // `relative` *before* the brace is treated as relative. This is a
            // faithful port of that quirk (see specs/qttext.md).
            self.absolute = !string_match(line, idx + 10, b"relative", next_index);
        } else {
            // Unused tag, ignored.
        }

        *i = next_index; // move past '}'
        true
    }

    /// Mirror of `qttext_parse_text`.
    fn parse_text(&mut self, line: &[u8], index: usize) {
        self.prepare_text();
        let text = String::from_utf8_lossy(&line[index..]);
        self.buf
            .as_mut()
            .expect("prepare_text always sets buf")
            .push_str(text.as_ref());
    }

    /// Mirror of `qttext_prepare_text`.
    fn prepare_text(&mut self) {
        match self.buf.as_mut() {
            Some(b) => b.push('\n'),
            None => self.buf = Some(String::with_capacity(256)),
        }

        if self.need_markup {
            if self.markup_open {
                self.buf.as_mut().unwrap().push_str("</span>");
            }
            self.open_markup();
            self.markup_open = true;
        }
    }

    /// Mirror of `qttext_open_markup`.
    fn open_markup(&mut self) {
        let mut s = String::from("<span");
        match &self.font {
            Some(f) => s.push_str(&format!(" font='{} {}'", f, self.font_size)),
            None => s.push_str(&format!(" font='{}'", self.font_size)),
        }
        if let Some(bg) = &self.bg_color {
            s.push_str(&format!(" bgcolor='{bg}'"));
        }
        if let Some(fg) = &self.fg_color {
            s.push_str(&format!(" color='{fg}'"));
        }
        if self.bold {
            s.push_str(" weight='bold'");
        }
        if self.italic {
            s.push_str(" style='italic'");
        }
        s.push('>');
        self.buf.as_mut().unwrap().push_str(&s);
    }

    /// Mirror of `qttext_get_text`. Caller guarantees `buf` is `Some`.
    fn get_text(&mut self) -> String {
        let mut buf = self.buf.take().expect("get_text called with pending buf");
        if self.markup_open {
            buf.push_str("</span>");
        }
        self.markup_open = false;
        buf
    }
}

/// Is `c` one of the characters C's `isspace()` treats as whitespace?
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Mirror of C's `atoi`. Skip leading whitespace, optional sign, decimal digits.
fn c_atoi(bytes: &[u8]) -> i32 {
    let mut i = 0;
    while i < bytes.len() && is_c_space(bytes[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let mut val: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    let val = if neg { val.wrapping_neg() } else { val };
    val as i32
}

/// Mirror of `read_int`. Find the ':' (bounded by '}') and `atoi` what follows.
fn read_int(bytes: &[u8]) -> i32 {
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx] != b':' && bytes[idx] != b'}' {
        idx += 1;
    }
    if idx >= bytes.len() || bytes[idx] != b':' {
        return 0;
    }
    idx += 1;
    c_atoi(&bytes[idx..])
}

/// Mirror of `read_str`. The string after ':' (leading spaces trimmed) up to the
/// closing brace at `end`. Returns `None` when '}' precedes ':'.
fn read_str(line: &[u8], start: usize, end: usize) -> Option<String> {
    let mut idx = start;
    while idx < line.len() && line[idx] != b':' && line[idx] != b'}' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b':' {
        return None;
    }
    idx += 1;
    while idx < line.len() && line[idx] == b' ' {
        idx += 1;
    }
    let stop = end.min(line.len()).max(idx);
    Some(String::from_utf8_lossy(&line[idx..stop]).into_owned())
}

/// Mirror of `read_color`. Parse `r,g,b` after ':' (each an `atoi`).
fn read_color(line: &[u8], start: usize) -> Option<(i32, i32, i32)> {
    let mut idx = start;
    while idx < line.len() && line[idx] != b':' && line[idx] != b'}' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b':' {
        return None;
    }
    idx += 1;
    let r = c_atoi(&line[idx..]);

    while idx < line.len() && line[idx] != b'}' && line[idx] != b',' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b',' {
        return None;
    }
    idx += 1;
    let g = c_atoi(&line[idx..]);

    while idx < line.len() && line[idx] != b'}' && line[idx] != b',' {
        idx += 1;
    }
    if idx >= line.len() || line[idx] != b',' {
        return None;
    }
    idx += 1;
    let b = c_atoi(&line[idx..]);

    Some((r, g, b))
}

/// Mirror of `make_color`. qttext channels are 0..65535, Pango wants 0..255.
fn make_color(r: i32, g: i32, b: i32) -> String {
    let r = r / 256;
    let g = g / 256;
    let b = b / 256;
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Mirror of `string_match(line + start, needle, line + upto)`. Upstream compares
/// the `strstr` result pointer against `upto`. A NULL (not found) result sorts
/// below `upto`, so "not found" yields `true`. Reproduced faithfully.
fn string_match(line: &[u8], start: usize, needle: &[u8], upto: usize) -> bool {
    match find_subslice(&line[start.min(line.len())..], needle) {
        Some(pos) => (start + pos) < upto,
        None => true,
    }
}

/// First index of `needle` within `hay`, or `None`. (`str::find` on bytes.)
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Mirror of `gst_util_uint64_scale` (truncating). Computes `val * num / denom`.
fn uint64_scale(val: u64, num: u64, denom: u64) -> u64 {
    if denom == 0 {
        return 0;
    }
    ((val as u128 * num as u128) / denom as u128) as u64
}

/// Scan a `%u` field (skip whitespace, optional sign, digits). `None` if no digit.
fn scan_u32(line: &[u8], i: &mut usize) -> Option<u32> {
    while *i < line.len() && is_c_space(line[*i]) {
        *i += 1;
    }
    let mut neg = false;
    if *i < line.len() && (line[*i] == b'+' || line[*i] == b'-') {
        neg = line[*i] == b'-';
        *i += 1;
    }
    let dstart = *i;
    let mut val: u64 = 0;
    while *i < line.len() && line[*i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((line[*i] - b'0') as u64);
        *i += 1;
    }
    if *i == dstart {
        return None;
    }
    let v = val as u32;
    Some(if neg { v.wrapping_neg() } else { v })
}

/// Mirror of `qttext_parse_timestamp`. Parse `[%u:%u:%u.%u]` at `start`.
///
/// Returns nanoseconds. A malformed timestamp yields `0` (as upstream), which
/// the caller treats as "do not advance the clock". Overflow yields
/// `CLOCK_TIME_NONE`.
fn parse_timestamp(line: &[u8], start: usize, timescale: i32) -> u64 {
    let mut i = start;
    if i >= line.len() || line[i] != b'[' {
        return 0;
    }
    i += 1;

    let hour = match scan_u32(line, &mut i) {
        Some(v) => v,
        None => return 0,
    };
    if i >= line.len() || line[i] != b':' {
        return 0;
    }
    i += 1;

    let min = match scan_u32(line, &mut i) {
        Some(v) => v,
        None => return 0,
    };
    if i >= line.len() || line[i] != b':' {
        return 0;
    }
    i += 1;

    let sec = match scan_u32(line, &mut i) {
        Some(v) => v,
        None => return 0,
    };

    // The decimal part is optional. A missing/malformed one is forgiven as 0.
    let mut dec: u32 = 0;
    if i < line.len() && line[i] == b'.' {
        i += 1;
        if let Some(d) = scan_u32(line, &mut i) {
            dec = d;
        }
    }

    let mut timestamp = uint64_scale(dec as u64, GST_SECOND, timescale as u64);
    timestamp = timestamp.wrapping_add((sec as u64).wrapping_mul(GST_SECOND));
    match (min as u64).checked_mul(MIN_TO_NSEC) {
        Some(t) => timestamp = timestamp.wrapping_add(t),
        None => return CLOCK_TIME_NONE,
    }
    match (hour as u64).checked_mul(HOUR_TO_NSEC) {
        Some(t) => timestamp = timestamp.wrapping_add(t),
        None => return CLOCK_TIME_NONE,
    }
    timestamp
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = GST_SECOND;

    fn parse(body: &str) -> Vec<Cue> {
        QtText::default()
            .parse(body, &ParseContext::default())
            .expect("qttext parse never errors")
    }

    // --- output format ------------------------------------------------------

    #[test]
    fn emits_pango_markup() {
        assert_eq!(QtText::default().output_format(), OutputFormat::PangoMarkup);
    }

    // --- basic timing -------------------------------------------------------

    #[test]
    fn basic_absolute_plain() {
        let cues = parse("{QTtext}\n[00:00:01.00]\nHello world\n[00:00:03.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(3 * S));
        assert_eq!(cues[0].text, "Hello world");
    }

    #[test]
    fn two_absolute_cues() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nfirst\n[00:00:02.00]\nsecond\n[00:00:05.00]\n");
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (0, Some(2 * S)));
        assert_eq!(cues[0].text, "first");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (2 * S, Some(5 * S)));
        assert_eq!(cues[1].text, "second");
    }

    #[test]
    fn multiline_plain_joined_with_newline() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nA\nB\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "A\nB");
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn leading_whitespace_is_stripped() {
        let cues = parse("{QTtext}\n[00:00:00.00]\n   Spaced\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Spaced");
    }

    #[test]
    fn last_block_without_trailing_timestamp_is_dropped() {
        // Upstream only flushes on the next timestamp. The EOS "\n\n" is a no-op
        // for qttext, so a final text block with no closing timestamp is lost.
        let cues = parse("{QTtext}\n[00:00:00.00]\nOnly line\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn malformed_timestamp_after_text_yields_an_open_ended_cue() {
        // A bad timestamp reports 0, and `0 - start_time` underflows in the C
        // into a ~584-year duration. We emit no end instead, so the cue never
        // ends before it starts.
        let cues = parse("{QTtext}\n[00:00:01.00]\nText\n[bad]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "Text");

        // Same for a well-formed timestamp that simply goes backwards.
        let cues = parse("{QTtext}\n[00:00:05.00]\nText\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * S);
        assert_eq!(cues[0].end_ns, None);
    }

    #[test]
    fn empty_and_header_only_produce_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("{QTtext}\n").is_empty());
        assert!(parse("{QTtext}{font:Sans}{size:18}\n").is_empty());
    }

    // --- timescale / decimal scaling ---------------------------------------

    #[test]
    fn default_timescale_decimal() {
        // timescale 1000, so `.500` is 500/1000 s.
        let cues = parse("{QTtext}\n[00:00:00.500]\nHalf\n[00:00:01.000]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 500_000_000);
        assert_eq!(cues[0].end_ns, Some(1_000_000_000));
        assert_eq!(cues[0].text, "Half");
    }

    #[test]
    fn custom_timescale() {
        // timescale 600, so `.300` is 300/600 s = 0.5 s.
        let cues = parse("{QTtext}{timescale:600}\n[00:00:00.300]\nTick\n[00:00:01.300]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 500_000_000);
        assert_eq!(cues[0].end_ns, Some(1_500_000_000));
        assert_eq!(cues[0].text, "Tick");
    }

    #[test]
    fn invalid_timescale_and_size_reset_to_defaults() {
        let cues = parse("{QTtext}{timescale:0}{size:0}\n[00:00:00.00]\nX\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        // timescale reset to 1000 -> `.00` at 1s == 1e9. Size reset to 12.
        assert_eq!(cues[0].end_ns, Some(S));
        assert_eq!(cues[0].text, "<span font='12'>X</span>");
    }

    #[test]
    fn missing_decimal_is_forgiven() {
        let cues = parse("{QTtext}\n[00:00:01]\nNoDec\n[00:00:02]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn hours_and_minutes() {
        let cues = parse("{QTtext}\n[01:02:03.00]\nT\n[01:02:04.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 3600 * S + 2 * 60 * S + 3 * S);
        assert_eq!(cues[0].end_ns, Some(3600 * S + 2 * 60 * S + 4 * S));
    }

    // --- relative timestamps ------------------------------------------------

    #[test]
    fn relative_timestamps_are_durations() {
        let cues = parse(
            "{QTtext}{timestamps:relative}\n[00:00:01.00]\nFirst\n[00:00:02.00]\nSecond\n[00:00:03.00]\n",
        );
        assert_eq!(cues.len(), 2);
        // start accumulates. Duration == the raw timestamp value.
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (S, Some(3 * S)));
        assert_eq!(cues[0].text, "First");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (3 * S, Some(6 * S)));
        assert_eq!(cues[1].text, "Second");
    }

    #[test]
    fn timestamps_absolute_is_parsed_as_relative_quirk() {
        // Faithful port of upstream `string_match`. `{timestamps:absolute}` is
        // (surprisingly) treated as relative because "relative" is not found
        // before the closing brace. If it were truly absolute the cue would end
        // at 2s; as relative it ends at 1s + 2s = 3s.
        let cues = parse("{QTtext}{timestamps:absolute}\n[00:00:01.00]\nA\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(3 * S));
    }

    // --- markup / descriptors ----------------------------------------------

    #[test]
    fn single_line_font_and_size_markup() {
        let cues = parse("{QTtext}{font:Sans}{size:18}\n[00:00:00.00]\nStyled\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span font='Sans 18'>Styled</span>");
    }

    #[test]
    fn multiline_markup_repeats_span_per_line() {
        let cues = parse("{QTtext}{size:12}\n[00:00:00.00]\nA\nB\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='12'>A\n</span><span font='12'>B</span>"
        );
    }

    #[test]
    fn colors_are_scaled_and_ordered() {
        let cues = parse(
            "{QTtext}{textColor:65535,0,0}{backColor:0,0,65535}\n[00:00:00.00]\nRed on blue\n[00:00:01.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='12' bgcolor='#0000FF' color='#FF0000'>Red on blue</span>"
        );
    }

    #[test]
    fn full_attribute_order() {
        let cues = parse(
            "{QTtext}{font:Arial}{size:20}{textColor:65535,65535,0}{backColor:0,0,0}{bold}\n[00:00:00.00]\nAll\n[00:00:01.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='Arial 20' bgcolor='#000000' color='#FFFF00' weight='bold'>All</span>"
        );
    }

    #[test]
    fn style_transitions_bold_italic_plain() {
        let cues = parse(
            "{QTtext}\n[00:00:00.00]\n{bold}Bold line\n{italic}Italic line\n{plain}Plain line\n[00:00:03.00]\n",
        );
        assert_eq!(cues.len(), 1);
        assert_eq!(
            cues[0].text,
            "<span font='12' weight='bold'>Bold line\n\
             </span><span font='12' style='italic'>Italic line\n\
             </span><span font='12'>Plain line</span>"
        );
    }

    #[test]
    fn back_color_failure_clears_background() {
        let cues = parse(
            "{QTtext}{backColor:255,255,255}{backColor:bad}\n[00:00:00.00]\nHi\n[00:00:01.00]\n",
        );
        assert_eq!(cues.len(), 1);
        // second (malformed) backColor clears the first -> no bgcolor attr.
        assert_eq!(cues[0].text, "<span font='12'>Hi</span>");
    }

    #[test]
    fn unknown_tag_is_ignored() {
        let cues = parse("{QTtext}{wibble:42}\n[00:00:00.00]\nHi\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        // no markup descriptor seen -> plain text.
        assert_eq!(cues[0].text, "Hi");
    }

    // --- lenient recovery ---------------------------------------------------

    #[test]
    fn malformed_tag_line_is_skipped_but_text_survives() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nHello\n{bad tag no close\n[00:00:02.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello");
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn bad_timestamp_still_flushes_pending_text() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nText\n[bad]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(0));
        assert_eq!(cues[0].text, "Text");
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let cues = parse("{QTtext}\r\n[00:00:01.00]\r\nCRLF\r\n[00:00:02.00]\r\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "CRLF");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn text_is_not_escaped() {
        // qttext appends text verbatim (no markup escaping), matching upstream.
        let cues = parse("{QTtext}\n[00:00:00.00]\na < b & c\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "a < b & c");
    }

    #[test]
    fn utf8_text_is_preserved() {
        let cues = parse("{QTtext}\n[00:00:00.00]\nПривет мир 🎬\n[00:00:01.00]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Привет мир 🎬");
    }

    // --- helper unit tests --------------------------------------------------

    #[test]
    fn make_color_divides_by_256() {
        assert_eq!(make_color(65535, 0, 0), "#FF0000");
        assert_eq!(make_color(0, 65535, 0), "#00FF00");
        assert_eq!(make_color(0, 0, 65535), "#0000FF");
        assert_eq!(make_color(256, 512, 768), "#010203");
    }

    #[test]
    fn timestamp_parsing_matches_reference() {
        assert_eq!(parse_timestamp(b"[00:00:00.00]", 0, 1000), 0);
        assert_eq!(parse_timestamp(b"[00:00:01.00]", 0, 1000), S);
        assert_eq!(parse_timestamp(b"[00:00:00.500]", 0, 1000), 500_000_000);
        assert_eq!(parse_timestamp(b"[00:01:00.00]", 0, 1000), 60 * S);
        assert_eq!(parse_timestamp(b"[garbage]", 0, 1000), 0);
        assert_eq!(parse_timestamp(b"[00:00]", 0, 1000), 0); // too few fields
    }
}
