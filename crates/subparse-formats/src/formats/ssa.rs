// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! SSA / ASS ("SubStation Alpha" / "Advanced SubStation Alpha") parser.
//!
//! See `specs/ssa.md`. The C reference is
//! `gst-plugins-base/gst/subparse/gstssaparse.c`, the standalone `ssaparse`
//! element. That element only **extracts text**. It strips `{...}` styling
//! override blocks, translates the SSA newline/hard-space escapes, and emits
//! Pango-escaped plain text. It does **not** render ASS styling. We reproduce
//! that text extraction byte-for-byte (see [`strip_to_pango_markup`] and
//! [`dialogue_to_pango_markup`]) and, on top of it, parse whole `.ass`/`.ssa`
//! file bodies (the `[Events]` section) into timed cues via [`Ssa::parse`].
//!
//! The two entry points below are deliberately public so the GStreamer
//! `ssaparse` element reuses the exact same text transform per buffer.

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};
use crate::format::{LineScanner, Parsed, SubtitleFormat};
use crate::ssastyle::{SsaDialogue, SsaStyles};

/// Parser for the SSA/ASS subtitle format (whole-file `[Events]` parsing).
///
/// Streaming: a `Dialogue:` line is a complete record on its own, so cues come
/// out per line. The state carried between calls is the section the scan is in
/// and the column layout the `[Events]` `Format:` header declared, both of
/// which are read once near the top of the file and then apply to every
/// following line.
#[derive(Debug)]
pub struct Ssa {
    lines: LineScanner,
    machine: Machine,
    bom_checked: bool,
}

impl Default for Ssa {
    fn default() -> Self {
        Ssa {
            lines: LineScanner::new(),
            machine: Machine::default(),
            bom_checked: false,
        }
    }
}

impl SubtitleFormat for Ssa {
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
            // The whole-body parser iterated `str::lines()`, which yields a
            // final unterminated line but *not* an empty one after a trailing
            // newline, and which does not strip a lone trailing '\r'.
            let tail = &body[consumed..];
            if !tail.is_empty() {
                machine.feed(tail, &mut cues);
            }
            consumed = body.len();
            // The flushed tail bypassed the scanner, so its scan offset no
            // longer matches the (now empty) remainder. Feeding after EOS is
            // a contract violation, but a stale offset must not make the
            // scanner merge two later lines into one.
            lines.reset();
        }

        Ok(Parsed { cues, consumed })
    }

    fn output_format(&self) -> OutputFormat {
        // gstssaparse.c advertises `text/x-raw, format=pango-markup`.
        OutputFormat::PangoMarkup
    }

    fn ssa_styles(&self) -> Option<&SsaStyles> {
        // Always present for SSA (an empty registry still lets the cue-ir
        // path parse override tags with default styling).
        Some(&self.machine.styles)
    }
}

/// The `[Events]`-section scanner, carried across `parse_incremental` calls.
#[derive(Debug)]
struct Machine {
    in_events: bool,
    /// Column layout of the `Dialogue:` fields. Defaults match the standard
    /// SSA v4 / ASS v4+ event order:
    ///   Marked/Layer, Start, End, Style, Name, MarginL, MarginR, MarginV,
    ///   Effect, Text
    /// A `Format:` line inside `[Events]` overrides these, resolving `Start`,
    /// `End` and `Text` by name. `Text` is the last column in well-formed
    /// files (it may itself contain commas).
    col_start: usize,
    col_end: usize,
    /// Index of the `Text` column, or `None` when a `Format:` header declared
    /// no `Text` column at all (Dialogue lines are then dropped, there is no
    /// text to emit).
    col_text: Option<usize>,
    /// Columns feeding the cue's [`SsaDialogue`] (styling for the `cue-ir`
    /// path). `None` when a `Format:` line omits them; the dialogue then
    /// carries defaults, which the text extraction never notices.
    col_style: Option<usize>,
    col_margin_l: Option<usize>,
    col_margin_r: Option<usize>,
    col_margin_v: Option<usize>,
    /// `[Script Info]` + `[V4(+) Styles]` collected on the side for the
    /// `cue-ir` path. Feeding it every line is parity-neutral: it only reads
    /// sections the text extraction ignores.
    styles: SsaStyles,
}

impl Default for Machine {
    fn default() -> Self {
        Machine {
            in_events: false,
            col_start: 1,
            col_end: 2,
            col_text: Some(9),
            col_style: Some(3),
            col_margin_l: Some(5),
            col_margin_r: Some(6),
            col_margin_v: Some(7),
            styles: SsaStyles::default(),
        }
    }
}

impl Machine {
    /// Feed one line (terminator and any `\r\n` already removed).
    fn feed(&mut self, line: &str, cues: &mut Vec<Cue>) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            return;
        }

        self.styles.feed_line(trimmed);

        // Section header, e.g. "[Events]", "[Script Info]". The name ends at
        // the first ']' and anything after it (a stray comment, say) is
        // ignored. A line missing the ']' still counts, with the name running
        // to the end of the line.
        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = match rest.split_once(']') {
                Some((name, _)) => name,
                None => rest.trim_end(),
            };
            self.in_events = name.eq_ignore_ascii_case("events");
            return;
        }

        if !self.in_events {
            return;
        }

        let Some((keyword, value)) = split_keyword(trimmed) else {
            return;
        };

        if keyword.eq_ignore_ascii_case("Format") {
            // Resolve every column we need by name. A trailing comma on the
            // Format line yields a final empty name, which harmlessly matches
            // nothing. A header that never names Text leaves `col_text` unset
            // and every following Dialogue line is dropped.
            self.col_text = None;
            self.col_style = None;
            self.col_margin_l = None;
            self.col_margin_r = None;
            self.col_margin_v = None;
            for (i, n) in value.split(',').map(str::trim).enumerate() {
                if n.eq_ignore_ascii_case("Start") {
                    self.col_start = i;
                } else if n.eq_ignore_ascii_case("End") {
                    self.col_end = i;
                } else if n.eq_ignore_ascii_case("Text") {
                    self.col_text = Some(i);
                } else if n.eq_ignore_ascii_case("Style") {
                    self.col_style = Some(i);
                } else if n.eq_ignore_ascii_case("MarginL") {
                    self.col_margin_l = Some(i);
                } else if n.eq_ignore_ascii_case("MarginR") {
                    self.col_margin_r = Some(i);
                } else if n.eq_ignore_ascii_case("MarginV") {
                    self.col_margin_v = Some(i);
                }
            }
        } else if keyword.eq_ignore_ascii_case("Dialogue")
            && let Some(cue) = self.parse_dialogue(value)
        {
            cues.push(cue);
        }
        // "Comment:" and any other keyword inside [Events] are ignored.
    }

    fn parse_dialogue(&self, value: &str) -> Option<Cue> {
        let col_text = self.col_text?;
        // Keep at most `col_text + 1` fields so the Text field retains its
        // commas. Text is the last column in a well-formed header, making
        // that the declared column count. The spec asserts Text is always
        // last, so when a header declares it earlier anyway we keep the same
        // reading: the text starts at its declared field index and runs to
        // the end of the line. Columns declared after Text are then
        // unreachable, and a Dialogue whose Start or End sits past Text is
        // dropped by the lookups below.
        let fields: Vec<&str> = value.splitn(col_text + 1, ',').collect();
        if fields.len() < col_text + 1 {
            return None; // missing fields, skip like the C's lenient recovery
        }
        let start = parse_ass_time(fields.get(self.col_start)?.trim())?;
        let end = parse_ass_time(fields.get(self.col_end)?.trim())?;
        let raw_text = fields[col_text];
        let text = strip_to_pango_markup(raw_text);

        let field = |col: Option<usize>| col.and_then(|i| fields.get(i)).map(|f| f.trim());
        let margin =
            |col: Option<usize>| field(col).and_then(|f| f.parse::<u32>().ok()).unwrap_or(0);
        let mut cue = Cue::new(start, Some(end), text);
        cue.ssa = Some(Box::new(SsaDialogue {
            raw_text: raw_text.to_owned(),
            style: field(self.col_style).unwrap_or_default().to_owned(),
            margin_l: margin(self.col_margin_l),
            margin_r: margin(self.col_margin_r),
            margin_v: margin(self.col_margin_v),
        }));
        Some(cue)
    }
}

/// Convert a single container-framed SSA/ASS dialogue line to Pango markup.
///
/// Byte-for-byte port of the `ssaparse` element's per-buffer path
/// (`gst_ssa_parse_push_line`). The framed buffers a demuxer hands to `ssaparse`
/// carry the fields `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,
/// Text`, so the text is reached by walking past **8 commas** and everything
/// after (commas intact) is the Text field. Returns `None` when the line has
/// fewer than 8 commas. The C code returns `GST_FLOW_ERROR` and emits nothing.
///
/// This is the entry point the GStreamer `ssaparse` element calls per buffer
/// when the caps carry `codec_data`, i.e. for container-embedded SSA/ASS.
pub fn dialogue_to_pango_markup(line: &str) -> Option<String> {
    let text_field = skip_commas(line, 8)?;
    Some(strip_to_pango_markup(text_field))
}

/// Extract the [`SsaDialogue`] extras from the same container-framed row
/// layout [`dialogue_to_pango_markup`] walks (`ReadOrder,Layer,Style,Name,
/// MarginL,MarginR,MarginV,Effect,Text`). Returns `None` when the line has
/// fewer than 8 commas, exactly when `dialogue_to_pango_markup` does. Only
/// the `cue-ir` output path calls this.
pub fn framed_dialogue(line: &str) -> Option<SsaDialogue> {
    let fields: Vec<&str> = line.splitn(9, ',').collect();
    if fields.len() < 9 {
        return None;
    }
    Some(SsaDialogue {
        raw_text: fields[8].to_owned(),
        style: fields[2].trim().to_owned(),
        margin_l: fields[4].trim().parse().unwrap_or(0),
        margin_r: fields[5].trim().parse().unwrap_or(0),
        margin_v: fields[6].trim().parse().unwrap_or(0),
    })
}

/// Turn a raw SSA/ASS *Text field* into Pango markup, exactly as the C
/// `ssaparse` element does.
///
/// Steps (mirroring `gst_ssa_parse_remove_override_codes` then
/// `g_markup_printf_escaped("%s", ...)`):
///
/// 1. Remove every `{...}` style-override block. On an **unmatched** `{`,
///    removal stops there and the remainder is kept verbatim. Matching the
///    C early `return`, the newline/hard-space escapes below are **not** applied.
/// 2. Otherwise translate the wrapping escapes. `\N` and `\n` become `" \n"`
///    (a space then a newline, this leading space is a C quirk we preserve),
///    and `\h` becomes `"  "` (two spaces).
/// 3. GLib/Pango-escape the result (`&`,`<`,`>`,`'`,`"` and C0/C1 controls).
pub fn strip_to_pango_markup(text_field: &str) -> String {
    let (stripped, translate) = remove_override_blocks(text_field);
    let mut out = String::with_capacity(stripped.len() + 8);
    if translate {
        translate_and_escape(&stripped, &mut out);
    } else {
        for ch in stripped.chars() {
            escape_char(ch, &mut out);
        }
    }
    out
}

/// Remove `{...}` override blocks. Returns the stripped text and whether the
/// caller should still apply the `\N`/`\n`/`\h` translation. An unmatched `{`
/// causes the C code to `return` before translating, so we report `false`.
fn remove_override_blocks(text: &str) -> (String, bool) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    // '{' (0x7B) and '}' (0x7D) are ASCII and never appear as UTF-8
    // continuation bytes, so byte scanning is safe on multi-byte text.
    while let Some(rel) = find_byte(&bytes[i..], b'{') {
        let open = i + rel;
        match find_byte(&bytes[open..], b'}') {
            Some(erel) => {
                let close = open + erel;
                out.push_str(&text[i..open]);
                i = close + 1; // drop '{' .. '}' inclusive
            }
            None => {
                // Missing '}'. Keep the rest verbatim, skip escape translation.
                out.push_str(&text[i..]);
                return (out, false);
            }
        }
    }
    out.push_str(&text[i..]);
    (out, true)
}

/// Single left-to-right pass that translates `\N`/`\n`/`\h` and Pango-escapes
/// everything else. Equivalent to the C's three global `strstr` passes because
/// each escape consumes its backslash + letter and never produces a new
/// backslash.
fn translate_and_escape(s: &str, out: &mut String) {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'n' | b'N' => {
                    out.push(' ');
                    out.push('\n');
                    i += 2;
                    continue;
                }
                b'h' => {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    continue;
                }
                _ => {
                    out.push('\\');
                    i += 1;
                    continue;
                }
            }
        }
        // `i` is always on a char boundary (we only step by whole ASCII bytes
        // above or by a full char's length below).
        let ch = s[i..].chars().next().unwrap();
        escape_char(ch, out);
        i += ch.len_utf8();
    }
}

/// GLib `g_markup_escape_text` semantics for a single char. GLib uses the
/// named references `&apos;` / `&quot;` for the quotes and escapes C0/C1
/// control characters as `&#x<hex>;`.
fn escape_char(c: char, out: &mut String) {
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
                push_control_ref(u, out);
            } else {
                out.push(c);
            }
        }
    }
}

/// Append `&#x<hex>;` for a control codepoint, lowercase hex, no allocation.
fn push_control_ref(u: u32, out: &mut String) {
    out.push_str("&#x");
    let mut started = false;
    let mut shift = 28;
    loop {
        let nibble = (u >> shift) & 0xf;
        if nibble != 0 || started || shift == 0 {
            started = true;
            out.push(char::from_digit(nibble, 16).unwrap());
        }
        if shift == 0 {
            break;
        }
        shift -= 4;
    }
    out.push(';');
}

fn find_byte(hay: &[u8], b: u8) -> Option<usize> {
    hay.iter().position(|&x| x == b)
}

/// Skip past `n` commas, returning the remaining suffix (or `None` if there are
/// fewer than `n` commas).
fn skip_commas(s: &str, n: usize) -> Option<&str> {
    let mut rest = s;
    for _ in 0..n {
        let idx = rest.find(',')?;
        rest = &rest[idx + 1..];
    }
    Some(rest)
}

/// Split a "Keyword: value" line on the first colon.
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(':')?;
    let key = line[..idx].trim();
    // ASS writes a single space after the colon ("Dialogue: 0,..."). Strip only
    // leading spaces so field[0] parsing is robust. Text-field spaces live past
    // the commas and are untouched.
    let val = line[idx + 1..].trim_start_matches(' ');
    Some((key, val))
}

/// Parse an SSA/ASS timestamp `H:MM:SS.cc` (hours 1+ digits, centiseconds by
/// convention but any fractional precision is accepted) into nanoseconds.
fn parse_ass_time(s: &str) -> Option<u64> {
    let mut parts = s.splitn(3, ':');
    let h: u64 = parts.next()?.trim().parse().ok()?;
    let m: u64 = parts.next()?.trim().parse().ok()?;
    let sec_field = parts.next()?.trim();
    if m >= 60 {
        return None;
    }
    let (sec_str, frac_str) = match sec_field.split_once('.') {
        Some((a, b)) => (a, b),
        None => (sec_field, ""),
    };
    let sec: u64 = sec_str.parse().ok()?;
    if sec >= 60 {
        return None;
    }
    let frac_ns = frac_to_ns(frac_str);
    // The hour field is not capped, so an absurd value must saturate rather
    // than overflow-panic. The trailing fraction add can overflow on its own
    // even when the whole-second product still fits.
    Some(
        h.saturating_mul(3600)
            .saturating_add(m.saturating_mul(60))
            .saturating_add(sec)
            .saturating_mul(1_000_000_000)
            .saturating_add(frac_ns),
    )
}

/// Interpret a decimal fraction string as nanoseconds. Right-pad/truncate the
/// digits to 9 places. E.g. "5" -> 500_000_000, "50" -> 500_000_000,
/// "05" -> 50_000_000.
fn frac_to_ns(frac: &str) -> u64 {
    let mut buf = [b'0'; 9];
    for (i, b) in frac.bytes().take(9).enumerate() {
        if b.is_ascii_digit() {
            buf[i] = b;
        } else {
            break;
        }
    }
    // buf is always 9 ASCII digits.
    std::str::from_utf8(&buf).unwrap().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    // --- strip_to_pango_markup: override blocks --------------------------

    #[test]
    fn strips_single_override_block() {
        assert_eq!(strip_to_pango_markup("{\\i1}Hello{\\i0}"), "Hello");
    }

    #[test]
    fn strips_multiple_override_blocks() {
        assert_eq!(strip_to_pango_markup("a{\\b1}b{\\b0}c"), "abc");
    }

    #[test]
    fn strips_leading_and_trailing_blocks() {
        assert_eq!(strip_to_pango_markup("{\\an8}text"), "text");
        assert_eq!(strip_to_pango_markup("text{\\r}"), "text");
    }

    #[test]
    fn empty_override_block_removed() {
        assert_eq!(strip_to_pango_markup("a{}b"), "ab");
    }

    #[test]
    fn stray_close_brace_before_open_is_kept() {
        // strchr('}') starts at the '{', so a '}' before it is left in place.
        assert_eq!(strip_to_pango_markup("a}b{\\i1}c"), "a}bc");
    }

    // --- unmatched brace: escapes are NOT translated ---------------------

    #[test]
    fn unmatched_brace_keeps_remainder_and_skips_escapes() {
        // The C returns early. "\N" stays literal and the '{' + rest are kept.
        assert_eq!(
            strip_to_pango_markup("a{\\i1 no close \\Nb"),
            "a{\\i1 no close \\Nb"
        );
    }

    #[test]
    fn unmatched_brace_after_a_valid_block() {
        // First block removed. Then unmatched '{' stops removal, escapes skipped.
        assert_eq!(
            strip_to_pango_markup("{\\b1}ok{oops\\Ntail"),
            "ok{oops\\Ntail"
        );
    }

    // --- newline / hard-space escapes ------------------------------------

    #[test]
    fn newline_escape_uppercase() {
        assert_eq!(strip_to_pango_markup("Line1\\NLine2"), "Line1 \nLine2");
    }

    #[test]
    fn newline_escape_lowercase() {
        assert_eq!(strip_to_pango_markup("Line1\\nLine2"), "Line1 \nLine2");
    }

    #[test]
    fn hard_space_escape() {
        assert_eq!(strip_to_pango_markup("a\\hb"), "a  b");
    }

    #[test]
    fn leading_newline_escape_has_space_before_newline() {
        assert_eq!(strip_to_pango_markup("\\Nx"), " \nx");
    }

    #[test]
    fn unknown_backslash_escape_kept_verbatim() {
        // Only \N \n \h are translated. Other backslash sequences pass through.
        assert_eq!(strip_to_pango_markup("a\\db"), "a\\db");
    }

    #[test]
    fn backslash_at_end_kept() {
        assert_eq!(strip_to_pango_markup("abc\\"), "abc\\");
    }

    #[test]
    fn double_backslash_before_n() {
        // "\\N" -> emit '\', then translate the second "\N".
        assert_eq!(strip_to_pango_markup("\\\\N"), "\\ \n");
    }

    // --- markup escaping (GLib g_markup_escape_text semantics) -----------

    #[test]
    fn escapes_angle_brackets_and_amp() {
        assert_eq!(strip_to_pango_markup("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn escapes_quotes_with_named_refs() {
        assert_eq!(strip_to_pango_markup("'\""), "&apos;&quot;");
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(strip_to_pango_markup("\u{7}"), "&#x7;");
        assert_eq!(strip_to_pango_markup("\u{7f}"), "&#x7f;");
    }

    #[test]
    fn tab_and_newline_pass_through() {
        // 0x09 and 0x0a are not in GLib's escaped control ranges.
        assert_eq!(strip_to_pango_markup("a\tb"), "a\tb");
    }

    #[test]
    fn combined_override_escape_and_markup() {
        // Remove {\i1}, translate \N, then escape the angle brackets & amp.
        assert_eq!(
            strip_to_pango_markup("{\\i1}<i>x & y</i>\\Nz"),
            "&lt;i&gt;x &amp; y&lt;/i&gt; \nz"
        );
    }

    #[test]
    fn utf8_text_preserved() {
        assert_eq!(strip_to_pango_markup("café — 日本語"), "café — 日本語");
    }

    // --- dialogue_to_pango_markup: container-framed per-line path --------

    #[test]
    fn dialogue_extracts_text_after_eight_commas() {
        // ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
        let line = "0,0,Default,,0,0,0,,{\\i1}Hello world{\\i0}";
        assert_eq!(dialogue_to_pango_markup(line).unwrap(), "Hello world");
    }

    #[test]
    fn dialogue_preserves_commas_in_text() {
        let line = "1,0,Default,,0,0,0,,Hello, world, again";
        assert_eq!(
            dialogue_to_pango_markup(line).unwrap(),
            "Hello, world, again"
        );
    }

    #[test]
    fn dialogue_translates_newline_in_text() {
        let line = "0,0,Default,,0,0,0,,First\\NSecond";
        assert_eq!(dialogue_to_pango_markup(line).unwrap(), "First \nSecond");
    }

    #[test]
    fn dialogue_missing_fields_returns_none() {
        assert_eq!(dialogue_to_pango_markup("0,0,Default"), None);
        assert_eq!(dialogue_to_pango_markup(""), None);
    }

    #[test]
    fn dialogue_exactly_eight_commas_empty_text() {
        // 8 commas, empty text field -> Some("").
        assert_eq!(dialogue_to_pango_markup("0,0,d,,0,0,0,,").unwrap(), "");
    }

    // --- whole-file [Events] parsing -------------------------------------

    #[test]
    fn parses_events_with_format_line() {
        let body = "\
[Script Info]
Title: Test

[V4+ Styles]
Format: Name, Fontname, Fontsize
Style: Default,Arial,20

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello {\\i1}world{\\i0}
Dialogue: 0,0:00:03.50,0:00:05.00,Default,,0,0,0,,Second\\Nline
Comment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,ignored
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(3 * S));
        assert_eq!(cues[0].text, "Hello world");
        assert_eq!(cues[1].start_ns, 3 * S + 500_000_000);
        assert_eq!(cues[1].end_ns, Some(5 * S));
        assert_eq!(cues[1].text, "Second \nline");
    }

    #[test]
    fn styles_format_line_does_not_affect_columns() {
        // The [V4+ Styles] "Format:" (3 cols) must be ignored. Only the
        // [Events] one counts. Without honoring the section this would break.
        let body = "\
[V4+ Styles]
Format: Name, Fontname, Fontsize
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,ok
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 2 * S);
        assert_eq!(cues[0].text, "ok");
    }

    #[test]
    fn parses_events_without_format_line_uses_defaults() {
        let body = "\
[Events]
Dialogue: 0,0:00:02.00,0:00:04.00,Default,,0,0,0,,Text here
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 2 * S);
        assert_eq!(cues[0].end_ns, Some(4 * S));
        assert_eq!(cues[0].text, "Text here");
    }

    #[test]
    fn dialogue_before_events_section_is_ignored() {
        let body = "\
[Script Info]
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,nope
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn ssa_v4_hmmss_hundredths_parses() {
        // SSA v4 uses "Marked=" for the first field. Timing is H:MM:SS.cc.
        let body = "\
[Events]
Format: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: Marked=0,0:00:00.05,0:00:10.00,Default,,0000,0000,0000,,Hi
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, 50_000_000); // 0.05 s
        assert_eq!(cues[0].end_ns, Some(10 * S));
    }

    #[test]
    fn malformed_time_line_is_skipped() {
        let body = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,not-a-time,0:00:03.00,Default,,0,0,0,,bad
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,good
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "good");
    }

    #[test]
    fn dialogue_missing_field_in_file_is_skipped() {
        let body = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Default
Dialogue: 0,0:00:04.00,0:00:06.00,Default,,0,0,0,,fine
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "fine");
    }

    // --- styles collection and SsaDialogue (the cue-ir side channel) --------

    #[test]
    fn styles_and_dialogue_extras_are_collected() {
        let body = "\
[Script Info]
PlayResX: 640
PlayResY: 480

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, Alignment
Style: Top,Arial,32,&H0000FFFF,8

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:01.00,0:00:03.00,Top,,12,0,48,,{\\i1}Hello
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        // Parity: the pango text is still the stripped form.
        assert_eq!(cues[0].text, "Hello");

        let d = cues[0].ssa.as_deref().expect("dialogue extras attached");
        assert_eq!(d.raw_text, "{\\i1}Hello");
        assert_eq!(d.style, "Top");
        assert_eq!((d.margin_l, d.margin_r, d.margin_v), (12, 0, 48));

        let styles = p.ssa_styles().expect("SSA always exposes a registry");
        assert_eq!(styles.play_res(), (640.0, 480.0));
        let style = styles.lookup("Top").expect("style collected");
        assert_eq!(style.font_size, Some(32.0));
        assert_eq!(style.alignment, Some(8));
    }

    #[test]
    fn custom_event_format_resolves_style_and_margins() {
        let body = "\
[Events]
Format: Start, End, MarginV, Style, Text
Dialogue: 0:00:01.00,0:00:02.00,24,Alt,Hi there
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        let d = cues[0].ssa.as_deref().unwrap();
        assert_eq!(d.style, "Alt");
        assert_eq!(d.margin_v, 24);
        assert_eq!(d.margin_l, 0);
        assert_eq!(d.raw_text, "Hi there");
    }

    #[test]
    fn non_ssa_body_yields_no_cues() {
        let mut p = Ssa::default();
        let cues = p
            .parse(
                "just some random text\nnot subtitles",
                &ParseContext::default(),
            )
            .unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn output_format_is_pango_markup() {
        assert_eq!(Ssa::default().output_format(), OutputFormat::PangoMarkup);
    }

    #[test]
    fn hours_over_one_digit() {
        assert_eq!(parse_ass_time("10:00:00.00"), Some(10 * 3600 * S));
    }

    // --- timestamp overflow -----------------------------------------------

    #[test]
    fn huge_hour_saturates_and_does_not_panic() {
        // 5124095 is the largest whole hour count that still fits in u64
        // nanoseconds. The hour field is not capped, so one hour more must
        // saturate rather than overflow (panic under overflow checks).
        assert_eq!(
            parse_ass_time("5124095:00:00.00"),
            Some(5_124_095 * 3600 * S)
        );
        assert_eq!(parse_ass_time("5124096:00:00.00"), Some(u64::MAX));
    }

    #[test]
    fn fraction_add_saturates_and_does_not_panic() {
        // The whole seconds still fit in u64 nanoseconds here, it is adding
        // the fraction that overflows.
        assert_eq!(parse_ass_time("5124095:34:33.9"), Some(u64::MAX));
    }

    // --- Format header Text column resolution ------------------------------

    #[test]
    fn format_without_text_column_drops_dialogue() {
        // No Text column means there is nothing to emit. The timing fields
        // must not leak out as subtitle text.
        let body = "\
[Events]
Format: Start, End
Dialogue: 0:00:01.00,0:00:02.00
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn format_with_text_last_keeps_commas() {
        let body = "\
[Events]
Format: Start, End, Text
Dialogue: 0:00:01.00,0:00:02.00,Hello, world, again
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].start_ns, S);
        assert_eq!(cues[0].end_ns, Some(2 * S));
        assert_eq!(cues[0].text, "Hello, world, again");
    }

    #[test]
    fn format_with_text_not_last_takes_rest_of_line() {
        // The spec says Text is always the last column. When a header
        // declares it earlier anyway, the text starts at its field index and
        // runs to the end of the line (columns past it are unreachable).
        let body = "\
[Events]
Format: Start, End, Text, Effect
Dialogue: 0:00:01.00,0:00:02.00,Hello, world
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello, world");
    }

    #[test]
    fn format_with_timing_after_text_drops_dialogue() {
        // Start/End declared after Text can never be reached (Text swallows
        // the rest of the line), so the line is dropped rather than mistimed.
        let body = "\
[Events]
Format: Text, Start, End
Dialogue: Hello,0:00:01.00,0:00:02.00
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn format_trailing_comma_tolerated() {
        // The trailing comma yields a final empty column name, which must not
        // inflate the expected field count (that used to fail every
        // well-formed Dialogue line in the file).
        let body = "\
[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text,
Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hello");
    }

    // --- leading BOM --------------------------------------------------------

    #[test]
    fn leading_bom_is_stripped() {
        let body = "\u{feff}[Events]\n\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi\n";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    // --- section header leniency --------------------------------------------

    #[test]
    fn section_header_with_trailing_content_is_recognized() {
        let body = "\
[Events] ; a comment
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,Hi
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "Hi");
    }

    #[test]
    fn section_exit_with_trailing_content_is_recognized() {
        // The leniency has to cut both ways: a decorated non-Events header
        // still ends the [Events] section.
        let body = "\
[Events]
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,kept
[Fonts] ; a comment
Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,dropped
";
        let mut p = Ssa::default();
        let cues = p.parse(body, &ParseContext::default()).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "kept");
    }

    // --- scanner state after the EOS tail flush ------------------------------

    #[test]
    fn scanner_resets_after_eos_tail_flush() {
        // Feeding after at_eos violates the streaming contract, but the
        // flushed unterminated tail never went through the LineScanner, and a
        // stale scan offset must not merge two later lines into one.
        let ctx = ParseContext::default();
        let mut p = Ssa::default();
        let head = "[Events]\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,unterminated-tail";
        let parsed = p.parse_incremental(head, &ctx, true).unwrap();
        assert_eq!(parsed.consumed, head.len());
        assert_eq!(parsed.cues.len(), 1);

        let next = "Dialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,a\n\
                    Dialogue: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,b\n";
        let parsed = p.parse_incremental(next, &ctx, false).unwrap();
        assert_eq!(parsed.cues.len(), 2);
        assert_eq!(parsed.cues[0].text, "a");
        assert_eq!(parsed.cues[1].text, "b");
    }
}
