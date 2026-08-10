// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The parser trait and the format registry.

use crate::cue::{Cue, OutputFormat, ParseContext, ParseError};

/// What one [`SubtitleFormat::parse_incremental`] call produced.
///
/// `consumed` is the number of **leading bytes of `body` the parser is done
/// with**. The caller must drop exactly that prefix before the next call. It is
/// always a `char` boundary and never exceeds `body.len()`, so
/// `String::drain(..consumed)` is always safe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Cues completed by this call, in order.
    pub cues: Vec<Cue>,
    /// Leading bytes of `body` the caller must now drop.
    pub consumed: usize,
}

/// A subtitle format parser.
///
/// Implementations are **dependency-free** (std only) and operate on a fully
/// decoded, UTF-8, newline-normalized (`\n`) body. Decoding and charset
/// handling live in the `gst-subparse` element, not here.
///
/// Style: prefer a small hand-written lexer feeding a simple recursive-descent
/// (or line-oriented) parser. Reach for Pratt parsing only where the grammar
/// has real nesting/precedence (e.g. SAMI tags, styled markup).
///
/// # Streaming contract
///
/// A parser instance is a **stateful stream position**, not a pure function.
/// [`parse_incremental`](SubtitleFormat::parse_incremental) is fed a sliding
/// window over one body: each call gets the bytes the previous call did not
/// consume, plus whatever arrived since. Concretely, the caller must maintain
///
/// ```text
/// buf.push_str(new_data);
/// let parsed = parser.parse_incremental(&buf, &ctx, at_eos)?;
/// buf.drain(..parsed.consumed);
/// ```
///
/// and the parser may assume it. Feeding an instance an unrelated body, or
/// draining something other than `consumed`, produces garbage rather than a
/// panic, but it is a contract violation either way.
///
/// The property every implementation owes: for any chunking of a body,
/// concatenating the `cues` of the successive calls (the last with
/// `at_eos == true`) equals a single `at_eos` call over the whole body. The
/// per-format `chunked_matches_whole` tests pin exactly that.
pub trait SubtitleFormat {
    /// Parse as much of `body` as forms **complete** records.
    ///
    /// Returns those cues and how many leading bytes of `body` are finished
    /// with. An incomplete trailing record is left for the next call, so
    /// `consumed` is normally short of `body.len()`.
    ///
    /// `at_eos` says no more data is coming: the parser may then finalise a
    /// trailing record (whatever its format's end-of-stream rule is) and must
    /// report `consumed == body.len()`.
    ///
    /// Be lenient on malformed records (skip and continue), matching the C
    /// parser's recovery behaviour. A malformed record must still advance
    /// `consumed`; refusing to consume it would wedge the stream.
    fn parse_incremental(
        &mut self,
        body: &str,
        ctx: &ParseContext,
        at_eos: bool,
    ) -> Result<Parsed, ParseError>;

    /// Parse a whole body into ordered cues, in one shot.
    ///
    /// This is exactly `parse_incremental(body, ctx, true)`, i.e. it treats
    /// `body` as the complete stream. It is the convenient entry point for
    /// tests, benches and [`parse_with`]; the streaming element uses
    /// [`parse_incremental`](SubtitleFormat::parse_incremental) directly.
    ///
    /// Like every other method here it takes `&mut self`, so calling it on an
    /// instance that has already been fed *continues* that stream rather than
    /// starting a new one. Call it on a fresh parser.
    fn parse(&mut self, body: &str, ctx: &ParseContext) -> Result<Vec<Cue>, ParseError> {
        Ok(self.parse_incremental(body, ctx, true)?.cues)
    }

    /// The text flavour this parser emits.
    fn output_format(&self) -> OutputFormat;

    /// The stylesheet this stream has declared so far, if the format has such
    /// a concept (WebVTT `STYLE` blocks). `STYLE` blocks precede the cues
    /// they style, so by the time a cue comes out of
    /// [`parse_incremental`](SubtitleFormat::parse_incremental) the sheet it
    /// should be rendered with is already here. The `cue-ir` output path
    /// feeds it to [`crate::ir::cue_to_ir`]; pango-markup output never reads
    /// it (the C element ignores styling, and parity holds).
    fn stylesheet(&self) -> Option<&crate::vttcss::Stylesheet> {
        None
    }
}

/// Splits an incrementally-growing body into complete (`\n`-terminated) lines.
///
/// Every format here is line-oriented, and every one of them needs the same two
/// things: hand out only whole lines, and remember how far it has already
/// looked so a body with no newline in it is not rescanned from the start on
/// every call. That second part is what keeps a pathological input (a huge file
/// with no line breaks at all) linear rather than quadratic, since without it
/// each call would re-scan the whole retained remainder.
#[derive(Debug, Default)]
pub struct LineScanner {
    /// Length of the remainder handed back last time. Those leading bytes of
    /// the next `body` are known to contain no `\n`, so scanning starts past
    /// them.
    scanned: usize,
}

impl LineScanner {
    /// A scanner positioned at the start of a stream.
    pub fn new() -> Self {
        LineScanner { scanned: 0 }
    }

    /// Call `line` for each complete `\n`-terminated line in `body`, with the
    /// terminator and one optional preceding `\r` removed. Returns how many
    /// bytes were consumed (always just past the last `\n`, or `0`).
    pub fn feed<F>(&mut self, body: &str, mut line: F) -> usize
    where
        F: FnMut(&str),
    {
        let bytes = body.as_bytes();
        let mut consumed = 0usize;
        // `\n` is ASCII and can never be a UTF-8 continuation byte, so a byte
        // search is safe even if `scanned` did not land on a char boundary
        // (it always does, but this way that is not load-bearing).
        let mut search = self.scanned.min(bytes.len());

        while let Some(rel) = bytes[search..].iter().position(|&b| b == b'\n') {
            let nl = search + rel;
            let raw = &body[consumed..nl];
            line(raw.strip_suffix('\r').unwrap_or(raw));
            consumed = nl + 1;
            search = consumed;
        }

        self.scanned = bytes.len() - consumed;
        consumed
    }

    /// Reset to the start of a stream.
    pub fn reset(&mut self) {
        self.scanned = 0;
    }
}

/// The subtitle formats recognized by autodetection (see [`crate::autodetect`]).
///
/// Mirrors `GST_SUB_PARSE_FORMAT_*` in the C `gstsubparse.h`. `Ssa` is exposed
/// upstream as the separate `ssaparse` element.
///
/// Every variant is gated behind its per-format Cargo feature. With
/// `--no-default-features` (no format enabled) this is an empty, uninhabited
/// enum, and every `match` on it degrades to `match self {}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    #[cfg(feature = "subrip")]
    SubRip,
    #[cfg(feature = "microdvd")]
    MicroDvd,
    #[cfg(feature = "mpl2")]
    Mpl2,
    #[cfg(feature = "subviewer")]
    SubViewer,
    #[cfg(feature = "sami")]
    Sami,
    #[cfg(feature = "tmplayer")]
    TmPlayer,
    #[cfg(feature = "mpsub")]
    MpSub,
    #[cfg(feature = "qttext")]
    QtText,
    #[cfg(feature = "lrc")]
    Lrc,
    #[cfg(feature = "dks")]
    Dks,
    #[cfg(feature = "webvtt")]
    WebVtt,
    #[cfg(feature = "ssa")]
    Ssa,
}

impl Format {
    /// Every format compiled into this build, in declaration order.
    ///
    /// Kept next to the enum so that adding a variant without adding it here is
    /// an obvious omission, and so table-driven tests (notably the incremental
    /// / chunk-equivalence suite) cover a new format automatically instead of
    /// silently skipping it.
    pub const ALL: &'static [Format] = &[
        #[cfg(feature = "subrip")]
        Format::SubRip,
        #[cfg(feature = "microdvd")]
        Format::MicroDvd,
        #[cfg(feature = "mpl2")]
        Format::Mpl2,
        #[cfg(feature = "subviewer")]
        Format::SubViewer,
        #[cfg(feature = "sami")]
        Format::Sami,
        #[cfg(feature = "tmplayer")]
        Format::TmPlayer,
        #[cfg(feature = "mpsub")]
        Format::MpSub,
        #[cfg(feature = "qttext")]
        Format::QtText,
        #[cfg(feature = "lrc")]
        Format::Lrc,
        #[cfg(feature = "dks")]
        Format::Dks,
        #[cfg(feature = "webvtt")]
        Format::WebVtt,
        #[cfg(feature = "ssa")]
        Format::Ssa,
    ];

    /// Construct a fresh parser for this format, positioned at the start of a
    /// stream.
    ///
    /// `Send` is part of the contract because the parser is now long-lived: an
    /// element keeps one for the whole stream inside its state mutex, and that
    /// state has to be `Send` for the mutex to be `Sync`.
    pub fn parser(self) -> Box<dyn SubtitleFormat + Send> {
        // Gated so the glob is not an unused import when no format is enabled
        // (then the body is just `match self {}`).
        #[cfg(any(
            feature = "subrip",
            feature = "microdvd",
            feature = "mpl2",
            feature = "subviewer",
            feature = "sami",
            feature = "tmplayer",
            feature = "mpsub",
            feature = "qttext",
            feature = "lrc",
            feature = "dks",
            feature = "webvtt",
            feature = "ssa",
        ))]
        use crate::formats::*;
        match self {
            #[cfg(feature = "subrip")]
            Format::SubRip => Box::<SubRip>::default(),
            #[cfg(feature = "microdvd")]
            Format::MicroDvd => Box::<MicroDvd>::default(),
            #[cfg(feature = "mpl2")]
            Format::Mpl2 => Box::<Mpl2>::default(),
            #[cfg(feature = "subviewer")]
            Format::SubViewer => Box::<SubViewer>::default(),
            #[cfg(feature = "sami")]
            Format::Sami => Box::<Sami>::default(),
            #[cfg(feature = "tmplayer")]
            Format::TmPlayer => Box::<TmPlayer>::default(),
            #[cfg(feature = "mpsub")]
            Format::MpSub => Box::<MpSub>::default(),
            #[cfg(feature = "qttext")]
            Format::QtText => Box::<QtText>::default(),
            #[cfg(feature = "lrc")]
            Format::Lrc => Box::<Lrc>::default(),
            #[cfg(feature = "dks")]
            Format::Dks => Box::<Dks>::default(),
            #[cfg(feature = "webvtt")]
            Format::WebVtt => Box::<WebVtt>::default(),
            #[cfg(feature = "ssa")]
            Format::Ssa => Box::<Ssa>::default(),
        }
    }

    /// The upstream sink-pad media type this format is advertised as.
    ///
    /// These strings match the static caps the C typefind suggests one-for-one
    /// (see `gst_sub_parse_type_find` and the `*_CAPS` `#define`s in
    /// `gstsubparseelement.c`). Note TMPlayer and MPL2 have their *own* media
    /// types (`-tmplayer` / `-mpl2`), not the generic `application/x-subtitle`.
    pub fn media_type(self) -> &'static str {
        // SubRip, MicroDvd, MpSub and SubViewer share the generic type
        // (C `SUB_CAPS`); the rest have their own. Every variant is its own arm
        // (no `_`) so the match stays exhaustive under any feature subset and
        // degrades to `match self {}` when no format is enabled.
        match self {
            #[cfg(feature = "sami")]
            Format::Sami => "application/x-subtitle-sami",
            #[cfg(feature = "tmplayer")]
            Format::TmPlayer => "application/x-subtitle-tmplayer",
            #[cfg(feature = "mpl2")]
            Format::Mpl2 => "application/x-subtitle-mpl2",
            #[cfg(feature = "dks")]
            Format::Dks => "application/x-subtitle-dks",
            #[cfg(feature = "qttext")]
            Format::QtText => "application/x-subtitle-qttext",
            #[cfg(feature = "lrc")]
            Format::Lrc => "application/x-subtitle-lrc",
            #[cfg(feature = "webvtt")]
            Format::WebVtt => "application/x-subtitle-vtt",
            #[cfg(feature = "ssa")]
            Format::Ssa => "application/x-ssa",
            #[cfg(feature = "subrip")]
            Format::SubRip => "application/x-subtitle",
            #[cfg(feature = "microdvd")]
            Format::MicroDvd => "application/x-subtitle",
            #[cfg(feature = "mpsub")]
            Format::MpSub => "application/x-subtitle",
            #[cfg(feature = "subviewer")]
            Format::SubViewer => "application/x-subtitle",
        }
    }
}

/// Convenience: detect nothing, just run a specific format's parser.
pub fn parse_with(format: Format, body: &str, ctx: &ParseContext) -> Result<Vec<Cue>, ParseError> {
    format.parser().parse(body, ctx)
}

#[cfg(test)]
mod tests {
    use super::Format;

    /// The media types must match the C typefind's `*_CAPS` one-for-one. The
    /// two easy-to-get-wrong ones are TMPlayer and MPL2, which the C maps to
    /// their own `-tmplayer` / `-mpl2` types rather than `application/x-subtitle`.
    #[test]
    fn media_type_matches_c_static_caps() {
        #[cfg(feature = "subrip")]
        assert_eq!(Format::SubRip.media_type(), "application/x-subtitle");
        #[cfg(feature = "microdvd")]
        assert_eq!(Format::MicroDvd.media_type(), "application/x-subtitle");
        #[cfg(feature = "mpsub")]
        assert_eq!(Format::MpSub.media_type(), "application/x-subtitle");
        #[cfg(feature = "subviewer")]
        assert_eq!(Format::SubViewer.media_type(), "application/x-subtitle");
        #[cfg(feature = "sami")]
        assert_eq!(Format::Sami.media_type(), "application/x-subtitle-sami");
        #[cfg(feature = "tmplayer")]
        assert_eq!(
            Format::TmPlayer.media_type(),
            "application/x-subtitle-tmplayer"
        );
        #[cfg(feature = "mpl2")]
        assert_eq!(Format::Mpl2.media_type(), "application/x-subtitle-mpl2");
        #[cfg(feature = "dks")]
        assert_eq!(Format::Dks.media_type(), "application/x-subtitle-dks");
        #[cfg(feature = "qttext")]
        assert_eq!(Format::QtText.media_type(), "application/x-subtitle-qttext");
        #[cfg(feature = "lrc")]
        assert_eq!(Format::Lrc.media_type(), "application/x-subtitle-lrc");
        #[cfg(feature = "webvtt")]
        assert_eq!(Format::WebVtt.media_type(), "application/x-subtitle-vtt");
        #[cfg(feature = "ssa")]
        assert_eq!(Format::Ssa.media_type(), "application/x-ssa");
    }
}
