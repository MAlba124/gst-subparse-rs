// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! SubRip (`.srt`) parser. See `specs/subrip.md`.
//!
//! Ported from the upstream GStreamer `subparse` plugin's `parse_subrip`
//! (and its helpers `parse_subrip_time`, `subrip_unescape_formatting`,
//! `subrip_remove_unhandled_tags`, `strip_trailing_newlines`,
//! `subrip_fix_up_markup`) in
//! `gst-plugins-base/gst/subparse/gstsubparse.c`.
//!
//! Output is Pango markup. The cue text is markup-escaped, then a small
//! white-list of tags (`<i>`, `<b>`, `<u>`) is un-escaped back into real
//! markup, unknown tags are dropped, and the markup is balanced (missing
//! closing tags added, stray closing tags removed).
//!
//! The parser is a tiny three-state line machine:
//!   0: expect a numeric cue id
//!   1: expect a `HH:MM:SS,mmm --> HH:MM:SS,mmm` time line
//!   2: accumulate text until a blank line, then emit the cue
//! Malformed lines drop the machine back to state 0 (lenient recovery).

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

/// Tags SubRip keeps as real markup. Everything else is escaped/stripped.
/// Mirrors `allowed_srt_tags` in the C (`{ "i", "b", "u" }`).
const ALLOWED_TAGS: [&[u8]; 3] = [b"i", b"b", b"u"];

const NS_PER_SECOND: u64 = 1_000_000_000;
const NS_PER_MSECOND: u64 = 1_000_000;

/// Parser for the SubRip (`.srt`) subtitle format.
///
/// Streaming: lines are fed to the state machine as they complete, and a cue is
/// emitted the moment its terminating blank line arrives. Only the partial
/// trailing line is left unconsumed, so a caller draining `consumed` retains
/// O(one line) regardless of how long the stream runs.
#[derive(Debug, Default)]
pub struct SubRip {
    lines: LineScanner,
    machine: Machine,
    /// Whether the one-time leading-BOM check has run.
    bom_checked: bool,
}

impl SubtitleFormat for SubRip {
    fn parse_incremental(
        &mut self,
        body: &str,
        _ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        let Self {
            lines,
            machine,
            bom_checked,
        } = self;

        // Charset/BOM handling lives in the element. Be robust standalone and
        // drop a leading UTF-8 BOM if one made it this far. `body` is a `&str`,
        // so it can never hold a *partial* BOM: the only undecidable case is an
        // empty body, which is worth waiting on rather than deciding wrongly.
        let mut skipped = 0usize;
        if !*bom_checked {
            if body.is_empty() && !at_eos {
                return Ok(Parsed::default());
            }
            *bom_checked = true;
            if body.starts_with('\u{feff}') {
                skipped = '\u{feff}'.len_utf8();
            }
        }
        let rest = &body[skipped..];

        let mut cues = Vec::new();
        let mut consumed = skipped + lines.feed(rest, |line| machine.feed(line, &mut cues));

        if at_eos {
            // The whole-body parser iterated `split('\n')`, which also yields the
            // unterminated remainder, and then appended one synthetic blank line
            // (the `"\n\n"` the element used to force-push at EOS) so a final
            // cue without a blank terminator is still emitted.
            let tail = &body[consumed..];
            let tail = tail.strip_suffix('\r').unwrap_or(tail);
            machine.feed(tail, &mut cues);
            machine.feed("", &mut cues);
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        OutputFormat::PangoMarkup
    }
}

/// The three-state line machine, carried across calls.
#[derive(Debug)]
struct Machine {
    state: State,
    /// Monotonicity guard. The C keeps `state->start_time` (which becomes the
    /// previous cue's end time after emission) and rejects a time line whose
    /// end is before it. Initialised to 0 like `parser_state_init`.
    prev_end: u64,
    cur_start: u64,
    cur_end: u64,
    textbuf: String,
}

impl Default for Machine {
    fn default() -> Self {
        Machine {
            state: State::Id,
            prev_end: 0,
            cur_start: 0,
            cur_end: 0,
            textbuf: String::new(),
        }
    }
}

impl Machine {
    /// Feed one line (terminator and any `\r` already removed).
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        match self.state {
            State::Id => {
                if is_cue_id_line(line) {
                    self.state = State::Time;
                }
            }
            State::Time => match parse_timeline(line) {
                Some((ts_start, ts_end)) if self.prev_end <= ts_end => {
                    self.cur_start = ts_start;
                    self.cur_end = ts_end;
                    self.prev_end = ts_end;
                    self.state = State::Text;
                }
                _ => self.state = State::Id,
            },
            State::Text => {
                if !self.textbuf.is_empty() {
                    self.textbuf.push('\n');
                }
                self.textbuf.push_str(line);
                if line.is_empty() {
                    let text = markup_from_srt(&self.textbuf, &ALLOWED_TAGS);
                    // The source text, kept for the cue-ir path: the pango
                    // transform above is lossy (only <i>/<b>/<u> survive it).
                    let raw = self.textbuf.trim_end_matches(['\n', '\r']).to_owned();
                    self.textbuf.clear();
                    // A reversed time line (end before start) passes the guard
                    // above and gives the C an underflowed, nonsensical
                    // duration. Clamp instead, so the cue honours the
                    // `end_ns >= start_ns` invariant `Cue` documents. The
                    // ordering guard still holds the parsed end, which is what
                    // the element compares against (it does
                    // `start_time += duration` after every push).
                    let end = self.cur_end.max(self.cur_start);
                    let mut cue = Cue::new(self.cur_start, Some(end), text);
                    cue.raw_text = Some(raw);
                    cues.push(cue);
                    self.state = State::Id;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Id,
    Time,
    Text,
}

// -- cue id ----------------------------------------------------------------

/// Whether `line` looks like a SubRip cue-id line.
///
/// Mirrors the C's use of `g_ascii_strtoull`. Leading whitespace and an
/// optional sign are skipped, then at least one digit is required. The line
/// counts as an id if the numeric run overflows (the C's `ERANGE` branch) or
/// if the whole line is consumed by the number (`endptr == '\0'`).
fn is_cue_id_line(line: &str) -> bool {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digit_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        return false;
    }
    // Overflow -> ERANGE branch, still treated as an id regardless of the tail.
    if line[digit_start..i].parse::<u64>().is_err() {
        return true;
    }
    // Otherwise the number must consume the whole line.
    i == b.len()
}

// -- timing ----------------------------------------------------------------

/// Parse a `start --> end` line into `(start_ns, end_ns)`.
fn parse_timeline(line: &str) -> Option<(u64, u64)> {
    let arrow = line.find(" --> ")?;
    // The C passes the whole line for the start (it truncates internally at the
    // first "-->") and the remainder after " --> " for the end.
    let start = parse_subrip_time(line)?;
    let end = parse_subrip_time(&line[arrow + 5..])?;
    Some((start, end))
}

/// Port of `parse_subrip_time`. Accepts `HH:MM:SS,mmm` and `MM:SS,mmm`, with
/// `.` accepted for `,`, spaces treated as `0`, and 1-3 sub-second digits.
fn parse_subrip_time(ts: &str) -> Option<u64> {
    // Skip leading literal spaces, then truncate at the first "-->".
    let ts = ts.trim_start_matches(' ');
    let s = match ts.find("-->") {
        Some(i) => &ts[..i],
        None => ts,
    };
    // g_strchomp strips trailing ASCII whitespace.
    let s = s.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    // g_strdelimit turns ' ' -> '0' and '.' -> ','.
    let munged: String = s
        .chars()
        .map(|c| match c {
            ' ' => '0',
            '.' => ',',
            other => other,
        })
        .collect();

    // A comma must be present, and not too far in (guards against huge hours,
    // since sizeof("hhh:mm:ss,") == 11).
    let comma = munged.find(',')?;
    if comma > 11 {
        return None;
    }

    // Normalise the sub-second field to exactly three digits (pad on the right).
    let mut ms_field = String::new();
    for c in munged[comma + 1..].chars().take(3) {
        ms_field.push(c);
    }
    while ms_field.len() < 3 {
        ms_field.push('0');
    }
    // The sub-second field is the format's last conversion, so nothing has to
    // follow it. Junk after its digits is simply not read (`,50A` -> 50 ms).
    let (msec, _) = scan_u(&ms_field)?;

    // Split the time portion. 3 parts -> hh:mm:ss, 2 parts -> mm:ss (hh = 0).
    let mut parts = munged[..comma].split(':');
    let (hour, min, sec) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), Some(s), None) => (scan_u_field(h)?, scan_u_field(m)?, scan_u_field(s)?),
        (Some(m), Some(s), None, None) => (0, scan_u_field(m)?, scan_u_field(s)?),
        _ => return None,
    };

    // The C holds these fields in `guint`, so the seconds sum wraps at 32 bits
    // before it is scaled. An absurd hour or minute field therefore wraps the
    // way it does upstream instead of saturating into the far future, which
    // matters because a far-future cue suppresses every cue after it (see the
    // ordering guard in `Machine::feed`). The products below cannot overflow:
    // `u32::MAX` seconds is about 4.3e18 ns.
    let secs = hour
        .wrapping_mul(3600)
        .wrapping_add(min.wrapping_mul(60))
        .wrapping_add(sec);
    Some(u64::from(secs) * NS_PER_SECOND + u64::from(msec) * NS_PER_MSECOND)
}

/// One `sscanf` `%u` conversion: the value plus the unread tail of the field.
///
/// `%u` skips leading whitespace, then hands the rest to `strtoul`, which takes
/// an optional sign and a decimal digit run. No digits is a matching failure
/// (`None` here). Overflow yields `ULONG_MAX` (`ERANGE`), and storing into the
/// C's `guint` truncates to 32 bits, so both are reproduced exactly.
fn scan_u(s: &str) -> Option<(u32, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_scanf_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if matches!(b.get(i), Some(b'+' | b'-')) {
        neg = b[i] == b'-';
        i += 1;
    }
    let digits = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits {
        return None;
    }
    let mag = s[digits..i].parse::<u64>().unwrap_or(u64::MAX);
    let val = if neg { mag.wrapping_neg() } else { mag };
    Some((val as u32, &s[i..]))
}

/// A `%u` conversion the format string follows with a literal (`:` or `,`).
///
/// We split the timestamp on those literals, so the whole field has to be read
/// for the literal to line up: `00:00:01x,000` fails in the C because `x` is
/// matched against the format's `,` and it fails here for the same reason.
fn scan_u_field(s: &str) -> Option<u32> {
    let (val, rest) = scan_u(s)?;
    rest.is_empty().then_some(val)
}

/// The whitespace `sscanf` skips before a conversion (C `isspace`). Plain
/// spaces are already `0`s by this point, but tabs and the rest survive.
fn is_scanf_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

// -- markup pipeline -------------------------------------------------------

/// Turn accumulated cue text into SubRip Pango markup, mirroring the C's
/// escape -> unescape-whitelist -> drop-unknown -> strip-newlines -> balance.
fn markup_from_srt(text: &str, allowed: &[&[u8]]) -> String {
    let escaped = escape_markup(text);
    let mut bytes = unescape_formatting(escaped.as_bytes(), allowed);
    remove_unhandled_tags(&mut bytes);
    strip_trailing_newlines(&mut bytes);
    let bytes = fix_up_markup(bytes, allowed);
    // All transforms operate only on ASCII tag boundaries, so UTF-8 is intact.
    String::from_utf8(bytes).unwrap_or_default()
}

/// Port of `g_markup_escape_text`. Escapes `& < > ' "` and stray control chars.
fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c if is_markup_control(c) => {
                out.push_str("&#x");
                push_lower_hex(&mut out, c as u32);
                out.push(';');
            }
            c => out.push(c),
        }
    }
    out
}

/// The control-character ranges `g_markup_escape_text` escapes numerically.
fn is_markup_control(c: char) -> bool {
    let c = c as u32;
    (0x1..=0x8).contains(&c)
        || (0xb..=0xc).contains(&c)
        || (0xe..=0x1f).contains(&c)
        || (0x7f..=0x84).contains(&c)
        || (0x86..=0x9f).contains(&c)
}

fn push_lower_hex(out: &mut String, mut v: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 8];
    let mut n = 0;
    while v != 0 {
        buf[n] = DIGITS[(v & 0xf) as usize];
        v >>= 4;
        n += 1;
    }
    for &d in buf[..n].iter().rev() {
        out.push(d as char);
    }
}

/// First index of `needle` in `haystack`, or `None`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Port of `subrip_unescape_formatting` (SubRip allows no tag attributes).
///
/// Turns escaped `&lt;tag&gt;` back into real `<tag>` for white-listed tags
/// (case-sensitive, exact match), leaving everything else escaped.
fn unescape_formatting(input: &[u8], allowed: &[&[u8]]) -> Vec<u8> {
    // No processing needed if there is no escaped tag marker at all.
    if find(input, b"&lt;").is_none() {
        return input.to_vec();
    }

    let mut out = Vec::with_capacity(input.len());
    let mut p = 0;
    while p < input.len() {
        let lt = match find(&input[p..], b"&lt;") {
            None => {
                out.extend_from_slice(&input[p..]);
                break;
            }
            Some(rel) => p + rel,
        };
        // Copy everything before the "&lt;".
        out.extend_from_slice(&input[p..lt]);
        let after_lt = lt + 4;

        let gt = match find(&input[after_lt..], b"&gt;") {
            None => {
                // No closing "&gt;", so copy the rest verbatim from "&lt;" on.
                out.extend_from_slice(&input[lt..]);
                break;
            }
            Some(rel) => after_lt + rel,
        };

        // Optional closing slash, optional whitespace, then the tag name.
        let mut ts = after_lt;
        let is_closing = ts < input.len() && input[ts] == b'/';
        if is_closing {
            ts += 1;
        }
        while ts < input.len() && (input[ts] == b' ' || input[ts] == b'\t') {
            ts += 1;
        }
        let name_start = ts;
        let mut te = ts;
        while te < input.len() && input[te].is_ascii_alphanumeric() {
            te += 1;
        }
        let name = &input[name_start..te];

        let is_allowed = allowed.contains(&name);
        if !is_allowed {
            // Copy "&lt;...&gt;" verbatim and continue past it.
            out.extend_from_slice(&input[lt..gt + 4]);
            p = gt + 4;
            continue;
        }

        out.push(b'<');
        if is_closing {
            out.push(b'/');
        }
        out.extend_from_slice(name);
        // SubRip disallows tag attributes, so they are dropped here.
        out.push(b'>');
        p = gt + 4;
    }
    out
}

/// Port of `subrip_remove_unhandled_tags`. Strips any leftover escaped
/// `&lt;...&gt;` whose name starts with an ASCII letter (e.g. `<font ...>`).
///
/// Single forward scan. The C searches for the closing `&gt;` from every `&lt;`
/// it passes, which rescans to the end of the buffer per escaped `<` and turns
/// one unterminated cue into quadratic work (a cue only ends at a blank line, so
/// a body without one is a single arbitrarily long cue). The `gt` cursor below
/// only ever moves forward, so the whole pass is linear.
fn remove_unhandled_tags(buf: &mut Vec<u8>) {
    let src = std::mem::take(buf);
    let mut out = Vec::with_capacity(src.len());
    let mut gt = find(&src, b"&gt;");
    let mut pos = 0;

    while pos < src.len() {
        if src[pos..].starts_with(b"&lt;") {
            // Advance to the first "&gt;" that can close this "&lt;".
            while let Some(g) = gt.filter(|&g| g < pos + 4) {
                gt = find(&src[g + 1..], b"&gt;").map(|rel| g + 1 + rel);
            }
            if let Some(g) = gt {
                let mut tag = pos + 4;
                if src.get(tag) == Some(&b'/') {
                    tag += 1;
                }
                if src.get(tag).is_some_and(|b| b.is_ascii_alphabetic()) {
                    // Drop the whole tag and resume just after it, which is
                    // where the C's `--pos; ++pos` lands after its drain.
                    pos = g + 4;
                    continue;
                }
            }
        }
        out.push(src[pos]);
        pos += 1;
    }

    *buf = out;
}

/// Port of `strip_trailing_newlines`. Drops trailing '\n' but keeps >= 1 byte.
fn strip_trailing_newlines(buf: &mut Vec<u8>) {
    while buf.len() > 1 && buf[buf.len() - 1] == b'\n' {
        buf.pop();
    }
}

/// Port of `subrip_fix_up_markup`. Adds missing closing tags and removes stray
/// closing tags for the white-listed set. Operates on real `<...>` markup.
fn fix_up_markup(mut buf: Vec<u8>, allowed: &[&[u8]]) -> Vec<u8> {
    let mut open_tags: Vec<Vec<u8>> = Vec::new();
    let mut cur = 0;

    while cur < buf.len() {
        let next_tag = match find(&buf[cur..], b"<") {
            None => break,
            Some(rel) => cur + rel,
        };

        let mut offset = 0usize;
        let mut is_closing = false;

        for &tag in allowed {
            let mut ts = next_tag + 1;
            is_closing = ts < buf.len() && buf[ts] == b'/';
            if is_closing {
                ts += 1;
            }
            while ts < buf.len() && (buf[ts] == b' ' || buf[ts] == b'\t') {
                ts += 1;
            }
            let name_start = ts;
            let mut te = ts;
            while te < buf.len() && buf[te].is_ascii_alphanumeric() {
                te += 1;
            }
            let name = &buf[name_start..te];

            if name.len() == tag.len() && name.eq_ignore_ascii_case(tag) {
                // Optional attributes.
                if matches!(buf.get(te), Some(b' ' | b'\t' | b'.')) {
                    while te < buf.len()
                        && buf[te] != b'>'
                        && (buf[te].is_ascii_alphanumeric()
                            || matches!(buf[te], b'.' | b' ' | b'\t' | b'(' | b')'))
                    {
                        te += 1;
                    }
                }
                if buf.get(te) == Some(&b'>') {
                    offset = te - (next_tag + 1);
                    if !is_closing {
                        open_tags.push(tag.to_ascii_lowercase());
                    }
                    break;
                }
            }
            offset = 0;
        }

        // Not a valid (white-listed) tag, so skip the '<' and continue.
        if offset == 0 {
            cur = next_tag + 1;
            continue;
        }

        // A valid opening tag, so skip over it.
        if !is_closing {
            cur = next_tag + offset;
            continue;
        }

        // A closing tag.
        let tag_end = match find(&buf[next_tag..], b">") {
            Some(rel) => next_tag + rel,
            None => break,
        };
        let matches_last = match open_tags.last() {
            Some(last) => {
                let cmp = next_tag + 2;
                cmp + last.len() <= buf.len()
                    && buf[cmp..cmp + last.len()].eq_ignore_ascii_case(last)
            }
            None => false,
        };

        if !matches_last {
            // Broken. The closing tag was never opened. Remove it.
            buf.drain(next_tag..tag_end + 1);
            cur = next_tag;
            continue;
        }

        open_tags.pop();
        cur = tag_end + 1;
    }

    // Close any still-open tags, innermost first.
    while let Some(tag) = open_tags.pop() {
        buf.push(b'<');
        buf.push(b'/');
        buf.extend_from_slice(&tag);
        buf.push(b'>');
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Cue> {
        SubRip::default()
            .parse(body, &ParseContext::default())
            .expect("subrip parse is infallible")
    }

    /// Parse a single well-formed chunk and return (start, end, text).
    fn one(body: &str) -> (u64, u64, String) {
        let cues = parse(body);
        assert_eq!(cues.len(), 1, "expected exactly one cue for {body:?}");
        let c = &cues[0];
        (c.start_ns, c.end_ns.unwrap(), c.text.clone())
    }

    const SEC: u64 = NS_PER_SECOND;
    const MS: u64 = NS_PER_MSECOND;

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(SubRip::default().output_format(), OutputFormat::PangoMarkup);
    }

    // ---- raw text side channel (cue-ir styling) --------------------------

    #[test]
    fn raw_text_carries_the_source_verbatim() {
        // The pango transform deletes <font ...>; the raw side channel keeps
        // the source so the cue-ir path can style it.
        let cues = parse(
            "1\n00:00:01,000 --> 00:00:02,000\n<font color=\"#ff0000\">Red</font> & <i>it</i>\nsecond line\n\n",
        );
        assert_eq!(cues.len(), 1);
        // Parity text: font tag gone, & escaped, <i> kept.
        assert_eq!(cues[0].text, "Red &amp; <i>it</i>\nsecond line");
        assert_eq!(
            cues[0].raw_text.as_deref(),
            Some("<font color=\"#ff0000\">Red</font> & <i>it</i>\nsecond line")
        );
    }

    // ---- timing --------------------------------------------------------

    #[test]
    fn basic_timing_and_text() {
        let (s, e, t) = one("1\n00:00:01,000 --> 00:00:02,000\nOne\n\n");
        assert_eq!((s, e, t.as_str()), (SEC, 2 * SEC, "One"));
    }

    #[test]
    fn hours_minutes_seconds() {
        let (s, e, _) = one("1\n01:02:03,004 --> 01:02:04,000\nx\n\n");
        assert_eq!(s, (3600 + 2 * 60 + 3) * SEC + 4 * MS);
        assert_eq!(e, (3600 + 2 * 60 + 4) * SEC);
    }

    #[test]
    fn minutes_seconds_only_defaults_hours_to_zero() {
        // WebVTT-style shorter timestamps are accepted by parse_subrip_time.
        let (s, e, _) = one("1\n00:26,500 --> 00:28,000\nx\n\n");
        assert_eq!(s, 26 * SEC + 500 * MS);
        assert_eq!(e, 28 * SEC);
    }

    #[test]
    fn comma_or_dot_subsecond_separator() {
        let (s, e, _) = one("1\n00:00:01.250 --> 00:00:02.750\nx\n\n");
        assert_eq!((s, e), (SEC + 250 * MS, 2 * SEC + 750 * MS));
    }

    #[test]
    fn subsecond_digit_normalisation() {
        // 1 digit -> *100ms, 2 -> *10ms, 3 -> *1ms (right-padded to 3).
        assert_eq!(one("1\n00:00:00,5 --> 00:00:01,0\nx\n\n").0, 500 * MS);
        assert_eq!(one("1\n00:00:00,05 --> 00:00:01,0\nx\n\n").0, 50 * MS);
        assert_eq!(one("1\n00:00:00,5000 --> 00:00:01,0\nx\n\n").0, 500 * MS);
    }

    #[test]
    fn spaces_become_zeros_in_timestamp() {
        // Mirrors srt_input1: " 0: 0:26, 26" -> 26s + 26ms.
        let (s, e, t) = one("1\n 0: 0:26, 26 --> 0: 0:28, 17\nI cant see.\n\n");
        assert_eq!(s, 26 * SEC + 26 * MS);
        assert_eq!(e, 28 * SEC + 17 * MS);
        assert_eq!(t, "I cant see.");
    }

    #[test]
    fn extra_spaces_around_arrow() {
        // Mirrors srt_input3[1]: "00:00:02,5   --> 00:00:03,  5 ".
        let (s, e, _) = one("1\n00:00:02,5   --> 00:00:03,  5 \nTwo\n\n");
        assert_eq!((s, e), (2500 * MS, 3005 * MS));
    }

    #[test]
    fn short_fraction_followed_by_junk_reads_leading_digits() {
        // The sub-second field is the format's last conversion, so junk after
        // its digits is never read. WebVTT cue settings end up here (the
        // ' ' -> '0' step munges them into the field) and the C still reports
        // 1.000 -> 2.050 rather than dropping the cue.
        let (s, e, t) = one("1\n00:00:01,000 --> 00:00:02,5 A:start\nOne\n\n");
        assert_eq!((s, e, t.as_str()), (SEC, 2 * SEC + 50 * MS, "One"));
        // A field that starts with junk still fails (no digits to convert).
        assert_eq!(parse_subrip_time("00:00:02,A"), None);
    }

    #[test]
    fn tab_in_timestamp_fields_is_skipped_like_sscanf() {
        // %u skips leading whitespace in every field, so a tab is tolerated
        // wherever a space would have been turned into a '0'.
        assert_eq!(one("1\n\t00:00:01,000 --> 00:00:02,000\nOne\n\n").0, SEC);
        assert_eq!(one("1\n00:\t0:01,000 --> 00:00:02,000\nOne\n\n").0, SEC);
        // Sub-second field too: ",\t5" pads to "\t50", i.e. 50 ms.
        assert_eq!(
            one("1\n00:00:01,\t5 --> 00:00:02,000\nOne\n\n").0,
            SEC + 50 * MS
        );
    }

    #[test]
    fn junk_inside_a_timestamp_field_rejects_the_cue() {
        // The C matches the format's ',' literal against the 'x', fails, and
        // drops the cue. Parsing recovers on the next one.
        let body = "1\n00:00:01x,000 --> 00:00:02,000\nJunk\n\n\
                    2\n00:00:03,000 --> 00:00:04,000\nGood\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Good");
        assert_eq!(parse_subrip_time("00:00:01x,000"), None);
        assert_eq!(parse_subrip_time("00:01x,000"), None);
    }

    #[test]
    fn signed_timestamp_field_matches_strtoul() {
        // %u defers to strtoul, which accepts a sign: '+' is a no-op and '-'
        // wraps into the C's unsigned field.
        assert_eq!(parse_subrip_time("00:00:+1,000"), Some(SEC));
        assert_eq!(
            parse_subrip_time("00:00:-1,000"),
            Some(u64::from(u32::MAX) * SEC)
        );
    }

    #[test]
    fn absurd_minute_field_wraps_at_32_bits() {
        // The comma guard still admits an 8-digit field in the hour-less form.
        // The C keeps it in a guint, so `min * 60` wraps rather than running off
        // into the far future, and we wrap with it.
        assert_eq!(
            parse_subrip_time("99999999:00,000"),
            Some(1_705_032_644 * SEC)
        );
        assert_eq!(parse_subrip_time("71582789:00,000"), Some(44 * SEC));
    }

    #[test]
    fn reversed_timing_is_clamped_to_zero_length() {
        // The ordering guard admits it (0 <= 2s) and the C derives a nonsensical
        // underflowed duration. The cue we hand out honours `end_ns >= start_ns`.
        let cues = parse("1\n00:00:05,000 --> 00:00:02,000\nA\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 5 * SEC);
        assert_eq!(cues[0].end_ns, Some(5 * SEC));
        assert_eq!(cues[0].duration_ns(), Some(0));
    }

    #[test]
    fn cue_after_a_reversed_one_is_still_emitted() {
        // Clamping the cue must not clamp the guard: it keeps the parsed end
        // (2s), which is where the element's `start_time += duration` lands, so
        // 2s <= 4s and the next cue survives.
        let body = "1\n00:00:05,000 --> 00:00:02,000\nA\n\n\
                    2\n00:00:03,000 --> 00:00:04,000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        assert_eq!(cues[1].text, "B");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (3 * SEC, Some(4 * SEC)));
    }

    // ---- markup / styling ---------------------------------------------

    #[test]
    fn preserves_italic_markup() {
        assert_eq!(
            one("6\n00:00:06,000 --> 00:00:07,000\n<i>Six</i>\n\n").2,
            "<i>Six</i>"
        );
    }

    #[test]
    fn closes_open_italic() {
        assert_eq!(
            one("7\n00:00:07,000 --> 00:00:08,000\n<i>Seven\n\n").2,
            "<i>Seven</i>"
        );
    }

    #[test]
    fn closes_open_nested_tags_in_reverse() {
        assert_eq!(
            one("8\n00:00:08,000 --> 00:00:09,000\n<b><i>Eight\n\n").2,
            "<b><i>Eight</i></b>"
        );
    }

    #[test]
    fn stray_closing_tag_removed_to_empty() {
        assert_eq!(one("9\n00:00:09,000 --> 00:00:10,000\n</b>\n\n").2, "");
        assert_eq!(one("10\n00:00:10,000 --> 00:00:11,000\n</b></i>\n\n").2, "");
    }

    #[test]
    fn broken_markup_fixed_up() {
        // Stray </b> removed, </i> matches -> <i>xyz</i>.
        assert_eq!(
            one("11\n00:00:11,000 --> 00:00:12,000\n<i>xyz</b></i>\n\n").2,
            "<i>xyz</i>"
        );
        // Stray </b> removed, <i> auto-closed.
        assert_eq!(
            one("12\n00:00:12,000 --> 00:00:13,000\n<i>xyz</b>\n\n").2,
            "<i>xyz</i>"
        );
    }

    #[test]
    fn deeply_nested_italics_all_closed() {
        let inner = "<i>".repeat(33);
        let body = format!("13\n00:00:13,000 --> 00:00:14,000\n{inner}Keep them comiiiiiing\n\n");
        let expected = format!("{inner}Keep them comiiiiiing{}", "</i>".repeat(33));
        assert_eq!(one(&body).2, expected);
    }

    #[test]
    fn escapes_ampersand_but_keeps_allowed_markup() {
        assert_eq!(
            one("25\n00:03:00,000 --> 00:04:00,000\ngave <i>Rock & Roll</i> to\n\n").2,
            "gave <i>Rock &amp; Roll</i> to"
        );
        assert_eq!(
            one("26\n00:04:00,000 --> 00:05:00,000\n<i>Rock & Roll</i>\n\n").2,
            "<i>Rock &amp; Roll</i>"
        );
        assert_eq!(
            one("27\n00:06:00,000 --> 00:08:00,000\nRock & Roll\n\n").2,
            "Rock &amp; Roll"
        );
    }

    #[test]
    fn font_and_unknown_tags_stripped_lt_escaped() {
        // Mirrors srt_input[27] (chunk "28").
        let body = "28\n00:10:00,000 --> 00:11:00,000\n\
                    <font \"#0000FF\"><joj>This is </xxx>in blue but <5</font>\n\n";
        assert_eq!(one(body).2, "This is in blue but &lt;5");
    }

    #[test]
    fn unhandled_tag_scan_with_a_distant_closing_marker() {
        // `remove_unhandled_tags` runs one forward cursor over the escaped
        // "&gt;"s instead of searching from every escaped '<'. A '<' followed by
        // a digit is not a tag and must survive even when the only "&gt;" in the
        // buffer is far to the right, while a real tag is still dropped.
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<5<5<5 <font x>y>\n\n").2,
            "&lt;5&lt;5&lt;5 y&gt;"
        );
        // "&lt;" with no "&gt;" anywhere is left completely alone.
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<font <i <b\n\n").2,
            "&lt;font &lt;i &lt;b"
        );
    }

    #[test]
    fn closing_tag_with_space_recognised() {
        assert_eq!(
            one("29\n00:11:00,000 --> 00:12:00,000\n<i>italics</ i>\n\n").2,
            "<i>italics</i>"
        );
    }

    #[test]
    fn unrecognised_closing_tag_escaped_and_balanced() {
        assert_eq!(
            one("30\n00:12:00,000 --> 00:12:01,000\n<i>italics</ x>\n\n").2,
            "<i>italics&lt;/ x&gt;</i>"
        );
    }

    // ---- webvtt-ish tags fed to SRT (srt_input4) ----------------------

    #[test]
    fn voice_tag_stripped() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<v>some text\n\n").2,
            "some text"
        );
    }

    #[test]
    fn tag_with_attribute_dropped_keeps_bold() {
        // "<b.loud>" -> name "b" allowed, attribute ".loud" dropped, auto-closed.
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<b.loud>some text\n\n").2,
            "<b>some text</b>"
        );
    }

    #[test]
    fn ruby_annotation_tags_stripped() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<ruby>base text<rt>annotation</rt></ruby>\n\n")
                .2,
            "base textannotation"
        );
    }

    // ---- structure / recovery -----------------------------------------

    #[test]
    fn multiple_cues() {
        let body = "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n\
                    2\n00:00:02,000 --> 00:00:03,000\nTwo\n\n\
                    3\n00:00:03,000 --> 00:00:04,000\nThree\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[1].text, "Two");
        assert_eq!(cues[2].text, "Three");
        assert_eq!(cues[1].start_ns, 2 * SEC);
        assert_eq!(cues[1].end_ns, Some(3 * SEC));
    }

    #[test]
    fn multi_line_text_joined_with_newline() {
        let (_, _, t) = one("1\n00:00:01,000 --> 00:00:02,000\nLine one\nLine two\n\n");
        assert_eq!(t, "Line one\nLine two");
    }

    #[test]
    fn cue_id_zero_is_valid() {
        assert_eq!(one("0\n00:00:01,000 --> 00:00:02,000\nOne\n\n").2, "One");
    }

    #[test]
    fn crlf_line_endings() {
        // A body that survived as CRLF. get_next_line strips the trailing '\r'.
        let (s, e, t) = one("1\r\n00:00:01,000 --> 00:00:02,000\r\nHello\r\n\r\n");
        assert_eq!((s, e, t.as_str()), (SEC, 2 * SEC, "Hello"));
    }

    #[test]
    fn leading_bom_stripped() {
        assert_eq!(
            one("\u{feff}1\n00:00:00,000 --> 00:00:03,50\nJust testing.\n\n").2,
            "Just testing."
        );
    }

    #[test]
    fn no_trailing_newline_still_emits() {
        // Mirrors srt_input6.
        let (s, e, t) = one("1\n00:00:01,000 --> 00:00:02,000\nLast cue, no newline at the end");
        assert_eq!((s, e), (SEC, 2 * SEC));
        assert_eq!(t, "Last cue, no newline at the end");
    }

    #[test]
    fn empty_cue_still_emitted() {
        // A time line immediately followed by a blank line yields an empty cue.
        let cues = parse("1\n00:00:01,000 --> 00:00:02,000\n\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "");
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (SEC, Some(2 * SEC)));
    }

    #[test]
    fn broken_end_timestamp_drops_cue_and_recovers() {
        // Mirrors srt_input5[1]. The "00:03:0" end has no comma -> whole cue is
        // dropped, parsing recovers and emits only the following good cue.
        let body = "2\n00:02:00,000 --> 00:03:0\n<v>some other text\n\n\
                    3\n00:00:03,000 --> 00:00:04,000\n<v>some more text\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "some more text");
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (3 * SEC, Some(4 * SEC)));
    }

    #[test]
    fn timeline_without_id_is_ignored() {
        // Without a preceding numeric id line, the time line is not recognised.
        let cues = parse("00:00:01,000 --> 00:00:02,000\nHello\n\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn non_numeric_id_line_ignored() {
        // Garbage before a cue must not start a cue on its own.
        let cues = parse("not a number\nalso junk\n\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn empty_input_yields_no_cues() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n\n").is_empty());
    }

    #[test]
    fn apostrophe_and_quote_are_markup_escaped() {
        // g_markup_escape_text escapes ' and " numerically.
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\ndon't \"stop\"\n\n").2,
            "don&apos;t &quot;stop&quot;"
        );
    }

    #[test]
    fn utf8_text_passes_through() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\ncafé — naïve\n\n").2,
            "café — naïve"
        );
    }

    #[test]
    fn utf8_with_markup_boundaries_stay_valid() {
        assert_eq!(
            one("1\n00:00:01,000 --> 00:00:02,000\n<i>café</i> & <b>naïve\n\n").2,
            "<i>café</i> &amp; <b>naïve</b>"
        );
    }

    // ---- helper-level unit tests --------------------------------------

    #[test]
    fn parse_subrip_time_examples() {
        assert_eq!(parse_subrip_time("00:00:01,000"), Some(SEC));
        assert_eq!(parse_subrip_time("01:00:00,000"), Some(3600 * SEC));
        assert_eq!(
            parse_subrip_time("00:03:00,50"),
            Some(3 * 60 * SEC + 500 * MS)
        );
        // No comma at all -> failure.
        assert_eq!(parse_subrip_time("00:03:0"), None);
        // Comma too far in (huge hours) -> failure.
        assert_eq!(parse_subrip_time("1234567890:00:00,000"), None);
    }

    #[test]
    fn is_cue_id_line_cases() {
        assert!(is_cue_id_line("1"));
        assert!(is_cue_id_line("0"));
        assert!(is_cue_id_line("  42"));
        assert!(!is_cue_id_line(""));
        assert!(!is_cue_id_line("1a"));
        assert!(!is_cue_id_line("00:00:01,000 --> 00:00:02,000"));
        assert!(!is_cue_id_line("hello"));
    }
}
