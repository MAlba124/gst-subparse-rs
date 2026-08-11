// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! SSA/ASS styling for the `cue-ir` output path: the `[V4(+) Styles]`
//! registry and the `{\...}` override-tag parser.
//!
//! The C `ssaparse` element (and our parity port in [`crate::formats::ssa`])
//! only **extracts text**: override blocks are stripped, style definitions
//! never read. This module is the styling layer on top, consumed exclusively
//! by `text-format=cue-ir`; the pango-markup output stays byte-identical to
//! the C.
//!
//! * [`SsaStyles`] collects `[Script Info]` (`PlayResX`/`PlayResY`) and the
//!   `[V4 Styles]` / `[V4+ Styles]` sections (`Format:`-driven columns), fed
//!   line by line by the whole-file parser or parsed in one go from a
//!   container's `codec_data` init section.
//! * [`SsaDialogue`] is what one `Dialogue:` event carries beyond its text:
//!   the raw Text field (override blocks intact), the style name, and the
//!   per-event margin overrides. The whole-file parser attaches it to each
//!   [`crate::cue::Cue`]; the framed `ssaparse` element builds it per row.
//! * [`dialogue_to_ir`] resolves the two into a [`CueIr`]: the named style
//!   becomes the cue's base [`SpanStyle`] and [`Layout`] (anchor from the
//!   alignment, margins and `\pos` normalised out of `PlayRes` space into
//!   frame percentages), and the override tags become per-span styling.
//!
//! ## Supported override tags
//!
//! `\i \b \u \s` (empty argument resets to the style), `\fn \fs \fsp
//! \fscx \fscy`, colors `\c \1c \3c \4c` and alphas `\alpha \1a \3a \4a`
//! (SSA alpha is inverted: `00` opaque), `\bord \shad \xshad \yshad`,
//! alignment `\an` (numpad) and `\a` (legacy) — first one wins, like
//! VSFilter — `\pos` / `\move` (start point; first wins), style resets `\r`
//! and `\rName`, karaoke `\k \K \kf \ko` (cumulative centiseconds become
//! [`Span::reveal_ns`]), and drawing mode `\p` (the drawing commands are
//! dropped — they are vector paths, not text). `\t \fad \fade \clip \org
//! \fr* \be \blur \q \fe \2c \2a` parse (their arguments are consumed) and
//! are ignored. Unknown tags are skipped.
//!
//! ## Units
//!
//! SSA positions and sizes live in `PlayRes` pixel space. Positions, margins
//! and font sizes are normalised to frame percentages (font sizes via the
//! [`FontSize::FrameHeightPercent`] variant). Outline widths, shadow offsets
//! and letter spacing map onto the IR's point-denominated fields as `1px ≈
//! 1pt` — an approximation (the IR has no frame-relative unit for them), but
//! one that keeps their relative proportions.

use crate::ir::{
    Anchor, Color, CueIr, FontSize, FontStyle, Layout, Line, Margins, Outline, Shadow, Span,
    SpanStyle, TextAlign,
};

// -- the per-dialogue carrier -------------------------------------------------

/// What one `Dialogue:` event contributes beyond its stripped text: enough to
/// rebuild the styled cue. Attached to [`crate::cue::Cue::ssa`] by the
/// whole-file parser; only the `cue-ir` output path reads it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SsaDialogue {
    /// The Text field verbatim, `{...}` override blocks intact.
    pub raw_text: String,
    /// The Style column: the name of a `[V4(+) Styles]` definition.
    pub style: String,
    /// Margin overrides in `PlayRes` pixels; `0` means "use the style's".
    pub margin_l: u32,
    pub margin_r: u32,
    pub margin_v: u32,
}

// -- the style registry -------------------------------------------------------

/// One `Style:` definition. Every field is optional: `None` means the column
/// was absent, unparsable, or the style itself was never found — the renderer
/// default applies.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Style {
    pub font_family: Option<String>,
    /// Font size in `PlayRes` pixels.
    pub font_size: Option<f32>,
    /// PrimaryColour: the text fill.
    pub primary: Option<Color>,
    /// OutlineColour (v4+) / TertiaryColour (v4): the border stroke.
    pub outline_color: Option<Color>,
    /// BackColour: the shadow color (and the `BorderStyle=3` box fill in
    /// VSFilter lineage renderers uses OutlineColour; we follow that).
    pub back_color: Option<Color>,
    /// CSS-style weight (SSA `Bold` is a boolean, `-1` = bold).
    pub font_weight: Option<u16>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strikeout: Option<bool>,
    /// ScaleX/ScaleY, percent (100 = unscaled).
    pub scale_x: Option<f32>,
    pub scale_y: Option<f32>,
    /// Extra letter spacing, `PlayRes` pixels.
    pub spacing: Option<f32>,
    /// 1 = outline + shadow, 3 = opaque box.
    pub border_style: Option<i32>,
    /// Outline width, `PlayRes` pixels.
    pub outline_w: Option<f32>,
    /// Shadow depth (dx = dy), `PlayRes` pixels.
    pub shadow_d: Option<f32>,
    /// Raw Alignment column (numpad in v4+, legacy encoding in v4).
    pub alignment: Option<i32>,
    /// Margins, `PlayRes` pixels.
    pub margin_l: Option<f32>,
    pub margin_r: Option<f32>,
    pub margin_v: Option<f32>,
}

/// Which section of the script a line belongs to, tracked by
/// [`SsaStyles::feed_line`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    Other,
    ScriptInfo,
    /// `[V4 Styles]` (`v4plus == false`) or `[V4+ Styles]`.
    Styles {
        v4plus: bool,
    },
}

/// The stylable context of one SSA/ASS script: `PlayRes` and the style
/// definitions. Fed line by line (it tracks sections itself, so callers can
/// forward every line) or built from a whole init section with
/// [`SsaStyles::parse`].
#[derive(Debug, Clone, Default)]
pub struct SsaStyles {
    pub play_res_x: Option<f32>,
    pub play_res_y: Option<f32>,
    /// Style definitions in declaration order, with the raw `Name` column.
    styles: Vec<(String, Style)>,
    /// Whether the styles came from a `[V4+ Styles]` section (numpad
    /// alignment) rather than `[V4 Styles]` (legacy alignment).
    v4plus: bool,
    section: Section,
    /// Column layout of `Style:` lines, lowercase, from the section's
    /// `Format:` line or the per-version default.
    format: Vec<String>,
}

/// The default `Format:` columns of a `[V4 Styles]` section (SSA v4).
const V4_FORMAT: &[&str] = &[
    "name",
    "fontname",
    "fontsize",
    "primarycolour",
    "secondarycolour",
    "tertiarycolour",
    "backcolour",
    "bold",
    "italic",
    "borderstyle",
    "outline",
    "shadow",
    "alignment",
    "marginl",
    "marginr",
    "marginv",
    "alphalevel",
    "encoding",
];

/// The default `Format:` columns of a `[V4+ Styles]` section (ASS).
const V4PLUS_FORMAT: &[&str] = &[
    "name",
    "fontname",
    "fontsize",
    "primarycolour",
    "secondarycolour",
    "outlinecolour",
    "backcolour",
    "bold",
    "italic",
    "underline",
    "strikeout",
    "scalex",
    "scaley",
    "spacing",
    "angle",
    "borderstyle",
    "outline",
    "shadow",
    "alignment",
    "marginl",
    "marginr",
    "marginv",
    "encoding",
];

impl SsaStyles {
    /// Parse a whole header/init section (the `codec_data` a demuxer hands
    /// the `ssaparse` element).
    pub fn parse(text: &str) -> SsaStyles {
        let mut s = SsaStyles::default();
        for line in text.lines() {
            s.feed_line(line.strip_suffix('\r').unwrap_or(line));
        }
        s
    }

    /// Whether anything usable was collected.
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty() && self.play_res_x.is_none() && self.play_res_y.is_none()
    }

    /// The styles in declaration order.
    pub fn styles(&self) -> impl Iterator<Item = (&str, &Style)> {
        self.styles.iter().map(|(n, s)| (n.as_str(), s))
    }

    /// Feed one line of the script. Tracks `[Section]` headers itself and
    /// ignores everything outside `[Script Info]` and the styles sections,
    /// so the whole-file parser can forward every line unconditionally.
    pub fn feed_line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = match rest.split_once(']') {
                Some((name, _)) => name,
                None => rest.trim_end(),
            };
            self.section = if name.eq_ignore_ascii_case("script info") {
                Section::ScriptInfo
            } else if name.eq_ignore_ascii_case("v4 styles") {
                self.enter_styles(false);
                Section::Styles { v4plus: false }
            } else if name.eq_ignore_ascii_case("v4+ styles")
                || name.eq_ignore_ascii_case("v4++ styles")
            {
                self.enter_styles(true);
                Section::Styles { v4plus: true }
            } else {
                Section::Other
            };
            return;
        }

        let Some((keyword, value)) = split_keyword(trimmed) else {
            return;
        };
        match self.section {
            Section::ScriptInfo => {
                if keyword.eq_ignore_ascii_case("PlayResX") {
                    self.play_res_x = value.trim().parse::<f32>().ok().filter(|v| *v > 0.0);
                } else if keyword.eq_ignore_ascii_case("PlayResY") {
                    self.play_res_y = value.trim().parse::<f32>().ok().filter(|v| *v > 0.0);
                }
            }
            Section::Styles { v4plus } => {
                if keyword.eq_ignore_ascii_case("Format") {
                    self.format = value
                        .split(',')
                        .map(|n| n.trim().to_ascii_lowercase())
                        .collect();
                } else if keyword.eq_ignore_ascii_case("Style") {
                    self.push_style(value, v4plus);
                }
            }
            Section::Other => {}
        }
    }

    fn enter_styles(&mut self, v4plus: bool) {
        self.v4plus = v4plus;
        let default = if v4plus { V4PLUS_FORMAT } else { V4_FORMAT };
        self.format = default.iter().map(|s| (*s).to_owned()).collect();
    }

    fn push_style(&mut self, value: &str, v4plus: bool) {
        let mut name = String::new();
        let mut style = Style::default();
        for (col, field) in self.format.iter().zip(value.split(',')) {
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            match col.as_str() {
                "name" => name = field.to_owned(),
                "fontname" => style.font_family = Some(field.to_owned()),
                "fontsize" => style.font_size = field.parse().ok(),
                "primarycolour" => style.primary = parse_ssa_color(field),
                "outlinecolour" | "tertiarycolour" => style.outline_color = parse_ssa_color(field),
                "backcolour" => style.back_color = parse_ssa_color(field),
                "bold" => style.font_weight = parse_bold(field),
                "italic" => style.italic = parse_ssa_bool(field),
                "underline" => style.underline = parse_ssa_bool(field),
                "strikeout" => style.strikeout = parse_ssa_bool(field),
                "scalex" => style.scale_x = field.parse().ok(),
                "scaley" => style.scale_y = field.parse().ok(),
                "spacing" => style.spacing = field.parse().ok(),
                "borderstyle" => style.border_style = field.parse().ok(),
                "outline" => style.outline_w = field.parse().ok(),
                "shadow" => style.shadow_d = field.parse().ok(),
                "alignment" => {
                    style.alignment = field
                        .parse::<i32>()
                        .ok()
                        .and_then(|a| if v4plus { Some(a) } else { legacy_to_numpad(a) })
                }
                "marginl" => style.margin_l = field.parse().ok(),
                "marginr" => style.margin_r = field.parse().ok(),
                "marginv" => style.margin_v = field.parse().ok(),
                _ => {}
            }
        }
        if !name.is_empty() {
            self.styles.push((name, style));
        }
    }

    /// Look up a style by name: exact, then ASCII case-insensitive, both with
    /// leading `*`s stripped (the VSFilter `*Default` quirk).
    pub fn lookup(&self, name: &str) -> Option<&Style> {
        let wanted = name.trim_start_matches('*');
        self.styles
            .iter()
            .find(|(n, _)| n.trim_start_matches('*') == wanted)
            .or_else(|| {
                self.styles
                    .iter()
                    .find(|(n, _)| n.trim_start_matches('*').eq_ignore_ascii_case(wanted))
            })
            .map(|(_, s)| s)
    }

    /// The effective script resolution: declared `PlayResX`/`PlayResY`, one
    /// derived 4:3 from the other, or the historical 384x288 default.
    pub fn play_res(&self) -> (f32, f32) {
        match (self.play_res_x, self.play_res_y) {
            (Some(x), Some(y)) => (x, y),
            (Some(x), None) => (x, x * 3.0 / 4.0),
            (None, Some(y)) => (y * 4.0 / 3.0, y),
            (None, None) => (384.0, 288.0),
        }
    }
}

/// Split a "Keyword: value" line on the first colon.
fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let idx = line.find(':')?;
    Some((line[..idx].trim(), line[idx + 1..].trim_start_matches(' ')))
}

// -- SSA scalars ---------------------------------------------------------------

/// Parse an SSA/ASS color: `&HAABBGGRR&` hex (`&` suffix and the alpha byte
/// both optional) or a (possibly negative) decimal integer with the same
/// byte layout. SSA alpha is inverted (`00` = opaque), ours is straight.
pub fn parse_ssa_color(s: &str) -> Option<Color> {
    let v = parse_ssa_scalar(s)?;
    Some(Color {
        r: (v & 0xff) as u8,
        g: ((v >> 8) & 0xff) as u8,
        b: ((v >> 16) & 0xff) as u8,
        a: 255 - ((v >> 24) & 0xff) as u8,
    })
}

/// An SSA alpha byte (`&H<AA>&` or decimal), converted from SSA's inverted
/// convention to a straight alpha.
fn parse_ssa_alpha(s: &str) -> Option<u8> {
    let v = parse_ssa_scalar(s)?;
    Some(255 - (v & 0xff) as u8)
}

/// The `&H...&` / decimal integer scalar both of the above share. Values wrap
/// to 32 bits like the C parsers these files were written against.
fn parse_ssa_scalar(s: &str) -> Option<u32> {
    let t = s.trim().trim_end_matches('&');
    if let Some(hex) = t
        .strip_prefix("&H")
        .or_else(|| t.strip_prefix("&h"))
        .or_else(|| t.strip_prefix("H"))
        .or_else(|| t.strip_prefix("h"))
    {
        return u64::from_str_radix(hex, 16).ok().map(|v| v as u32);
    }
    t.parse::<i64>().ok().map(|v| v as u32)
}

/// SSA booleans: `0` false, anything else (canonically `-1`) true.
fn parse_ssa_bool(s: &str) -> Option<bool> {
    s.parse::<i32>().ok().map(|v| v != 0)
}

/// The `Bold` column / `\b` tag: a boolean or an explicit weight.
fn parse_bold(s: &str) -> Option<u16> {
    let v = s.parse::<i32>().ok()?;
    Some(match v {
        0 => 400,
        -1 | 1 => 700,
        100..=1000 => v as u16,
        _ => 700,
    })
}

/// Numpad alignment (ASS `\an`, v4+ `Alignment`) to the IR anchor + the text
/// alignment it implies. Also used by the raw-SRT path in [`crate::ir`]
/// (`{\anN}` blocks in SubRip text).
pub(crate) fn numpad_anchor(n: i32) -> Option<(Anchor, TextAlign)> {
    Some(match n {
        1 => (Anchor::BottomLeft, TextAlign::Left),
        2 => (Anchor::BottomCenter, TextAlign::Center),
        3 => (Anchor::BottomRight, TextAlign::Right),
        4 => (Anchor::CenterLeft, TextAlign::Left),
        5 => (Anchor::Center, TextAlign::Center),
        6 => (Anchor::CenterRight, TextAlign::Right),
        7 => (Anchor::TopLeft, TextAlign::Left),
        8 => (Anchor::TopCenter, TextAlign::Center),
        9 => (Anchor::TopRight, TextAlign::Right),
        _ => return None,
    })
}

/// Legacy alignment (SSA v4 `Alignment`, `\a`) to numpad: 1-3 bottom,
/// +4 = top ("toptitle"), +8 = middle ("midtitle").
pub(crate) fn legacy_to_numpad(a: i32) -> Option<i32> {
    Some(match a {
        1..=3 => a,
        5..=7 => a + 2,
        9..=11 => a - 5,
        _ => return None,
    })
}

// -- style -> IR base ------------------------------------------------------------

/// Default outline width (px≈pt) when a tag needs one and neither the style
/// nor an earlier override set it (VSFilter's default `Outline` is 2).
const DEFAULT_OUTLINE_W: f32 = 2.0;

/// The character-level part of a style, as a [`SpanStyle`] delta.
fn style_span(style: &Style, res_y: f32) -> SpanStyle {
    let mut s = SpanStyle {
        font_family: style.font_family.clone(),
        font_style: style.italic.map(|i| {
            if i {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            }
        }),
        font_weight: style.font_weight,
        underline: style.underline,
        strikethrough: style.strikeout,
        foreground: style.primary,
        letter_spacing: style.spacing.filter(|v| *v != 0.0),
        ..SpanStyle::default()
    };
    if let Some(size) = style.font_size.filter(|v| *v > 0.0) {
        s.font_size = Some(FontSize::FrameHeightPercent(size / res_y * 100.0));
    }
    // BorderStyle 3 draws an opaque box instead of an outline; the box itself
    // is cue-level (see `style_layout`).
    if style.border_style != Some(3) {
        let width = style.outline_w.unwrap_or(DEFAULT_OUTLINE_W);
        if width > 0.0 && (style.outline_color.is_some() || style.outline_w.is_some()) {
            s.outline = Some(Outline {
                color: style.outline_color.unwrap_or(Color::BLACK),
                width,
            });
        }
        if let Some(d) = style.shadow_d.filter(|d| *d > 0.0) {
            s.shadow = Some(Shadow {
                color: style.back_color.unwrap_or(Color::rgba(0, 0, 0, 128)),
                dx: d,
                dy: d,
                blur: 0.0,
            });
        }
    }
    match (style.scale_x, style.scale_y) {
        (None, None) => {}
        (sx, sy) => {
            let (sx, sy) = (sx.unwrap_or(100.0) / 100.0, sy.unwrap_or(100.0) / 100.0);
            if (sx, sy) != (1.0, 1.0) {
                s.scale = Some((sx, sy));
            }
        }
    }
    s
}

/// The cue-level part of a style: anchor, margins, opaque box.
fn style_layout(style: &Style, d: &SsaDialogue, res: (f32, f32)) -> Layout {
    let mut layout = Layout::default();
    if let Some((anchor, align)) = style.alignment.and_then(numpad_anchor) {
        layout.anchor = Some(anchor);
        layout.align = Some(align);
    }

    // Dialogue margins override the style's when non-zero.
    let margin = |over: u32, style_m: Option<f32>| -> Option<f32> {
        if over > 0 {
            Some(over as f32)
        } else {
            style_m.filter(|m| *m > 0.0)
        }
    };
    let (res_x, res_y) = res;
    let l = margin(d.margin_l, style.margin_l).map(|m| m / res_x * 100.0);
    let r = margin(d.margin_r, style.margin_r).map(|m| m / res_x * 100.0);
    let v = margin(d.margin_v, style.margin_v).map(|m| m / res_y * 100.0);
    if l.is_some() || r.is_some() || v.is_some() {
        layout.margins = Some(Margins {
            left: l.unwrap_or(0.0),
            right: r.unwrap_or(0.0),
            vertical: v.unwrap_or(0.0),
        });
    }

    // BorderStyle 3: an opaque box behind the cue, drawn with the outline
    // color (the VSFilter behaviour; BackColour is the shadow's).
    if style.border_style == Some(3) {
        layout.background = Some(style.outline_color.unwrap_or(Color::rgba(0, 0, 0, 128)));
    }
    layout
}

// -- dialogue text -> spans ------------------------------------------------------

/// Build the styled IR for one dialogue event. `start_ns` is the cue's
/// presentation start (karaoke reveal times are absolute).
pub fn dialogue_to_ir(d: &SsaDialogue, styles: &SsaStyles, start_ns: u64) -> CueIr {
    let res = styles.play_res();
    let style = styles.lookup(&d.style);
    let base = style.map(|s| style_span(s, res.1)).unwrap_or_default();
    let layout = style
        .map(|s| style_layout(s, d, res))
        .unwrap_or_else(|| style_layout(&Style::default(), d, res));

    let mut p = TextParser {
        styles,
        res,
        lines: Vec::new(),
        cur_line: Vec::new(),
        cur_text: String::new(),
        cur_style: SpanStyle::default(),
        reveal_ns: None,
        karaoke_ns: start_ns,
        layout,
        an_set: false,
        pos_set: false,
        drawing: false,
    };
    p.run(&d.raw_text);
    p.finish(base)
}

struct TextParser<'a> {
    styles: &'a SsaStyles,
    res: (f32, f32),
    lines: Vec<Line>,
    cur_line: Vec<Span>,
    cur_text: String,
    /// Overrides accumulated from `{\...}` blocks; `None` fields inherit the
    /// base (the dialogue's style).
    cur_style: SpanStyle,
    /// Reveal time for spans emitted from here on (karaoke).
    reveal_ns: Option<u64>,
    /// The karaoke clock: cue start plus every `\k` duration seen so far.
    karaoke_ns: u64,
    layout: Layout,
    /// `\an`/`\a`: the first one wins (VSFilter).
    an_set: bool,
    /// `\pos`/`\move`: the first one wins.
    pos_set: bool,
    /// Inside `\p<n>` drawing mode: the "text" is vector path commands and is
    /// dropped.
    drawing: bool,
}

impl TextParser<'_> {
    fn run(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'{' => match text[i + 1..].find('}') {
                    Some(rel) => {
                        let block = &text[i + 1..i + 1 + rel];
                        self.override_block(block);
                        i += rel + 2;
                    }
                    None => {
                        // Unmatched '{': the C keeps the remainder verbatim
                        // (escapes untranslated); mirror that for the text.
                        self.text(&text[i..]);
                        return;
                    }
                },
                b'\\' if i + 1 < bytes.len() => {
                    match bytes[i + 1] {
                        b'N' | b'n' => self.end_line(),
                        b'h' => self.text("\u{a0}"),
                        _ => self.text(&text[i..i + 2]),
                    }
                    i += 2;
                }
                _ => {
                    let end = text[i..]
                        .find(['{', '\\'])
                        .map(|rel| i + rel)
                        .unwrap_or(text.len());
                    self.text(&text[i..end]);
                    i = end;
                }
            }
        }
    }

    fn text(&mut self, s: &str) {
        if !self.drawing {
            self.cur_text.push_str(s);
        }
    }

    fn flush(&mut self) {
        if self.cur_text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.cur_text);
        if let Some(last) = self.cur_line.last_mut()
            && last.style == self.cur_style
            && last.reveal_ns == self.reveal_ns
        {
            last.text.push_str(&text);
            return;
        }
        self.cur_line.push(Span {
            text,
            style: self.cur_style.clone(),
            reveal_ns: self.reveal_ns,
            ..Span::default()
        });
    }

    fn end_line(&mut self) {
        self.flush();
        let spans = std::mem::take(&mut self.cur_line);
        self.lines.push(Line { spans });
    }

    fn finish(mut self, base: SpanStyle) -> CueIr {
        self.flush();
        if !self.cur_line.is_empty() || self.lines.is_empty() {
            let spans = std::mem::take(&mut self.cur_line);
            self.lines.push(Line { spans });
        }
        CueIr {
            layout: self.layout,
            base,
            lines: self.lines,
        }
    }

    /// One `{...}` block: a sequence of `\tag<arg>` overrides (plus free-form
    /// comment text, which is skipped).
    fn override_block(&mut self, block: &str) {
        // A style change happens between spans by definition.
        self.flush();
        let bytes = block.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'\\' {
                i += 1;
                continue;
            }
            i += 1;
            let (name, arg, next) = read_tag(block, i);
            if name.is_empty() {
                continue;
            }
            self.apply_tag(name, arg);
            i = next;
        }
    }

    fn apply_tag(&mut self, name: &str, arg: &str) {
        let arg = arg.trim();
        let s = &mut self.cur_style;
        match name {
            "i" => {
                s.font_style = flag(arg).map(|on| {
                    if on {
                        FontStyle::Italic
                    } else {
                        FontStyle::Normal
                    }
                })
            }
            "b" => {
                s.font_weight = if arg.is_empty() {
                    None
                } else {
                    parse_bold(arg)
                }
            }
            "u" => s.underline = flag(arg),
            "s" => s.strikethrough = flag(arg),
            "fn" => s.font_family = (!arg.is_empty()).then(|| arg.to_owned()),
            "fs" => {
                // `\fs+n`/`\fs-n` are relative to the inherited size, which a
                // delta cannot express; only absolute sizes apply.
                s.font_size = match arg.parse::<f32>() {
                    Ok(v) if v > 0.0 && !arg.starts_with(['+', '-']) => {
                        Some(FontSize::FrameHeightPercent(v / self.res.1 * 100.0))
                    }
                    _ => None,
                };
            }
            "fsp" => s.letter_spacing = arg.parse().ok(),
            "fscx" => {
                let (_, y) = current_scale(s);
                s.scale = arg.parse::<f32>().ok().map(|x| (x / 100.0, y));
            }
            "fscy" => {
                let (x, _) = current_scale(s);
                s.scale = arg.parse::<f32>().ok().map(|y| (x, y / 100.0));
            }
            "c" | "1c" => s.foreground = parse_ssa_color(arg),
            "3c" => {
                let width = s.outline.map_or(DEFAULT_OUTLINE_W, |o| o.width);
                s.outline = parse_ssa_color(arg).map(|color| Outline { color, width });
            }
            "4c" => {
                if let Some(color) = parse_ssa_color(arg) {
                    let sh = s.shadow.unwrap_or(Shadow {
                        color,
                        dx: 0.0,
                        dy: 0.0,
                        blur: 0.0,
                    });
                    s.shadow = Some(Shadow { color, ..sh });
                }
            }
            "alpha" => {
                if let Some(a) = parse_ssa_alpha(arg) {
                    set_alpha(&mut s.foreground, a);
                    if let Some(o) = &mut s.outline {
                        o.color.a = a;
                    }
                    if let Some(sh) = &mut s.shadow {
                        sh.color.a = a;
                    }
                }
            }
            "1a" => {
                if let Some(a) = parse_ssa_alpha(arg) {
                    set_alpha(&mut s.foreground, a);
                }
            }
            "3a" => {
                if let (Some(a), Some(o)) = (parse_ssa_alpha(arg), &mut s.outline) {
                    o.color.a = a;
                }
            }
            "4a" => {
                if let (Some(a), Some(sh)) = (parse_ssa_alpha(arg), &mut s.shadow) {
                    sh.color.a = a;
                }
            }
            "bord" => {
                if let Ok(w) = arg.parse::<f32>() {
                    s.outline = (w > 0.0).then(|| Outline {
                        color: s.outline.map_or(Color::BLACK, |o| o.color),
                        width: w,
                    });
                }
            }
            "shad" => {
                if let Ok(v) = arg.parse::<f32>() {
                    s.shadow = (v > 0.0).then(|| Shadow {
                        dx: v,
                        dy: v,
                        ..s.shadow.unwrap_or(Shadow {
                            color: Color::rgba(0, 0, 0, 128),
                            dx: 0.0,
                            dy: 0.0,
                            blur: 0.0,
                        })
                    });
                }
            }
            "xshad" | "yshad" => {
                if let Ok(v) = arg.parse::<f32>() {
                    let mut sh = s.shadow.unwrap_or(Shadow {
                        color: Color::rgba(0, 0, 0, 128),
                        dx: 0.0,
                        dy: 0.0,
                        blur: 0.0,
                    });
                    if name == "xshad" {
                        sh.dx = v;
                    } else {
                        sh.dy = v;
                    }
                    s.shadow = Some(sh);
                }
            }
            "an" | "a" => {
                if !self.an_set
                    && let Ok(v) = arg.parse::<i32>()
                {
                    let numpad = if name == "an" {
                        Some(v)
                    } else {
                        legacy_to_numpad(v)
                    };
                    if let Some((anchor, align)) = numpad.and_then(numpad_anchor) {
                        self.layout.anchor = Some(anchor);
                        self.layout.align = Some(align);
                        self.an_set = true;
                    }
                }
            }
            "pos" | "move" => {
                if !self.pos_set
                    && let Some((x, y)) = parse_xy(arg)
                {
                    let (rx, ry) = self.res;
                    self.layout.origin = Some((x / rx * 100.0, y / ry * 100.0));
                    self.pos_set = true;
                }
            }
            "r" => {
                *s = match self.styles.lookup(arg).filter(|_| !arg.is_empty()) {
                    // `\rName`: the named style's character styling replaces
                    // the base's where it sets something.
                    Some(style) => style_span(style, self.res.1),
                    // Plain `\r` (or unknown name): back to the base.
                    None => SpanStyle::default(),
                };
            }
            "k" | "K" | "kf" | "ko" => {
                if let Ok(cs) = arg.parse::<f32>() {
                    // Text after this tag is the syllable this duration times:
                    // it highlights when the clock reaches the accumulated sum
                    // of everything before it.
                    self.reveal_ns = Some(self.karaoke_ns);
                    let ns = (cs.max(0.0) * 10_000_000.0) as u64;
                    self.karaoke_ns = self.karaoke_ns.saturating_add(ns);
                }
            }
            "p" => {
                self.drawing = arg.parse::<f32>().map(|v| v > 0.0).unwrap_or(false);
            }
            // Parsed (argument consumed) but not representable: animations,
            // fades, clips, rotation, blur, wrap mode, secondary color...
            _ => {}
        }
    }
}

fn current_scale(s: &SpanStyle) -> (f32, f32) {
    s.scale.unwrap_or((1.0, 1.0))
}

fn set_alpha(color: &mut Option<Color>, a: u8) {
    match color {
        Some(c) => c.a = a,
        // Alpha on the inherited (unknown) fill: assume the customary white.
        None => *color = Some(Color::rgba(0xff, 0xff, 0xff, a)),
    }
}

/// `\i1` / `\u0` style flags: empty resets to the base (`None`).
fn flag(arg: &str) -> Option<bool> {
    match arg {
        "" => None,
        "0" => Some(false),
        _ => Some(true),
    }
}

/// `(x, y[, ...])` — the leading point of `\pos` and `\move`.
fn parse_xy(arg: &str) -> Option<(f32, f32)> {
    let inner = arg.trim().strip_prefix('(')?;
    let inner = inner.strip_suffix(')').unwrap_or(inner);
    let mut it = inner.split(',');
    let x = it.next()?.trim().parse().ok()?;
    let y = it.next()?.trim().parse().ok()?;
    Some((x, y))
}

/// The known override tag names, longest first so `\shad` is never read as
/// `\s` + junk, `\an` never as `\a`, `\fsp`/`\fscx` never as `\fs`.
const TAG_NAMES: &[&str] = &[
    "alpha", "iclip", "xshad", "yshad", "xbord", "ybord", "shad", "fscx", "fscy", "bord", "blur",
    "clip", "fade", "move", "fad", "fsp", "org", "pos", "frx", "fry", "frz", "1c", "2c", "3c",
    "4c", "1a", "2a", "3a", "4a", "an", "be", "fe", "fn", "fr", "fs", "kf", "ko", "a", "b", "c",
    "i", "k", "K", "p", "q", "r", "s", "t", "u",
];

/// Read one `\tag<arg>` starting just past the backslash at `i`. The argument
/// runs to the next `\` (or the block's end), except that a parenthesised
/// argument is consumed to its matching `)` — which is what keeps the tags
/// inside a `\t(...)` from leaking out. Unknown tags consume their argument
/// the same way. Returns `(name, arg, next_index)`.
fn read_tag(block: &str, i: usize) -> (&str, &str, usize) {
    let rest = &block[i..];
    let name = TAG_NAMES
        .iter()
        .find(|n| rest.starts_with(**n))
        .copied()
        .unwrap_or("");
    let mut j = i + name.len();
    let bytes = block.as_bytes();
    let mut depth = 0i32;
    while j < bytes.len() {
        match bytes[j] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'\\' if depth <= 0 => break,
            _ => {}
        }
        j += 1;
    }
    (name, &block[i + name.len()..j], j)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    /// A registry with one 384x288-space style, the shape most tests want.
    fn registry(style_line: &str) -> SsaStyles {
        SsaStyles::parse(&format!(
            "[Script Info]\nPlayResX: 384\nPlayResY: 288\n\n\
             [V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: {style_line}\n"
        ))
    }

    fn default_registry() -> SsaStyles {
        registry(
            "Default,Arial,20,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,1,2,10,10,10,1",
        )
    }

    fn ir_for(text: &str) -> CueIr {
        let d = SsaDialogue {
            raw_text: text.to_owned(),
            style: "Default".to_owned(),
            ..SsaDialogue::default()
        };
        dialogue_to_ir(&d, &default_registry(), 0)
    }

    fn spans_of(ir: &CueIr) -> Vec<&Span> {
        ir.lines.iter().flat_map(|l| l.spans.iter()).collect()
    }

    // ---- colors -----------------------------------------------------------

    #[test]
    fn ssa_colors_parse() {
        // &HBBGGRR: blue=0xFF in the high byte -> blue channel.
        assert_eq!(parse_ssa_color("&HFF0000&"), Some(Color::rgb(0, 0, 255)));
        assert_eq!(parse_ssa_color("&H0000FF"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(
            parse_ssa_color("&H00FFFFFF"),
            Some(Color::rgb(255, 255, 255))
        );
        // Alpha byte is inverted: &H80...& is 50%-ish transparent.
        assert_eq!(
            parse_ssa_color("&H800000FF"),
            Some(Color::rgba(255, 0, 0, 127))
        );
        // Decimal (SSA v4 files): 65535 = 0x00FFFF = yellow.
        assert_eq!(parse_ssa_color("65535"), Some(Color::rgb(255, 255, 0)));
        // Negative decimals wrap like the 32-bit C parsers.
        assert_eq!(
            parse_ssa_color("-2147483640"),
            Some(Color::rgba(8, 0, 0, 255 - 0x80))
        );
        assert_eq!(parse_ssa_color("nope"), None);
    }

    // ---- style parsing -------------------------------------------------------

    #[test]
    fn v4plus_style_fields_parse() {
        let reg = registry(
            "Default,DejaVu Sans,24,&H0000FFFF,&H000000FF,&H00FF0000,&H80000000,-1,-1,-1,-1,120,80,1.5,0,1,3,2,8,12,14,16,1",
        );
        let style = reg.lookup("Default").unwrap();
        assert_eq!(style.font_family.as_deref(), Some("DejaVu Sans"));
        assert_eq!(style.font_size, Some(24.0));
        assert_eq!(style.primary, Some(Color::rgb(255, 255, 0)));
        assert_eq!(style.outline_color, Some(Color::rgb(0, 0, 255)));
        assert_eq!(style.back_color, Some(Color::rgba(0, 0, 0, 127)));
        assert_eq!(style.font_weight, Some(700));
        assert_eq!(style.italic, Some(true));
        assert_eq!(style.underline, Some(true));
        assert_eq!(style.strikeout, Some(true));
        assert_eq!(style.scale_x, Some(120.0));
        assert_eq!(style.scale_y, Some(80.0));
        assert_eq!(style.spacing, Some(1.5));
        assert_eq!(style.border_style, Some(1));
        assert_eq!(style.outline_w, Some(3.0));
        assert_eq!(style.shadow_d, Some(2.0));
        assert_eq!(style.alignment, Some(8));
        assert_eq!(style.margin_l, Some(12.0));
    }

    #[test]
    fn v4_style_uses_legacy_alignment_and_tertiary() {
        let reg = SsaStyles::parse(
            "[V4 Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\n\
             Style: Default,Arial,18,16777215,65535,255,0,-1,0,1,2,1,6,10,10,10,0,1\n",
        );
        let style = reg.lookup("Default").unwrap();
        // Legacy 6 = toptitle center -> numpad 8.
        assert_eq!(style.alignment, Some(8));
        // TertiaryColour fills the outline slot; 255 = 0x0000FF = red.
        assert_eq!(style.outline_color, Some(Color::rgb(255, 0, 0)));
        assert_eq!(style.font_weight, Some(700));
    }

    #[test]
    fn style_lookup_is_lenient() {
        let reg = default_registry();
        assert!(reg.lookup("Default").is_some());
        assert!(reg.lookup("default").is_some());
        assert!(reg.lookup("*Default").is_some());
        assert!(reg.lookup("Missing").is_none());
    }

    #[test]
    fn play_res_defaults_and_derivation() {
        assert_eq!(SsaStyles::parse("").play_res(), (384.0, 288.0));
        assert_eq!(
            SsaStyles::parse("[Script Info]\nPlayResY: 480\n").play_res(),
            (640.0, 480.0)
        );
        assert_eq!(
            SsaStyles::parse("[Script Info]\nPlayResX: 1280\nPlayResY: 720\n").play_res(),
            (1280.0, 720.0)
        );
    }

    // ---- style -> base/layout --------------------------------------------------

    #[test]
    fn style_becomes_base_and_layout() {
        let d = SsaDialogue {
            raw_text: "Hi".to_owned(),
            style: "Default".to_owned(),
            ..SsaDialogue::default()
        };
        let ir = dialogue_to_ir(&d, &default_registry(), 0);
        // Fontsize 20 at PlayResY 288.
        assert_eq!(
            ir.base.font_size,
            Some(FontSize::FrameHeightPercent(20.0 / 288.0 * 100.0))
        );
        assert_eq!(ir.base.font_family.as_deref(), Some("Arial"));
        assert_eq!(ir.base.foreground, Some(Color::WHITE));
        assert_eq!(
            ir.base.outline,
            Some(Outline {
                color: Color::BLACK,
                width: 2.0
            })
        );
        assert_eq!(ir.base.shadow.map(|s| (s.dx, s.dy)), Some((1.0, 1.0)));
        assert_eq!(ir.layout.anchor, Some(Anchor::BottomCenter));
        assert_eq!(ir.layout.align, Some(TextAlign::Center));
        // Margins 10/384 and 10/288.
        let m = ir.layout.margins.unwrap();
        assert!((m.left - 10.0 / 384.0 * 100.0).abs() < 1e-4);
        assert!((m.vertical - 10.0 / 288.0 * 100.0).abs() < 1e-4);
        assert_eq!(ir.plain_text(), "Hi");
    }

    #[test]
    fn dialogue_margins_override_style() {
        let d = SsaDialogue {
            raw_text: "x".to_owned(),
            style: "Default".to_owned(),
            margin_v: 72,
            ..SsaDialogue::default()
        };
        let ir = dialogue_to_ir(&d, &default_registry(), 0);
        assert_eq!(ir.layout.margins.unwrap().vertical, 25.0); // 72/288
    }

    #[test]
    fn border_style_3_becomes_cue_background() {
        let reg = registry(
            "Default,Arial,20,&H00FFFFFF,&H000000FF,&H00404040,&H00000000,0,0,0,0,100,100,0,0,3,2,0,2,0,0,0,1",
        );
        let d = SsaDialogue {
            raw_text: "x".to_owned(),
            style: "Default".to_owned(),
            ..SsaDialogue::default()
        };
        let ir = dialogue_to_ir(&d, &reg, 0);
        assert_eq!(ir.layout.background, Some(Color::rgb(0x40, 0x40, 0x40)));
        assert_eq!(ir.base.outline, None, "no stroke in box mode");
    }

    #[test]
    fn unknown_style_leaves_renderer_defaults() {
        let d = SsaDialogue {
            raw_text: "x".to_owned(),
            style: "Nope".to_owned(),
            ..SsaDialogue::default()
        };
        let ir = dialogue_to_ir(&d, &default_registry(), 0);
        assert!(ir.base.is_plain());
        assert_eq!(ir.plain_text(), "x");
    }

    // ---- override tags -----------------------------------------------------------

    #[test]
    fn basic_flags_and_reset() {
        let ir = ir_for("{\\i1}it{\\i0}no{\\b1\\u1}bu{\\r}plain");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
        assert_eq!(spans[1].style.font_style, Some(FontStyle::Normal));
        assert_eq!(spans[2].style.font_weight, Some(700));
        assert_eq!(spans[2].style.underline, Some(true));
        assert!(spans[3].style.is_plain());
        assert_eq!(ir.plain_text(), "itnobuplain");
    }

    #[test]
    fn empty_flag_resets_to_base() {
        let ir = ir_for("{\\i1}a{\\i}b");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
        assert_eq!(spans[1].style.font_style, None);
    }

    #[test]
    fn font_and_size_and_spacing() {
        let ir = ir_for("{\\fnComic Sans\\fs36\\fsp2}x");
        let s = &spans_of(&ir)[0].style;
        assert_eq!(s.font_family.as_deref(), Some("Comic Sans"));
        assert_eq!(s.font_size, Some(FontSize::FrameHeightPercent(12.5))); // 36/288
        assert_eq!(s.letter_spacing, Some(2.0));
    }

    #[test]
    fn colors_and_alpha() {
        let ir = ir_for("{\\c&H0000FF&}red{\\1a&H80&}faded");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, Some(Color::rgb(255, 0, 0)));
        assert_eq!(spans[1].style.foreground, Some(Color::rgba(255, 0, 0, 127)));
    }

    #[test]
    fn outline_and_shadow_overrides() {
        let ir = ir_for("{\\bord4\\3c&HFF0000&\\shad3\\4c&H00FF00&}x{\\bord0}y");
        let spans = spans_of(&ir);
        assert_eq!(
            spans[0].style.outline,
            Some(Outline {
                color: Color::rgb(0, 0, 255),
                width: 4.0
            })
        );
        let sh = spans[0].style.shadow.unwrap();
        assert_eq!((sh.dx, sh.dy), (3.0, 3.0));
        assert_eq!(sh.color, Color::rgb(0, 255, 0));
        // \bord0 kills the override outline (the base still applies via
        // inheritance rules? No: Some-field overrides; bord0 -> None means
        // inherit base). VSFilter semantics for \bord0 are "no border", which
        // inheritance cannot express exactly; nearest is clearing the
        // override.
        assert_eq!(spans[1].style.outline, None);
    }

    #[test]
    fn scale_overrides() {
        let ir = ir_for("{\\fscx200}wide{\\fscy50}also-flat");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.scale, Some((2.0, 1.0)));
        assert_eq!(spans[1].style.scale, Some((2.0, 0.5)));
    }

    #[test]
    fn alignment_and_position() {
        let ir = ir_for("{\\an8\\pos(192,36)}top");
        assert_eq!(ir.layout.anchor, Some(Anchor::TopCenter));
        assert_eq!(ir.layout.align, Some(TextAlign::Center));
        assert_eq!(ir.layout.origin, Some((50.0, 12.5)));
    }

    #[test]
    fn first_alignment_and_position_win() {
        let ir = ir_for("{\\an8}x{\\an1}y{\\pos(0,0)}z");
        assert_eq!(ir.layout.anchor, Some(Anchor::TopCenter));
        assert_eq!(ir.layout.origin, Some((0.0, 0.0)));
        let ir = ir_for("{\\pos(192,144)}x{\\pos(0,0)}y");
        assert_eq!(ir.layout.origin, Some((50.0, 50.0)));
    }

    #[test]
    fn legacy_alignment_tag() {
        let ir = ir_for("{\\a6}top");
        assert_eq!(ir.layout.anchor, Some(Anchor::TopCenter));
    }

    #[test]
    fn move_takes_start_point() {
        let ir = ir_for("{\\move(96,72,192,144)}x");
        assert_eq!(ir.layout.origin, Some((25.0, 25.0)));
    }

    #[test]
    fn reset_to_named_style() {
        let mut reg = default_registry();
        reg.feed_line("[V4+ Styles]");
        reg.feed_line("Format: Name, PrimaryColour, Italic");
        reg.feed_line("Style: Alt,&H0000FF&,-1");
        let d = SsaDialogue {
            raw_text: "a{\\rAlt}b".to_owned(),
            style: "Default".to_owned(),
            ..SsaDialogue::default()
        };
        let ir = dialogue_to_ir(&d, &reg, 0);
        let spans = spans_of(&ir);
        assert!(spans[0].style.is_plain());
        assert_eq!(spans[1].style.foreground, Some(Color::rgb(255, 0, 0)));
        assert_eq!(spans[1].style.font_style, Some(FontStyle::Italic));
    }

    // ---- line breaks / hard space / drawing ------------------------------------

    #[test]
    fn line_breaks_and_hard_space() {
        let ir = ir_for("one\\Ntwo\\nthree\\hx");
        assert_eq!(ir.lines.len(), 3);
        assert_eq!(ir.plain_text(), "one\ntwo\nthree\u{a0}x");
    }

    #[test]
    fn other_backslash_escapes_stay_literal() {
        assert_eq!(ir_for("a\\db").plain_text(), "a\\db");
    }

    #[test]
    fn drawing_commands_are_dropped() {
        let ir = ir_for("{\\p1}m 0 0 l 100 0 100 100{\\p0}after");
        assert_eq!(ir.plain_text(), "after");
    }

    #[test]
    fn unmatched_brace_keeps_remainder() {
        // Mirrors the C text path: the '{' and everything after stay.
        assert_eq!(ir_for("ok{\\i1 oops").plain_text(), "ok{\\i1 oops");
    }

    // ---- karaoke ------------------------------------------------------------------

    #[test]
    fn karaoke_accumulates_reveal_times() {
        let ir = {
            let d = SsaDialogue {
                raw_text: "{\\k50}la{\\k100}lala{\\kf25}end".to_owned(),
                style: "Default".to_owned(),
                ..SsaDialogue::default()
            };
            dialogue_to_ir(&d, &default_registry(), 10 * S)
        };
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].reveal_ns, Some(10 * S));
        assert_eq!(spans[1].reveal_ns, Some(10 * S + 500_000_000));
        assert_eq!(spans[2].reveal_ns, Some(10 * S + 1_500_000_000));
        assert_eq!(ir.plain_text(), "lalalaend");
    }

    // ---- animation args are consumed ----------------------------------------------

    #[test]
    fn transform_args_do_not_leak() {
        // The \c inside \t(...) must not apply statically.
        let ir = ir_for("{\\t(0,500,\\c&H0000FF&)}steady");
        assert_eq!(spans_of(&ir)[0].style.foreground, None);
        // ...but a tag after the \t block still does.
        let ir = ir_for("{\\t(0,500,\\c&H0000FF&)\\b1}bold");
        assert_eq!(spans_of(&ir)[0].style.font_weight, Some(700));
        assert_eq!(spans_of(&ir)[0].style.foreground, None);
    }

    #[test]
    fn shad_is_not_read_as_s() {
        let ir = ir_for("{\\shad2}x");
        let s = &spans_of(&ir)[0].style;
        assert_eq!(s.strikethrough, None);
        assert_eq!(s.shadow.map(|sh| sh.dx), Some(2.0));
    }

    #[test]
    fn adjacent_identical_spans_merge() {
        let ir = ir_for("a{\\i1}{\\i1}b{comment}c");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "a");
        assert_eq!(spans[1].text, "bc");
    }
}
