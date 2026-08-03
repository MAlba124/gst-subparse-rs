// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! SubViewer parser. See `specs/subviewer.md`.
//!
//! C reference: `parse_subviewer` in
//! `gst-plugins-base/gst/subparse/gstsubparse.c`. A cue is a
//! `HH:MM:SS.mmm,HH:MM:SS.mmm` timing line followed by one or more text lines,
//! terminated by a blank line. Header/metadata lines (`[INFORMATION]`,
//! `[TITLE]...`, `[COLF]...`, etc.) simply fail the timing match and are
//! skipped. `[br]` markers become newlines and trailing newlines are stripped.
//!
//! Quirk carried over from upstream: the fractional field is taken as a literal
//! millisecond count, not a decimal fraction. `44.40` is 44 s + 40 ms, and
//! `11.91` is 11 s + 91 ms.
use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;
const GST_MSECOND: u64 = 1_000_000;

/// Parser for the SubViewer subtitle format.
///
/// Streaming: a cue is emitted on its terminating blank line. Like the C
/// driver, only `\n`-terminated lines are parsed, so at EOS an unterminated
/// remainder is discarded rather than parsed (it can never complete a cue
/// anyway, since a cue needs a blank line after it).
#[derive(Debug, Default)]
pub struct SubViewer {
    lines: LineScanner,
    machine: Machine,
}

impl SubtitleFormat for SubViewer {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self { lines, machine } = self;

        let mut cues = Vec::new();
        let mut consumed = lines.feed(body, |line| machine.feed(line, &mut cues));

        if at_eos {
            // The unterminated remainder is dropped, matching `get_next_line`.
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::Utf8
    }
}

/// The SubViewer line machine, carried across `parse_incremental` calls.
#[derive(Debug, Default)]
struct Machine {
    /// 0 = expect timing line, 1 = accumulate text.
    state: u8,
    start_ns: u64,
    duration_ns: u64,
    buf: String,
}

impl Machine {
    /// Feed one complete line (terminator and any `\r` already removed).
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        if self.state == 0 {
            if let Some((start, dur)) = parse_timing(line) {
                self.state = 1;
                self.start_ns = start;
                self.duration_ns = dur;
            }
            // Non-timing lines (headers) are skipped.
        } else {
            if !self.buf.is_empty() {
                self.buf.push('\n');
            }
            self.buf.push_str(line);
            if line.is_empty() {
                let mut text = unescape_newlines_br(&self.buf);
                strip_trailing_newlines(&mut text);
                self.buf.clear();
                self.state = 0;
                cues.push(Cue::new(
                    self.start_ns,
                    Some(self.start_ns.saturating_add(self.duration_ns)),
                    text,
                ));
            }
        }
    }
}

/// Parse a `%u:%u:%u.%u,%u:%u:%u.%u` timing line into `(start_ns, duration_ns)`.
fn parse_timing(line: &str) -> Option<(u64, u64)> {
    let b = line.as_bytes();
    let mut pos = 0;

    let h1 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let m1 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let s1 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b'.') {
        return None;
    }
    let ms1 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b',') {
        return None;
    }
    let h2 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let m2 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b':') {
        return None;
    }
    let s2 = read_uint(b, &mut pos)?;
    if !lit(b, &mut pos, b'.') {
        return None;
    }
    let ms2 = read_uint(b, &mut pos)?;

    let start = hmsms_to_ns(h1, m1, s1, ms1);
    let end = hmsms_to_ns(h2, m2, s2, ms2);
    Some((start, end.saturating_sub(start)))
}

fn hmsms_to_ns(h: u64, m: u64, s: u64, ms: u64) -> u64 {
    h.saturating_mul(3600)
        .saturating_add(m.saturating_mul(60))
        .saturating_add(s)
        .saturating_mul(GST_SECOND)
        .saturating_add(ms.saturating_mul(GST_MSECOND))
}

/// Strip trailing `\n` bytes, keeping at least one character (upstream
/// `strip_trailing_newlines`).
fn strip_trailing_newlines(s: &mut String) {
    while s.len() > 1 && s.ends_with('\n') {
        s.pop();
    }
}

/// Replace every `[br]` with a newline (upstream `unescape_newlines_br`).
fn unescape_newlines_br(text: &str) -> String {
    let b = text.as_bytes();
    if b.len() < 4 {
        return text.to_string();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b.len() - i >= 4 && &b[i..i + 4] == b"[br]" {
            out.push(b'\n');
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("ascii-only substitution preserves utf8")
}

/// Read a run of ASCII digits as a decimal integer (scanf `%u`), skipping
/// leading ASCII whitespace. Returns `None` when no digit is present.
fn read_uint(b: &[u8], pos: &mut usize) -> Option<u64> {
    while *pos < b.len() && b[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    let mut val: u64 = 0;
    let mut n = 0;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
        val = val
            .saturating_mul(10)
            .saturating_add((b[*pos] - b'0') as u64);
        *pos += 1;
        n += 1;
    }
    if n == 0 { None } else { Some(val) }
}

fn lit(b: &[u8], pos: &mut usize, c: u8) -> bool {
    if *pos < b.len() && b[*pos] == c {
        *pos += 1;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Cue> {
        SubViewer::default()
            .parse(body, &ParseContext::default())
            .unwrap()
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(SubViewer::default().output_format(), OutputFormat::Utf8);
    }

    // Ported from the C suite `test_subviewer` (subparse.c).
    #[test]
    fn c_parity_with_header() {
        let body = concat!(
            "[INFORMATION]\n",
            "[TITLE]xxxxxxxxxx\n",
            "[AUTHOR]xxxxxxxx\n",
            "[SOURCE]xxxxxxxxxxxxxxxx\n",
            "[FILEPATH]\n",
            "[DELAY]0\n",
            "[COMMENT]\n",
            "[END INFORMATION]\n",
            "[SUBTITLE]\n",
            "[COLF]&HFFFFFF,[STYLE]bd,[SIZE]18,[FONT]Arial\n",
            "00:00:41.00,00:00:44.40\n",
            "The Age of Gods was closing.\n",
            "Eternity had come to an end.\n",
            "\n",
            "00:00:55.00,00:00:58.40\n",
            "The heavens shook as the armies\n",
            "of Falis, God of Light...\n",
            "\n",
        );
        let cues = parse(body);
        assert_eq!(cues.len(), 2);

        assert_eq!(cues[0].start_ns, 41 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(44 * GST_SECOND + 40 * GST_MSECOND));
        assert_eq!(
            cues[0].text,
            "The Age of Gods was closing.\nEternity had come to an end."
        );

        assert_eq!(cues[1].start_ns, 55 * GST_SECOND);
        assert_eq!(cues[1].end_ns, Some(58 * GST_SECOND + 40 * GST_MSECOND));
        assert_eq!(
            cues[1].text,
            "The heavens shook as the armies\nof Falis, God of Light..."
        );
    }

    // Ported from the C suite `test_subviewer2` (subparse.c), covering `[br]` newlines.
    #[test]
    fn c_parity_br_newlines() {
        let body = concat!(
            "[INFORMATION]\n",
            "[TITLE]xxxxxxxxxx\n",
            "[END INFORMATION]\n",
            "[SUBTITLE]\n",
            "[COLF]&H00FFFF,[STYLE]no,[SIZE]12,[FONT]Courier New\n",
            "00:00:07.00,00:00:11.91\n",
            "THERE IS A PLACE ON EARTH WHERE IT[br]IS STILL THE MORNING OF LIFE...\n",
            "\n",
            "00:00:12.48,00:00:15.17\n",
            "AND THE GREAT HERDS RUN FREE.[br]SO WHAT?!\n",
            "\n",
        );
        let cues = parse(body);
        assert_eq!(cues.len(), 2);

        assert_eq!(cues[0].start_ns, 7 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(11 * GST_SECOND + 91 * GST_MSECOND));
        assert_eq!(
            cues[0].text,
            "THERE IS A PLACE ON EARTH WHERE IT\nIS STILL THE MORNING OF LIFE..."
        );

        assert_eq!(cues[1].start_ns, 12 * GST_SECOND + 48 * GST_MSECOND);
        assert_eq!(cues[1].end_ns, Some(15 * GST_SECOND + 17 * GST_MSECOND));
        assert_eq!(cues[1].text, "AND THE GREAT HERDS RUN FREE.\nSO WHAT?!");
    }

    #[test]
    fn fraction_is_literal_milliseconds() {
        // ".5" is 5 ms, not 500 ms (upstream quirk).
        let cues = parse("00:00:01.5,00:00:02.5\ntext\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, GST_SECOND + 5 * GST_MSECOND);
        assert_eq!(cues[0].end_ns, Some(2 * GST_SECOND + 5 * GST_MSECOND));
    }

    #[test]
    fn single_line_cue() {
        let cues = parse("00:01:00.00,00:01:02.00\nHello\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 60 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(62 * GST_SECOND));
        assert_eq!(cues[0].text, "Hello");
    }

    #[test]
    fn hours_are_honored() {
        let cues = parse("01:00:00.00,01:00:01.00\nx\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 3600 * GST_SECOND);
    }

    #[test]
    fn trailing_newlines_are_stripped() {
        // Blank text lines before the terminator become trailing newlines and
        // are stripped down to (at most) the content.
        let cues = parse("00:00:01.00,00:00:02.00\nline\n\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "line");
    }

    #[test]
    fn cue_with_no_text_is_emitted_empty() {
        let cues = parse("00:00:01.00,00:00:02.00\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
    }

    #[test]
    fn unterminated_final_cue_is_dropped() {
        // No trailing blank line -> the last cue is never flushed.
        let cues = parse("00:00:01.00,00:00:02.00\nkept\n\n00:00:03.00,00:00:04.00\nlost\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "kept");
    }

    #[test]
    fn empty_body() {
        assert!(parse("").is_empty());
    }
}
