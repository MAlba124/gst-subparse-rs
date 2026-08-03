// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! MPL2 parser.
//!
//! See `specs/mpl2.md`. C reference: `mpl2_parse_line` in
//! `gst-plugins-base/gst/subparse/mpl2parse.c`.
//!
//! MPL2 is a line format: `[start][end]text`, where `start`/`end` are
//! **deciseconds** (tenths of a second). Within the text, `|` separates visual
//! lines and a leading `/` on a line marks it italic. Output is Pango markup.
//! Italic runs become `<i>...</i>` and the text is GLib-escaped. The final
//! markup is whitespace-stripped at both ends (the C's `g_strstrip`).

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

/// One decisecond in nanoseconds (`GST_SECOND / 10`).
const DECISECOND_NS: u64 = 100_000_000;

/// Parser for the MPL2 subtitle format.
///
/// Streaming: one line is one complete record with absolute times, so there is
/// no carried state at all beyond the scan position.
#[derive(Debug, Default)]
pub struct Mpl2 {
    lines: LineScanner,
}

impl SubtitleFormat for Mpl2 {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let mut cues = Vec::new();
        let mut consumed = self.lines.feed(body, |line| parse_line(line, &mut cues));

        if at_eos {
            // The whole-body parser iterated `split('\n')`, which also yields
            // the unterminated remainder.
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            parse_line(tail, &mut cues);
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        // parse_mpl2 advertises `text/x-raw, format=pango-markup`.
        OutputFormat::PangoMarkup
    }
}

/// Handle one line (terminator and any `\r` already removed).
fn parse_line(line: &str, cues: &mut Vec<Cue>) {
    // `[start][end]` in deciseconds. Missing/invalid -> skip the line.
    let Some((dc_start, dc_stop)) = parse_timestamps(line) else {
        return;
    };

    // Text begins after the second ']' (the C walks two `strchr`s).
    let Some(text) = text_after_two_brackets(line) else {
        return;
    };

    let start_ns = dc_start.saturating_mul(DECISECOND_NS);
    let end_ns = dc_stop.saturating_mul(DECISECOND_NS);

    let markup = build_markup(text);
    cues.push(Cue::new(start_ns, Some(end_ns), markup));
}

/// Parse a leading `[<digits>][<digits>]` into `(start, stop)` deciseconds.
///
/// This is the C's `sscanf (line, "[%u][%u]") != 2` test, and `sscanf` counts
/// *assigned* conversions: the `]` closing the second bracket sits after the
/// last conversion, so whether it is there at all cannot change the count.
/// `[123][456 x]` is therefore a valid record for the C, which then locates the
/// text with two independent `strchr`s. `%u` also skips leading whitespace and
/// takes an optional sign.
fn parse_timestamps(line: &str) -> Option<(u64, u64)> {
    let b = line.as_bytes();
    let mut i = 0;
    let start = scan_bracketed_uint(b, &mut i, true)?;
    let stop = scan_bracketed_uint(b, &mut i, false)?;
    Some((start, stop))
}

/// One `[%u` field of the format above. `need_close` demands the literal `]`
/// that the C's format string has *between* the two conversions.
///
/// A `-` sign makes the C's `%u` wrap into a huge unsigned value, which it
/// stores in a `gint` and multiplies into a wrapped timestamp: garbage either
/// way. We accept the sign, so the same lines count as records as in the C, but
/// read a negative value as 0 rather than wrap, which keeps a cue's end at or
/// after its start.
fn scan_bracketed_uint(b: &[u8], i: &mut usize, need_close: bool) -> Option<u64> {
    if b.get(*i) != Some(&b'[') {
        return None;
    }
    *i += 1;
    // The whitespace C's `isspace()` reports, which is what `%u` skips.
    while matches!(b.get(*i), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
        *i += 1;
    }
    let negative = match b.get(*i) {
        Some(b'+') => {
            *i += 1;
            false
        }
        Some(b'-') => {
            *i += 1;
            true
        }
        _ => false,
    };
    let start = *i;
    let mut val: u64 = 0;
    while let Some(d @ b'0'..=b'9') = b.get(*i) {
        val = val.saturating_mul(10).saturating_add((d - b'0') as u64);
        *i += 1;
    }
    if *i == start {
        return None;
    }
    if need_close {
        if b.get(*i) != Some(&b']') {
            return None;
        }
        *i += 1;
    }
    Some(if negative { 0 } else { val })
}

/// Return everything after the second `]` in the line, mirroring the C's two
/// successive `strchr(line, ']')` steps.
fn text_after_two_brackets(line: &str) -> Option<&str> {
    let first = line.find(']')?;
    let second = line[first + 1..].find(']')? + first + 1;
    Some(&line[second + 1..])
}

/// Turn an MPL2 text field (after the brackets) into Pango markup.
fn build_markup(text: &str) -> String {
    let mut markup = String::with_capacity(text.len() + 8);
    let mut rest = text;

    loop {
        // Skip leading spaces and tabs (only these two, per the C).
        rest = rest.trim_start_matches([' ', '\t']);

        let italic = rest.starts_with('/');
        if italic {
            markup.push_str("<i>");
            rest = &rest[1..];
        }

        let (chunk_src, next) = match rest.find('|') {
            Some(i) => (&rest[..i], Some(i + 1)),
            None => (rest, None),
        };

        markup_escape(chunk_src, &mut markup);

        if italic {
            markup.push_str("</i>");
        }

        match next {
            Some(off) => {
                markup.push('\n');
                rest = &rest[off..];
            }
            None => break,
        }
    }

    // g_strstrip: trim ASCII whitespace (g_ascii_isspace) from both ends.
    strip_ascii_ws(&markup).to_string()
}

/// Trim GLib `g_ascii_isspace` characters from both ends: space, `\t`, `\n`,
/// `\r`, `\v` (0x0b) and `\f` (0x0c).
fn strip_ascii_ws(s: &str) -> &str {
    s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}'))
}

/// GLib `g_markup_escape_text` semantics: named references for the five XML
/// metacharacters, `&#x<hex>;` for the controls GLib escapes, which are the C0
/// set plus most of C1 (`0x7f..=0x84` and `0x86..=0x9f`, so `0x85` is *not*
/// escaped).
fn markup_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => {
                let u = c as u32;
                if (0x1..=0x8).contains(&u)
                    || (0xb..=0xc).contains(&u)
                    || (0xe..=0x1f).contains(&u)
                    || (0x7f..=0x84).contains(&u)
                    || (0x86..=0x9f).contains(&u)
                {
                    out.push_str("&#x");
                    push_hex(u, out);
                    out.push(';');
                } else {
                    out.push(c);
                }
            }
        }
    }
}

fn push_hex(mut v: u32, out: &mut String) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 8];
    let mut n = 0;
    while v > 0 {
        buf[n] = char::from_digit(v & 0xf, 16).unwrap() as u8;
        v >>= 4;
        n += 1;
    }
    for &d in buf[..n].iter().rev() {
        out.push(d as char);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn parse(body: &str) -> Vec<Cue> {
        Mpl2::default()
            .parse(body, &ParseContext::default())
            .unwrap()
    }

    // --- parity with subparse.c: test_mpl2 -------------------------------

    #[test]
    fn c_parity_full() {
        let body = "[123][456] This is the Earth at a time|when the dinosaurs roamed\n\
[1234][5678]a lush and fertile planet.\n\
[12345][27890] /Italic|Normal\n\
[32345][37890]/Italic|/Italic\n\
[42345][47890] Normal|/Italic";
        let cues = parse(body);
        assert_eq!(cues.len(), 5);

        assert_eq!(cues[0].start_ns, 123 * S / 10);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
        assert_eq!(
            cues[0].text,
            "This is the Earth at a time\nwhen the dinosaurs roamed"
        );

        assert_eq!(cues[1].start_ns, 1234 * S / 10);
        assert_eq!(cues[1].end_ns, Some(5678 * S / 10));
        assert_eq!(cues[1].text, "a lush and fertile planet.");

        assert_eq!(cues[2].start_ns, 12345 * S / 10);
        assert_eq!(cues[2].end_ns, Some(27890 * S / 10));
        assert_eq!(cues[2].text, "<i>Italic</i>\nNormal");

        assert_eq!(cues[3].text, "<i>Italic</i>\n<i>Italic</i>");

        assert_eq!(cues[4].start_ns, 42345 * S / 10);
        assert_eq!(cues[4].end_ns, Some(47890 * S / 10));
        assert_eq!(cues[4].text, "Normal\n<i>Italic</i>");
    }

    // --- deciseconds -----------------------------------------------------

    #[test]
    fn deciseconds_timing() {
        // [5][15] -> 0.5 s .. 1.5 s
        let cues = parse("[5][15]hi\n");
        assert_eq!(cues[0].start_ns, 500_000_000);
        assert_eq!(cues[0].end_ns, Some(1_500_000_000));
    }

    // --- italics & escaping ----------------------------------------------

    #[test]
    fn escapes_metacharacters() {
        let cues = parse("[1][2]a<b>&'c\n");
        assert_eq!(cues[0].text, "a&lt;b&gt;&amp;&apos;c");
    }

    #[test]
    fn escapes_c1_controls_like_glib() {
        // g_markup_escape_text escapes 0x7f..=0x84 and 0x86..=0x9f as well as
        // the C0 range, but leaves 0x85 alone.
        let cues = parse("[1][2]a\u{80}b\u{85}c\u{9f}d\u{7f}\n");
        assert_eq!(cues[0].text, "a&#x80;b\u{85}c&#x9f;d&#x7f;");
    }

    #[test]
    fn trailing_whitespace_stripped() {
        // g_strstrip removes the trailing space that survives escaping.
        let cues = parse("[1][2]text   \n");
        assert_eq!(cues[0].text, "text");
    }

    #[test]
    fn optional_space_after_bracket() {
        // The space between ']' and text is optional. Both forms parse.
        assert_eq!(parse("[1][2] hi\n")[0].text, "hi");
        assert_eq!(parse("[1][2]hi\n")[0].text, "hi");
    }

    #[test]
    fn empty_text_yields_empty_cue() {
        let cues = parse("[1][2]\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
    }

    // --- parity with mpl2parse.c: the sscanf count -----------------------

    #[test]
    fn c_parity_junk_after_the_second_timestamp() {
        // `sscanf("[%u][%u]")` has already assigned both numbers when it reaches
        // the final `]`, so it returns 2 whatever follows the second one. The
        // text then starts after the second real `]` in the line.
        let cues = parse("[123][456 x]y\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 123 * S / 10);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
        assert_eq!(cues[0].text, "y");
    }

    #[test]
    fn c_parity_signed_timestamp() {
        // `%u` is strtoul-based, so it takes a sign. Autodetect agrees.
        let cues = parse("[+123][456]y\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 123 * S / 10);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
        assert_eq!(cues[0].text, "y");

        // A negative value wraps into garbage in the C. We read it as 0, which
        // at least keeps the cue's end from preceding its start.
        let cues = parse("[-123][456]y\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(456 * S / 10));
    }

    #[test]
    fn missing_first_closing_bracket_is_not_a_record() {
        // The `]` between the two conversions *is* load-bearing: without it
        // sscanf stops at one assignment and the C skips the line.
        assert!(parse("[123 x][456]y\n").is_empty());
    }

    // --- lenient recovery ------------------------------------------------

    #[test]
    fn malformed_lines_skipped() {
        let cues = parse("not mpl2\n[1][2]ok\n[bad]\n[3][4]two\n");
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "ok");
        assert_eq!(cues[1].text, "two");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(Mpl2::default().output_format(), OutputFormat::PangoMarkup);
    }
}
