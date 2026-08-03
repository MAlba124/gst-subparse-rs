// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! TMPlayer parser.
//!
//! See `specs/tmplayer.md`. C reference: `tmplayer_parse_line` in
//! `gst-plugins-base/gst/subparse/tmplayerparse.c`.
//!
//! TMPlayer is a line format with several varieties, all `HH:MM:SS<sep>text`:
//!   * single line, `<sep>` is `:` or `=`  (`00:00:50:text` / `00:00:50=text`);
//!   * multiline,   `HH:MM:SS,<n>=text`     (`00:00:50,1=` .. `,2=` ..).
//!
//! Hours may be one or two digits (a longer hour field scans, but then the C's
//! fixed-offset text search misplaces the text, see [`parse_single`]). `|`
//! inside a line is a hard line break.
//!
//! A cue has **no explicit end time**. Its duration is deduced from the *next*
//! timestamp. So parsing buffers text until the next timestamped (or empty)
//! line closes it. Because open-ended cues could otherwise linger, the element
//! clamps each duration to [`MAX_DURATION`] (`max_duration = 5 * GST_SECOND`).
//! The very last cue (flushed at end-of-stream) has no end at all.
//!
//! Output is plain UTF-8 (no markup, no escaping).

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;
/// The element's `max_duration` for TMPlayer: cap a deduced duration at 5 s.
const MAX_DURATION: u64 = 5 * GST_SECOND;

/// Parser for the TMPlayer subtitle format.
///
/// Streaming: a TMPlayer cue has no end time of its own, so it is closed by the
/// *next* timestamp. Exactly one cue is therefore held open at any moment, and
/// that (plus the running line counter the C gates its "close previous unit"
/// path on) is the state carried between calls.
#[derive(Debug, Default)]
pub struct TmPlayer {
    lines: LineScanner,
    machine: State,
}

impl SubtitleFormat for TmPlayer {
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
            // The whole-body parser iterated `split('\n')`, which also yields
            // the unterminated remainder, and then flushed whatever text was
            // still buffered (the "\n\n" the element used to inject at EOS).
            // The final cue gets no end time.
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            machine.feed(tail, &mut cues);
            machine.flush_eos(&mut cues);
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        // parse_tmplayer advertises `text/x-raw, format=utf8`.
        OutputFormat::Utf8
    }
}

#[derive(Debug, Default)]
struct State {
    /// Start time of the text currently accumulating in `buf`. The C inits this
    /// to 0, which `GST_CLOCK_TIME_IS_VALID` treats as valid, so it is always
    /// valid for TMPlayer and we track it as a plain `u64`.
    start_time: u64,
    /// Accumulated text for the cue being built (`|` kept until flush).
    buf: String,
    /// The C's line counter (`state->state`) starts at 0 and increments per
    /// line. The "close previous unit" path is gated on `line_num > 0`, so it
    /// has to survive across chunk boundaries.
    line_num: u64,
}

impl State {
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        let line_num = self.line_num;
        self.line_num += 1;

        if let Some((ts, l, text)) = parse_multiline(line) {
            self.handle_timestamped(ts, l, text, line_num, cues);
        } else if let Some((ts, text)) = parse_single(line) {
            self.handle_timestamped(ts, 1, text, line_num, cues);
        } else if line.is_empty() && !self.buf.is_empty() {
            // Empty line with buffered text: push it out with no duration.
            let start = self.start_time;
            self.emit(start, None, cues);
        }
        // else: unparsable line -> skip (lenient recovery).
    }

    /// A line that carries a timestamp (single or multiline).
    fn handle_timestamped(
        &mut self,
        ts: u64,
        l: u64,
        text: &str,
        line_num: u64,
        cues: &mut Vec<Cue>,
    ) {
        let end_of_unit = text.is_empty() || (l == 1 && !self.buf.is_empty());

        if end_of_unit {
            // Close the previous subtitle unit, if any.
            if self.start_time < ts && line_num > 0 {
                let d = ts - self.start_time;
                let start = self.start_time;
                self.emit(start, Some(d), cues);
                // Advance by the *unclamped* duration so the next start lands on
                // `ts` (durations are deduced from consecutive start times).
                self.start_time = start + d;
                // Carry this line's own text into the next unit.
                if !text.is_empty() {
                    self.buf.push_str(text);
                }
            }
            // else: no valid previous unit / first line -> nothing to close.
        } else {
            // Still accumulating this unit's text.
            if l > 1 {
                self.buf.push('\n');
            }
            self.buf.push_str(text);
            self.start_time = ts;
        }
    }

    /// Emit the buffered text as a cue starting at `start`. `dur` is the
    /// unclamped deduced duration (`None` = open-ended). Clears the buffer.
    fn emit(&mut self, start: u64, dur: Option<u64>, cues: &mut Vec<Cue>) {
        let text = self.buf.replace('|', "\n");
        self.buf.clear();
        let end = dur.map(|d| start.saturating_add(d.min(MAX_DURATION)));
        cues.push(Cue::new(start, end, text));
    }

    fn flush_eos(&mut self, cues: &mut Vec<Cue>) {
        if !self.buf.is_empty() {
            let start = self.start_time;
            self.emit(start, None, cues);
        }
    }
}

/// Parse the multiline variety `HH:MM:SS,<n>=text`. Returns `(ts_ns, n, text)`.
fn parse_multiline(line: &str) -> Option<(u64, u64, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    let h = scan_uint(b, &mut i, 0)?;
    expect(b, &mut i, b':')?;
    let m = scan_uint(b, &mut i, 2)?;
    expect(b, &mut i, b':')?;
    let s = scan_uint(b, &mut i, 2)?;
    expect(b, &mut i, b',')?;
    let l = scan_uint(b, &mut i, 0)?;
    // The separator must be '=' for the multiline variety.
    if b.get(i) != Some(&b'=') {
        return None;
    }
    i += 1;
    // The C takes the text from `strchr (line, '=')`, which can only land on
    // this separator: everything the scan consumed before it is whitespace,
    // digits, colons and the comma.
    Some((to_ns(h, m, s), l, &line[i..]))
}

/// Parse the single-line variety `HH:MM:SS<sep>text`, `<sep>` in {`:`, `=`}.
///
/// The text does *not* begin where the scan stopped. The C looks the separator
/// up again with `strchr (line + 6, divc)` (tmplayerparse.c:100), so the text is
/// whatever follows the **first** separator at or after byte 6. For a one- or
/// two-digit hour that is the separator just matched, but a three-digit hour
/// puts byte 6 inside the timestamp: `100:00:10:text` has its text as
/// `10:text`. A separator that occurs only before byte 6 gives the C a NULL
/// `text_start`, which it then treats exactly like an empty text (its append is
/// guarded by `if (text_start)`), so both cases map to `""` here.
fn parse_single(line: &str) -> Option<(u64, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    let h = scan_uint(b, &mut i, 0)?;
    expect(b, &mut i, b':')?;
    let m = scan_uint(b, &mut i, 2)?;
    expect(b, &mut i, b':')?;
    let s = scan_uint(b, &mut i, 2)?;
    let sep = *b.get(i)?;
    if sep != b':' && sep != b'=' {
        return None;
    }
    // A line matching this format is at least six bytes of ASCII (`0:0:0:`), so
    // byte 6 is always a character boundary, as is the byte after an ASCII
    // separator found from there on.
    let text = match b.iter().skip(6).position(|&c| c == sep) {
        Some(rel) => &line[6 + rel + 1..],
        None => "",
    };
    Some((to_ns(h, m, s), text))
}

fn to_ns(h: u64, m: u64, s: u64) -> u64 {
    // TMPlayer does not cap the hour field, so an absurd value must saturate
    // rather than overflow-panic.
    h.saturating_mul(3600)
        .saturating_add(m.saturating_mul(60))
        .saturating_add(s)
        .saturating_mul(GST_SECOND)
}

fn expect(b: &[u8], i: &mut usize, c: u8) -> Option<()> {
    if b.get(*i) == Some(&c) {
        *i += 1;
        Some(())
    } else {
        None
    }
}

/// Is `c` one of the characters C's `isspace()` treats as whitespace?
fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Read ASCII decimal digits as `u64`. `max_digits > 0` caps the field width
/// (the C's `%02u` for minutes/seconds). `0` means unlimited (`%u`).
///
/// Leading whitespace is skipped, because the C's `%u`/`%02u` skip it and the
/// autodetect probe (which decides this format is TMPlayer at all) reproduces
/// that. Without the skip here, `00: 0:10:Hello` would be detected as TMPlayer
/// and then parse to nothing. Skipped whitespace does not count against
/// `max_digits`, matching scanf's field width, which only counts the input item.
fn scan_uint(b: &[u8], i: &mut usize, max_digits: usize) -> Option<u64> {
    let entry = *i;
    while matches!(b.get(*i), Some(&c) if is_c_space(c)) {
        *i += 1;
    }
    let start = *i;
    let mut val: u64 = 0;
    let mut n = 0;
    while let Some(d @ b'0'..=b'9') = b.get(*i) {
        if max_digits != 0 && n == max_digits {
            break;
        }
        val = val.saturating_mul(10).saturating_add((d - b'0') as u64);
        *i += 1;
        n += 1;
    }
    if *i == start {
        // No digits: the conversion failed, so leave the cursor untouched.
        *i = entry;
        None
    } else {
        Some(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    fn parse(body: &str) -> Vec<Cue> {
        TmPlayer::default()
            .parse(body, &ParseContext::default())
            .unwrap()
    }

    #[test]
    fn huge_hour_saturates_and_does_not_panic() {
        // TMPlayer does not cap the hour field, so `h * 3600 * GST_SECOND` would
        // overflow (panic under overflow checks) on a giant hour. Found by fuzz.
        let cues = parse("5555001070:0:00=hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, u64::MAX);
    }

    // --- parity with subparse.c: multiline -------------------------------

    #[test]
    fn c_parity_multiline() {
        let body = "00:00:10,1=This is the Earth at a time\n\
00:00:10,2=when the dinosaurs roamed...\n\
00:00:13,1=\n\
00:00:14,1=a lush and fertile planet.\n\
00:00:16,1=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(
            cues[0].text,
            "This is the Earth at a time\nwhen the dinosaurs roamed..."
        );
        assert_eq!(cues[1].start_ns, 14 * S);
        assert_eq!(cues[1].end_ns, Some(16 * S));
        assert_eq!(cues[1].text, "a lush and fertile planet.");
    }

    #[test]
    fn c_parity_multiline_with_bogus_lines() {
        let body = "00:00:10,1=This is the Earth at a time\n\
Yooboo wabahablablahuguug bogus line hello test 1-2-3-4\n\
00:00:10,2=when the dinosaurs roamed...\n\
00:00:13,1=\n\
00:00:14,1=a lush and fertile planet.\n\
00:00:16,1=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(
            cues[0].text,
            "This is the Earth at a time\nwhen the dinosaurs roamed..."
        );
        assert_eq!(cues[1].text, "a lush and fertile planet.");
    }

    // --- parity with subparse.c: single-line styles ----------------------

    #[test]
    fn c_parity_style1_colon_two_digit_hour() {
        let body = "00:00:10:This is the Earth at a time|when the dinosaurs roamed...\n\
00:00:13:\n\
00:00:14:a lush and fertile planet.\n\
00:00:16:\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(
            cues[0].text,
            "This is the Earth at a time\nwhen the dinosaurs roamed..."
        );
        assert_eq!(cues[1].start_ns, 14 * S);
        assert_eq!(cues[1].end_ns, Some(16 * S));
    }

    #[test]
    fn c_parity_style2_equals() {
        let body = "00:00:10=This is the Earth at a time|when the dinosaurs roamed...\n\
00:00:13=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
    }

    #[test]
    fn c_parity_style3_one_digit_hour_colon() {
        let body = "0:00:10:This is the Earth at a time|when the dinosaurs roamed...\n\
0:00:13:\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(
            cues[0].text,
            "This is the Earth at a time\nwhen the dinosaurs roamed..."
        );
    }

    #[test]
    fn c_parity_style4_one_digit_hour_equals() {
        let body = "0:00:10=This is the Earth at a time|when the dinosaurs roamed...\n\
0:00:13=\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
    }

    #[test]
    fn c_parity_style4_with_bogus_lines() {
        // A comment line and a whitespace-only line are both skipped (neither is
        // an empty line, so neither flushes the buffer). Built via join so the
        // whitespace-only line keeps its literal spaces.
        let body = [
            "0:00:10=This is the Earth at a time|when the dinosaurs roamed...",
            "# This is a bogus line with a comment and should just be skipped",
            "0:00:13=",
            "0:00:14=a lush and fertile planet.",
            "                                                            ",
            "0:00:16=",
            "",
        ]
        .join("\n");
        let cues = parse(&body);
        assert_eq!(cues.len(), 2);
        assert_eq!(
            cues[0].text,
            "This is the Earth at a time\nwhen the dinosaurs roamed..."
        );
        assert_eq!(cues[1].text, "a lush and fertile planet.");
    }

    // --- parity with subparse.c: style3b (max_duration + final flush) ----

    #[test]
    fn c_parity_style3b_clamp_and_final_flush() {
        // No trailing empty lines. Consecutive timestamps deduce durations, the
        // 14 s gap is clamped to 5 s, and the last line flushes open-ended.
        let body = "0:00:10:This is the Earth at a time|when the dinosaurs roamed...\n\
0:00:14:a lush and fertile planet.\n\
0:00:16:And they liked it a lot.\n\
0:00:30:Last line.";
        let cues = parse(body);
        assert_eq!(cues.len(), 4);

        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(14 * S));

        assert_eq!(cues[1].start_ns, 14 * S);
        assert_eq!(cues[1].end_ns, Some(16 * S));

        // 16 s -> 30 s is 14 s, clamped to 5 s.
        assert_eq!(cues[2].start_ns, 16 * S);
        assert_eq!(cues[2].end_ns, Some((16 + 5) * S));
        assert_eq!(cues[2].text, "And they liked it a lot.");

        // Last line: open-ended (no next timestamp).
        assert_eq!(cues[3].start_ns, 30 * S);
        assert_eq!(cues[3].end_ns, None);
        assert_eq!(cues[3].text, "Last line.");
    }

    // --- parity with subparse.c: sscanf quirks ---------------------------

    #[test]
    fn c_parity_whitespace_inside_the_timestamp() {
        // `%02u` skips leading whitespace, so ` 0` is a valid minutes field and
        // this is a TMPlayer line (which is also what autodetect concludes).
        let body = "00: 0:10:Hello|there\n00:00:13:\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 10 * S);
        assert_eq!(cues[0].end_ns, Some(13 * S));
        assert_eq!(cues[0].text, "Hello\nthere");
    }

    #[test]
    fn c_parity_text_starts_at_the_first_separator_from_byte_six() {
        // `strchr (line + 6, divc)` finds no ':' past byte 6 here, which the C
        // treats as a line without text: nothing is buffered, so nothing is
        // flushed at EOS either.
        assert!(parse("0:0:1:text\n").is_empty());

        // With a three-digit hour, byte 6 lands *inside* the timestamp, so the
        // ':' before the seconds is taken as the separator and the text keeps
        // the seconds field.
        let cues = parse("100:00:10:text\n");
        assert_eq!(cues.len(), 1);
        // 100 h + 10 s.
        assert_eq!(cues[0].start_ns, 360_010 * S);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "10:text");
    }

    // --- edge cases ------------------------------------------------------

    #[test]
    fn final_unterminated_line_is_flushed() {
        // No trailing newline. EOS flush still emits the pending cue.
        let cues = parse("0:00:05:hello");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * S);
        assert_eq!(cues[0].end_ns, None);
        assert_eq!(cues[0].text, "hello");
    }

    #[test]
    fn pipe_becomes_newline() {
        let cues = parse("0:00:01:a|b|c\n0:00:02:\n");
        assert_eq!(cues[0].text, "a\nb\nc");
    }

    #[test]
    fn no_escaping_of_markup_chars() {
        // TMPlayer output is plain utf8. '<', '&', '\'' pass through untouched.
        let cues = parse("0:00:01:a<b>&'c\n0:00:02:\n");
        assert_eq!(cues[0].text, "a<b>&'c");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn only_bogus_lines_yield_no_cues() {
        assert!(parse("hello\nworld\n").is_empty());
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(TmPlayer::default().output_format(), OutputFormat::Utf8);
    }
}
