// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! MicroDVD (`.sub`) parser.
//!
//! See `specs/microdvd.md`. C reference: `parse_mdvdsub` in
//! `gst-plugins-base/gst/subparse/gstsubparse.c`.
//!
//! MicroDVD is a **frame-based** line format: `{start_frame}{end_frame}text`.
//! Timing is derived from a frames-per-second rate, so the same file plays at
//! different wall-clock times depending on the video's fps. The rate can be
//! carried in-band as a `{1}{1}<fps>` header line (frame 1→1 is never a real
//! cue). Otherwise the caller's [`ParseContext::fps`] is used, defaulting to the
//! upstream `24000/1001` when unset.
//!
//! Inline style codes (`{y:i}`, `{y:b}`, `{s:NN}`, and leading/trailing `/`) are
//! translated to Pango markup. Every visual line becomes a `<span ...>...</span>`
//! and the text is GLib-`g_markup_escape_text`-escaped.

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};

const GST_SECOND: u64 = 1_000_000_000;

/// Parser for the MicroDVD subtitle format.
///
/// Streaming: one line is one complete record, so a line is emitted the moment
/// its `\n` arrives. The only state carried between calls is the running frame
/// rate, which a `{1}{1}<fps>` header line can change part-way through a file.
#[derive(Debug, Default)]
pub struct MicroDvd {
    lines: LineScanner,
    /// Running frame rate, once decided. `None` until the first call, which
    /// seeds it from [`ParseContext::fps`].
    fps: Option<(u64, u64)>,
}

impl SubtitleFormat for MicroDvd {
    fn parse_incremental(
        &mut self,
        body: &str,
        ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError> {
        // Running frame rate. Starts at the caller-provided fps (or the upstream
        // default 24000/1001) and can be overridden by a `{1}{1}<fps>` line.
        let fps = self.fps.get_or_insert(match ctx.fps {
            Some((n, d)) if n != 0 && d != 0 => (n as u64, d as u64),
            _ => (24000, 1001),
        });

        let mut cues = Vec::new();
        let mut consumed = self
            .lines
            .feed(body, |line| parse_line(line, fps, &mut cues));

        if at_eos {
            // The unterminated remainder is dropped, matching `get_next_line`.
            // The element force-feeds a "\n\n" at EOS to flush a trailing
            // record, but only for SubRip, TMPlayer, MPL2, QTtext and WebVTT
            // (gstsubparse.c:1889). MicroDVD is not on that list, so its final
            // line is lost unless the file ends with a newline. That is a
            // one-line cue format where flushing would clearly be nicer, but
            // this is a drop-in replacement: LRC, DKS, SubViewer and MPSub drop
            // the tail here for exactly the same reason.
            consumed = body.len();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        // parse_mdvdsub advertises `text/x-raw, format=pango-markup`.
        OutputFormat::PangoMarkup
    }
}

/// Handle one line (terminator and any `\r` already removed), updating `fps` on
/// a `{1}{1}` header and pushing a cue for a real subtitle line.
fn parse_line(line: &str, fps: &mut (u64, u64), cues: &mut Vec<Cue>) {
    // `{start}{end}` prefix. If it isn't there, this isn't a MicroDVD line, so
    // skip it (lenient recovery, like the C's warning + NULL).
    let Some((start_frame, end_frame, text_off)) = parse_two_braced(line) else {
        return;
    };
    let text = &line[text_off..];

    // `{1}{1}` is the fps header, never a subtitle. Try to read a rate from it
    // (comma is accepted as the decimal separator). Always emit nothing for
    // this line.
    if start_frame == 1 && end_frame == 1 {
        if let Some((n, d)) = parse_fps(text) {
            *fps = (n, d);
        }
        return;
    }

    let (fps_n, fps_d) = *fps;
    // frame -> ns, truncating like gst_util_uint64_scale.
    let start_ns = scale(start_frame, GST_SECOND * fps_d, fps_n);
    let dur_ns = scale(
        end_frame.saturating_sub(start_frame),
        GST_SECOND * fps_d,
        fps_n,
    );
    let end_ns = start_ns.saturating_add(dur_ns);

    // A malformed `{s:...}` (missing closing brace) makes the C free the markup
    // and drop the whole line.
    if let Some(markup) = build_markup(text) {
        cues.push(Cue::new(start_ns, Some(end_ns), markup));
    }
}

/// Parse a leading `{<digits>}{<digits>}` and return `(start, end, offset)`,
/// where `offset` is the byte index just past the second `}` (the start of the
/// text), mirroring the C's two `strchr(line, '}')` steps.
fn parse_two_braced(line: &str) -> Option<(u64, u64, usize)> {
    let b = line.as_bytes();
    let mut i = 0;
    let start = scan_braced_uint(b, &mut i)?;
    // The second group's closing brace is not required for acceptance: the
    // C's sscanf has completed both %u assignments before the trailing '}'
    // literal can fail, and the text is then located with strchr, so junk
    // between the second number and its brace is skipped rather than
    // rejecting the line ("{12}{34 x}y" reads frames 12/34 with text "y").
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    skip_c_space(b, &mut i);
    let end = scan_uint(b, &mut i)?;
    // strchr for the group's '}'. A line with no '}' left is undefined
    // behavior in the C (strchr returns NULL and the parser adds 1 to it);
    // dropping the line is the only sane reading.
    let close = b[i..].iter().position(|&c| c == b'}')? + i;
    Some((start, end, close + 1))
}

/// Match a single `{<ws?><digits>}` group, advancing `i` past the closing brace.
/// `%u` in the C `sscanf` skips leading whitespace, so we do too.
fn scan_braced_uint(b: &[u8], i: &mut usize) -> Option<u64> {
    if b.get(*i) != Some(&b'{') {
        return None;
    }
    *i += 1;
    skip_c_space(b, i);
    let val = scan_uint(b, i)?;
    if b.get(*i) != Some(&b'}') {
        return None;
    }
    *i += 1;
    Some(val)
}

/// Skip the characters C's `isspace()` treats as whitespace, which is what the
/// `sscanf` conversions in this format skip before their digits.
fn skip_c_space(b: &[u8], i: &mut usize) {
    while matches!(b.get(*i), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)) {
        *i += 1;
    }
}

/// Read a run of ASCII decimal digits as a `u64` (saturating on overflow).
/// Returns `None` if there is no digit at `*i`.
fn scan_uint(b: &[u8], i: &mut usize) -> Option<u64> {
    let start = *i;
    let mut val: u64 = 0;
    while let Some(d @ b'0'..=b'9') = b.get(*i) {
        val = val.saturating_mul(10).saturating_add((d - b'0') as u64);
        *i += 1;
    }
    if *i == start { None } else { Some(val) }
}

/// `gst_util_uint64_scale(val, num, denom)`: `val * num / denom` computed in
/// 128-bit and truncated toward zero. `denom == 0` is treated as `0`.
///
/// A result past `u64::MAX` saturates there, as upstream does (it returns
/// `G_MAXUINT64` on overflow). Wrapping instead would turn an absurd frame
/// number into a small, plausible-looking timestamp.
fn scale(val: u64, num: u64, denom: u64) -> u64 {
    if denom == 0 {
        return 0;
    }
    let scaled = (val as u128 * num as u128) / denom as u128;
    scaled.min(u64::MAX as u128) as u64
}

/// Read an fps value from a `{1}{1}` header's text. The C replaces `,` with `.`,
/// runs `g_ascii_strtod`, and accepts the result only if a number was parsed and
/// `0.001 <= fps <= 1000.0`. We take the *exact* rational value of the decimal
/// literal (reduced), which equals GLib's double→fraction for the terminating
/// decimals these headers use.
///
/// The accepted syntax follows `g_ascii_strtod` (i.e. C `strtod` in the C
/// locale): leading whitespace, an optional sign, digits with an optional
/// fractional part, and an optional decimal exponent. `1e2` is 100 fps, and
/// missing that used to leave the rate at its default, mistiming the file by the
/// full exponent. Two things `strtod` also accepts are deliberately left out:
/// hexadecimal floats (`0x19p0`), which no header uses and which do not survive
/// the C's `,`→`.` pass unscathed either, and `inf`/`nan`, which fail the range
/// check below in the C exactly as they fail the "at least one digit" check here.
fn parse_fps(text: &str) -> Option<(u64, u64)> {
    let b = text.as_bytes();
    let mut i = 0;
    // g_ascii_strtod skips leading whitespace and an optional sign.
    skip_c_space(b, &mut i);
    if matches!(b.get(i), Some(b'+' | b'-')) {
        // A negative rate can never pass the range check below. Bail on '-'.
        if b[i] == b'-' {
            return None;
        }
        i += 1;
    }

    let mut int_part: u64 = 0;
    let mut int_digits = 0u32;
    while let Some(d @ b'0'..=b'9') = b.get(i) {
        int_part = int_part
            .saturating_mul(10)
            .saturating_add((*d - b'0') as u64);
        int_digits += 1;
        i += 1;
    }

    let mut num: u64 = int_part;
    let mut den: u64 = 1;
    let mut frac_digits = 0u32;
    // Accept both '.' and ',' as the decimal separator (the C converts ',').
    if matches!(b.get(i), Some(b'.' | b',')) {
        i += 1;
        // Cap the fractional digits so num/den stay well within u64.
        while let (Some(d @ b'0'..=b'9'), true) = (b.get(i), frac_digits < 9) {
            num = num.saturating_mul(10).saturating_add((*d - b'0') as u64);
            den = den.saturating_mul(10);
            frac_digits += 1;
            i += 1;
        }
    }

    // "end != rest": at least one digit must have been consumed.
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }

    // An `e`/`E` exponent, only honored when at least one exponent digit
    // follows it (strtod otherwise ends the number before the `e`, which for our
    // purposes is the same as there being no exponent at all).
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        let negative = match b.get(j) {
            Some(b'+') => {
                j += 1;
                false
            }
            Some(b'-') => {
                j += 1;
                true
            }
            _ => false,
        };
        let mut exp: i32 = 0;
        let mut exp_digits = 0u32;
        while let Some(d @ b'0'..=b'9') = b.get(j) {
            // Clamped: anything past MAX_EXP is out of range whatever follows.
            exp = (exp.saturating_mul(10).saturating_add((*d - b'0') as i32)).min(MAX_EXP + 1);
            exp_digits += 1;
            j += 1;
        }
        if exp_digits > 0 {
            (num, den) = apply_exponent(num, den, if negative { -exp } else { exp })?;
        }
    }

    let value = num as f64 / den as f64;
    if !(0.001..=1000.0).contains(&value) {
        return None;
    }

    let g = gcd(num, den);
    Some((num / g, den / g))
}

/// Beyond this an exponent can only push the value out of `[0.001, 1000]`: the
/// mantissa is at most `1e12 / 1` and at least `1 / MAX_DEN`.
const MAX_EXP: i32 = 40;
/// The largest denominator kept, which is also the one the 9-fractional-digit
/// cap above implies. It keeps `GST_SECOND * den` (computed per cue) in range.
const MAX_DEN: u64 = 1_000_000_000;

/// Multiply the rational `num / den` by `10^exp`, exactly, or `None` when the
/// result cannot be an in-range frame rate (see [`MAX_EXP`], [`MAX_DEN`]).
fn apply_exponent(mut num: u64, mut den: u64, exp: i32) -> Option<(u64, u64)> {
    // A zero mantissa stays zero, which the range check would reject anyway, and
    // bailing here also keeps the loop below short for an absurd exponent.
    if num == 0 || exp.abs() > MAX_EXP {
        return None;
    }
    for _ in 0..exp.abs() {
        if exp > 0 {
            // Prefer shrinking the denominator, so the numerator stays small.
            if den > 1 && den.is_multiple_of(10) {
                den /= 10;
            } else {
                num = num.checked_mul(10)?;
            }
        } else if num.is_multiple_of(10) {
            num /= 10;
        } else {
            den = den.checked_mul(10).filter(|d| *d <= MAX_DEN)?;
        }
    }
    Some((num, den))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.max(1)
}

/// Build the Pango markup for one MicroDVD cue's text (already past the frame
/// braces). Returns `None` if a `{s:...}` size code lacks a closing brace, which
/// the C treats as a fatal parse of the line (no cue emitted).
fn build_markup(text: &str) -> Option<String> {
    let mut markup = String::with_capacity(text.len() + 16);
    let mut rest = text;

    loop {
        let mut italic = false;
        let mut bold = false;
        let mut fontsize: u64 = 0;

        // Style codes, each checked once, in the C's order.
        if let Some(r) = rest.strip_prefix("{y:i}") {
            italic = true;
            rest = r;
        }
        if let Some(r) = rest.strip_prefix("{y:b}") {
            bold = true;
            rest = r;
        }
        if let Some(after) = rest.strip_prefix("{s:") {
            // `sscanf (line, "{s:%u}", &fontsize)`, whose `%u` skips leading
            // whitespace, so `{s: 20}` is a size code as much as `{s:20}` is.
            let b = after.as_bytes();
            let mut i = 0;
            skip_c_space(b, &mut i);
            if let Some(value) = scan_uint(b, &mut i) {
                // sscanf assigned the size. Then strchr finds the closing brace.
                // Its absence makes the C drop the whole line, hence the `?`.
                let close = rest.find('}')?;
                fontsize = value;
                rest = &rest[close + 1..];
            }
        }
        // A leading '/' also means italics.
        if let Some(r) = rest.strip_prefix('/') {
            italic = true;
            rest = r;
        }

        let (chunk_src, next) = match rest.find('|') {
            Some(i) => (&rest[..i], Some(i + 1)),
            None => (rest, None),
        };

        let mut chunk = String::with_capacity(chunk_src.len());
        markup_escape(chunk_src, &mut chunk);
        // Trailing '/' (a stray end-italics marker) is dropped.
        if chunk.ends_with('/') {
            chunk.pop();
        }

        markup.push_str("<span");
        if italic {
            markup.push_str(" style=\"italic\"");
        }
        if bold {
            markup.push_str(" weight=\"bold\"");
        }
        if fontsize != 0 {
            markup.push_str(" size=\"");
            push_u64(fontsize.saturating_mul(1000), &mut markup);
            markup.push('"');
        }
        markup.push('>');
        markup.push_str(&chunk);
        markup.push_str("</span>");

        match next {
            Some(off) => {
                markup.push('\n');
                rest = &rest[off..];
            }
            None => break,
        }
    }

    Some(markup)
}

/// GLib `g_markup_escape_text` semantics (as shipped in GLib 2.x): named
/// references for the five XML metacharacters and `&#x<hex>;` for the control
/// characters GLib escapes, which are the C0 set plus most of C1 (`0x7f..=0x84`
/// and `0x86..=0x9f`, so `0x85` is *not* escaped). Other multi-byte UTF-8 passes
/// through untouched.
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

fn push_u64(mut v: u64, out: &mut String) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = 0;
    while v > 0 {
        buf[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
    }
    for &d in buf[..n].iter().rev() {
        out.push(d as char);
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
        MicroDvd::default()
            .parse(body, &ParseContext::default())
            .unwrap()
    }

    fn parse_fps_ctx(body: &str, fps: (u32, u32)) -> Vec<Cue> {
        MicroDvd::default()
            .parse(body, &ParseContext { fps: Some(fps) })
            .unwrap()
    }

    // --- parity with subparse.c: test_microdvd_with_italics --------------

    #[test]
    fn c_parity_italics() {
        let body = "{1}{1}25.000 movie info: XVID  608x256 25.0fps 699.0 MB|\
/SubEdit b.4060(http://subedit.com.pl)/\n\
{100}{200}/italics/|not italics\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 4 * S);
        assert_eq!(cues[0].end_ns, Some(8 * S));
        assert_eq!(
            cues[0].text,
            "<span style=\"italic\">italics</span>\n<span>not italics</span>"
        );
    }

    // --- parity with subparse.c: test_microdvd_with_fps ------------------

    #[test]
    fn c_parity_fps_dot() {
        let body = "{1}{1}12.500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 8 * S);
        assert_eq!(cues[0].end_ns, Some(16 * S));
        assert_eq!(
            cues[0].text,
            "<span>- Hi, Eddie.</span>\n<span>- Hiya, Scotty.</span>"
        );
    }

    #[test]
    fn c_parity_fps_comma_separator() {
        // ',' is accepted as the decimal separator, same value.
        let body = "{1}{1}12,500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 8 * S);
        assert_eq!(cues[0].end_ns, Some(16 * S));
    }

    #[test]
    fn c_parity_apostrophe_escaped() {
        let body = "{1}{1}12.500\n\
{1250}{1350}- Cold enough for you?|- Well, I'm only faintly alive. It's 25 below\n";
        let cues = parse(body);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 100 * S);
        assert_eq!(cues[0].end_ns, Some(108 * S));
        assert_eq!(
            cues[0].text,
            "<span>- Cold enough for you?</span>\n\
<span>- Well, I&apos;m only faintly alive. It&apos;s 25 below</span>"
        );
    }

    // --- fps handling ----------------------------------------------------

    #[test]
    fn default_fps_is_24000_1001() {
        // No header, no ctx fps: uses the upstream default 24000/1001 (~23.976
        // fps), so 24000 frames span exactly 1001 seconds.
        let cues = parse("{24000}{48000}hi\n");
        // start = 24000 * 1e9 * 1001 / 24000 = 1001 s
        assert_eq!(cues[0].start_ns, 1001 * S);
        // end = start + (48000-24000)*1e9*1001/24000 = 2002 s
        assert_eq!(cues[0].end_ns, Some(2002 * S));
    }

    #[test]
    fn ctx_fps_used_when_no_header() {
        // 30/1 fps: frame 30 -> 1 s, frame 60 -> 2 s.
        let cues = parse_fps_ctx("{30}{60}hi\n", (30, 1));
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn header_overrides_ctx_fps() {
        // ctx says 30/1, but the file header says 10 fps -> frame 10 = 1 s.
        let cues = parse_fps_ctx("{1}{1}10.000\n{10}{20}hi\n", (30, 1));
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn one_one_line_is_never_a_cue() {
        // Even without a valid rate, `{1}{1}...` yields no cue.
        let cues = parse("{1}{1}not a number\n{25}{50}hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>hi</span>");
    }

    #[test]
    fn out_of_range_fps_is_ignored() {
        // 5000 fps is out of [0.001, 1000]. The default fps stays in effect.
        let cues = parse_fps_ctx("{1}{1}5000\n{30}{60}hi\n", (30, 1));
        assert_eq!(cues[0].start_ns, S);
    }

    #[test]
    fn c_parity_fps_exponent() {
        // g_ascii_strtod reads `1e2` as 100.0, so frame 100 is 1 s. Without
        // exponent support the header would be read as 1 fps, a 100x error.
        let cues = parse("{1}{1}1e2\n{100}{200}hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));

        // Same rate written with a fraction and a negative exponent.
        let cues = parse("{1}{1}1000.0e-1\n{100}{200}hi\n");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));

        // A trailing `e` with no digits is not an exponent: strtod stops before
        // it, leaving 25 fps.
        let cues = parse("{1}{1}25e\n{25}{50}hi\n");
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn c_parity_fps_trailing_zeros() {
        // `25.000` is 25/1 exactly (the reduced rational), so frame 25 is 1 s.
        let cues = parse("{1}{1}25.000\n{25}{50}hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
    }

    #[test]
    fn huge_frame_numbers_saturate() {
        // `500000000000 * 1e9 * 1001 / 24000` is past u64::MAX, where
        // gst_util_uint64_scale returns G_MAXUINT64. Truncating the 128-bit
        // product instead would wrap it to a small, plausible timestamp.
        let cues = parse("{500000000000}{500000000001}x\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, u64::MAX);
        assert_eq!(cues[0].end_ns, Some(u64::MAX));
    }

    #[test]
    fn c_parity_junk_after_the_second_frame_number() {
        // sscanf("{%u}{%u}") has completed both assignments before the
        // trailing '}' literal fails, and the text starts after the
        // strchr'd second '}', so the junk is skipped, not fatal.
        let cues = parse("{1}{1}25.000\n{25}{50 x}hello\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
        assert_eq!(cues[0].text, "<span>hello</span>");
    }

    #[test]
    fn a_line_with_no_second_closing_brace_is_dropped() {
        // The C also reads two frame numbers here (sscanf returns 2), but
        // then walks off a NULL strchr result, which is undefined behavior.
        // Dropping the line is the only sane reading.
        let cues = parse("{1}{1}25.000\n{25}{50\n{75}{100}ok\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>ok</span>");
    }

    // --- inline style codes ---------------------------------------------

    #[test]
    fn bold_and_italic_and_size() {
        let cues = parse("{1}{1}25.000\n{25}{50}{y:i}{y:b}{s:20}Hi\n");
        assert_eq!(
            cues[0].text,
            "<span style=\"italic\" weight=\"bold\" size=\"20000\">Hi</span>"
        );
    }

    #[test]
    fn size_without_closing_brace_drops_line() {
        let cues = parse("{1}{1}25.000\n{25}{50}{s:20 broken\n");
        assert!(cues.is_empty());
    }

    #[test]
    fn c_parity_size_with_space_before_the_digits() {
        // `sscanf("{s:%u}")` skips whitespace before the digits, so this is a
        // size code, not literal text.
        let cues = parse("{1}{1}25.000\n{25}{50}{s: 20}Hi\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span size=\"20000\">Hi</span>");
    }

    #[test]
    fn trailing_slash_removed_without_leading_slash() {
        // A trailing '/' is stripped even when the chunk is not marked italic
        // by a leading '/'.
        let cues = parse("{1}{1}25.000\n{25}{50}plain/\n");
        assert_eq!(cues[0].text, "<span>plain</span>");
    }

    // --- escaping & structure -------------------------------------------

    #[test]
    fn escapes_markup_metacharacters() {
        let cues = parse("{1}{1}25.000\n{25}{50}a<b>&\"c\n");
        assert_eq!(cues[0].text, "<span>a&lt;b&gt;&amp;&quot;c</span>");
    }

    #[test]
    fn escapes_c1_controls_like_glib() {
        // g_markup_escape_text escapes 0x7f..=0x84 and 0x86..=0x9f as well as
        // the C0 range, but leaves 0x85 alone.
        let cues = parse("{1}{1}25.000\n{25}{50}a\u{80}b\u{85}c\u{9f}d\u{7f}\n");
        assert_eq!(cues[0].text, "<span>a&#x80;b\u{85}c&#x9f;d&#x7f;</span>");
    }

    #[test]
    fn multiline_pipe_split() {
        let cues = parse("{1}{1}25.000\n{25}{50}one|two|three\n");
        assert_eq!(
            cues[0].text,
            "<span>one</span>\n<span>two</span>\n<span>three</span>"
        );
    }

    #[test]
    fn utf8_passes_through() {
        let cues = parse("{1}{1}25.000\n{25}{50}café 日本語\n");
        assert_eq!(cues[0].text, "<span>café 日本語</span>");
    }

    // --- lenient recovery ------------------------------------------------

    #[test]
    fn non_microdvd_lines_are_skipped() {
        let cues = parse("this is not microdvd\n{1}{1}25.000\n{25}{50}ok\ngarbage\n");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>ok</span>");
    }

    #[test]
    fn unterminated_final_line_is_dropped() {
        // MicroDVD is not one of the formats the element flushes at EOS, so a
        // last line without its newline never reaches the parser. The line
        // before it, being terminated, does.
        let cues = parse("{1}{1}25.000\n{25}{50}kept\n{75}{100}dropped");
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "<span>kept</span>");

        // With the newline, the same line is a cue.
        let cues = parse("{1}{1}25.000\n{25}{50}kept\n{75}{100}dropped\n");
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[1].text, "<span>dropped</span>");
    }

    #[test]
    fn empty_body_yields_no_cues() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(
            MicroDvd::default().output_format(),
            OutputFormat::PangoMarkup
        );
    }
}
