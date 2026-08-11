// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! WebVTT parser. See `specs/webvtt.md`.
//!
//! Drop-in parity with the upstream C subparse element
//! (`gst-plugins-base/gst/subparse/gstsubparse.c`, `parse_webvtt` and the
//! `parse_subrip` text path it delegates to). subparse only implements a
//! **subset** of the W3C WebVTT spec (<https://www.w3.org/TR/webvtt1/>). It is a
//! line-oriented state machine that recognises `HH:MM:SS.mmm --> HH:MM:SS.mmm`
//! cue-timing lines (with optional cue settings), accumulates the following
//! lines as cue text, and converts a small whitelist of inline tags to
//! Pango markup. Output is `format=pango-markup`.
//!
//! One deliberate exception to that parity, because the C's behaviour is
//! plainly broken: a cue's text ends at a line containing `-->`, not only at a
//! blank line. The spec's block collector rewinds to such a line and starts the
//! next block there; the C, whose only exit from its text state is the blank
//! line, swallows it and *displays the timestamps* of a file whose blocks are
//! not blank-line separated (WPT's `embedded_style_urls.vtt` is exactly that
//! file). See [`Machine::feed`].
//!
//! Everything here is std-only and self-contained (no dependency on the SubRip
//! module, even though the C shares helpers), so the module builds and tests
//! independently of its siblings.

use crate::cue::{Cue, CueSettings, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};
use crate::vttcss::Stylesheet;

const GST_SECOND: u64 = 1_000_000_000;
const GST_MSECOND: u64 = 1_000_000;

/// Inline tags the C keeps (`allowed_vtt_tags`). Anything else is stripped.
const ALLOWED_VTT_TAGS: &[&str] = &["i", "b", "c", "u", "v", "ruby", "rt"];

/// Parser for the WebVTT subtitle format.
///
/// Streaming: identical shape to SubRip. Lines feed a persistent state machine
/// and a cue is emitted on its terminating blank line, so only a partial
/// trailing line is ever retained.
#[derive(Debug, Default)]
pub struct WebVtt {
    lines: LineScanner,
    machine: Machine,
}

impl SubtitleFormat for WebVtt {
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
            // The whole-body parser iterated `split('\n')` (which also yields
            // the unterminated remainder) and then one synthetic empty line: the
            // `"\n\n"` the element used to force-push at EOS, which is what
            // flushes a final cue with no blank terminator.
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

    fn stylesheet(&self) -> Option<&Stylesheet> {
        (!self.machine.sheet.is_empty()).then_some(&self.machine.sheet)
    }
}

/// State of the line-oriented machine, mirroring the C `ParserState.state`.
/// The C uses states 0/1 (both "seek the timing line") and 2 ("collect text").
/// We collapse 0/1 into `SeekTiming`. `CollectStyle` is ours alone: the C
/// ignores `STYLE` blocks, and since it also ignores every other non-timing
/// line while seeking, collecting them on the side cannot change which cues
/// come out (a timing line inside a style block is handed back, see
/// [`Machine::feed`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum St {
    #[default]
    SeekTiming,
    CollectStyle,
    CollectText,
}

/// The WebVTT line machine, carried across `parse_incremental` calls.
#[derive(Debug, Default)]
struct Machine {
    state: St,
    /// Mirrors `ParserState.start_time`. After a cue is pushed the element does
    /// `start_time += duration`, so by the next timing line this holds the
    /// previous cue's end time. The timing line is rejected unless
    /// `start_time <= ts_end`, giving a weak monotonicity guard.
    prev_end: u64,
    cur_start: u64,
    /// The end timestamp exactly as parsed, which is what the guard above
    /// compares against. It can be *before* `cur_start`, so the cue itself uses
    /// a clamped copy (see `Cue`'s `end_ns >= start_ns` invariant).
    cur_end: u64,
    cur_settings: CueSettings,
    /// The identifier of the cue being collected (the line immediately
    /// preceding its timing line).
    cur_id: Option<String>,
    /// Candidate identifier: the last non-blank, non-timing line seen while
    /// seeking. A blank line resets it (the id line must immediately precede
    /// the timing line).
    pending_id: Option<String>,
    /// Whether a timing line has been accepted yet. Per the spec, `STYLE`
    /// blocks are only valid before the first cue; later ones are ignored
    /// (WPT `embedded_style_invalid_format.vtt` pins this).
    saw_cue: bool,
    /// CSS text of the `STYLE` block being collected.
    style_buf: String,
    /// Rules of every completed `STYLE` block. Exposed via
    /// [`SubtitleFormat::stylesheet`]; only the `cue-ir` output path reads it.
    sheet: Stylesheet,
    buf: String,
}

impl Machine {
    /// Feed one line (terminator and any `\r` already removed).
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        match self.state {
            St::SeekTiming => self.feed_seek(line),
            St::CollectStyle => {
                if line.is_empty() {
                    self.end_style_block();
                } else if line.contains("-->") {
                    // A timing line terminates an unfinished style block and
                    // starts its cue (the spec's "collect a WebVTT block"
                    // does the same).
                    self.end_style_block();
                    self.feed_seek(line);
                } else {
                    self.style_buf.push_str(line);
                    self.style_buf.push('\n');
                }
            }
            St::CollectText => {
                if line.contains("-->") {
                    // Spec: once a cue's timing line has been seen, a later
                    // line holding `-->` ends the block — "collect a WebVTT
                    // block" rewinds to it, so it is reprocessed as the first
                    // line of the *next* block, where it starts a new cue.
                    // The cue being collected keeps the text it has, and the
                    // timing line never becomes subtitle text. This is the one
                    // place we deliberately leave the C, which only ever exits
                    // its state 2 on a blank line and so displays the
                    // timestamps of a blank-line-less file as cue text (WPT
                    // `embedded_style_urls.vtt` is exactly that file).
                    self.finish_cue(cues);
                    self.feed_seek(line);
                } else {
                    if !self.buf.is_empty() {
                        self.buf.push('\n');
                    }
                    self.buf.push_str(line);
                    if line.is_empty() {
                        self.finish_cue(cues);
                    }
                }
            }
        }
    }

    /// Emit the cue whose text has accumulated in `buf` and go back to seeking.
    fn finish_cue(&mut self, cues: &mut Vec<Cue>) {
        let text = finalize_text(&self.buf);
        // A reversed time line (end before start) is accepted by the guard in
        // [`Machine::feed_seek`] and leaves the C with an underflowed duration,
        // so `start_time += duration` lands it back on the parsed end. Feed the
        // guard that same value, and clamp only the cue we hand out.
        let mut cue = Cue::new(self.cur_start, Some(self.cur_end.max(self.cur_start)), text);
        cue.settings = std::mem::take(&mut self.cur_settings);
        cue.id = self.cur_id.take();
        cues.push(cue);
        self.prev_end = self.cur_end; // element: start_time += duration
        self.buf.clear();
        self.state = St::SeekTiming;
    }

    /// One line in the seeking state.
    fn feed_seek(&mut self, line: &str) {
        if let Some((ts_start, ts_end, settings)) = parse_timing_line(line)
            && self.prev_end <= ts_end
        {
            self.cur_start = ts_start;
            self.cur_end = ts_end;
            self.cur_settings = match settings {
                Some(s) => parse_cue_settings(s),
                None => CueSettings::default(),
            };
            self.cur_id = self.pending_id.take();
            self.saw_cue = true;
            self.buf.clear();
            self.state = St::CollectText;
        } else if !self.saw_cue && line == "STYLE" {
            // Spec: the block starts with a line that is exactly "STYLE"
            // (case-sensitive), before the first cue. The C ignores these
            // lines like any other non-timing line, so collecting them is
            // parity-neutral for the cue stream.
            self.style_buf.clear();
            self.pending_id = None;
            self.state = St::CollectStyle;
        } else {
            // Not a timing line (cue id, `WEBVTT` header, `NOTE`, blank line,
            // rejected timing, ...). Ignored while seeking, exactly like the
            // C; remembered as the candidate cue identifier when it could be
            // one.
            self.pending_id = if line.is_empty() || line.contains("-->") {
                None
            } else {
                Some(line.to_owned())
            };
        }
    }

    fn end_style_block(&mut self) {
        if !self.style_buf.is_empty() {
            self.sheet.push_css(&self.style_buf);
            self.style_buf.clear();
        }
        self.state = St::SeekTiming;
    }
}

/// Parse a `start --> end [cue settings]` line.
///
/// Returns `(start_ns, end_ns, cue_settings)` when both timestamps parse.
/// `cue_settings` is the substring after the first space that follows the end
/// timestamp (`strstr(end_time, " ") + 1` in the C), or `None` when absent.
fn parse_timing_line(line: &str) -> Option<(u64, u64, Option<&str>)> {
    let idx = line.find(" --> ")?;
    // The C passes the whole line as the start (parse_subrip_time truncates at
    // "-->" itself) and everything past " --> " as the end.
    let after = &line[idx + 5..];
    let ts_start = parse_subrip_time(line)?;
    let ts_end = parse_subrip_time(after)?;
    let settings = after.find(' ').map(|sp| &after[sp + 1..]);
    Some((ts_start, ts_end, settings))
}

/// Port of the C `parse_subrip_time`, shared by SRT/VTT.
///
/// Accepts `HH:MM:SS[.,]mmm` and the hour-less `MM:SS[.,]mmm`. A `.` or `,`
/// fractional separator both work. Interior spaces are treated as `0`. The
/// fractional part is right-padded / truncated to exactly three digits (ms).
fn parse_subrip_time(ts_string: &str) -> Option<u64> {
    // while (*ts_string == ' ') ++ts_string;
    let trimmed = ts_string.trim_start_matches(' ');

    // g_strlcpy into a 128-byte buffer, keeping at most 127 bytes (char-safe).
    let mut end = trimmed.len().min(127);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = trimmed[..end].to_string();

    // if ((end = strstr (s, "-->"))) *end = '\0';
    if let Some(pos) = s.find("-->") {
        s.truncate(pos);
    }

    // g_strchomp drops trailing ASCII whitespace.
    let s = s.trim_end_matches(|c: char| c.is_ascii_whitespace());

    // g_strdelimit (s, " ", '0'); g_strdelimit (s, ".", ',');
    let mut s: String = s
        .chars()
        .map(|c| match c {
            ' ' => '0',
            '.' => ',',
            other => other,
        })
        .collect();

    // p = strchr (s, ','); if NULL -> broken.
    let comma = s.find(',')?;

    // Guard against absurdly long hour fields (C: (p - s) > sizeof("hhh:mm:ss,")).
    if comma > 11 {
        return None;
    }

    // Make exactly three digits after the comma.
    let frac: String = {
        let f = &s[comma + 1..];
        if f.len() > 3 {
            // Truncate to first three chars (char-safe, real input is ASCII).
            f.chars().take(3).collect()
        } else {
            let mut f = f.to_string();
            while f.len() < 3 {
                f.push('0');
            }
            f
        }
    };
    s.truncate(comma); // keep the "HH:MM:SS" (or "MM:SS") part

    let (hour, min, sec) = parse_hms(&s)?;
    // The fractional part is the format's last conversion, so nothing has to
    // follow it: junk after its digits is simply not read. That is what keeps
    // cue settings from breaking a short fraction, because the ' ' -> '0' step
    // above munges them into this field (`,5 A:start` -> `,50A` -> 50 ms).
    let (msec, _) = scan_u(&frac)?;

    // The C holds these fields in `guint`, so the seconds sum wraps at 32 bits
    // before it is scaled. An absurd hour or minute field therefore wraps the
    // way it does upstream instead of saturating into the far future, which
    // matters because a far-future cue suppresses every cue after it (see the
    // monotonic guard in `Machine::feed`). The products below cannot overflow:
    // `u32::MAX` seconds is about 4.3e18 ns.
    let secs = hour
        .wrapping_mul(3600)
        .wrapping_add(min.wrapping_mul(60))
        .wrapping_add(sec);
    Some(u64::from(secs) * GST_SECOND + u64::from(msec) * GST_MSECOND)
}

/// sscanf `"%u:%u:%u"` (three fields) falling back to `"%u:%u"` (hours = 0).
fn parse_hms(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [h, m, sec] => Some((scan_u_field(h)?, scan_u_field(m)?, scan_u_field(sec)?)),
        [m, sec] => Some((0, scan_u_field(m)?, scan_u_field(sec)?)),
        _ => None,
    }
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

/// Port of `parse_webvtt_cue_settings`, extended with the modern syntax.
///
/// The C only understands the archaic one-letter forms (`T:`/`S:`/`L:`/`D:`/
/// `A:`); the modern `name:value` settings every current WebVTT file uses
/// (`align:center position:50%`) fall through its `switch` unrecognised. The
/// upstream element parses what it does recognise and then **discards** it
/// (settings never reach the output), so surfacing both syntaxes on
/// [`CueSettings`] is richer, not a parity break.
fn parse_cue_settings(settings: &str) -> CueSettings {
    let mut cs = CueSettings::default();

    for tok in settings.split([' ', '\t']) {
        let Some(&first) = tok.as_bytes().first() else {
            continue; // empty token (consecutive separators)
        };
        // Modern `name:value` settings. The names are lowercase words, so
        // they can never collide with the uppercase one-letter C forms.
        if let Some((key, value)) = tok.split_once(':')
            && !value.is_empty()
        {
            match key {
                "line" => {
                    // `line:10%[,align]` or `line:<int>[,align]`.
                    let (value, align) = match value.split_once(',') {
                        Some((v, a)) => (v, Some(a)),
                        None => (value, None),
                    };
                    if let Some(pct) = value.strip_suffix('%') {
                        if let Some(v) = parse_percent(pct) {
                            cs.line_position = Some(v);
                        }
                    } else if let Ok(v) = value.parse::<i32>() {
                        cs.line_number = Some(v);
                    }
                    if let Some(a) = align.filter(|a| !a.is_empty()) {
                        cs.line_align = Some(a.to_string());
                    }
                    continue;
                }
                "position" => {
                    let (value, align) = match value.split_once(',') {
                        Some((v, a)) => (v, Some(a)),
                        None => (value, None),
                    };
                    if let Some(v) = value.strip_suffix('%').and_then(parse_percent) {
                        cs.text_position = Some(v);
                    }
                    if let Some(a) = align.filter(|a| !a.is_empty()) {
                        cs.position_align = Some(a.to_string());
                    }
                    continue;
                }
                "size" => {
                    if let Some(v) = value.strip_suffix('%').and_then(parse_percent) {
                        cs.text_size = Some(v);
                    }
                    continue;
                }
                "align" => {
                    cs.alignment = Some(value.to_string());
                    continue;
                }
                "vertical" => {
                    cs.vertical = Some(value.to_string());
                    continue;
                }
                // `region:` needs region blocks, which nothing here models.
                "region" => continue,
                _ => {} // fall through to the one-letter C forms
            }
        }
        match first {
            b'T' => {
                if let Some(v) = scan_short_after(tok, "T:") {
                    cs.text_position = Some(v as u8);
                }
            }
            b'S' => {
                if let Some(v) = scan_short_after(tok, "S:") {
                    cs.text_size = Some(v as u8);
                }
            }
            b'L' => {
                if tok.ends_with('%')
                    && let Some(v) = scan_short_after(tok, "L:")
                {
                    cs.line_position = Some(v as u8);
                }
                // Otherwise the `L:<line-number>` form, not representable in CueSettings.
            }
            b'D' => {
                if tok.len() > 2 && tok.is_char_boundary(2) {
                    cs.vertical = Some(tok[2..].to_string());
                }
            }
            b'A' if tok.len() > 2 && tok.is_char_boundary(2) => {
                cs.alignment = Some(tok[2..].to_string());
            }
            _ => {}
        }
    }

    cs
}

/// A modern-syntax percentage: a non-negative number in `[0, 100]`, rounded
/// to the `u8` the settings fields hold.
fn parse_percent(s: &str) -> Option<u8> {
    let v = s.parse::<f32>().ok()?;
    (v.is_finite() && (0.0..=100.0).contains(&v)).then(|| v.round() as u8)
}

/// Mimic sscanf `"<prefix>%hd"`. Require the literal prefix, then read an
/// optional sign and decimal digits into a wrapping `i16`. `None` if the prefix
/// is absent or no digit follows.
fn scan_short_after(tok: &str, prefix: &str) -> Option<i16> {
    let rest = tok.strip_prefix(prefix)?;
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut val: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        val = val.wrapping_mul(10).wrapping_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    if i == start {
        return None;
    }
    if neg {
        val = -val;
    }
    Some(val as i16)
}

/// The text pipeline the C runs when a cue's blank line is reached.
/// `g_markup_escape_text` -> `subrip_unescape_formatting` ->
/// `subrip_remove_unhandled_tags` -> `strip_trailing_newlines` ->
/// `subrip_fix_up_markup`. Produces Pango markup.
fn finalize_text(buf: &str) -> String {
    let mut s = markup_escape_text(buf);
    unescape_formatting(&mut s, ALLOWED_VTT_TAGS, true);
    remove_unhandled_tags(&mut s);
    strip_trailing_newlines(&mut s);
    fix_up_markup(&mut s, ALLOWED_VTT_TAGS);
    s
}

/// Port of GLib's `g_markup_escape_text`.
fn markup_escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => {
                let u = c as u32;
                let control = (0x1..=0x8).contains(&u)
                    || (0xb..=0xc).contains(&u)
                    || (0xe..=0x1f).contains(&u)
                    || (0x7f..=0x84).contains(&u)
                    || (0x86..=0x9f).contains(&u);
                if control {
                    out.push_str(&format!("&#x{u:x};"));
                } else {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Port of `subrip_unescape_formatting`. Turns escaped `&lt;tag&gt;` back into
/// real `<tag>` for whitelisted tags (case-sensitive match), optionally keeping
/// a limited set of attribute characters. Disallowed tags stay escaped.
fn unescape_formatting(txt: &mut String, allowed: &[&str], allows_attrs: bool) {
    if !txt.contains("&lt;") {
        return;
    }

    let src = std::mem::take(txt);
    let src = src.as_str();
    let mut out = String::with_capacity(src.len());
    let mut p = 0usize;

    loop {
        let rest = &src[p..];
        let lt_rel = match rest.find("&lt;") {
            Some(i) => i,
            None => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..lt_rel]);
        let lt = p + lt_rel + 4; // just past "&lt;"

        let gt_rel = match src[lt..].find("&gt;") {
            Some(i) => i,
            None => {
                // No closing "&gt;", so copy from "&lt;" to the end verbatim.
                out.push_str(&src[lt - 4..]);
                break;
            }
        };
        let gt = lt + gt_rel; // start of "&gt;"
        let region = &src[lt..gt];
        let rb = region.as_bytes();

        let is_closing = rb.first() == Some(&b'/');
        let mut ts = if is_closing { 1 } else { 0 };
        while ts < rb.len() && (rb[ts] == b' ' || rb[ts] == b'\t') {
            ts += 1;
        }
        let mut te = ts;
        while te < rb.len() && rb[te].is_ascii_alphanumeric() {
            te += 1;
        }
        let name = &region[ts..te];

        let allowed_match = allowed.contains(&name);
        if !allowed_match {
            // Keep the whole "&lt;...&gt;" escaped.
            out.push_str(&src[lt - 4..gt + 4]);
            p = gt + 4;
            continue;
        }

        out.push('<');
        if is_closing {
            out.push('/');
        }
        out.push_str(name);
        if allows_attrs {
            let mut ae = te;
            while ae < rb.len() {
                let b = rb[ae];
                if b.is_ascii_alphanumeric()
                    || b == b'.'
                    || b == b' '
                    || b == b'\t'
                    || b == b'('
                    || b == b')'
                {
                    ae += 1;
                } else {
                    break;
                }
            }
            out.push_str(&region[te..ae]);
        }
        out.push('>');
        p = gt + 4;
    }

    *txt = out;
}

/// Port of `subrip_remove_unhandled_tags`. Deletes any still-escaped
/// `&lt;...&gt;` whose (post-`/`) first character is an ASCII letter, keeping
/// the rest (e.g. escaped inline timestamps `&lt;00:00:00,200&gt;`).
///
/// Single forward scan. The C searches for the closing `&gt;` from every `&lt;`
/// it passes, which rescans to the end of the buffer per escaped `<` and turns
/// one unterminated cue into quadratic work (a cue only ends at a blank line, so
/// a body without one is a single arbitrarily long cue). The `gt` cursor below
/// only ever moves forward, so the whole pass is linear.
fn remove_unhandled_tags(txt: &mut String) {
    if !txt.contains("&lt;") {
        return;
    }

    let src = std::mem::take(txt);
    let src = src.as_str();
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut gt = src.find("&gt;");
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i..].starts_with(b"&lt;") {
            // Advance to the first "&gt;" that can close this "&lt;".
            while let Some(g) = gt.filter(|&g| g < i + 4) {
                gt = src[g + 1..].find("&gt;").map(|rel| g + 1 + rel);
            }
            if let Some(g) = gt {
                let mut t = i + 4;
                if bytes.get(t) == Some(&b'/') {
                    t += 1;
                }
                if bytes.get(t).is_some_and(|b| b.is_ascii_alphabetic()) {
                    i = g + 4; // drop the whole tag
                    continue;
                }
            }
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    *txt = out;
}

/// Port of `strip_trailing_newlines`. Drops trailing `\n` while length > 1.
fn strip_trailing_newlines(txt: &mut String) {
    while txt.len() > 1 && txt.as_bytes()[txt.len() - 1] == b'\n' {
        txt.pop();
    }
}

/// Port of `subrip_fix_up_markup`. Assuming a whitelisted, escaped-then-
/// unescaped input, adds missing closing tags and drops closing tags that were
/// never opened. Tag-name matching is ASCII case-insensitive here (unlike the
/// case-sensitive unescape step above). Operates on bytes. Every cut/append is
/// within ASCII tag regions, so the result stays valid UTF-8.
fn fix_up_markup(txt: &mut String, allowed: &[&str]) {
    let mut buf = std::mem::take(txt).into_bytes();
    let mut open_tags: Vec<String> = Vec::new();
    let mut cur = 0usize;

    while let Some(rel) = buf[cur..].iter().position(|&b| b == b'<') {
        let next_tag = cur + rel;

        let mut offset = 0usize;
        let mut is_closing = false;

        for &tag in allowed {
            let mut ts = next_tag + 1;
            let closing = buf.get(ts) == Some(&b'/');
            if closing {
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
            let name_len = te - name_start;

            let name_matches =
                tag.len() == name_len && tag.as_bytes().eq_ignore_ascii_case(&buf[name_start..te]);
            if name_matches {
                // Optional attributes after the tag name.
                if te < buf.len() && (buf[te] == b' ' || buf[te] == b'\t' || buf[te] == b'.') {
                    while te < buf.len()
                        && buf[te] != b'>'
                        && (buf[te].is_ascii_alphanumeric()
                            || buf[te] == b'.'
                            || buf[te] == b' '
                            || buf[te] == b'\t'
                            || buf[te] == b'('
                            || buf[te] == b')')
                    {
                        te += 1;
                    }
                }
                if te < buf.len() && buf[te] == b'>' {
                    offset = te - (next_tag + 1);
                    is_closing = closing;
                    if !closing {
                        open_tags.push(tag.to_ascii_lowercase());
                    }
                    break;
                }
            }
            offset = 0;
        }

        if offset == 0 {
            cur = next_tag + 1;
            continue;
        }
        if !is_closing {
            cur = next_tag + offset;
            continue;
        }

        // Closing tag.
        let tag_end = match buf[next_tag..].iter().position(|&b| b == b'>') {
            Some(rel) => next_tag + rel,
            None => {
                cur = next_tag + 1;
                continue;
            }
        };

        let matches_last = match open_tags.last() {
            Some(last) => {
                let cmp = next_tag + 2;
                let ll = last.len();
                cmp + ll <= buf.len() && buf[cmp..cmp + ll].eq_ignore_ascii_case(last.as_bytes())
            }
            None => false,
        };

        if !matches_last {
            buf.drain(next_tag..tag_end + 1); // drop the stray closing tag
            cur = next_tag;
            continue;
        }

        open_tags.pop();
        cur = tag_end + 1;
    }

    for t in open_tags.iter().rev() {
        buf.extend_from_slice(b"</");
        buf.extend_from_slice(t.as_bytes());
        buf.push(b'>');
    }

    *txt = String::from_utf8(buf).unwrap_or_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = GST_SECOND;

    fn parse(body: &str) -> Vec<Cue> {
        WebVtt::default()
            .parse(body, &ParseContext::default())
            .unwrap()
    }

    /// Prefix a chunk with the `WEBVTT FILE\n` header, like `test_vtt_do_test`.
    fn vtt(chunk: &str) -> String {
        format!("WEBVTT FILE\n{chunk}")
    }

    /// Parse a single chunk (header-prefixed) and return its lone cue.
    fn one(chunk: &str) -> Cue {
        let cues = parse(&vtt(chunk));
        assert_eq!(cues.len(), 1, "expected exactly one cue for {chunk:?}");
        cues.into_iter().next().unwrap()
    }

    fn assert_cue(chunk: &str, from: u64, to: u64, text: &str) {
        let c = one(chunk);
        assert_eq!(c.start_ns, from, "start for {chunk:?}");
        assert_eq!(c.end_ns, Some(to), "end for {chunk:?}");
        assert_eq!(c.text, text, "text for {chunk:?}");
    }

    #[test]
    fn reversed_timing_saturates_and_does_not_panic() {
        // end < start would underflow `ts_end - ts_start` (a panic under
        // overflow checks). It must saturate to a zero-length cue at `start`.
        let c = one("\n00:00:05.000 --> 00:00:02.000\nx");
        assert_eq!(c.start_ns, 5 * S);
        assert_eq!(c.end_ns, Some(5 * S));
        assert_eq!(c.text, "x");
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(WebVtt::default().output_format(), OutputFormat::PangoMarkup);
    }

    // ---- C test_webvtt: webvtt_input -------------------------------------

    #[test]
    fn c_webvtt_input_cue_settings_are_ignored_for_text() {
        // D: / T: / L: / S: / A: settings do not change text or timing.
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n",
            S,
            2 * S,
            "One",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n",
            S,
            2 * S,
            "One",
        );
    }

    #[test]
    fn c_webvtt_input_escaping() {
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\nOne & Two\n\n",
            S,
            2 * S,
            "One &amp; Two",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\nOne < Two\n\n",
            S,
            2 * S,
            "One &lt; Two",
        );
    }

    #[test]
    fn c_webvtt_input_markup_tags() {
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<v Spoke>Live long and prosper\n\n",
            S,
            2 * S,
            "<v Spoke>Live long and prosper</v>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<v The Joker>HAHAHA\n\n",
            S,
            2 * S,
            "<v The Joker>HAHAHA</v>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<c.someclass>some text\n\n",
            S,
            2 * S,
            "<c.someclass>some text</c>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<b.loud>some text\n\n",
            S,
            2 * S,
            "<b.loud>some text</b>",
        );
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000\n<ruby>base text<rt>annotation</rt></ruby>\n\n",
            S,
            2 * S,
            "<ruby>base text<rt>annotation</rt></ruby>",
        );
    }

    #[test]
    fn c_webvtt_input_inline_timestamps_stay_escaped() {
        assert_cue(
            "1\n00:00:01.000 --> 00:00:03.000\nOne... <00:00:00,200>Two... <00:00:00,500>Three...\n\n",
            S,
            3 * S,
            "One... &lt;00:00:00,200&gt;Two... &lt;00:00:00,500&gt;Three...",
        );
    }

    #[test]
    fn c_webvtt_input_multiline() {
        assert_cue(
            "1\n00:00:02.000 --> 00:00:03.000\nHello\nWorld\n\n",
            2 * S,
            3 * S,
            "Hello\nWorld",
        );
    }

    #[test]
    fn c_webvtt_input_combined_stream() {
        // The C test pushes every chunk into one element instance. State (and
        // the monotonic timing guard) persists across chunks.
        let chunks = [
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\nOne & Two\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\nOne < Two\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<v Spoke>Live long and prosper\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<v The Joker>HAHAHA\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<c.someclass>some text\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<b.loud>some text\n\n",
            "1\n00:00:01.000 --> 00:00:02.000\n<ruby>base text<rt>annotation</rt></ruby>\n\n",
            "1\n00:00:01.000 --> 00:00:03.000\nOne... <00:00:00,200>Two... <00:00:00,500>Three...\n\n",
            "1\n00:00:02.000 --> 00:00:03.000\nHello\nWorld\n\n",
        ];
        let body: String = chunks.iter().map(|c| vtt(c)).collect();
        let cues = parse(&body);
        assert_eq!(cues.len(), chunks.len());
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[10].text, "One &amp; Two");
        assert_eq!(cues[16].text, "<ruby>base text<rt>annotation</rt></ruby>");
        assert_eq!(cues[17].start_ns, S);
        assert_eq!(cues[17].end_ns, Some(3 * S));
        assert_eq!(cues[18].text, "Hello\nWorld");
    }

    // ---- C test_webvtt: webvtt_input1 (no hour component) -----------------

    #[test]
    fn c_webvtt_input1_no_hour() {
        assert_cue(
            "1\n00:01.000 --> 00:02.000 D:vertical T:50%\nNo hour component\n\n",
            S,
            2 * S,
            "No hour component",
        );
    }

    // ---- C test_webvtt: webvtt_input2 (no trailing newline) ---------------

    #[test]
    fn c_webvtt_input2_no_trailing_newline() {
        assert_cue(
            "1\n00:00:01,000 --> 00:00:02,000\nLast cue, no newline at the end",
            S,
            2 * S,
            "Last cue, no newline at the end",
        );
    }

    // ---- C test_webvtt: webvtt_input3 (broken closing tags) ---------------

    #[test]
    fn c_webvtt_input3_broken_closing_tags() {
        assert_cue(
            "1\n00:00:00,000 --> 00:00:01,000\n<ruby>Hello!</ruby>World!\n\n",
            0,
            S,
            "<ruby>Hello!</ruby>World!",
        );
        assert_cue(
            "1\n00:00:01,000 --> 00:00:02,000\n<ruby>Hello!</i></ruby>World!\n\n",
            S,
            2 * S,
            "<ruby>Hello!</ruby>World!",
        );
        assert_cue(
            "1\n00:00:02,000 --> 00:00:03,000\n<i>World!</ruby></i>Hello!\n\n",
            2 * S,
            3 * S,
            "<i>World!</i>Hello!",
        );
    }

    // ---- Cue-settings parsing (our surfaced fields) -----------------------

    fn settings_of(chunk: &str) -> CueSettings {
        one(chunk).settings
    }

    #[test]
    fn settings_vertical_and_text_position() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n");
        assert_eq!(cs.vertical.as_deref(), Some("vertical"));
        assert_eq!(cs.text_position, Some(50));
        assert_eq!(cs.line_position, None);
        assert_eq!(cs.text_size, None);
        assert_eq!(cs.alignment, None);
    }

    #[test]
    fn settings_vertical_lr() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n");
        assert_eq!(cs.vertical.as_deref(), Some("vertical-lr"));
    }

    #[test]
    fn settings_line_percent_vs_line_number() {
        // "L:12%" fills line_position. "L:123"/"L:-123" are the (unstored) line-number form.
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n").line_position,
            Some(12)
        );
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n").line_position,
            None
        );
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n").line_position,
            None
        );
    }

    #[test]
    fn settings_size_alignment_combo() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n");
        assert_eq!(cs.line_position, Some(12));
        assert_eq!(cs.text_size, Some(35));
        assert_eq!(cs.alignment.as_deref(), Some("start"));
    }

    #[test]
    fn settings_alignment_values() {
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n")
                .alignment
                .as_deref(),
            Some("middle")
        );
        assert_eq!(
            settings_of("00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n")
                .alignment
                .as_deref(),
            Some("end")
        );
    }

    #[test]
    fn settings_separators_tab_and_multispace() {
        let tab = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n");
        assert_eq!(tab.vertical.as_deref(), Some("vertical"));
        assert_eq!(tab.text_position, Some(50));

        let multi = settings_of("00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n");
        assert_eq!(multi.vertical.as_deref(), Some("vertical"));
        assert_eq!(multi.text_position, Some(50));
    }

    #[test]
    fn settings_absent_are_all_none() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000\nOne\n\n");
        assert_eq!(cs, CueSettings::default());
    }

    // ---- Modern (name:value) cue settings ----------------------------------

    #[test]
    fn modern_settings_are_parsed() {
        let cs = settings_of(
            "00:00:01.000 --> 00:00:02.000 align:center position:50%,line-left size:35% line:10%,end vertical:rl\nOne\n\n",
        );
        assert_eq!(cs.alignment.as_deref(), Some("center"));
        assert_eq!(cs.text_position, Some(50));
        assert_eq!(cs.position_align.as_deref(), Some("line-left"));
        assert_eq!(cs.text_size, Some(35));
        assert_eq!(cs.line_position, Some(10));
        assert_eq!(cs.line_align.as_deref(), Some("end"));
        assert_eq!(cs.vertical.as_deref(), Some("rl"));
    }

    #[test]
    fn modern_line_number_form() {
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 line:-1\nOne\n\n");
        assert_eq!(cs.line_number, Some(-1));
        assert_eq!(cs.line_position, None);
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 line:3,start\nOne\n\n");
        assert_eq!(cs.line_number, Some(3));
        assert_eq!(cs.line_align.as_deref(), Some("start"));
    }

    #[test]
    fn modern_settings_do_not_change_cue_text_or_timing() {
        // Parity: the C drops every setting; text/timing must be identical.
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.000 align:center position:50%\nOne\n\n",
            S,
            2 * S,
            "One",
        );
    }

    #[test]
    fn modern_percent_values_are_validated() {
        // Out-of-range or junk percentages are dropped, not wrapped.
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 position:150% size:abc%\nOne\n\n");
        assert_eq!(cs.text_position, None);
        assert_eq!(cs.text_size, None);
    }

    #[test]
    fn old_single_letter_forms_still_parse() {
        // The archaic syntax keeps working alongside the modern one.
        let cs = settings_of("00:00:01.000 --> 00:00:02.000 T:50% align:end\nOne\n\n");
        assert_eq!(cs.text_position, Some(50));
        assert_eq!(cs.alignment.as_deref(), Some("end"));
    }

    // ---- STYLE blocks and cue identifiers ---------------------------------

    /// Parse a whole body and hand back the cues plus the parser (for its
    /// stylesheet).
    fn parse_keeping_parser(body: &str) -> (Vec<Cue>, WebVtt) {
        let mut p = WebVtt::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        (cues, p)
    }

    #[test]
    fn style_block_is_collected_and_cues_are_untouched() {
        let body = "WEBVTT\n\nSTYLE\n::cue { color: lime }\n\n00:00:01.000 --> 00:00:02.000\n<i>Hi</i>\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 1);
        // Parity: text and timing exactly as without the STYLE block.
        assert_eq!(cues[0].text, "<i>Hi</i>");
        assert_eq!(cues[0].start_ns, S);
        let sheet = p.stylesheet().expect("stylesheet collected");
        assert_eq!(sheet.rules().len(), 1);
    }

    #[test]
    fn multiple_style_blocks_accumulate() {
        // WPT embedded_style_cascade_priority.vtt: both blocks apply.
        let body = "WEBVTT\n\nSTYLE\n::cue { color: green }\n\nSTYLE\n::cue { background: green }\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n";
        let (_, p) = parse_keeping_parser(body);
        assert_eq!(p.stylesheet().unwrap().rules().len(), 2);
    }

    #[test]
    fn style_lines_must_match_exactly_and_precede_cues() {
        // WPT embedded_style_invalid_format.vtt distillate: none of these
        // start a style block.
        for bad in ["STYLE Invalid", " STYLE", "style", "S T Y L E", "STYLE {"] {
            let body = format!(
                "WEBVTT\n\n{bad}\n::cue {{ color: red }}\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n"
            );
            let (cues, p) = parse_keeping_parser(&body);
            assert_eq!(cues.len(), 1, "{bad:?} must not eat the cue");
            assert!(p.stylesheet().is_none(), "{bad:?} must not collect CSS");
        }
        // A STYLE block after the first cue is ignored (spec).
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n\nSTYLE\n::cue { color: red }\n\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 2);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn timing_line_terminates_a_style_block() {
        // Spec: a "-->" line ends the block and starts its cue.
        let body = "WEBVTT\n\nSTYLE\n::cue { color: lime }\n00:00:01.000 --> 00:00:02.000\nOne\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "One");
        assert_eq!(p.stylesheet().unwrap().rules().len(), 1);
    }

    #[test]
    fn empty_style_block_collects_nothing() {
        // "STYLE" followed by a blank line: the block ends empty, and the
        // CSS-looking lines after it are ordinary ignored seek lines.
        let body =
            "WEBVTT\n\nSTYLE\n\n::cue { color: red }\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n";
        let (cues, p) = parse_keeping_parser(body);
        assert_eq!(cues.len(), 1);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn cue_identifier_is_captured() {
        let cues = parse(&vtt(
            "greeting\n00:00:01.000 --> 00:00:02.000\nHi\n\n00:00:03.000 --> 00:00:04.000\nAnon\n\n",
        ));
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].id.as_deref(), Some("greeting"));
        // No id line for the second cue; the blank separator reset it.
        assert_eq!(cues[1].id, None);
    }

    #[test]
    fn note_line_directly_before_timing_becomes_the_id() {
        // The id is the immediately preceding non-blank line; a NOTE block
        // separated by a blank line is not.
        let cues = parse(&vtt(
            "NOTE a comment\n\n00:00:01.000 --> 00:00:02.000\nOne\n\n",
        ));
        assert_eq!(cues[0].id, None);
    }

    #[test]
    fn stylesheet_survives_incremental_chunking() {
        let body = "WEBVTT\n\nSTYLE\n::cue(b) { color: red }\n\n00:00:01.000 --> 00:00:02.000\n<b>One</b>\n\n";
        // Feed one byte at a time through the streaming contract.
        let mut p = WebVtt::default();
        let mut buf = String::new();
        let mut cues = Vec::new();
        for (i, ch) in body.char_indices() {
            buf.push_str(&body[i..i + ch.len_utf8()]);
            let parsed = p
                .parse_incremental(&buf, &ParseContext::default(), false)
                .unwrap();
            cues.extend(parsed.cues);
            buf.drain(..parsed.consumed);
        }
        let parsed = p
            .parse_incremental(&buf, &ParseContext::default(), true)
            .unwrap();
        cues.extend(parsed.cues);

        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<b>One</b>");
        assert_eq!(p.stylesheet().unwrap().rules().len(), 1);
    }

    // ---- Block separation: the spec's `-->` resynchronisation -------------

    /// The `NOTE` block of WPT `embedded_style_urls.vtt`, verbatim.
    const WPT_URLS_NOTE: &str = "NOTE\n\
        Background for Voice1 should apply.\n\
        The other two backgrounds should not render because non-data URLs are not supported.";

    /// Its `STYLE` block, verbatim: three rules, brace-on-its-own-line and
    /// brace-on-the-selector-line, a `data:` URL holding `;` `,` `/` and `+`,
    /// and a multi-declaration rule.
    const WPT_URLS_STYLE: &str = "STYLE\n\
        ::cue(v[voice=Voice1])\n\
        {\n    \
            background: url(data:image/gif;base64,R0lGODlhEAAQAMQAAORHHOVSKudfOulrSOp3WOyDZu6QdvCchPGolfO0o/XBs/fNwfjZ0frl3/zy7////wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACH5BAkAABAALAAAAAAQABAAAAVVICSOZGlCQAosJ6mu7fiyZeKqNKToQGDsM8hBADgUXoGAiqhSvp5QAnQKGIgUhwFUYLCVDFCrKUE1lBavAViFIDlTImbKC5Gm2hB0SlBCBMQiB0UjIQA7)\n\
        }\n\
        ::cue(b)\n\
        {\n    \
            background: url(\"support/background.png\")\n\
        }\n\
        ::cue(i) {\n    \
            background: url(\"support/background.png\");\n    \
            background: -webkit-image-set(url(\"support/background.png\") 1x, url(\"support/background.png\") 2x);\n\
        }";

    const WPT_URLS_CUE1: &str = "00:00:00.000 --> 00:00:05.000\n\
        <v Voice1>This <i>is</i> a <b>test</b> subtitle";
    const WPT_URLS_CUE2: &str = "00:00:00.000 --> 00:00:05.000\n\
        <v Voice2>Here <i>is</i> a <b>second</b> subtitle";

    /// Both cues, whatever separates the blocks: two cues over the same
    /// interval, tags intact, and — the bug this pins — no timing line
    /// anywhere in the text.
    fn assert_wpt_urls_cues(cues: &[Cue]) {
        assert_eq!(cues.len(), 2, "expected exactly two cues, got {cues:?}");
        for c in cues {
            assert_eq!(c.start_ns, 0);
            assert_eq!(c.end_ns, Some(5 * S));
            assert!(
                !c.text.contains("--&gt;") && !c.text.contains("-->"),
                "a timing line leaked into the cue text: {:?}",
                c.text
            );
            assert!(
                !c.text.contains("00:00:00.000"),
                "a timing line leaked into the cue text: {:?}",
                c.text
            );
        }
        assert_eq!(
            cues[0].text,
            "<v Voice1>This <i>is</i> a <b>test</b> subtitle</v>"
        );
        assert_eq!(
            cues[1].text,
            "<v Voice2>Here <i>is</i> a <b>second</b> subtitle</v>"
        );
    }

    #[test]
    fn wpt_embedded_style_urls_run_together() {
        // The file exactly as WPT ships it: not one blank line in it, so every
        // block is terminated by the `-->` line that follows it. The C runs
        // the second timing line into the first cue's text; the spec's block
        // collector rewinds to it instead.
        let body = format!(
            "WEBVTT\n{WPT_URLS_NOTE}\n{WPT_URLS_STYLE}\n{WPT_URLS_CUE1}\n{WPT_URLS_CUE2}\n"
        );
        let (cues, p) = parse_keeping_parser(&body);
        assert_wpt_urls_cues(&cues);
        // The CSS parses without disturbing the block structure. All three
        // rules declare nothing but an image background, which we do not
        // support, so the sheet comes out empty rather than partial.
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn wpt_embedded_style_urls_blank_line_separated() {
        // The same file with the blocks separated the conventional way. Same
        // two cues: the `-->` rule and the blank-line rule agree.
        let body = format!(
            "WEBVTT\n\n{WPT_URLS_NOTE}\n\n{WPT_URLS_STYLE}\n\n{WPT_URLS_CUE1}\n\n{WPT_URLS_CUE2}\n"
        );
        let (cues, p) = parse_keeping_parser(&body);
        assert_wpt_urls_cues(&cues);
        assert!(p.stylesheet().is_none());
    }

    #[test]
    fn style_block_extent_survives_urls_and_braces() {
        // Same STYLE block, with one supported declaration bolted onto its
        // last rule: it is collected, which pins the block's extent — the
        // braces and the `;`-bearing `data:` URL before it neither ended the
        // block early nor swallowed the cues after it.
        let style = format!("{WPT_URLS_STYLE}\n::cue(b) {{ color: lime }}");
        let body = format!("WEBVTT\n{WPT_URLS_NOTE}\n{style}\n{WPT_URLS_CUE1}\n{WPT_URLS_CUE2}\n");
        let (cues, p) = parse_keeping_parser(&body);
        assert_wpt_urls_cues(&cues);
        let sheet = p.stylesheet().expect("the last rule is applicable");
        assert_eq!(sheet.rules().len(), 1);
    }

    #[test]
    fn timing_line_ends_the_cue_being_collected() {
        // Reduced form of the bug: no blank line between the cues.
        let body =
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[1].text, "Two");
        assert_eq!(cues[1].start_ns, 3 * S);
    }

    #[test]
    fn resynchronisation_keeps_the_identifier_rules() {
        // Spec: the resynchronised timing line is line 1 of its own block, so
        // its cue has no identifier — the line before it belongs to the text
        // of the cue that just ended.
        let body = "WEBVTT\n\nid1\n00:00:01.000 --> 00:00:02.000\nOne\nid2\n\
                    00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].id.as_deref(), Some("id1"));
        assert_eq!(cues[0].text, "One\nid2");
        assert_eq!(cues[1].id, None);
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn unparsable_arrow_line_ends_the_cue_and_eats_its_block() {
        // Any `-->` line ends the cue, not just a well-formed timing line.
        // The new block then fails cue creation, so its lines are dropped —
        // the spec discards a block whose cue is null.
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\n\
                    broken --> line\ndropped\n\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn back_to_back_timing_lines_leave_the_first_cue_empty() {
        let body =
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n00:00:03.000 --> 00:00:04.000\nTwo\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn multi_line_cue_text_is_still_joined() {
        // The resynchronisation must not split ordinary multi-line text.
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nOne\nTwo\nThree\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "One\nTwo\nThree");
    }

    // ---- Timestamp edge cases --------------------------------------------

    #[test]
    fn timestamp_dot_and_comma_both_accepted() {
        assert_cue(
            "00:00:01.500 --> 00:00:02.250\nX\n\n",
            1_500 * GST_MSECOND,
            2_250 * GST_MSECOND,
            "X",
        );
        assert_cue(
            "00:00:01,500 --> 00:00:02,250\nX\n\n",
            1_500 * GST_MSECOND,
            2_250 * GST_MSECOND,
            "X",
        );
    }

    #[test]
    fn timestamp_short_fraction_is_right_padded() {
        // "1,5" -> 500ms (padded to "500"), matching parse_subrip_time.
        assert_cue(
            "00:00:01,5 --> 00:00:02,25\nX\n\n",
            1_500 * GST_MSECOND,
            2_250 * GST_MSECOND,
            "X",
        );
    }

    #[test]
    fn timestamp_hours_component() {
        assert_cue(
            "01:02:03.004 --> 01:02:04.005\nX\n\n",
            (3600 + 2 * 60 + 3) * S + 4 * GST_MSECOND,
            (3600 + 2 * 60 + 4) * S + 5 * GST_MSECOND,
            "X",
        );
    }

    #[test]
    fn short_fraction_with_cue_settings_keeps_the_cue() {
        // The ' ' -> '0' step munges the settings into the fractional field
        // ("2.5 A:start" -> "2,50A:start" -> "50A"), and the field is sscanf's
        // last conversion, so its leading digits are all that is read. The C
        // reports 1.000 -> 2.050 for every form below. Rejecting the field
        // silently dropped the cue *and* its text.
        for tail in ["A:start", "align:middle", "T:50%", "L:12% S:35% A:start"] {
            assert_cue(
                &format!("1\n00:00:01.000 --> 00:00:02.5 {tail}\nOne\n\n"),
                S,
                2 * S + 50 * GST_MSECOND,
                "One",
            );
        }
        // Hour-less form, same munging.
        assert_cue(
            "1\n00:01.000 --> 00:02.5 A:start\nOne\n\n",
            S,
            2 * S + 50 * GST_MSECOND,
            "One",
        );
        // Settings still reach `CueSettings` when the fraction is short.
        let cs = settings_of("00:00:01.000 --> 00:00:02.5 A:start\nOne\n\n");
        assert_eq!(cs.alignment.as_deref(), Some("start"));
        // A conformant three-digit fraction is unchanged.
        assert_cue(
            "1\n00:00:01.000 --> 00:00:02.500 A:start\nOne\n\n",
            S,
            2 * S + 500 * GST_MSECOND,
            "One",
        );
    }

    #[test]
    fn tab_in_timestamp_fields_is_skipped_like_sscanf() {
        // %u skips leading whitespace in every field, so a tab is tolerated
        // wherever a space would have been turned into a '0'.
        assert_cue("\t00:00:01,000 --> 00:00:02,000\nOne\n\n", S, 2 * S, "One");
        assert_cue("00:\t0:01,000 --> 00:00:02,000\nOne\n\n", S, 2 * S, "One");
        // Sub-second field too: ",\t5" pads to "\t50", i.e. 50 ms.
        assert_cue(
            "00:00:01,\t5 --> 00:00:02,000\nOne\n\n",
            S + 50 * GST_MSECOND,
            2 * S,
            "One",
        );
    }

    #[test]
    fn junk_inside_a_timestamp_field_rejects_the_cue() {
        // The C matches the format's ',' literal against the 'x', fails, and
        // drops the cue. Parsing recovers on the next one.
        let body = vtt(
            "1\n00:00:01x,000 --> 00:00:02,000\nJunk\n\n00:00:03.000 --> 00:00:04.000\nGood\n\n",
        );
        let cues = parse(&body);
        assert_eq!(cues.len(), 1, "cues: {cues:#?}");
        assert_eq!(cues[0].text, "Good");
        assert_eq!(parse_subrip_time("00:00:01x,000"), None);
        assert_eq!(parse_subrip_time("00:01x,000"), None);
    }

    #[test]
    fn signed_timestamp_field_matches_strtoul() {
        // %u defers to strtoul, which accepts a sign: '+' is a no-op and '-'
        // wraps into the C's unsigned field.
        assert_eq!(parse_subrip_time("00:00:+1,000"), Some(S));
        assert_eq!(
            parse_subrip_time("00:00:-1,000"),
            Some(u64::from(u32::MAX) * S)
        );
    }

    #[test]
    fn absurd_minute_field_wraps_at_32_bits() {
        // The comma guard still admits an 8-digit field in the hour-less form.
        // The C keeps it in a guint, so `min * 60` wraps rather than running off
        // into the far future, and we wrap with it.
        assert_eq!(
            parse_subrip_time("99999999:00,000"),
            Some(1_705_032_644 * S)
        );
        assert_eq!(parse_subrip_time("71582789:00,000"), Some(44 * S));
    }

    #[test]
    fn cue_after_a_reversed_one_is_still_emitted() {
        // The clamped cue must not clamp the guard. The element pushes the
        // underflowed duration and then does `start_time += duration`, which
        // lands back on the parsed end (2s), so 2s <= 4s and the second cue
        // survives. Feeding the guard the clamped end (5s) dropped it.
        let body =
            "WEBVTT\n\n00:00:05.000 --> 00:00:02.000\nA\n\n00:00:03.000 --> 00:00:04.000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2, "cues: {cues:#?}");
        assert_eq!(cues[0].text, "A");
        assert_eq!((cues[0].start_ns, cues[0].end_ns), (5 * S, Some(5 * S)));
        assert_eq!(cues[1].text, "B");
        assert_eq!((cues[1].start_ns, cues[1].end_ns), (3 * S, Some(4 * S)));
    }

    #[test]
    fn unhandled_tag_scan_with_a_distant_closing_marker() {
        // `remove_unhandled_tags` runs one forward cursor over the escaped
        // "&gt;"s instead of searching from every escaped '<'. A '<' followed by
        // a digit is not a tag and must survive even when the only "&gt;" in the
        // buffer is far to the right, while a real tag is still dropped.
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<5<5<5 <font x>y>\n\n",
            S,
            2 * S,
            "&lt;5&lt;5&lt;5 y&gt;",
        );
        // "&lt;" with no "&gt;" anywhere is left completely alone.
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<font <i <b\n\n",
            S,
            2 * S,
            "&lt;font &lt;i &lt;b",
        );
    }

    #[test]
    fn broken_timestamp_without_comma_is_skipped() {
        // End time "00:03:0" has no fractional separator -> whole cue dropped.
        // A following well-formed cue is still parsed. Mirrors srt_input5.
        let body = vtt(
            "2\n00:02:00,000 --> 00:03:0\nsome other text\n\n3\n00:00:03,000 --> 00:00:04,000\nsome more text\n\n",
        );
        let cues = parse(&body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 3 * S);
        assert_eq!(cues[0].end_ns, Some(4 * S));
        assert_eq!(cues[0].text, "some more text");
    }

    // ---- State-machine / structural edge cases ----------------------------

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
        assert!(parse("WEBVTT\n\n").is_empty());
        assert!(parse("WEBVTT FILE\n\n\n").is_empty());
    }

    #[test]
    fn header_and_cue_id_lines_are_ignored() {
        // Cue identifier "greeting" and NOTE block must not become text.
        let body =
            "WEBVTT\n\nNOTE this is a comment\n\ngreeting\n00:00:00.000 --> 00:00:01.000\nHi\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
        assert_eq!(cues[0].start_ns, 0);
        assert_eq!(cues[0].end_ns, Some(S));
    }

    #[test]
    fn crlf_line_endings() {
        let body = "WEBVTT\r\n\r\n00:00:01.000 --> 00:00:02.000\r\nHi\r\n\r\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn monotonic_guard_drops_backwards_cue() {
        // After a 5-6s cue, prev end = 6s. A 1-2s cue fails `prev <= ts_end`.
        let body =
            "WEBVTT\n\n00:00:05.000 --> 00:00:06.000\nA\n\n00:00:01.000 --> 00:00:02.000\nB\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "A");
        assert_eq!(cues[0].start_ns, 5 * S);
    }

    #[test]
    fn two_cues_in_one_body() {
        let body = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nFirst\n\n00:00:02.000 --> 00:00:03.000\nSecond\n\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "First");
        assert_eq!(cues[1].text, "Second");
        assert_eq!(cues[1].start_ns, 2 * S);
        assert_eq!(cues[1].end_ns, Some(3 * S));
    }

    #[test]
    fn disallowed_tag_is_stripped() {
        // <font> is not in the VTT whitelist, so it is removed entirely.
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<font color=\"red\">hi</font>\n\n",
            S,
            2 * S,
            "hi",
        );
    }

    #[test]
    fn unclosed_allowed_tag_is_auto_closed() {
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\n<i>italic\n\n",
            S,
            2 * S,
            "<i>italic</i>",
        );
    }

    #[test]
    fn double_quote_and_apostrophe_escape() {
        assert_cue(
            "00:00:01.000 --> 00:00:02.000\nsay \"hi\" it's ok\n\n",
            S,
            2 * S,
            "say &quot;hi&quot; it&apos;s ok",
        );
    }

    #[test]
    fn duration_helper() {
        let c = one("00:00:01.000 --> 00:00:02.500\nX\n\n");
        assert_eq!(c.duration_ns(), Some(1_500 * GST_MSECOND));
    }
}
