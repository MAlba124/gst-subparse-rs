// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! A renderer-oriented intermediate representation (IR) for subtitle cues.
//!
//! [`CueIr`] describes one cue as plain-old-data: a list of visual
//! [`Line`]s, each a list of styled [`Span`]s, plus cue-level [`Layout`]
//! (positioning, alignment, writing direction). It exists so a custom renderer
//! (e.g. one built on `parley` for text layout and `vello_cpu` for rasterizing)
//! can draw styled cues without parsing Pango markup, while the default
//! pango-markup output stays exactly what the C `subparse` emits.
//!
//! The types deliberately cover more than what today's parsers produce (SSA
//! outline/shadow/scale, WebVTT regions-ish positioning, ruby, karaoke reveal
//! times), so parsers can grow into them without another IR revision:
//!
//! * [`SpanStyle`] maps 1:1 onto `parley` style properties (font stack, size,
//!   weight, style, underline/strikethrough, brush) plus the paint-level
//!   extras `parley` leaves to the renderer (background box, outline, shadow,
//!   baseline shift).
//! * [`Layout`] carries WebVTT cue settings (`line`/`position`/`size`/`align`/
//!   `vertical`) and the SSA/ASS placement model (numpad anchor, margins,
//!   explicit origin). All positional values are percentages of the video
//!   frame in `[0, 100]`, so the renderer needs no knowledge of the source
//!   format's coordinate space.
//!
//! The constructors cover everything the parsers emit today:
//! [`CueIr::from_pango_markup`] understands the (closed) markup subset the
//! `subparse-formats` parsers generate — `<i>/<b>/<u>` (SubRip, MPL2),
//! `<span>` with `foreground`/`font_family`/`size`/`rise` (SAMI),
//! `style`/`weight`/`size` (MicroDVD), `font`/`color`/`bgcolor` (QTtext) — as
//! well as the WebVTT tags the C keeps verbatim (`<c>`, `<v>`, `<ruby>`,
//! `<rt>`). [`CueIr::from_pango_markup_styled`] additionally applies a WebVTT
//! [`Stylesheet`] (`STYLE` blocks, see [`crate::vttcss`]) with CSS cascade
//! semantics. [`CueIr::from_plain_text`] wraps unstyled text. [`cue_to_ir`]
//! picks between them and folds the cue's [`CueSettings`] into the layout.
//!
//! Like the rest of this crate, the module is dependency-free (std only).

use crate::cue::{Cue, CueSettings, OutputFormat};
use crate::vttcss::Stylesheet;

// -- colors ------------------------------------------------------------------

/// An sRGB color with straight (non-premultiplied) alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const WHITE: Color = Color::rgb(0xff, 0xff, 0xff);
    pub const BLACK: Color = Color::rgb(0x00, 0x00, 0x00);
    pub const TRANSPARENT: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 0xff }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    /// Parse a color the way `pango_color_parse` does: `#rgb`, `#rrggbb`,
    /// `#rrrgggbbb`, `#rrrrggggbbbb` or a named CSS/X11 color. Returns `None`
    /// for anything unrecognised.
    pub fn parse(s: &str) -> Option<Color> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            return Color::parse_hex(hex);
        }
        named_color(s)
    }

    fn parse_hex(hex: &str) -> Option<Color> {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        // Per-channel digit count, as in pango: 1..=4 digits per channel.
        let per = match hex.len() {
            3 => 1,
            6 => 2,
            9 => 3,
            12 => 4,
            _ => return None,
        };
        let chan = |i: usize| -> u8 {
            let v = u32::from_str_radix(&hex[i * per..(i + 1) * per], 16).unwrap();
            // Scale an n-digit channel to 8 bits (top byte of the value
            // left-aligned to 16 bits, which is what pango does).
            let max = (1u32 << (4 * per)) - 1;
            ((v * 255 + max / 2) / max) as u8
        };
        Some(Color::rgb(chan(0), chan(1), chan(2)))
    }
}

/// The CSS3 / X11 named colors (the set `pango_color_parse` accepts, matched
/// ASCII case-insensitively), sorted by name for binary search. Public so the
/// `pango-markup` crate shares the table instead of duplicating it.
pub const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

pub(crate) fn named_color(name: &str) -> Option<Color> {
    let lower = name.to_ascii_lowercase();
    NAMED_COLORS
        .binary_search_by(|(n, _)| n.cmp(&lower.as_str()))
        .ok()
        .map(|i| {
            let v = NAMED_COLORS[i].1;
            Color::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
        })
}

// -- span-level styling --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
}

/// A font size, either absolute or relative to the renderer's base size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FontSize {
    /// Absolute size in points.
    Points(f32),
    /// A factor applied to the renderer's base cue size (`1.0` = base).
    Scale(f32),
    /// Percent of the video frame height in `[0, 100]` (SSA/ASS sizes, which
    /// live in `PlayRes` pixel space and scale with the frame).
    FrameHeightPercent(f32),
}

/// A text outline (SSA/ASS border, CC edge). Drawn as a stroke behind the
/// glyph fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Outline {
    pub color: Color,
    /// Stroke width in points.
    pub width: f32,
}

/// A drop shadow behind the text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    pub color: Color,
    /// Offset in points, positive right/down.
    pub dx: f32,
    pub dy: f32,
    /// Gaussian blur radius in points (`0.0` = hard shadow).
    pub blur: f32,
}

/// Character styling for one span. Every field is optional; `None` means
/// "inherit", first from [`CueIr::base`], then from the renderer's defaults.
///
/// The fields map onto `parley` style properties (font stack/size/weight/
/// style, underline, strikethrough, letter spacing, brush color, locale). The
/// rest — background box, outline, shadow, baseline shift, glyph scale — are
/// paint-level and handled by the renderer around parley's layout.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SpanStyle {
    /// Font family name (a parley font stack entry). `None` = default font.
    pub font_family: Option<String>,
    pub font_size: Option<FontSize>,
    pub font_style: Option<FontStyle>,
    /// CSS-style weight: 400 normal, 700 bold.
    pub font_weight: Option<u16>,
    pub underline: Option<bool>,
    pub strikethrough: Option<bool>,
    /// Text (fill) color.
    pub foreground: Option<Color>,
    /// Background box painted behind this span's glyphs.
    pub background: Option<Color>,
    pub outline: Option<Outline>,
    pub shadow: Option<Shadow>,
    /// Additional letter spacing in points (SSA `\fsp`, pango
    /// `letter_spacing`).
    pub letter_spacing: Option<f32>,
    /// Vertical baseline displacement in points, positive = raised (pango
    /// `rise`, sub/superscript).
    pub baseline_shift: Option<f32>,
    /// Horizontal/vertical glyph scale factors (SSA `\fscx`/`\fscy`, `1.0` =
    /// unscaled).
    pub scale: Option<(f32, f32)>,
    /// BCP-47 language tag for shaping/line breaking (WebVTT `<lang>`, pango
    /// `lang`).
    pub language: Option<String>,
}

impl SpanStyle {
    /// Whether every field is `None` (the span adds nothing over the base).
    pub fn is_plain(&self) -> bool {
        *self == SpanStyle::default()
    }
}

/// Where a ruby annotation is drawn relative to its base text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RubyPosition {
    /// Above horizontal text / right of vertical text.
    #[default]
    Over,
    /// Below horizontal text / left of vertical text.
    Under,
}

/// A ruby annotation attached to a base [`Span`].
#[derive(Debug, Clone, PartialEq)]
pub struct Ruby {
    pub text: String,
    pub position: RubyPosition,
}

/// A run of text with uniform styling.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    /// WebVTT voice (`<v Fred>`): the speaker this span is attributed to.
    pub voice: Option<String>,
    /// WebVTT classes (`<c.yellow.bg_blue>`), verbatim. The standard color
    /// classes are *also* resolved into `style.foreground`/`.background`.
    pub classes: Vec<String>,
    /// Ruby annotation over this span's text (WebVTT `<ruby>`/`<rt>`).
    pub ruby: Option<Ruby>,
    /// For karaoke/rolling captions: presentation time (nanoseconds, same
    /// timeline as the cue) at which this span becomes visible. `None` =
    /// visible for the cue's whole duration.
    pub reveal_ns: Option<u64>,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Span {
        Span {
            text: text.into(),
            ..Span::default()
        }
    }
}

/// One visual line (spans are laid out left to right / top to bottom in the
/// cue's writing direction; there are no newlines inside spans).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Line {
    pub spans: Vec<Span>,
}

// -- cue-level layout ------------------------------------------------------

/// Block progression direction (WebVTT `vertical` setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WritingMode {
    /// Horizontal lines, stacked top to bottom.
    #[default]
    HorizontalTb,
    /// Vertical lines, stacked right to left.
    VerticalRl,
    /// Vertical lines, stacked left to right.
    VerticalLr,
}

/// Text alignment within the cue box (WebVTT `align`, SSA style alignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    /// Toward the start of the text direction (left for LTR text).
    Start,
    Center,
    /// Toward the end of the text direction.
    End,
    Left,
    Right,
}

/// Which edge of the cue box the `line` position pins (WebVTT `line` align).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAlign {
    Start,
    Center,
    End,
}

/// Alignment of the cue box around its `position` (WebVTT position align).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionAlign {
    Auto,
    LineLeft,
    Center,
    LineRight,
}

/// The WebVTT `line` setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinePosition {
    /// Percentage of the video frame in the block direction, `[0, 100]`.
    Percent(f32),
    /// A line *number*: 0-based from the start edge, negative counts from the
    /// end edge (`-1` is the last line).
    Line(i32),
}

/// A 9-way anchor (SSA/ASS numpad alignment, CEA-708 anchor points): which
/// point of the cue box the cue's position refers to, and the default
/// placement when no explicit position is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    TopLeft,
    TopCenter,
    TopRight,
    CenterLeft,
    Center,
    CenterRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Distances from the video frame edges the cue must keep clear, as
/// percentages of the frame in `[0, 100]` (SSA margins, normalised out of
/// `PlayRes` space by the parser).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Margins {
    pub left: f32,
    pub right: f32,
    pub vertical: f32,
}

/// Cue-level placement. Everything is optional: an all-default layout means
/// "bottom-center, per the renderer's house style", which is what plain SRT
/// wants.
///
/// All positional values are percentages of the video frame in `[0, 100]`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub writing_mode: WritingMode,
    /// WebVTT `line`: where the cue box sits in the block direction.
    pub line: Option<LinePosition>,
    pub line_align: Option<LineAlign>,
    /// WebVTT `position`: where the cue box sits in the inline direction
    /// (percent).
    pub position: Option<f32>,
    pub position_align: Option<PositionAlign>,
    /// WebVTT `size`: extent of the cue box in the inline direction (percent).
    pub size: Option<f32>,
    /// Text alignment within the cue box.
    pub align: Option<TextAlign>,
    /// SSA-style anchor of the cue box (numpad alignment).
    pub anchor: Option<Anchor>,
    /// Explicit anchor-point position (SSA `\pos`), percent of the frame.
    pub origin: Option<(f32, f32)>,
    pub margins: Option<Margins>,
    /// Fill behind the whole cue box (SSA `BorderStyle=3`, CC window color).
    pub background: Option<Color>,
}

// -- the cue ---------------------------------------------------------------

/// A fully styled subtitle cue, ready for a custom renderer. Timing stays on
/// the `GstBuffer` (PTS/duration), exactly as for pango-markup output.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CueIr {
    pub layout: Layout,
    /// Style every span inherits where its own [`SpanStyle`] is `None` (SSA
    /// style defaults live here). Renderer defaults fill whatever is still
    /// unset after that.
    pub base: SpanStyle,
    pub lines: Vec<Line>,
}

impl CueIr {
    /// The unstyled text: span texts concatenated, lines joined with `\n`.
    /// Ruby annotations are not included. This is what the buffer payload
    /// carries in `cue-ir` output mode.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i != 0 {
                out.push('\n');
            }
            for span in &line.spans {
                out.push_str(&span.text);
            }
        }
        out
    }

    /// Wrap plain UTF-8 text (no markup) into an unstyled cue.
    pub fn from_plain_text(text: &str) -> CueIr {
        CueIr {
            lines: text
                .split('\n')
                .map(|l| {
                    let l = l.strip_suffix('\r').unwrap_or(l);
                    Line {
                        spans: if l.is_empty() {
                            Vec::new()
                        } else {
                            vec![Span::plain(l)]
                        },
                    }
                })
                .collect(),
            ..CueIr::default()
        }
    }

    /// Parse the pango-markup subset the `subparse-formats` parsers emit into
    /// styled spans. Lenient: unknown tags are dropped (their content kept),
    /// unclosed tags close at the end of the cue, stray closers are ignored.
    pub fn from_pango_markup(markup: &str) -> CueIr {
        markup::parse(markup, None, None)
    }

    /// Like [`CueIr::from_pango_markup`], additionally applying a WebVTT
    /// [`Stylesheet`] (from the file's `STYLE` blocks). `::cue` rules land in
    /// [`CueIr::base`]; `::cue(...)` rules are matched against each tag as it
    /// opens, with the author CSS overriding the tag-derived styling, and
    /// deeper tags overriding inherited values — CSS cascade semantics.
    /// `cue_id` is the cue's identifier, for `::cue(#id)` selectors.
    pub fn from_pango_markup_styled(
        markup: &str,
        sheet: Option<&Stylesheet>,
        cue_id: Option<&str>,
    ) -> CueIr {
        markup::parse(markup, sheet, cue_id)
    }

    /// Parse raw SubRip cue text ([`Cue::raw_text`], the source before the
    /// C's lossy markup transform) into styled spans. Recognises
    /// `<i>/<b>/<u>/<s>` and `<font color|face|size>` — leniently: names are
    /// case-insensitive, unclosed tags close at the cue's end, stray closers
    /// are ignored. Unknown letter-initiated tags are dropped with their
    /// content kept (matching the C's text output); anything else (`<3`,
    /// `< junk>`) stays literal, and entities are **not** decoded (this is
    /// source text, not markup). `{\anN}`/`{\aN}` blocks — never part of
    /// SubRip, but honoured by every player — set the cue anchor; other
    /// `{\...}` blocks are stripped.
    pub fn from_srt_text(text: &str) -> CueIr {
        markup::parse_srt(text)
    }
}

/// Build the IR for one parsed [`Cue`], honouring the flavour `output` says
/// its text is in and folding its WebVTT [`CueSettings`] into the layout.
/// `sheet` is the WebVTT stylesheet collected from the stream's `STYLE`
/// blocks ([`crate::format::SubtitleFormat::stylesheet`]); `None` for formats
/// without one.
pub fn cue_to_ir(cue: &Cue, output: OutputFormat, sheet: Option<&Stylesheet>) -> CueIr {
    let text = cue.text.trim_end_matches(['\n', '\r']);
    let mut ir = if let Some(raw) = cue.raw_text.as_deref() {
        // The source text carries styling the pango transform lost
        // (SubRip's <font>).
        CueIr::from_srt_text(raw.trim_end_matches(['\n', '\r']))
    } else {
        match output {
            OutputFormat::Utf8 => CueIr::from_plain_text(text),
            OutputFormat::PangoMarkup => {
                CueIr::from_pango_markup_styled(text, sheet, cue.id.as_deref())
            }
        }
    };
    apply_settings(&mut ir.layout, &cue.settings);
    ir
}

/// Fold parsed WebVTT cue settings into a [`Layout`].
fn apply_settings(layout: &mut Layout, s: &CueSettings) {
    if let Some(v) = s.line_position {
        layout.line = Some(LinePosition::Percent(v as f32));
    } else if let Some(n) = s.line_number {
        layout.line = Some(LinePosition::Line(n));
    }
    if let Some(a) = s.line_align.as_deref() {
        layout.line_align = match a {
            "start" => Some(LineAlign::Start),
            "center" => Some(LineAlign::Center),
            "end" => Some(LineAlign::End),
            _ => layout.line_align,
        };
    }
    if let Some(v) = s.text_position {
        layout.position = Some(v as f32);
    }
    if let Some(a) = s.position_align.as_deref() {
        layout.position_align = match a {
            "line-left" => Some(PositionAlign::LineLeft),
            "center" => Some(PositionAlign::Center),
            "line-right" => Some(PositionAlign::LineRight),
            "auto" => Some(PositionAlign::Auto),
            _ => layout.position_align,
        };
    }
    if let Some(v) = s.text_size {
        layout.size = Some(v as f32);
    }
    if let Some(v) = s.vertical.as_deref() {
        layout.writing_mode = match v {
            // Old syntax (`D:vertical`) on the left, modern (`vertical:rl`)
            // on the right.
            "vertical" | "rl" => WritingMode::VerticalRl,
            "vertical-lr" | "lr" => WritingMode::VerticalLr,
            _ => WritingMode::HorizontalTb,
        };
    }
    if let Some(a) = s.alignment.as_deref() {
        layout.align = match a {
            "start" => Some(TextAlign::Start),
            "middle" | "center" => Some(TextAlign::Center),
            "end" => Some(TextAlign::End),
            "left" => Some(TextAlign::Left),
            "right" => Some(TextAlign::Right),
            _ => layout.align,
        };
    }
}

// -- pango-markup subset parser ---------------------------------------------

mod markup {
    use super::*;
    use crate::vttcss::Node;

    /// The parse state that tags push/pop.
    #[derive(Debug, Clone, Default)]
    struct Ctx {
        style: SpanStyle,
        voice: Option<String>,
        classes: Vec<String>,
        /// Where a ruby annotation closing in this context is drawn
        /// (CSS `ruby-position`, inherited).
        ruby_position: RubyPosition,
        /// Inside `<ruby>`.
        in_ruby: bool,
        /// Inside `<rt>` (text goes to the annotation, not the line).
        in_rt: bool,
    }

    struct Parser<'a> {
        lines: Vec<Line>,
        cur_line: Vec<Span>,
        cur_text: String,
        ctx: Ctx,
        /// (tag name, state to restore on close).
        stack: Vec<(String, Ctx)>,
        /// Annotation text accumulating inside `<rt>`.
        rt_text: String,
        /// WebVTT `STYLE` rules to apply as tags open (`None` = no styling).
        sheet: Option<&'a Stylesheet>,
        /// Cue-wide style: `::cue` and `::cue(#id)` rules land here.
        base: SpanStyle,
        /// Cue-level placement (raw-SRT `{\anN}` blocks land here).
        layout: Layout,
        /// `{\anN}`: the first one wins, like the SSA renderers.
        an_set: bool,
        /// Reveal time for spans emitted from here on: set by WebVTT inline
        /// timestamps (`&lt;00:00:00.200&gt;` in the C's markup), absolute on
        /// the cue's timeline.
        reveal_ns: Option<u64>,
    }

    pub(super) fn parse(input: &str, sheet: Option<&Stylesheet>, cue_id: Option<&str>) -> CueIr {
        parse_impl(input, sheet, cue_id, false)
    }

    /// Raw SubRip cue text: same tag machinery, but the input is source text
    /// rather than markup — no entity decoding, only letter-initiated `<...>`
    /// regions are tags (the rest stays literal, mirroring how the C's
    /// escape/unescape pipeline classifies them), and `{\...}` override
    /// blocks are honoured for `\an` and otherwise stripped.
    pub(super) fn parse_srt(input: &str) -> CueIr {
        parse_impl(input, None, None, true)
    }

    fn parse_impl(
        input: &str,
        sheet: Option<&Stylesheet>,
        cue_id: Option<&str>,
        raw: bool,
    ) -> CueIr {
        let sheet = sheet.filter(|s| !s.is_empty());

        // Rules matching the cue root apply to the whole cue: the argless
        // `::cue`, `::cue(#id)` and `::cue(*)`. They become the IR's base
        // style, which spans inherit where their own fields stay `None`.
        let mut base = SpanStyle::default();
        let mut root_ruby = RubyPosition::default();
        if let Some(sheet) = sheet {
            let root = Node {
                id: cue_id,
                ..Node::default()
            };
            sheet.apply(&root, &mut base, &mut root_ruby);
        }

        let mut p = Parser {
            lines: Vec::new(),
            cur_line: Vec::new(),
            cur_text: String::new(),
            ctx: Ctx {
                ruby_position: root_ruby,
                ..Ctx::default()
            },
            stack: Vec::new(),
            rt_text: String::new(),
            sheet,
            base,
            layout: Layout::default(),
            an_set: false,
            reveal_ns: None,
        };

        let bytes = input.as_bytes();
        let mut pos = 0;
        while pos < input.len() {
            match bytes[pos] {
                b'<' => match input[pos + 1..].find('>') {
                    Some(rel) => {
                        let tag = &input[pos + 1..pos + 1 + rel];
                        if raw && !is_srt_tag(tag) {
                            // Not a tag by the C's reckoning (`<3`, `< x>`):
                            // literal text, brackets and all.
                            p.text(&input[pos..pos + rel + 2]);
                        } else {
                            p.handle_tag(tag);
                        }
                        pos += rel + 2;
                    }
                    None => {
                        // No closing '>': treat the rest as text, like the
                        // GMarkup-based C strip does for recovery.
                        p.text(&input[pos..]);
                        break;
                    }
                },
                b'&' if !raw => {
                    // A WebVTT inline timestamp (which the C keeps escaped in
                    // its markup) becomes the reveal time of the spans after
                    // it; everything else is an ordinary entity.
                    if let Some((ns, used)) = parse_timestamp_ref(&input[pos..]) {
                        p.timestamp(ns);
                        pos += used;
                    } else {
                        let (ch, used) = decode_entity(&input[pos..]);
                        match ch {
                            Some(c) => p.push_char(c),
                            None => p.push_char('&'),
                        }
                        pos += used;
                    }
                }
                b'{' if raw => {
                    if bytes.get(pos + 1) != Some(&b'\\') {
                        // A plain brace is ordinary text. (This arm must
                        // consume it: the text arm below stops at '{'.)
                        p.push_char('{');
                        pos += 1;
                    } else {
                        match input[pos + 1..].find('}') {
                            Some(rel) => {
                                p.ssa_block(&input[pos + 1..pos + 1 + rel]);
                                pos += rel + 2;
                            }
                            None => {
                                // Unclosed block: literal, like the ssaparse
                                // text path keeps an unmatched '{'.
                                p.text(&input[pos..]);
                                break;
                            }
                        }
                    }
                }
                b'\n' => {
                    p.end_line();
                    pos += 1;
                }
                _ => {
                    let stops: &[char] = if raw {
                        &['<', '{', '\n']
                    } else {
                        &['<', '&', '\n']
                    };
                    let end = input[pos..]
                        .find(stops)
                        .map(|rel| pos + rel)
                        .unwrap_or(input.len());
                    p.text(&input[pos..end]);
                    pos = end;
                }
            }
        }
        p.finish()
    }

    /// Whether a raw `<...>` body is a tag: whitelisted names may follow
    /// leading spaces (the C's unescape step skips them), anything else needs
    /// a letter right after the optional `/` (the C's unhandled-tag removal
    /// check). Everything else — `<3`, `< junk>`, inline timestamps — stays
    /// literal text.
    fn is_srt_tag(tag: &str) -> bool {
        let body = tag.strip_prefix('/').unwrap_or(tag);
        if body.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return true;
        }
        let name = tag_name(body.trim_start_matches([' ', '\t']));
        matches!(name.as_str(), "i" | "b" | "u" | "s" | "font")
    }

    /// Try to read an escaped WebVTT inline timestamp `&lt;...&gt;` starting
    /// at `input` (which begins with `&`). The C's markup keeps these tags
    /// escaped (they start with a digit, so its unhandled-tag removal spares
    /// them), which is exactly the form that reaches this parser. Returns
    /// the timestamp and how many bytes the whole reference consumed.
    fn parse_timestamp_ref(input: &str) -> Option<(u64, usize)> {
        let rest = input.strip_prefix("&lt;")?;
        let end = rest.find("&gt;")?;
        // Timestamps are short; a distant "&gt;" means this is ordinary text.
        if end > 24 {
            return None;
        }
        let ns = parse_vtt_timestamp(&rest[..end])?;
        Some((ns, 4 + end + 4))
    }

    /// A WebVTT timestamp `[HH:]MM:SS.mmm`, lenient like the rest of the VTT
    /// path: `,` works as the separator too (the C passes the file's bytes
    /// through) and the fraction may be shorter or longer than 3 digits. The
    /// fraction is required — without one, digits around a colon are far
    /// more likely to be prose (`<12:30>`) than a timestamp.
    fn parse_vtt_timestamp(s: &str) -> Option<u64> {
        fn dec(s: &str) -> Option<u64> {
            (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().unwrap())
        }
        let parts: Vec<&str> = s.split(':').collect();
        let (h, m, sec_frac) = match parts.as_slice() {
            [h, m, s] => (dec(h)?, dec(m)?, *s),
            [m, s] => (0, dec(m)?, *s),
            _ => return None,
        };
        let (sec, frac) = sec_frac.split_once(['.', ','])?;
        let sec = dec(sec)?;
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Right-pad / truncate the fraction to nanoseconds.
        let mut frac_ns = 0u64;
        for i in 0..9 {
            frac_ns = frac_ns * 10 + frac.as_bytes().get(i).map_or(0, |b| (b - b'0') as u64);
        }
        // An absurd timestamp is prose, not a reveal marker: reject instead
        // of saturating, so it stays literal text. Renderers turn reveal
        // times into `GstClockTime`, whose `u64::MAX` is the NONE sentinel —
        // a saturated value here used to panic a consumer downstream.
        let ns = h
            .checked_mul(3600)?
            .checked_add(m.checked_mul(60)?)?
            .checked_add(sec)?
            .checked_mul(1_000_000_000)?
            .checked_add(frac_ns)?;
        (ns < u64::MAX).then_some(ns)
    }

    /// Decode one `&...;` reference starting at `input` (which begins with
    /// `&`). Returns the character and how many bytes were consumed; an
    /// unrecognised or unterminated reference consumes only the `&`.
    fn decode_entity(input: &str) -> (Option<char>, usize) {
        let semi = match input[1..].find(';') {
            // Entity names are short; don't scan the whole cue for a stray ';'.
            Some(rel) if rel <= 12 => 1 + rel,
            _ => return (None, 1),
        };
        let name = &input[1..semi];
        let ch = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                } else if let Some(dec) = name.strip_prefix('#') {
                    dec.parse::<u32>().ok().and_then(char::from_u32)
                } else {
                    None
                }
            }
        };
        match ch {
            Some(c) => (Some(c), semi + 1),
            None => (None, 1),
        }
    }

    impl Parser<'_> {
        fn text(&mut self, s: &str) {
            if self.ctx.in_rt {
                self.rt_text.push_str(s);
            } else {
                self.cur_text.push_str(s);
            }
        }

        fn push_char(&mut self, c: char) {
            if self.ctx.in_rt {
                self.rt_text.push(c);
            } else {
                self.cur_text.push(c);
            }
        }

        /// Emit the accumulated text as a span with the current state.
        fn flush(&mut self) {
            if self.cur_text.is_empty() {
                return;
            }
            let text = std::mem::take(&mut self.cur_text);
            // Merge with the previous span when nothing about the styling
            // differs; markup like `a<x>b</x>c` should not fragment "abc".
            if let Some(last) = self.cur_line.last_mut()
                && last.style == self.ctx.style
                && last.voice == self.ctx.voice
                && last.classes == self.ctx.classes
                && last.reveal_ns == self.reveal_ns
                && last.ruby.is_none()
            {
                last.text.push_str(&text);
                return;
            }
            self.cur_line.push(Span {
                text,
                style: self.ctx.style.clone(),
                voice: self.ctx.voice.clone(),
                classes: self.ctx.classes.clone(),
                ruby: None,
                reveal_ns: self.reveal_ns,
            });
        }

        /// A WebVTT inline timestamp: the text after it reveals at `ns`.
        fn timestamp(&mut self, ns: u64) {
            self.flush();
            self.reveal_ns = Some(ns);
        }

        fn end_line(&mut self) {
            self.flush();
            let spans = std::mem::take(&mut self.cur_line);
            self.lines.push(Line { spans });
        }

        fn finish(mut self) -> CueIr {
            // Close whatever is still open (attaches a pending annotation).
            while let Some((name, saved)) = self.stack.pop() {
                self.close_frame(&name, saved);
            }
            self.flush();
            if !self.cur_line.is_empty() || self.lines.is_empty() {
                let spans = std::mem::take(&mut self.cur_line);
                self.lines.push(Line { spans });
            }
            CueIr {
                layout: self.layout,
                lines: self.lines,
                base: self.base,
            }
        }

        /// A `{\...}` override block inside raw SRT text. Never part of the
        /// format, but every player honours them, so `\anN` (numpad) and
        /// `\aN` (legacy) set the cue anchor — first one wins, like the SSA
        /// renderers — and everything else is stripped.
        fn ssa_block(&mut self, block: &str) {
            for seg in block.split('\\').skip(1) {
                let seg = seg.trim();
                let numpad = if let Some(n) = seg.strip_prefix("an") {
                    n.parse::<i32>().ok()
                } else if let Some(n) = seg.strip_prefix('a') {
                    n.parse::<i32>()
                        .ok()
                        .and_then(crate::ssastyle::legacy_to_numpad)
                } else {
                    None
                };
                if !self.an_set
                    && let Some((anchor, align)) = numpad.and_then(crate::ssastyle::numpad_anchor)
                {
                    self.layout.anchor = Some(anchor);
                    self.layout.align = Some(align);
                    self.an_set = true;
                }
            }
        }

        /// `tag` is the text between `<` and `>`.
        fn handle_tag(&mut self, tag: &str) {
            let tag = tag.trim();
            if let Some(rest) = tag.strip_prefix('/') {
                let name = tag_name(rest.trim_start());
                self.close_tag(&name);
            } else {
                self.open_tag(tag);
            }
        }

        fn open_tag(&mut self, tag: &str) {
            let name = tag_name(tag);
            if name.is_empty() {
                return;
            }
            let rest = &tag[name.len()..];
            // The state change happens between spans by definition.
            self.flush();
            let saved = self.ctx.clone();

            // Any WebVTT tag may carry `.class`es after its name; what follows
            // the first whitespace is the annotation (`<v.quiet Fred>`).
            let vtt_node = matches!(
                name.as_str(),
                "i" | "b" | "u" | "c" | "v" | "ruby" | "rt" | "lang"
            );
            let (own_classes, annotation) = if vtt_node {
                split_tag_rest(rest)
            } else {
                (Vec::new(), rest.trim())
            };

            match name.as_str() {
                "i" => self.ctx.style.font_style = Some(FontStyle::Italic),
                "b" => self.ctx.style.font_weight = Some(700),
                "u" => self.ctx.style.underline = Some(true),
                "s" => self.ctx.style.strikethrough = Some(true),
                "big" => self.ctx.style.font_size = Some(FontSize::Scale(1.2)),
                "small" => self.ctx.style.font_size = Some(FontSize::Scale(1.0 / 1.2)),
                "tt" => self.ctx.style.font_family = Some("monospace".to_owned()),
                "sub" => self.ctx.style.baseline_shift = Some(-3.0),
                "sup" => self.ctx.style.baseline_shift = Some(3.0),
                "span" => self.apply_span_attrs(rest),
                "font" => self.apply_font_attrs(rest),
                "v" => {
                    // WebVTT voice: the annotation is the speaker, e.g.
                    // `<v Fred>`.
                    if !annotation.is_empty() {
                        self.ctx.voice = Some(annotation.to_owned());
                    }
                }
                "c" => {}
                "lang" => {
                    if !annotation.is_empty() {
                        self.ctx.style.language = Some(annotation.to_owned());
                    }
                }
                "ruby" => self.ctx.in_ruby = true,
                // <rt> outside <ruby> has no base text to annotate.
                "rt" if self.ctx.in_ruby => self.ctx.in_rt = true,
                "rt" => {}
                // Unknown tag: keep the content, drop the styling.
                _ => {}
            }

            // The standard WebVTT color classes style whatever tag carries
            // them (browsers match them via `::cue(.white)` UA rules); every
            // class is also recorded verbatim on the span.
            for class in &own_classes {
                if let Some(bg) = class.strip_prefix("bg_") {
                    if let Some(c) = vtt_class_color(bg) {
                        self.ctx.style.background = Some(c);
                    }
                } else if let Some(c) = vtt_class_color(class) {
                    self.ctx.style.foreground = Some(c);
                }
                self.ctx.classes.push(class.clone());
            }

            // Author CSS for this node, over the tag-derived (UA-level)
            // styling. Descendant tags then layer their own styling on top,
            // which is exactly CSS inheritance: a value specified on the
            // node beats one inherited from an ancestor.
            if vtt_node && let Some(sheet) = self.sheet {
                let voice = if name == "v" {
                    self.ctx.voice.clone()
                } else {
                    None
                };
                let lang = self.ctx.style.language.clone();
                let node = Node {
                    element: Some(name.as_str()),
                    classes: &own_classes,
                    voice: voice.as_deref(),
                    lang: lang.as_deref(),
                    id: None,
                };
                sheet.apply(&node, &mut self.ctx.style, &mut self.ctx.ruby_position);
            }

            self.stack.push((name, saved));
        }

        fn close_tag(&mut self, name: &str) {
            // Lenient matching: close up to the innermost open tag with this
            // name; a closer that was never opened does nothing.
            let Some(at) = self.stack.iter().rposition(|(n, _)| n == name) else {
                return;
            };
            self.flush();
            while self.stack.len() > at {
                let (n, saved) = self.stack.pop().unwrap();
                self.close_frame(&n, saved);
            }
        }

        /// Restore `saved` for one popped frame, attaching a finished ruby
        /// annotation when the frame is the `<rt>` that collected it.
        fn close_frame(&mut self, name: &str, saved: Ctx) {
            self.flush();
            if name == "rt" && self.ctx.in_rt {
                let text = std::mem::take(&mut self.rt_text);
                if !text.is_empty() {
                    let ruby = Ruby {
                        text,
                        // The context's (CSS `ruby-position`) placement; the
                        // <rt>'s own ctx is still current here.
                        position: self.ctx.ruby_position,
                    };
                    // Annotate the base text: the span just before the <rt>.
                    if let Some(base) = self.cur_line.last_mut() {
                        base.ruby = Some(ruby);
                    } else {
                        // <rt> with no base text: emit an empty base span so
                        // the annotation is not lost.
                        self.cur_line.push(Span {
                            ruby: Some(ruby),
                            ..Span::default()
                        });
                    }
                }
            }
            self.ctx = saved;
        }

        /// `<span key="value" ...>` attributes (the pango set our parsers
        /// emit, plus pango's documented aliases).
        fn apply_span_attrs(&mut self, attrs: &str) {
            for (key, value) in AttrIter::new(attrs) {
                let style = &mut self.ctx.style;
                match key.to_ascii_lowercase().as_str() {
                    "style" => {
                        style.font_style = match value.to_ascii_lowercase().as_str() {
                            "italic" => Some(FontStyle::Italic),
                            "oblique" => Some(FontStyle::Oblique),
                            "normal" => Some(FontStyle::Normal),
                            _ => style.font_style,
                        }
                    }
                    "weight" => {
                        style.font_weight = match value.to_ascii_lowercase().as_str() {
                            "thin" => Some(100),
                            "ultralight" => Some(200),
                            "light" => Some(300),
                            "semilight" => Some(350),
                            "book" => Some(380),
                            "normal" => Some(400),
                            "medium" => Some(500),
                            "semibold" => Some(600),
                            "bold" => Some(700),
                            "ultrabold" => Some(800),
                            "heavy" => Some(900),
                            "ultraheavy" => Some(1000),
                            v => v.parse::<u16>().ok().or(style.font_weight),
                        }
                    }
                    "size" => {
                        if let Some(size) = parse_size(&value) {
                            style.font_size = Some(size);
                        }
                    }
                    "foreground" | "fgcolor" | "color" => {
                        if let Some(c) = Color::parse(&value) {
                            style.foreground = Some(c);
                        }
                    }
                    "background" | "bgcolor" => {
                        if let Some(c) = Color::parse(&value) {
                            style.background = Some(c);
                        }
                    }
                    "font_family" | "face" => {
                        style.font_family = Some(value);
                    }
                    "font" | "font_desc" => {
                        let (family, size) = parse_font_desc(&value);
                        if let Some(f) = family {
                            style.font_family = Some(f);
                        }
                        if let Some(s) = size {
                            style.font_size = Some(FontSize::Points(s));
                        }
                    }
                    "underline" => {
                        style.underline = match value.to_ascii_lowercase().as_str() {
                            "none" => Some(false),
                            "single" | "double" | "low" | "error" => Some(true),
                            _ => style.underline,
                        }
                    }
                    "strikethrough" => {
                        style.strikethrough = match value.to_ascii_lowercase().as_str() {
                            "true" => Some(true),
                            "false" => Some(false),
                            _ => style.strikethrough,
                        }
                    }
                    // 1024ths of a point, like pango.
                    "rise" => {
                        if let Ok(v) = value.parse::<f32>() {
                            style.baseline_shift = Some(v / 1024.0);
                        }
                    }
                    "letter_spacing" => {
                        if let Ok(v) = value.parse::<f32>() {
                            style.letter_spacing = Some(v / 1024.0);
                        }
                    }
                    "lang" => style.language = Some(value),
                    _ => {}
                }
            }
        }

        /// `<font color=... face=... size=...>`: the HTML-ish tag SubRip
        /// files use (the C deletes it; the raw-SRT cue-ir path keeps it).
        /// Values may be quoted or bare; colors are pango-style (named or
        /// `#hex`), sizes the legacy HTML `1..7` ladder.
        fn apply_font_attrs(&mut self, attrs: &str) {
            for (key, value) in AttrIter::new(attrs) {
                let style = &mut self.ctx.style;
                match key.to_ascii_lowercase().as_str() {
                    "color" => {
                        if let Some(c) = Color::parse(&value) {
                            style.foreground = Some(c);
                        }
                    }
                    "face" => style.font_family = Some(value),
                    "size" => {
                        if let Some(s) = html_font_size(&value) {
                            style.font_size = Some(s);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The legacy HTML `<font size>`: absolute `1..7` (3 = medium) or
    /// `+n`/`-n` relative to 3, mapped onto the same 1.2-step ladder the
    /// keyword sizes use.
    fn html_font_size(value: &str) -> Option<FontSize> {
        let v = value.trim();
        let n = v.parse::<i32>().ok()?;
        let abs = if v.starts_with(['+', '-']) { 3 + n } else { n };
        Some(FontSize::Scale(1.2f32.powi(abs.clamp(1, 7) - 3)))
    }

    /// Split a WebVTT tag's post-name text into its `.class` list and its
    /// annotation: `.quiet.fast Fred` -> `(["quiet", "fast"], "Fred")`.
    fn split_tag_rest(rest: &str) -> (Vec<String>, &str) {
        let rest = rest.trim();
        if !rest.starts_with('.') {
            return (Vec::new(), rest);
        }
        let (class_part, annotation) = match rest.find([' ', '\t']) {
            Some(at) => (&rest[..at], rest[at + 1..].trim()),
            None => (rest, ""),
        };
        let classes = class_part
            .split('.')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        (classes, annotation)
    }

    /// The leading alphanumeric run of a tag body (`v Fred` -> `v`,
    /// `c.yellow` -> `c`).
    fn tag_name(tag: &str) -> String {
        tag.chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }

    /// The WebVTT default color classes ("WebVTT cue text tracks display"),
    /// the same eight colors browsers style `::cue(.<name>)` with.
    fn vtt_class_color(name: &str) -> Option<Color> {
        Some(match name {
            "white" => Color::rgb(0xff, 0xff, 0xff),
            "lime" => Color::rgb(0x00, 0xff, 0x00),
            "cyan" => Color::rgb(0x00, 0xff, 0xff),
            "red" => Color::rgb(0xff, 0x00, 0x00),
            "yellow" => Color::rgb(0xff, 0xff, 0x00),
            "magenta" => Color::rgb(0xff, 0x00, 0xff),
            "blue" => Color::rgb(0x00, 0x00, 0xff),
            "black" => Color::rgb(0x00, 0x00, 0x00),
            _ => return None,
        })
    }

    /// The pango `size` attribute: `1024`ths of a point, a named CSS size, or
    /// a relative keyword.
    fn parse_size(value: &str) -> Option<FontSize> {
        let v = value.trim().to_ascii_lowercase();
        // Steps of 1.2 around "medium", like pango's scale factors.
        let scale = |steps: i32| FontSize::Scale(1.2f32.powi(steps));
        Some(match v.as_str() {
            "xx-small" => scale(-3),
            "x-small" => scale(-2),
            "small" | "smaller" => scale(-1),
            "medium" => scale(0),
            "large" | "larger" => scale(1),
            "x-large" => scale(2),
            "xx-large" => scale(3),
            _ => {
                if let Some(pt) = v.strip_suffix("pt") {
                    FontSize::Points(pt.trim().parse::<f32>().ok()?)
                } else {
                    FontSize::Points(v.parse::<f32>().ok()? / 1024.0)
                }
            }
        })
    }

    /// A pango font description like `Sans Bold 18` or just `12`: trailing
    /// number = size in points, the (optional) rest = family.
    fn parse_font_desc(value: &str) -> (Option<String>, Option<f32>) {
        let value = value.trim();
        if value.is_empty() {
            return (None, None);
        }
        let (family, size) = match value.rsplit_once(char::is_whitespace) {
            Some((head, tail)) => match tail.parse::<f32>() {
                Ok(s) => (head.trim(), Some(s)),
                Err(_) => (value, None),
            },
            None => match value.parse::<f32>() {
                Ok(s) => ("", Some(s)),
                Err(_) => (value, None),
            },
        };
        let family = (!family.is_empty()).then(|| family.to_owned());
        (family, size)
    }

    /// Iterator over `key="value"` / `key='value'` attribute pairs.
    struct AttrIter<'a> {
        rest: &'a str,
    }

    impl<'a> AttrIter<'a> {
        fn new(rest: &'a str) -> Self {
            AttrIter { rest }
        }
    }

    impl Iterator for AttrIter<'_> {
        type Item = (String, String);

        fn next(&mut self) -> Option<(String, String)> {
            loop {
                let s = self.rest.trim_start();
                if s.is_empty() {
                    self.rest = s;
                    return None;
                }
                let eq = s.find('=')?;
                let key = s[..eq].trim();
                let after = s[eq + 1..].trim_start();
                let quote = after.chars().next()?;
                if quote != '"' && quote != '\'' {
                    // Unquoted value (common in the wild: `color=red`): runs
                    // to the next whitespace.
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    let value = &after[..end];
                    self.rest = &after[end..];
                    if key.is_empty() || value.is_empty() {
                        continue;
                    }
                    return Some((key.to_owned(), value.to_owned()));
                }
                let close = after[1..].find(quote)?;
                let value = &after[1..1 + close];
                self.rest = &after[1 + close + 1..];
                if key.is_empty() {
                    continue;
                }
                return Some((key.to_owned(), value.to_owned()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_of(ir: &CueIr) -> Vec<&Span> {
        ir.lines.iter().flat_map(|l| l.spans.iter()).collect()
    }

    // ---- colors ---------------------------------------------------------

    #[test]
    fn parses_hex_colors() {
        assert_eq!(Color::parse("#ff0000"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse("#f00"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse("#123456"), Some(Color::rgb(0x12, 0x34, 0x56)));
        assert_eq!(
            Color::parse("#ffffffffffff"),
            Some(Color::rgb(255, 255, 255))
        );
        assert_eq!(Color::parse("#12345"), None);
        assert_eq!(Color::parse("#nothex"), None);
    }

    /// `named_color` binary-searches the table, which is only correct while
    /// the table is strictly sorted.
    #[test]
    fn named_color_table_is_sorted() {
        for pair in NAMED_COLORS.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "NAMED_COLORS out of order: {:?} before {:?}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn parses_named_colors_case_insensitively() {
        assert_eq!(Color::parse("red"), Some(Color::rgb(255, 0, 0)));
        assert_eq!(Color::parse("Yellow"), Some(Color::rgb(255, 255, 0)));
        assert_eq!(
            Color::parse("LightSlateGrey"),
            Some(Color::rgb(119, 136, 153))
        );
        assert_eq!(Color::parse("notacolor"), None);
    }

    // ---- plain text -------------------------------------------------------

    #[test]
    fn plain_text_round_trips() {
        let ir = CueIr::from_plain_text("One\nTwo");
        assert_eq!(ir.lines.len(), 2);
        assert_eq!(ir.plain_text(), "One\nTwo");
        assert!(ir.lines[0].spans[0].style.is_plain());
    }

    #[test]
    fn plain_text_keeps_empty_lines() {
        let ir = CueIr::from_plain_text("a\n\nb");
        assert_eq!(ir.lines.len(), 3);
        assert!(ir.lines[1].spans.is_empty());
        assert_eq!(ir.plain_text(), "a\n\nb");
    }

    // ---- subrip / mpl2 / webvtt tags ---------------------------------------

    #[test]
    fn simple_tags_map_to_styles() {
        let ir = CueIr::from_pango_markup("<i>it</i><b>bo</b><u>un</u>");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
        assert_eq!(spans[1].style.font_weight, Some(700));
        assert_eq!(spans[2].style.underline, Some(true));
        assert_eq!(ir.plain_text(), "itboun");
    }

    #[test]
    fn nested_tags_compose() {
        let ir = CueIr::from_pango_markup("<b><i>x</i>y</b>");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].style.font_weight, Some(700));
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
        assert_eq!(spans[1].style.font_weight, Some(700));
        assert_eq!(spans[1].style.font_style, None);
    }

    #[test]
    fn unclosed_tags_close_at_the_end() {
        let ir = CueIr::from_pango_markup("<i>Seven");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn stray_closers_are_ignored() {
        let ir = CueIr::from_pango_markup("</b>text</i>");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].style.is_plain());
        assert_eq!(spans[0].text, "text");
    }

    #[test]
    fn entities_are_decoded() {
        let ir = CueIr::from_pango_markup("Rock &amp; Roll &lt;5 &#x41;&#66;");
        assert_eq!(ir.plain_text(), "Rock & Roll <5 AB");
    }

    #[test]
    fn adjacent_identical_styling_merges() {
        let ir = CueIr::from_pango_markup("a<unknowntag>b</unknowntag>c");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "abc");
    }

    #[test]
    fn newlines_split_lines_across_open_tags() {
        let ir = CueIr::from_pango_markup("<i>a\nb</i>");
        assert_eq!(ir.lines.len(), 2);
        assert_eq!(
            ir.lines[0].spans[0].style.font_style,
            Some(FontStyle::Italic)
        );
        assert_eq!(
            ir.lines[1].spans[0].style.font_style,
            Some(FontStyle::Italic)
        );
        assert_eq!(ir.plain_text(), "a\nb");
    }

    // ---- microdvd -----------------------------------------------------------

    #[test]
    fn microdvd_span_attrs() {
        let ir = CueIr::from_pango_markup(
            "<span style=\"italic\" weight=\"bold\" size=\"20000\">Hi</span>",
        );
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
        assert_eq!(spans[0].style.font_weight, Some(700));
        assert_eq!(
            spans[0].style.font_size,
            Some(FontSize::Points(20000.0 / 1024.0))
        );
    }

    // ---- sami ---------------------------------------------------------------

    #[test]
    fn sami_foreground_and_family() {
        let ir = CueIr::from_pango_markup(
            "<span foreground=\"#00ffff\" font_family=\"Arial\">Hi</span>",
        );
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, Some(Color::rgb(0, 255, 255)));
        assert_eq!(spans[0].style.font_family.as_deref(), Some("Arial"));
    }

    #[test]
    fn sami_named_foreground() {
        let ir = CueIr::from_pango_markup("<span foreground=\"red\">Red</span>");
        assert_eq!(
            spans_of(&ir)[0].style.foreground,
            Some(Color::rgb(255, 0, 0))
        );
    }

    #[test]
    fn sami_ruby_shape_maps_rise_and_size() {
        // What the SAMI parser emits for <rt> annotations.
        let ir = CueIr::from_pango_markup("<span size='xx-small' rise='-100'> anno </span>\nbase");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].text, " anno ");
        assert_eq!(
            spans[0].style.font_size,
            Some(FontSize::Scale(1.2f32.powi(-3)))
        );
        assert_eq!(spans[0].style.baseline_shift, Some(-100.0 / 1024.0));
        assert_eq!(spans[1].text, "base");
    }

    // ---- qttext ---------------------------------------------------------------

    #[test]
    fn qttext_font_desc_and_colors() {
        let ir = CueIr::from_pango_markup(
            "<span font='Arial 20' bgcolor='#000000' color='#FFFF00' weight='bold'>All</span>",
        );
        let s = &spans_of(&ir)[0].style;
        assert_eq!(s.font_family.as_deref(), Some("Arial"));
        assert_eq!(s.font_size, Some(FontSize::Points(20.0)));
        assert_eq!(s.background, Some(Color::rgb(0, 0, 0)));
        assert_eq!(s.foreground, Some(Color::rgb(255, 255, 0)));
        assert_eq!(s.font_weight, Some(700));
    }

    #[test]
    fn qttext_bare_numeric_font_is_a_size() {
        let ir = CueIr::from_pango_markup("<span font='12'>X</span>");
        let s = &spans_of(&ir)[0].style;
        assert_eq!(s.font_family, None);
        assert_eq!(s.font_size, Some(FontSize::Points(12.0)));
    }

    // ---- webvtt voice / class / ruby -------------------------------------------

    #[test]
    fn vtt_voice_is_attributed() {
        let ir = CueIr::from_pango_markup("<v Fred>Hi there");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].voice.as_deref(), Some("Fred"));
        assert_eq!(spans[0].text, "Hi there");
    }

    #[test]
    fn vtt_color_classes_style_and_record() {
        let ir = CueIr::from_pango_markup("<c.yellow.bg_blue>warn</c>done");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, Some(Color::rgb(255, 255, 0)));
        assert_eq!(spans[0].style.background, Some(Color::rgb(0, 0, 255)));
        assert_eq!(spans[0].classes, vec!["yellow", "bg_blue"]);
        assert!(spans[1].style.is_plain());
    }

    #[test]
    fn vtt_ruby_annotation_attaches_to_base() {
        let ir = CueIr::from_pango_markup("<ruby>base<rt>anno</rt></ruby>tail");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].text, "base");
        assert_eq!(
            spans[0].ruby,
            Some(Ruby {
                text: "anno".to_owned(),
                position: RubyPosition::Over
            })
        );
        assert_eq!(spans[1].text, "tail");
        assert!(spans[1].ruby.is_none());
        // Annotation text is not part of the plain text.
        assert_eq!(ir.plain_text(), "basetail");
    }

    #[test]
    fn vtt_unclosed_ruby_still_attaches() {
        let ir = CueIr::from_pango_markup("<ruby>base<rt>anno");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].text, "base");
        assert_eq!(
            spans[0].ruby.as_ref().map(|r| r.text.as_str()),
            Some("anno")
        );
    }

    // ---- raw subrip text (Cue::raw_text) -----------------------------------------

    #[test]
    fn srt_font_color_tags() {
        let ir = CueIr::from_srt_text(
            "<font color=\"#ff0000\">red</font> and <font color=lime>green</font>",
        );
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, Some(Color::rgb(255, 0, 0)));
        assert_eq!(spans[1].text, " and ");
        assert!(spans[1].style.is_plain());
        assert_eq!(spans[2].style.foreground, Some(Color::rgb(0, 255, 0)));
        assert_eq!(ir.plain_text(), "red and green");
    }

    #[test]
    fn srt_font_face_and_size() {
        let ir = CueIr::from_srt_text("<font face=\"Comic Sans\" size=\"5\">x</font>");
        let s = &spans_of(&ir)[0].style;
        assert_eq!(s.font_family.as_deref(), Some("Comic Sans"));
        assert_eq!(s.font_size, Some(FontSize::Scale(1.2f32.powi(2))));
        // Relative size, unquoted.
        let ir = CueIr::from_srt_text("<font size=+1>x</font>");
        assert_eq!(spans_of(&ir)[0].style.font_size, Some(FontSize::Scale(1.2)));
    }

    #[test]
    fn srt_simple_tags_and_case_insensitivity() {
        let ir = CueIr::from_srt_text("<I>it</I><B>bo</B><u>un</u>");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.font_style, Some(FontStyle::Italic));
        assert_eq!(spans[1].style.font_weight, Some(700));
        assert_eq!(spans[2].style.underline, Some(true));
    }

    #[test]
    fn srt_unclosed_font_closes_at_end() {
        let ir = CueIr::from_srt_text("<font color=\"cyan\">all of it");
        assert_eq!(
            spans_of(&ir)[0].style.foreground,
            Some(Color::rgb(0, 255, 255))
        );
    }

    #[test]
    fn srt_raw_text_is_not_markup() {
        // No entity decoding, literal '<' runs stay literal.
        let ir = CueIr::from_srt_text("5 &lt; 6 & <3 love");
        assert_eq!(ir.plain_text(), "5 &lt; 6 & <3 love");
        // `<3>` has a '>' but is not letter-initiated: literal.
        assert_eq!(CueIr::from_srt_text("a <3> b").plain_text(), "a <3> b");
        // `< junk>` (leading space, not whitelisted): literal, like the C.
        assert_eq!(CueIr::from_srt_text("a < x> b").plain_text(), "a < x> b");
        // `< i>` is whitelisted despite the space (the C unescapes it too).
        let ir = CueIr::from_srt_text("< i>x</i>");
        assert_eq!(spans_of(&ir)[0].style.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn srt_unknown_letter_tags_drop_keeping_content() {
        // What the C's remove_unhandled_tags does to the visible text.
        assert_eq!(
            CueIr::from_srt_text("a <blink>b</blink> c").plain_text(),
            "a b c"
        );
    }

    #[test]
    fn srt_an_blocks_set_the_anchor() {
        let ir = CueIr::from_srt_text("{\\an8}on top");
        assert_eq!(ir.layout.anchor, Some(Anchor::TopCenter));
        assert_eq!(ir.layout.align, Some(TextAlign::Center));
        assert_eq!(ir.plain_text(), "on top");
        // Legacy \a form; first block wins.
        let ir = CueIr::from_srt_text("{\\a6}{\\an1}x");
        assert_eq!(ir.layout.anchor, Some(Anchor::TopCenter));
        // Other override blocks are stripped, plain braces stay.
        assert_eq!(
            CueIr::from_srt_text("{\\pos(1,2)}a {not a tag} b").plain_text(),
            "a {not a tag} b"
        );
        // Unclosed block stays literal.
        assert_eq!(CueIr::from_srt_text("a {\\an8").plain_text(), "a {\\an8");
    }

    // ---- webvtt inline timestamps (karaoke) ---------------------------------

    #[test]
    fn inline_timestamps_become_reveal_times() {
        // What the C's VTT markup carries for karaoke cues.
        let ir = CueIr::from_pango_markup(
            "One... &lt;00:00:00,200&gt;Two... &lt;00:00:00,500&gt;Three...",
        );
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].text, "One... ");
        assert_eq!(spans[0].reveal_ns, None);
        assert_eq!(spans[1].text, "Two... ");
        assert_eq!(spans[1].reveal_ns, Some(200_000_000));
        assert_eq!(spans[2].text, "Three...");
        assert_eq!(spans[2].reveal_ns, Some(500_000_000));
        // The timestamps are markers, not text.
        assert_eq!(ir.plain_text(), "One... Two... Three...");
    }

    #[test]
    fn inline_timestamp_variants() {
        // Dot separator, hour component, hour-less form, short fraction.
        let ir = CueIr::from_pango_markup("&lt;01:02:03.004&gt;x");
        assert_eq!(
            spans_of(&ir)[0].reveal_ns,
            Some((3600 + 2 * 60 + 3) * 1_000_000_000 + 4_000_000)
        );
        let ir = CueIr::from_pango_markup("&lt;00:01.5&gt;x");
        assert_eq!(spans_of(&ir)[0].reveal_ns, Some(1_500_000_000));
    }

    #[test]
    fn inline_timestamps_keep_the_surrounding_styling() {
        let ir = CueIr::from_pango_markup("<c.yellow>la&lt;00:00:01.000&gt;la</c>");
        let spans = spans_of(&ir);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].reveal_ns, None);
        assert_eq!(spans[1].reveal_ns, Some(1_000_000_000));
        for s in spans {
            assert_eq!(s.style.foreground, Some(Color::rgb(255, 255, 0)));
            assert_eq!(s.classes, vec!["yellow"]);
        }
    }

    #[test]
    fn absurd_inline_timestamps_stay_literal_text() {
        // A saturating parse used to hand consumers u64::MAX, which is
        // GST_CLOCK_TIME_NONE. Now it is not a timestamp at all.
        let ir = CueIr::from_pango_markup("&lt;999999999:00:00.0&gt;x");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].reveal_ns, None);
        assert_eq!(ir.plain_text(), "<999999999:00:00.0>x");
    }

    #[test]
    fn non_timestamps_stay_literal_text() {
        // No fraction (prose like "<12:30>"), no colon, junk: all literal.
        assert_eq!(
            CueIr::from_pango_markup("see you &lt;12:30&gt;").plain_text(),
            "see you <12:30>"
        );
        assert_eq!(
            CueIr::from_pango_markup("&lt;123&gt;").plain_text(),
            "<123>"
        );
        assert_eq!(
            CueIr::from_pango_markup("a &lt; b &gt; c").plain_text(),
            "a < b > c"
        );
    }

    #[test]
    fn cue_to_ir_prefers_raw_text() {
        // The parity text lost the font tag; the raw text keeps it.
        let mut cue = Cue::new(0, Some(1), "colored");
        cue.raw_text = Some("<font color=\"red\">colored</font>".to_owned());
        let ir = cue_to_ir(&cue, OutputFormat::PangoMarkup, None);
        assert_eq!(
            spans_of(&ir)[0].style.foreground,
            Some(Color::rgb(255, 0, 0))
        );
        assert_eq!(ir.plain_text(), "colored");
    }

    // ---- webvtt stylesheets (STYLE blocks) --------------------------------------

    fn styled(markup: &str, css: &str) -> CueIr {
        let sheet = Stylesheet::parse(css);
        CueIr::from_pango_markup_styled(markup, Some(&sheet), None)
    }

    #[test]
    fn argless_cue_rule_becomes_base_style() {
        let ir = styled(
            "plain <b>bold</b>",
            "::cue { color: yellow; background: black }",
        );
        assert_eq!(ir.base.foreground, Some(Color::rgb(255, 255, 0)));
        assert_eq!(ir.base.background, Some(Color::rgb(0, 0, 0)));
        // Spans keep their own fields None (inheritance is the renderer's).
        assert_eq!(spans_of(&ir)[0].style.foreground, None);
    }

    #[test]
    fn type_rule_styles_matching_tags() {
        let ir = styled("a<b>bee</b><i>eye</i>", "::cue(b) { color: red }");
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, None);
        assert_eq!(spans[1].style.foreground, Some(Color::rgb(255, 0, 0)));
        assert_eq!(spans[1].style.font_weight, Some(700));
        assert_eq!(spans[2].style.foreground, None);
    }

    #[test]
    fn author_css_overrides_tag_styling() {
        let ir = styled("<i>x</i>", "::cue(i) { font-style: normal }");
        assert_eq!(spans_of(&ir)[0].style.font_style, Some(FontStyle::Normal));
    }

    #[test]
    fn author_css_overrides_default_color_classes() {
        let ir = styled("<c.yellow>x</c>", "::cue(.yellow) { color: #123456 }");
        let span = &spans_of(&ir)[0];
        assert_eq!(span.style.foreground, Some(Color::rgb(0x12, 0x34, 0x56)));
        assert_eq!(span.classes, vec!["yellow"]);
    }

    #[test]
    fn voice_rule_matches_annotation() {
        let ir = styled(
            "<v Fred>hi</v><v Wilma>ho</v>",
            "::cue(v[voice=\"Fred\"]) { color: red }",
        );
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, Some(Color::rgb(255, 0, 0)));
        assert_eq!(spans[1].style.foreground, None);
    }

    #[test]
    fn classes_and_annotation_split_on_any_tag() {
        // `<v.loud Fred>`: classes before the space, the voice after it.
        let ir = styled("<v.loud Fred>hi", "::cue(.loud) { color: red }");
        let span = &spans_of(&ir)[0];
        assert_eq!(span.voice.as_deref(), Some("Fred"));
        assert_eq!(span.classes, vec!["loud"]);
        assert_eq!(span.style.foreground, Some(Color::rgb(255, 0, 0)));
        // `<b.loud>` records its classes too (previously only <c> did).
        let ir = CueIr::from_pango_markup("<b.loud>x</b>");
        assert_eq!(spans_of(&ir)[0].classes, vec!["loud"]);
    }

    #[test]
    fn descendant_tag_styling_beats_inherited_author_value() {
        // CSS: a value specified on the node (the UA italic on <i>) wins over
        // one inherited from an ancestor's author rule.
        let ir = styled("<v Fred><i>x</i></v>", "::cue(v) { font-style: normal }");
        assert_eq!(spans_of(&ir)[0].style.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn deeper_author_rule_beats_shallower_one() {
        let ir = styled(
            "<v Fred><c>x</c></v>",
            "::cue(v) { color: blue } ::cue(c) { color: red }",
        );
        assert_eq!(
            spans_of(&ir)[0].style.foreground,
            Some(Color::rgb(255, 0, 0))
        );
    }

    #[test]
    fn id_rule_matches_cue_identifier() {
        let sheet = Stylesheet::parse("::cue(#intro) { color: red }");
        let ir = CueIr::from_pango_markup_styled("x", Some(&sheet), Some("intro"));
        assert_eq!(ir.base.foreground, Some(Color::rgb(255, 0, 0)));
        let ir = CueIr::from_pango_markup_styled("x", Some(&sheet), Some("outro"));
        assert_eq!(ir.base.foreground, None);
    }

    #[test]
    fn lang_rule_matches_lang_tags() {
        let ir = styled(
            "<lang en-GB>tea</lang><lang fr>café</lang>",
            "::cue(:lang(en)) { color: red }",
        );
        let spans = spans_of(&ir);
        assert_eq!(spans[0].style.foreground, Some(Color::rgb(255, 0, 0)));
        assert_eq!(spans[0].style.language.as_deref(), Some("en-GB"));
        assert_eq!(spans[1].style.foreground, None);
    }

    #[test]
    fn ruby_position_rule_moves_annotation() {
        let ir = styled(
            "<ruby>base<rt>anno</rt></ruby>",
            "::cue(rt) { ruby-position: under }",
        );
        let span = &spans_of(&ir)[0];
        assert_eq!(
            span.ruby,
            Some(Ruby {
                text: "anno".to_owned(),
                position: RubyPosition::Under
            })
        );
    }

    #[test]
    fn cue_to_ir_feeds_id_and_sheet() {
        let sheet = Stylesheet::parse("::cue(#greeting) { color: lime }");
        let mut cue = Cue::new(0, Some(1), "hello");
        cue.id = Some("greeting".to_owned());
        let ir = cue_to_ir(&cue, OutputFormat::PangoMarkup, Some(&sheet));
        assert_eq!(ir.base.foreground, Some(Color::rgb(0, 255, 0)));
    }

    #[test]
    fn empty_sheet_changes_nothing() {
        let sheet = Stylesheet::parse("");
        let with = CueIr::from_pango_markup_styled("<i>x</i>", Some(&sheet), None);
        let without = CueIr::from_pango_markup("<i>x</i>");
        assert_eq!(with, without);
    }

    // ---- cue settings ------------------------------------------------------------

    #[test]
    fn cue_settings_fold_into_layout() {
        let mut cue = Cue::new(0, Some(1), "x");
        cue.settings = CueSettings {
            line_position: Some(10),
            text_position: Some(50),
            text_size: Some(35),
            vertical: Some("vertical-lr".to_owned()),
            alignment: Some("middle".to_owned()),
            ..CueSettings::default()
        };
        let ir = cue_to_ir(&cue, OutputFormat::PangoMarkup, None);
        assert_eq!(ir.layout.line, Some(LinePosition::Percent(10.0)));
        assert_eq!(ir.layout.position, Some(50.0));
        assert_eq!(ir.layout.size, Some(35.0));
        assert_eq!(ir.layout.writing_mode, WritingMode::VerticalLr);
        assert_eq!(ir.layout.align, Some(TextAlign::Center));
    }

    #[test]
    fn modern_cue_settings_fold_into_layout() {
        let mut cue = Cue::new(0, Some(1), "x");
        cue.settings = CueSettings {
            line_number: Some(-1),
            line_align: Some("end".to_owned()),
            text_position: Some(50),
            position_align: Some("line-left".to_owned()),
            vertical: Some("rl".to_owned()),
            alignment: Some("center".to_owned()),
            ..CueSettings::default()
        };
        let ir = cue_to_ir(&cue, OutputFormat::PangoMarkup, None);
        assert_eq!(ir.layout.line, Some(LinePosition::Line(-1)));
        assert_eq!(ir.layout.line_align, Some(LineAlign::End));
        assert_eq!(ir.layout.position, Some(50.0));
        assert_eq!(ir.layout.position_align, Some(PositionAlign::LineLeft));
        assert_eq!(ir.layout.writing_mode, WritingMode::VerticalRl);
        assert_eq!(ir.layout.align, Some(TextAlign::Center));
    }

    #[test]
    fn cue_to_ir_trims_trailing_newlines() {
        let cue = Cue::new(0, Some(1), "One\n\n");
        let ir = cue_to_ir(&cue, OutputFormat::Utf8, None);
        assert_eq!(ir.lines.len(), 1);
        assert_eq!(ir.plain_text(), "One");
    }

    // ---- ir plain text matches the element's strip ---------------------------------

    #[test]
    fn markup_plain_text_matches_strip_semantics() {
        // The same inputs subparse's strip_pango_markup handles.
        assert_eq!(CueIr::from_pango_markup("<i>Six</i>").plain_text(), "Six");
        assert_eq!(
            CueIr::from_pango_markup("gave <i>Rock &amp; Roll</i> to").plain_text(),
            "gave Rock & Roll to"
        );
        assert_eq!(
            CueIr::from_pango_markup("a &lt; b &unknown; &#177;").plain_text(),
            format!("a < b &unknown; {}", char::from_u32(177).unwrap())
        );
    }
}
