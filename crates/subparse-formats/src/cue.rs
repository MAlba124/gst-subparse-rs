// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Core types shared by every format parser. Dependency-free (std only).

/// Text flavour a parser emits, mirroring subparse's `text/x-raw` `format` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Plain UTF-8 (`format=utf8`).
    Utf8,
    /// Pango markup (`format=pango-markup`).
    PangoMarkup,
}

/// Optional per-cue presentation settings. The first five fields mirror the
/// positioning fields of the C `ParserState` (the archaic `T:`/`L:`/`S:`/
/// `D:`/`A:` WebVTT syntax); the rest only exist in the modern
/// `name:value` syntax, which the C never parsed at all. Non-WebVTT parsers
/// leave this default. Like the C, the pango-markup output discards all of
/// it; the `cue-ir` path folds it into the IR's layout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CueSettings {
    /// Line position, percent (`L:10%`, `line:10%`).
    pub line_position: Option<u8>,
    /// Text position, percent (`T:50%`, `position:50%`).
    pub text_position: Option<u8>,
    /// Text size, percent (`S:35%`, `size:35%`).
    pub text_size: Option<u8>,
    /// `D:` / `vertical:` value: "vertical"/"rl", "vertical-lr"/"lr".
    pub vertical: Option<String>,
    /// `A:` / `align:` value: "start", "middle", "center", "end", ...
    pub alignment: Option<String>,
    /// Modern `line:<int>` form: a line *number*, 0-based from the start
    /// edge, negative from the end edge.
    pub line_number: Option<i32>,
    /// Modern `line:...,<align>` suffix: "start", "center", "end".
    pub line_align: Option<String>,
    /// Modern `position:...,<align>` suffix: "line-left", "center",
    /// "line-right".
    pub position_align: Option<String>,
}

/// A parsed subtitle cue. Timing is nanoseconds from stream start (wraps to
/// `GstClockTime` in the element). `end_ns` is exclusive. `None` means open /
/// "until the next cue".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    /// Presentation start, nanoseconds.
    pub start_ns: u64,
    /// Presentation end (exclusive), nanoseconds.
    pub end_ns: Option<u64>,
    /// Cue payload: plain UTF-8 or Pango markup per the parser's `output_format`.
    pub text: String,
    /// Optional presentation settings (WebVTT).
    pub settings: CueSettings,
    /// The cue identifier (WebVTT: the line preceding the timing line). Only
    /// consumed by the `cue-ir` output path (`::cue(#id)` selectors); the
    /// pango-markup output ignores it, like the C element.
    pub id: Option<String>,
    /// SSA/ASS extras (the raw Text field with override blocks intact, style
    /// name, margin overrides). Only consumed by the `cue-ir` output path;
    /// the pango-markup output ignores it, like the C element.
    pub ssa: Option<Box<crate::ssastyle::SsaDialogue>>,
    /// The cue's source text before the markup pipeline, for formats whose
    /// pango transform is lossy (SubRip: the C whitelist keeps only
    /// `<i>/<b>/<u>` and deletes e.g. `<font color>`). Only consumed by the
    /// `cue-ir` output path ([`crate::ir::CueIr::from_srt_text`]); the
    /// pango-markup output ignores it, like the C element.
    pub raw_text: Option<String>,
}

impl Cue {
    /// Convenience constructor with default (empty) settings.
    pub fn new(start_ns: u64, end_ns: Option<u64>, text: impl Into<String>) -> Self {
        Cue {
            start_ns,
            end_ns,
            text: text.into(),
            settings: CueSettings::default(),
            id: None,
            ssa: None,
            raw_text: None,
        }
    }

    /// Duration in nanoseconds, if the cue has an end.
    pub fn duration_ns(&self) -> Option<u64> {
        self.end_ns.map(|e| e.saturating_sub(self.start_ns))
    }
}

/// Context a parser may need beyond the raw text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParseContext {
    /// Frames/second for frame-based formats (MicroDVD) as `(num, den)`.
    /// `None` means "use the format's default" (parsers decide the default).
    pub fps: Option<(u32, u32)>,
}

/// A parse failure. Parsers should mirror the C's lenient recovery (skip and
/// continue on malformed cues). Reserve `Invalid` for input that is not this
/// format at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The input is not valid for this format.
    Invalid(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Invalid(msg) => write!(f, "invalid subtitle input: {msg}"),
        }
    }
}

impl std::error::Error for ParseError {}
