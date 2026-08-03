// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! MPSub parser. See `specs/mpsub.md`.
//!
//! C reference: `parse_mpsub` in
//! `gst-plugins-base/gst/subparse/gstsubparse.c`. MPSub uses *relative* timing.
//! A `<offset> <duration>` line (two floats, in seconds) opens a cue, where the
//! offset is measured from the end of the previous cue. Text lines follow and a
//! blank line terminates the cue. Header lines (e.g. `FORMAT=TIME`) fail the
//! two-float match and are skipped.
//!
//! The arithmetic is done in `f32` to match upstream, which stores the times as
//! `float`. Timestamps therefore accumulate `float` rounding exactly as the C
//! does. Unlike SubViewer, MPSub performs **no** `[br]` unescaping and does
//! **not** strip the trailing newline the blank terminator adds to the text.
//!
//! The running start is also advanced by the finished cue's duration a *second*
//! time, once by the parser and once by the driver. That double count is
//! upstream's, and reproducing it is deliberate. See [`Machine::feed`].
use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;

/// Parser for the MPSub subtitle format.
///
/// Streaming: MPSub timings are *relative*, so the running start/duration is
/// exactly the state that has to survive between calls. It lives in
/// [`Machine`], which also keeps the `f32` accumulation faithful to upstream:
/// the rounding depends only on the sequence of cues, not on how the bytes were
/// chunked.
#[derive(Debug, Default)]
pub struct MpSub {
    lines: LineScanner,
    machine: Machine,
}

impl SubtitleFormat for MpSub {
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

/// The MPSub line machine, carried across `parse_incremental` calls.
#[derive(Debug, Default)]
struct Machine {
    /// 0 = expect a timing line, 1 = accumulate text.
    state: u8,
    /// Running cue start (relative timings accumulate into this).
    start_ns: u64,
    duration_ns: u64,
    buf: String,
}

impl Machine {
    /// Feed one complete line (terminator and any `\r` already removed).
    ///
    /// One quirk is load-bearing here. An MPSub offset is measured from the end
    /// of the previous cue, and `parse_mpsub` already accounts for that with
    /// `start_time += duration + GST_SECOND * t1` (gstsubparse.c:1344). The
    /// element's push loop then advances the very same field again, by the very
    /// same duration, right after it pushes a buffer (gstsubparse.c:1848, added
    /// for TMPlayer, which needs it). So every MPSub cue after the first starts
    /// one duration late, and the error accumulates over the file.
    ///
    /// This port reproduces that. It is a drop-in replacement for the C element
    /// (the same bytes have to yield the same timestamps, down to the `f32`
    /// rounding above), not a corrected MPSub implementation, and the C is the
    /// only reference this de-facto format has. `specs/mpsub.md` spells the
    /// arithmetic out, including what the "correct" reading would be.
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        if self.state == 0 {
            if let Some((t1, t2)) = parse_two_floats(line) {
                self.state = 1;
                // start_time += duration + GST_SECOND * t1  (all in f32).
                let inc = self.duration_ns as f32 + GST_SECOND as f32 * t1;
                self.start_ns = (self.start_ns as f32 + inc) as u64;
                self.duration_ns = (GST_SECOND as f32 * t2) as u64;
            }
        } else {
            if !self.buf.is_empty() {
                self.buf.push('\n');
            }
            self.buf.push_str(line);
            if line.is_empty() {
                let text = std::mem::take(&mut self.buf);
                self.state = 0;
                cues.push(Cue::new(
                    self.start_ns,
                    Some(self.start_ns.saturating_add(self.duration_ns)),
                    text,
                ));
                // The driver's post-push `start_time += duration`, in `u64` like
                // the C. The next timing line adds the same duration once more.
                self.start_ns = self.start_ns.saturating_add(self.duration_ns);
            }
        }
    }
}

/// Parse two leading floats (scanf `"%f %f"`). Returns `None` unless both are
/// present, mirroring sscanf returning fewer than 2.
fn parse_two_floats(line: &str) -> Option<(f32, f32)> {
    let b = line.as_bytes();
    let mut pos = 0;
    let t1 = read_float(b, &mut pos)?;
    let t2 = read_float(b, &mut pos)?;
    Some((t1, t2))
}

/// Read one decimal float token (scanf `%f`), skipping leading ASCII
/// whitespace. Accepts an optional sign, integer/fraction digits and an
/// optional exponent. Returns `None` when no numeric token is present.
fn read_float(b: &[u8], pos: &mut usize) -> Option<f32> {
    while *pos < b.len() && b[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
    let start = *pos;

    if *pos < b.len() && (b[*pos] == b'+' || b[*pos] == b'-') {
        *pos += 1;
    }
    let mut digits = false;
    while *pos < b.len() && b[*pos].is_ascii_digit() {
        *pos += 1;
        digits = true;
    }
    if *pos < b.len() && b[*pos] == b'.' {
        *pos += 1;
        while *pos < b.len() && b[*pos].is_ascii_digit() {
            *pos += 1;
            digits = true;
        }
    }
    if !digits {
        *pos = start;
        return None;
    }
    // Optional exponent. Back out if there are no exponent digits.
    if *pos < b.len() && (b[*pos] == b'e' || b[*pos] == b'E') {
        let save = *pos;
        *pos += 1;
        if *pos < b.len() && (b[*pos] == b'+' || b[*pos] == b'-') {
            *pos += 1;
        }
        let mut exp_digits = false;
        while *pos < b.len() && b[*pos].is_ascii_digit() {
            *pos += 1;
            exp_digits = true;
        }
        if !exp_digits {
            *pos = save;
        }
    }

    // The token is ASCII, so this slice is always valid UTF-8.
    std::str::from_utf8(&b[start..*pos])
        .ok()?
        .parse::<f32>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Cue> {
        MpSub::default()
            .parse(body, &ParseContext::default())
            .unwrap()
    }

    #[test]
    fn output_format_is_utf8() {
        assert_eq!(MpSub::default().output_format(), OutputFormat::Utf8);
    }

    #[test]
    fn relative_timing_accumulates() {
        // cue0: start += 0 + 2.0 = 2.0, dur = 3.0 -> [2.0, 5.0]
        // push:  start += 3.0            = 5.0
        // cue1: start += 3.0 + 1.0 = 9.0, dur = 2.0 -> [9.0, 11.0], and 9e9 is
        //       not exactly representable in f32 (see below).
        let body = "FORMAT=TIME\n2.0 3.0\nHello\n\n1.0 2.0\nWorld\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);

        assert_eq!(cues[0].start_ns, 2 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(5 * GST_SECOND));
        // MPSub keeps the trailing newline the blank terminator adds.
        assert_eq!(cues[0].text, "Hello\n");

        assert_eq!(cues[1].start_ns, 8_999_999_488);
        assert_eq!(cues[1].end_ns, Some(10_999_999_488));
        assert_eq!(cues[1].text, "World\n");
    }

    #[test]
    fn c_parity_duration_is_counted_twice_per_cue() {
        // The upstream double count (parser + driver both advance the running
        // start by the finished cue's duration), pinned so it cannot silently
        // turn into the arithmetic the format actually calls for. That reading
        // would put these three cues at 2 s, 6 s and 9 s. The C, and therefore
        // this port, puts them at 2 s, 9 s and 14 s (modulo f32 rounding).
        let body = "FORMAT=TIME\n2.0 3.0\nHello\n\n1.0 2.0\nWorld\n\n1.0 2.0\nThird\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].start_ns, 2 * GST_SECOND);
        assert_eq!(cues[1].start_ns, 8_999_999_488);
        assert_eq!(cues[2].start_ns, 13_999_998_976);
    }

    #[test]
    fn f32_accumulation_is_faithful() {
        // Upstream keeps the running time as C `float`. 9.5e9 is not exactly
        // representable in f32, so `2.0 3.0` then `1.5 2.0` lands a couple of
        // hundred ns past 9.5 s. We reproduce that exactly rather than round.
        let body = "2.0 3.0\nx\n\n1.5 2.0\ny\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].start_ns, 9_500_000_256);
        assert_eq!(cues[1].end_ns, Some(11_500_000_256));
    }

    #[test]
    fn multiline_text_preserves_trailing_newline() {
        let body = "0.0 4.0\nLine1\nLine2\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(4 * GST_SECOND));
        assert_eq!(cues[0].text, "Line1\nLine2\n");
    }

    #[test]
    fn header_lines_are_skipped() {
        // Anything that is not two floats up front is ignored while in state 0.
        let body = "FORMAT=TIME\n\n5.0 1.0\nHi\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * GST_SECOND);
        assert_eq!(cues[0].end_ns, Some(6 * GST_SECOND));
    }

    #[test]
    fn single_float_line_is_not_a_cue() {
        // sscanf("%f %f") would return 1, i.e. not a timing line.
        let body = "10\nnot text\n\n";
        let cues = parse(body);
        assert!(cues.is_empty());
    }

    #[test]
    fn fractional_offsets_and_durations() {
        // cue0: start = 1.5, dur = 0.5 -> [1.5, 2.0]
        let body = "1.5 0.5\nx\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 1_500_000_000);
        assert_eq!(cues[0].end_ns, Some(2_000_000_000));
    }

    #[test]
    fn unterminated_final_cue_is_dropped() {
        // No blank terminator line -> the last cue is never flushed.
        let body = "2.0 3.0\nHello\n";
        let cues = parse(body);
        assert!(cues.is_empty());
    }

    #[test]
    fn empty_body() {
        assert!(parse("").is_empty());
    }
}
