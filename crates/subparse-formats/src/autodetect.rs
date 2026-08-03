// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Subtitle format auto-detection.
//!
//! Mirrors `gst_sub_parse_data_format_autodetect` /
//! `gst_sub_parse_data_format_autodetect_regex_once` in the C
//! `gstsubparseelement.c`. The heuristic order and match rules MUST match the C
//! for drop-in compatibility. Downstream negotiation relies on the same format
//! being detected for the same bytes.
//!
//! The C code uses `GRegex` (PCRE) for the MicroDVD / SubRip / DKS / WebVTT
//! probes and `sscanf`/`strstr`/`strncmp` for the rest. This module ports the
//! *equivalent* matching by hand (no regex crate). Each `matches_*` helper is
//! annotated with the exact C construct it reproduces.
//!
//! Detection order (identical to the C `if`-cascade, and authoritative):
//!
//! 1. MicroDVD  `^\{[0-9]+\}\{[0-9]+\}`
//! 2. SubRip    `^[\s\n]*[\n]? {0,3}[ 0-9]{1,4}\s*(\x0d)?\x0a …HH:MM:SS[,.]ms --> …`
//! 3. DKS       `^\[[0-9]+:[0-9]+:[0-9]+\].*`
//! 4. WebVTT    `^(\xef\xbb\xbf)?WEBVTT[\xa\xd\x20\x9]`
//! 5. MPSub     `strncmp(str, "FORMAT=TIME", 11) == 0`
//! 6. SAMI      `strstr(str, "<SAMI>")` || `strstr(str, "<sami>")`
//! 7. TMPlayer  `sscanf` `0:%02u:%02u:` / `…=` / `00:%02u:%02u:` / `…=` / `00:%02u:%02u,%u=`
//! 8. MPL2      `sscanf(str, "[%u][%u]") == 2`
//! 9. SubViewer `strstr(str, "[INFORMATION]")`
//! 10. QTtext   `strstr(str, "{QTtext}")`
//! 11. LRC      `str[0] == '['` and every line but the last is "LRC-good"

use crate::format::Format;

/// Sniff the subtitle format from the start of the decoded body.
///
/// `body` is the UTF-8 text `gst_sub_parse_gst_convert_to_utf8` produces (which
/// strips a leading UTF-8 BOM), i.e. exactly what the C `_autodetect` is handed.
/// We strip a stray leading BOM here too so the byte-level probes below line up
/// with the C. Line terminators are *not* normalized: nothing removes a `\r`
/// before detection, in the C or here, and the probes are written for that.
///
/// One consequence is worth naming, because it looks like a bug and is not: in a
/// CRLF `.lrc` file every line ends in `\r`, which fails the LRC per-line check
/// (`lrc_line_good` wants the line to end in `]`), so such a file is not
/// detected as LRC. The C does the same. Parity wins over the nicer answer here,
/// since the detected media type is what downstream negotiates on.
///
/// How much of the body to hand over is the caller's business, and the C has two
/// different answers: the element sniffs the first 35 bytes once at least 6 have
/// arrived (`g_strndup (self->textbuf->str, 35)`), while the typefinder peeks up
/// to 128. `detect` sniffs whatever it is given.
///
/// Returns `None` when nothing matches (`GST_SUB_PARSE_FORMAT_UNKNOWN`).
// With no detectable format enabled the body/`b` bindings and the `body`
// parameter go unused; silence that only in that (empty) configuration.
#[cfg_attr(
    not(any(
        feature = "microdvd",
        feature = "subrip",
        feature = "dks",
        feature = "webvtt",
        feature = "mpsub",
        feature = "sami",
        feature = "tmplayer",
        feature = "mpl2",
        feature = "subviewer",
        feature = "qttext",
        feature = "lrc",
    )),
    allow(unused_variables)
)]
pub fn detect(body: &str) -> Option<Format> {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    // Only the byte-oriented probes need `b`; the SAMI/SubViewer/QTtext probes
    // work on `&str`. Gate it so it is never an unused binding.
    #[cfg(any(
        feature = "microdvd",
        feature = "subrip",
        feature = "dks",
        feature = "webvtt",
        feature = "mpsub",
        feature = "tmplayer",
        feature = "mpl2",
        feature = "lrc",
    ))]
    let b = body.as_bytes();

    #[cfg(feature = "microdvd")]
    if matches_microdvd(b) {
        return Some(Format::MicroDvd);
    }
    #[cfg(feature = "subrip")]
    if matches_subrip(b) {
        return Some(Format::SubRip);
    }
    #[cfg(feature = "dks")]
    if matches_dks(b) {
        return Some(Format::Dks);
    }
    #[cfg(feature = "webvtt")]
    if matches_webvtt(b) {
        return Some(Format::WebVtt);
    }
    #[cfg(feature = "mpsub")]
    if matches_mpsub(b) {
        return Some(Format::MpSub);
    }
    #[cfg(feature = "sami")]
    if matches_sami(body) {
        return Some(Format::Sami);
    }
    #[cfg(feature = "tmplayer")]
    if matches_tmplayer(b) {
        return Some(Format::TmPlayer);
    }
    #[cfg(feature = "mpl2")]
    if matches_mpl2(b) {
        return Some(Format::Mpl2);
    }
    #[cfg(feature = "subviewer")]
    if matches_subviewer(body) {
        return Some(Format::SubViewer);
    }
    #[cfg(feature = "qttext")]
    if matches_qttext(body) {
        return Some(Format::QtText);
    }
    #[cfg(feature = "lrc")]
    if matches_lrc(b) {
        return Some(Format::Lrc);
    }
    None
}

// -------------------------------------------------------------------------
// Low-level byte-cursor helpers
//
// Every helper advances `*i` on a successful match and leaves it unchanged on
// failure, so they compose with `&&` (short-circuit = a sequence point, so the
// shared `&mut i` is reborrowed safely between calls).
// -------------------------------------------------------------------------

/// PCRE `\s` (and C `isspace`): the ASCII whitespace set.
#[cfg(any(
    feature = "subrip",
    feature = "tmplayer",
    feature = "mpl2",
    feature = "lrc"
))]
#[inline]
fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `[0-9]{min,max}`. Consume up to `max` digits, fail if fewer than `min`.
#[cfg(any(feature = "microdvd", feature = "subrip", feature = "dks"))]
fn take_digits(b: &[u8], i: &mut usize, min: usize, max: usize) -> bool {
    let start = *i;
    let mut n = 0;
    while n < max && *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
        n += 1;
    }
    if n < min {
        *i = start;
        false
    } else {
        true
    }
}

/// ` {min,max}`. Consume up to `max` ASCII spaces, fail if fewer than `min`.
#[cfg(feature = "subrip")]
fn take_spaces(b: &[u8], i: &mut usize, min: usize, max: usize) -> bool {
    let start = *i;
    let mut n = 0;
    while n < max && *i < b.len() && b[*i] == b' ' {
        *i += 1;
        n += 1;
    }
    if n < min {
        *i = start;
        false
    } else {
        true
    }
}

/// ` +`. One or more ASCII spaces.
#[cfg(feature = "subrip")]
fn take_spaces_plus(b: &[u8], i: &mut usize) -> bool {
    take_spaces(b, i, 1, usize::MAX)
}

/// ` ?`. An optional single ASCII space (never fails).
#[cfg(feature = "subrip")]
fn opt_space(b: &[u8], i: &mut usize) {
    if *i < b.len() && b[*i] == b' ' {
        *i += 1;
    }
}

/// A single literal byte.
#[cfg(any(
    feature = "microdvd",
    feature = "subrip",
    feature = "dks",
    feature = "tmplayer",
    feature = "mpl2",
    feature = "lrc"
))]
fn lit(b: &[u8], i: &mut usize, ch: u8) -> bool {
    if *i < b.len() && b[*i] == ch {
        *i += 1;
        true
    } else {
        false
    }
}

/// A single byte from `set` (a char class like `[,.]`).
#[cfg(feature = "subrip")]
fn one_of(b: &[u8], i: &mut usize, set: &[u8]) -> bool {
    if *i < b.len() && set.contains(&b[*i]) {
        *i += 1;
        true
    } else {
        false
    }
}

/// A literal byte string.
#[cfg(feature = "subrip")]
fn lit_str(b: &[u8], i: &mut usize, s: &[u8]) -> bool {
    if *i <= b.len() && b[*i..].starts_with(s) {
        *i += s.len();
        true
    } else {
        false
    }
}

/// C `%u` / `%Nu`: skip leading ASCII whitespace, then read `1..=max` digits.
/// `max == None` is the width-less `%u` (unbounded). Returns whether at least
/// one digit was consumed (i.e. the conversion succeeded).
///
/// `sign` says whether to also accept the leading `+`/`-` that C's `%u` takes
/// (it is `strtoul`-based, so `[+123]` does scan). Only the MPL2 probe passes
/// `true`, because only the MPL2 parser accepts a sign as well. The TMPlayer and
/// LRC probes stay unsigned so that they keep agreeing with *their* parsers:
/// detecting a format whose parser then yields nothing is the worse failure of
/// the two. It makes those two probes narrower than the C for a signed
/// timestamp field, which no real file has (noted in `specs/autodetect.md`).
///
/// A consumed sign counts against `max`, as scanf's field width counts every
/// character of the input item. (Only the width-less `%u` is used with `sign`
/// today, so that arm is there for correctness rather than for a caller.)
#[cfg(any(feature = "tmplayer", feature = "mpl2", feature = "lrc"))]
fn scan_uint(b: &[u8], i: &mut usize, max: Option<usize>, sign: bool) -> bool {
    let start = *i;
    while *i < b.len() && is_ws(b[*i]) {
        *i += 1;
    }
    let cap = max.unwrap_or(usize::MAX);
    let mut n = 0;
    if sign && n < cap && matches!(b.get(*i), Some(b'+' | b'-')) {
        *i += 1;
        n += 1;
    }
    let mut digits = 0;
    while n < cap && *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
        n += 1;
        digits += 1;
    }
    if digits == 0 {
        *i = start;
        false
    } else {
        true
    }
}

// -------------------------------------------------------------------------
// Per-format probes (order matches `detect`)
// -------------------------------------------------------------------------

/// MicroDVD: `^\{[0-9]+\}\{[0-9]+\}`
#[cfg(feature = "microdvd")]
fn matches_microdvd(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'{')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b'}')
        && lit(b, &mut i, b'{')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b'}')
}

/// SubRip:
/// `^[\s\n]*[\n]? {0,3}[ 0-9]{1,4}\s*(\x0d)?\x0a` followed by a timestamp line.
///
/// The whole preamble is anchored at offset 0 and consists only of
/// whitespace/digit bytes, ending at the mandatory `\x0a`. We enumerate each
/// candidate `\n` as that `\x0a`: the prefix before it must satisfy the
/// preamble, and the timestamp pattern must match immediately after. This is
/// the hand-rolled equivalent of the regex engine's greedy `\s*` + backtrack.
#[cfg(feature = "subrip")]
fn matches_subrip(b: &[u8]) -> bool {
    let mut i = 0;
    loop {
        if i < b.len() && b[i] == b'\n' && preamble_ok(&b[..i]) && srt_timestamp(b, i + 1) {
            return true;
        }
        if i >= b.len() {
            return false;
        }
        // The preamble is whitespace/digits only. The first other byte ends it.
        if !(is_ws(b[i]) || b[i].is_ascii_digit()) {
            return false;
        }
        i += 1;
    }
}

/// The preamble minus the trailing `\x0a`, i.e.
/// `[\s\n]*[\n]? {0,3}[ 0-9]{1,4}\s*(\x0d)?`. Reduces to:
/// `whitespace* · <1..=4 space/digit> · whitespace*`, with every digit inside
/// the middle block (the surrounding `\s*` cannot match digits).
#[cfg(feature = "subrip")]
fn preamble_ok(r: &[u8]) -> bool {
    if !r.iter().all(|&c| is_ws(c) || c.is_ascii_digit()) {
        return false;
    }
    let n = r.len();
    for s in 0..n {
        // Everything before the `[ 0-9]{1,4}` block must be whitespace. Once a
        // non-whitespace byte (a digit) is behind `s`, no later `s` can work.
        if !r[..s].iter().all(|&c| is_ws(c)) {
            break;
        }
        for len in 1..=4 {
            let end = s + len;
            if end > n {
                break;
            }
            let block = &r[s..end];
            if block.iter().all(|&c| c == b' ' || c.is_ascii_digit())
                && r[end..].iter().all(|&c| is_ws(c))
            {
                return true;
            }
        }
    }
    false
}

/// The SubRip timestamp line, deterministic (every variable run is followed
/// by a distinct-class byte, so greedy matching needs no backtracking):
/// ` ?[0-9]{1,2}: ?[0-9]{1,2}: ?[0-9]{1,2}[,.] {0,2}[0-9]{1,3}`
/// ` +--> +`
/// `[0-9]{1,2}: ?[0-9]{1,2}: ?[0-9]{1,2}[,.] {0,2}[0-9]{1,2}`
#[cfg(feature = "subrip")]
fn srt_timestamp(b: &[u8], mut i: usize) -> bool {
    // start time
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !one_of(b, &mut i, b",.") {
        return false;
    }
    take_spaces(b, &mut i, 0, 2);
    if !take_digits(b, &mut i, 1, 3) {
        return false;
    }
    // arrow
    if !take_spaces_plus(b, &mut i) || !lit_str(b, &mut i, b"-->") || !take_spaces_plus(b, &mut i) {
        return false;
    }
    // end time
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !lit(b, &mut i, b':') {
        return false;
    }
    opt_space(b, &mut i);
    if !take_digits(b, &mut i, 1, 2) || !one_of(b, &mut i, b",.") {
        return false;
    }
    take_spaces(b, &mut i, 0, 2);
    take_digits(b, &mut i, 1, 2)
}

/// DKS: `^\[[0-9]+:[0-9]+:[0-9]+\].*` (the `.*` tail is unconditional).
#[cfg(feature = "dks")]
fn matches_dks(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'[')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b':')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b':')
        && take_digits(b, &mut i, 1, usize::MAX)
        && lit(b, &mut i, b']')
}

/// WebVTT: `^(\xef\xbb\xbf)?WEBVTT[\xa\xd\x20\x9]`.
/// The BOM is normally stripped before we get here, but honored anyway per the regex.
#[cfg(feature = "webvtt")]
fn matches_webvtt(b: &[u8]) -> bool {
    let mut i = 0;
    if b[i..].starts_with(&[0xEF, 0xBB, 0xBF]) {
        i += 3;
    }
    if !b[i..].starts_with(b"WEBVTT") {
        return false;
    }
    i += 6;
    matches!(b.get(i), Some(&(b'\n' | b'\r' | b' ' | b'\t')))
}

/// MPSub: `strncmp(str, "FORMAT=TIME", 11) == 0`.
#[cfg(feature = "mpsub")]
fn matches_mpsub(b: &[u8]) -> bool {
    b.starts_with(b"FORMAT=TIME")
}

/// SAMI: contains `<SAMI>` or `<sami>` (case-sensitive, exactly as the C
/// `strstr` pair, no mixed case).
#[cfg(feature = "sami")]
fn matches_sami(body: &str) -> bool {
    body.contains("<SAMI>") || body.contains("<sami>")
}

/// TMPlayer: any of the C `sscanf` probes succeeding.
///
/// The C tries `0:%02u:%02u:`, `0:%02u:%02u=`, `00:%02u:%02u:`, `00:%02u:%02u=`
/// and `00:%02u:%02u,%u=`. `sscanf`'s return value is the number of *assigned*
/// conversions, so any literal *after* the last `%u` (the trailing `:`/`=`) is
/// irrelevant to the `== 2` / `== 3` test. That collapses the five formats to
/// three distinct checks (and the `,%u=` one is already subsumed by the plain
/// `00:` check, matching the C's dead-branch quirk).
#[cfg(feature = "tmplayer")]
fn matches_tmplayer(b: &[u8]) -> bool {
    tmp_hour_1(b) || tmp_hour_2(b) || tmp_hour_2_frac(b)
}

/// `0:%02u:%02u` (covers `0:%02u:%02u:` and `0:%02u:%02u=`).
#[cfg(feature = "tmplayer")]
fn tmp_hour_1(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'0')
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
}

/// `00:%02u:%02u` (covers `00:%02u:%02u:` and `00:%02u:%02u=`).
#[cfg(feature = "tmplayer")]
fn tmp_hour_2(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'0')
        && lit(b, &mut i, b'0')
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
}

/// `00:%02u:%02u,%u` (the `== 3` probe `00:%02u:%02u,%u=`).
#[cfg(feature = "tmplayer")]
fn tmp_hour_2_frac(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'0')
        && lit(b, &mut i, b'0')
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b':')
        && scan_uint(b, &mut i, Some(2), false)
        && lit(b, &mut i, b',')
        && scan_uint(b, &mut i, None, false)
}

/// MPL2: `sscanf(str, "[%u][%u]") == 2`.
///
/// The `]` closing the second bracket comes after the last conversion, so it is
/// irrelevant to the count (`[123][456 x]` detects), and both `%u` take the sign
/// `strtoul` allows (`[+123][456]` detects). The MPL2 parser accepts both too.
#[cfg(feature = "mpl2")]
fn matches_mpl2(b: &[u8]) -> bool {
    let mut i = 0;
    lit(b, &mut i, b'[')
        && scan_uint(b, &mut i, None, true)
        && lit(b, &mut i, b']')
        && lit(b, &mut i, b'[')
        && scan_uint(b, &mut i, None, true)
}

/// SubViewer: contains `[INFORMATION]`.
#[cfg(feature = "subviewer")]
fn matches_subviewer(body: &str) -> bool {
    body.contains("[INFORMATION]")
}

/// QTtext: contains `{QTtext}`.
#[cfg(feature = "qttext")]
fn matches_qttext(body: &str) -> bool {
    body.contains("{QTtext}")
}

/// LRC: `str[0] == '['` and every line *except the last* is LRC-good.
///
/// The C loops with `while (*ptr && *(ptr+1))` over a `g_strsplit(str, "\n")`,
/// which visits every element but the final one. `all_lines_good` starts `TRUE`,
/// so a single-line `[`-prefixed body already qualifies.
#[cfg(feature = "lrc")]
fn matches_lrc(b: &[u8]) -> bool {
    if b.first() != Some(&b'[') {
        return false;
    }
    let mut lines = b.split(|&c| c == b'\n').peekable();
    while let Some(line) = lines.next() {
        if lines.peek().is_none() {
            break; // skip the final split element
        }
        if !lrc_line_good(line) {
            return false;
        }
    }
    true
}

/// A single LRC line is "good" if it starts with an LRC timestamp
/// (`[%u:%02u.%02u]` or `[%u:%02u.%03u]`), or is non-empty, ends with `]`, and
/// contains a `:` (metadata like `[ar:Artist]`).
#[cfg(feature = "lrc")]
fn lrc_line_good(line: &[u8]) -> bool {
    if lrc_timestamp(line, 2) || lrc_timestamp(line, 3) {
        return true;
    }
    matches!(line.last(), Some(&b']')) && line.contains(&b':')
}

/// `[%u:%02u.%0{ms}u]`. The trailing `]` is after the third `%u`, so it does
/// not affect the C's `== 3` test.
#[cfg(feature = "lrc")]
fn lrc_timestamp(line: &[u8], ms_width: usize) -> bool {
    let mut i = 0;
    lit(line, &mut i, b'[')
        && scan_uint(line, &mut i, None, false)
        && lit(line, &mut i, b':')
        && scan_uint(line, &mut i, Some(2), false)
        && lit(line, &mut i, b'.')
        && scan_uint(line, &mut i, Some(ms_width), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // One positive detection per format (snippets derived from the C
    // typefind/detect logic and tests/check/elements/subparse.c).
    // ------------------------------------------------------------------

    #[cfg(feature = "microdvd")]
    #[test]
    fn detect_microdvd() {
        // subparse.c microdvd_input
        assert_eq!(
            detect("{1}{1}12.500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n"),
            Some(Format::MicroDvd)
        );
        assert_eq!(
            detect("{100}{200}/italics/|not italics\n"),
            Some(Format::MicroDvd)
        );
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn detect_subrip() {
        // subparse.c srt_input[0]
        assert_eq!(
            detect("1\n00:00:01,000 --> 00:00:02,000\nOne\n\n"),
            Some(Format::SubRip)
        );
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn detect_subrip_with_loose_spacing() {
        // subparse.c whitespace-tolerant timestamps
        assert_eq!(
            detect("1\n 0: 0:26, 26 --> 0: 0:28, 17\nI cant see.\n\n"),
            Some(Format::SubRip)
        );
        assert_eq!(
            detect("2\n00:00:03, 9 --> 00:00:04,0   \nThree\n\n"),
            Some(Format::SubRip)
        );
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn detect_subrip_with_leading_blank_lines() {
        // `[\s\n]*` swallows leading blank lines before the index.
        assert_eq!(
            detect("\n\n42\n00:00:01,000 --> 00:00:02,000\nHi\n"),
            Some(Format::SubRip)
        );
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn detect_subrip_blank_line_between_index_and_timestamp() {
        // `\s*` before `\x0a` allows a blank line after the index.
        assert_eq!(
            detect("1\n\n00:00:01,000 --> 00:00:02,000\nOne\n"),
            Some(Format::SubRip)
        );
    }

    #[cfg(feature = "dks")]
    #[test]
    fn detect_dks() {
        // subparse.c dks_input
        assert_eq!(
            detect("[00:00:07]THERE IS A PLACE ON EARTH[br]...\n[00:00:12]\n"),
            Some(Format::Dks)
        );
    }

    #[cfg(feature = "webvtt")]
    #[test]
    fn detect_webvtt() {
        // subparse.c prepends "WEBVTT FILE\n" to every vtt case
        assert_eq!(
            detect("WEBVTT FILE\n\n00:00:00.000 --> 00:00:02.000\nHi\n"),
            Some(Format::WebVtt)
        );
    }

    #[cfg(feature = "webvtt")]
    #[test]
    fn detect_webvtt_bare_header() {
        assert_eq!(detect("WEBVTT\n\nfoo"), Some(Format::WebVtt));
        assert_eq!(detect("WEBVTT\t"), Some(Format::WebVtt));
    }

    #[cfg(feature = "webvtt")]
    #[test]
    fn detect_webvtt_with_bom() {
        // Element normally strips the BOM. Regex tolerates it either way.
        assert_eq!(detect("\u{feff}WEBVTT\n"), Some(Format::WebVtt));
    }

    #[cfg(feature = "mpsub")]
    #[test]
    fn detect_mpsub() {
        // Marker: `strncmp(str, "FORMAT=TIME", 11)`
        assert_eq!(
            detect("FORMAT=TIME\n\n0.00 3.00\nHello world\n"),
            Some(Format::MpSub)
        );
    }

    #[cfg(feature = "sami")]
    #[test]
    fn detect_sami_uppercase() {
        // subparse.c sami_input
        assert_eq!(
            detect("<SAMI>\n<BODY>\n<SYNC Start=1000>Hi</SYNC>\n</BODY>\n</SAMI>\n"),
            Some(Format::Sami)
        );
    }

    #[cfg(feature = "sami")]
    #[test]
    fn detect_sami_lowercase() {
        assert_eq!(
            detect("<sami>\n<body></body>\n</sami>\n"),
            Some(Format::Sami)
        );
    }

    #[cfg(feature = "tmplayer")]
    #[test]
    fn detect_tmplayer_colon_style() {
        // subparse.c tmplayer_style1 (00:%02u:%02u:)
        assert_eq!(
            detect("00:00:10:This is the Earth|when...\n00:00:13:\n"),
            Some(Format::TmPlayer)
        );
    }

    #[cfg(feature = "tmplayer")]
    #[test]
    fn detect_tmplayer_equals_style() {
        // tmplayer_style2 (00:%02u:%02u=) and multiline (00:%02u:%02u,%u=)
        assert_eq!(
            detect("00:00:10=This is the Earth|when...\n00:00:13=\n"),
            Some(Format::TmPlayer)
        );
        assert_eq!(
            detect("00:00:10,1=This is the Earth at a time\n00:00:13,1=\n"),
            Some(Format::TmPlayer)
        );
    }

    #[cfg(feature = "tmplayer")]
    #[test]
    fn detect_tmplayer_short_hour() {
        // tmplayer_style3 (0:%02u:%02u:)
        assert_eq!(
            detect("0:00:10:This is the Earth|when...\n0:00:13:\n"),
            Some(Format::TmPlayer)
        );
    }

    #[cfg(feature = "mpl2")]
    #[test]
    fn detect_mpl2() {
        // subparse.c mpl2_input
        assert_eq!(
            detect("[123][456] This is the Earth at a time|when...\n"),
            Some(Format::Mpl2)
        );
    }

    #[cfg(feature = "mpl2")]
    #[test]
    fn detect_mpl2_sscanf_count_leniency() {
        // `sscanf("[%u][%u]")` has assigned both numbers before it reaches the
        // final `]`, so junk in its place still returns 2.
        assert_eq!(detect("[123][456 x]y\n"), Some(Format::Mpl2));
        // `%u` is strtoul-based, so a sign scans.
        assert_eq!(detect("[+123][456]y\n"), Some(Format::Mpl2));
        // The `]` between the two conversions is required, though.
        assert_eq!(detect("[123 x][456]y\n"), None);
    }

    #[cfg(feature = "subviewer")]
    #[test]
    fn detect_subviewer() {
        // subparse.c subviewer_input
        assert_eq!(
            detect("[INFORMATION]\n[TITLE]xxx\n[END INFORMATION]\n00:00:41.00,00:00:44.40\nHi\n"),
            Some(Format::SubViewer)
        );
    }

    #[cfg(feature = "qttext")]
    #[test]
    fn detect_qttext() {
        // Marker: `strstr(str, "{QTtext}")`
        assert_eq!(
            detect("{QTtext}{timeScale:100}\n[00:00:00.00]\nHello\n"),
            Some(Format::QtText)
        );
    }

    #[cfg(feature = "lrc")]
    #[test]
    fn detect_lrc() {
        // subparse.c lrc_input
        assert_eq!(
            detect("[ar:123]\n[ti:Title]\n[al:Album]\n[00:02.23]Line 1\n"),
            Some(Format::Lrc)
        );
    }

    #[cfg(feature = "lrc")]
    #[test]
    fn detect_lrc_timestamps_only() {
        assert_eq!(
            detect("[00:12.34]Lyric one\n[00:15.00]Lyric two\n"),
            Some(Format::Lrc)
        );
        // 3-digit fractional form.
        assert_eq!(detect("[00:06.123]Line 3\n[00:08.00]\n"), Some(Format::Lrc));
    }

    // ------------------------------------------------------------------
    // Negative cases (unrecognized -> None).
    // ------------------------------------------------------------------

    #[test]
    fn reject_empty() {
        assert_eq!(detect(""), None);
    }

    #[test]
    fn reject_plain_text() {
        assert_eq!(
            detect("Just some random text\nnot a subtitle at all\n"),
            None
        );
        assert_eq!(detect("Hello world"), None);
    }

    #[test]
    fn reject_incomplete_microdvd() {
        // Needs both `{n}{n}` groups.
        assert_eq!(detect("{1}foo\n"), None);
        assert_eq!(detect("{1}\n"), None);
    }

    #[test]
    fn reject_subrip_without_arrow() {
        // An index line + a lone timestamp is not SubRip (no `-->`), and nothing
        // else claims it.
        assert_eq!(detect("1\n00:00:01,000\nOne\n"), None);
    }

    #[test]
    fn reject_webvtt_without_separator() {
        // `WEBVTT` must be followed by \n, \r, space or tab.
        assert_eq!(detect("WEBVTTX\n"), None);
        assert_eq!(detect("WEBVTT"), None);
    }

    // ------------------------------------------------------------------
    // Ambiguous / ordering cases. Several probes could fire, but the earliest
    // in the C cascade must win.
    // ------------------------------------------------------------------

    #[cfg(feature = "qttext")]
    #[test]
    fn qttext_not_microdvd() {
        // `{QTtext}` starts with `{` but MicroDVD needs `{[0-9]+}`.
        assert_eq!(detect("{QTtext}foo\n"), Some(Format::QtText));
    }

    #[cfg(feature = "dks")]
    #[test]
    fn dks_beats_lrc_and_subviewer() {
        // `[00:00:07]…` starts with `[` (LRC-ish) but DKS is probed first.
        assert_eq!(detect("[00:00:07]hello\n[00:00:12]\n"), Some(Format::Dks));
    }

    #[cfg(feature = "subviewer")]
    #[test]
    fn subviewer_beats_lrc() {
        // `[INFORMATION]` starts with `[` but SubViewer precedes LRC.
        assert_eq!(detect("[INFORMATION]\n[TITLE]x\n"), Some(Format::SubViewer));
    }

    #[cfg(feature = "mpl2")]
    #[test]
    fn mpl2_beats_dks_and_lrc() {
        // `[123][456]` is not DKS (`h:m:s`) and MPL2 precedes LRC.
        assert_eq!(detect("[123][456]text\n"), Some(Format::Mpl2));
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn subrip_beats_tmplayer() {
        // A well-formed SubRip cue also contains `00:00:0x` runs, but SubRip is
        // probed long before TMPlayer.
        assert_eq!(
            detect("1\n00:00:01,000 --> 00:00:02,000\nOne\n"),
            Some(Format::SubRip)
        );
    }

    #[cfg(feature = "lrc")]
    #[test]
    fn lrc_is_last_resort_for_bracket_prefix() {
        // A `[`-prefixed body that matches no earlier probe falls through to LRC
        // (single line -> loop body never runs -> "all good").
        assert_eq!(detect("[whatever:x]"), Some(Format::Lrc));
    }
}
