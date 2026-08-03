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

/// Optional per-cue presentation settings. Mirrors the positioning fields of the
/// C `ParserState` (WebVTT cue settings). Non-WebVTT parsers leave this default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CueSettings {
    /// Line position, percent.
    pub line_position: Option<u8>,
    /// Text position, percent.
    pub text_position: Option<u8>,
    /// Text size, percent.
    pub text_size: Option<u8>,
    /// "", "vertical", "vertical-lr".
    pub vertical: Option<String>,
    /// "start", "middle", "end", ...
    pub alignment: Option<String>,
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
}

impl Cue {
    /// Convenience constructor with default (empty) settings.
    pub fn new(start_ns: u64, end_ns: Option<u64>, text: impl Into<String>) -> Self {
        Cue {
            start_ns,
            end_ns,
            text: text.into(),
            settings: CueSettings::default(),
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
