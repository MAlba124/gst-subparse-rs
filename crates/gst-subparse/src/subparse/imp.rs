// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The `subparse` element.
//!
//! Behaviour is modeled on the upstream C `GstSubParse`
//! (`gst-plugins-base/gst/subparse/gstsubparse.c`) and structured after
//! gst-plugins-rs' `closedcaption` `scc_parse` element.
//!
//! # How this differs from the C, and why
//!
//! The C parser is *line-incremental*. It feeds one line at a time to a
//! stateful `parse_line` callback that emits a cue whenever a cue's terminating
//! blank line is consumed. The `subparse-formats` parsers here work the same
//! way, but one call ahead: [`SubtitleFormat::parse_incremental`] takes as much
//! of the accumulated body as it has, parses whatever forms **complete**
//! records, and reports how many bytes it is done with.
//!
//! So this element holds **one** parser instance for the whole stream (built
//! when the format is detected), hands it the decoded body on every chain call,
//! pushes the cues it returns, and drops the prefix it consumed. Total work is
//! therefore linear in the input: every byte is parsed exactly once and
//! `textbuf` never holds more than one partial record.
//!
//! This replaced a whole-body `parse(body) -> Vec<Cue>` on every chain call,
//! which was O(chunks x size) and kept the entire file in memory. That version
//! also had to *guess* which cues were safe to push (hold back the last one
//! unless the body already ended in a blank line) and to force-append `"\n\n"`
//! at EOS for the blank-line formats. Neither is needed now: the parser answers
//! "is this record complete?" directly, and `at_eos` tells it when to finalise a
//! trailing record. The `"\n\n"` is gone because `parse_incremental(.., true)`
//! *is* the end-of-stream flush those two bytes were emulating.
//!
//! EOS is a two-step at this layer, and the order matters. First the charset
//! decoder is told the stream ended, which resolves an incomplete multi-byte
//! tail and forces the [`crate::encoding`] sniff to commit; only then is the
//! decoded body read and parsed with `at_eos`.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use subparse_formats::{Format, OutputFormat, ParseContext, SubtitleFormat, autodetect, ir};

use crate::cueir::{CueIrMeta, TextFormat};
use crate::encoding::Decoder;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rssubparse",
        gst::DebugColorFlags::empty(),
        Some("Subtitle parser element"),
    )
});

const DEFAULT_FPS_N: u32 = 24000;
const DEFAULT_FPS_D: u32 = 1001;

/// How much decoded text may pile up with no line break in it before format
/// autodetection stops waiting for one and decides anyway.
///
/// Detection wants a complete line (see `process`), but a file that genuinely
/// has no line breaks must not stall the element forever. 4 KiB is the default
/// `filesrc` read, i.e. the amount the C would have been handed in one go, so
/// this is also the point beyond which waiting buys nothing. The bound matters
/// for cost as well: the `contains('\n')` scan below only runs while `textbuf`
/// is under this size, so it is O(1) per chain call rather than O(stream).
const DETECT_MAX_WAIT: usize = 4096;

/// How much of the decoded body the format probes are allowed to see.
///
/// The C hands autodetection `g_strndup (self->textbuf->str, 35)`
/// (`gstsubparse.c:1510`), so every probe in `autodetect` is a statement about
/// the first 35 bytes of the file and nothing else. Handing it the whole body
/// changes real answers, in both directions:
///
/// * The `strstr` probes (SAMI, SubViewer, QTtext) are unanchored, so a marker
///   sitting anywhere in a large file wins over the format its head actually
///   is.
/// * LRC requires every line but the last to be LRC-shaped, so any later line
///   that is not (a blank line between two stanzas, for instance) rejects a
///   file the C accepts.
///
/// The C's `g_strndup` can cut a multi-byte character in half. A `&str` cannot,
/// so [`detect_window`] trims to the character boundary at or below this. Every
/// probe matches ASCII, so a trailing character the window drops cannot change
/// an answer.
const DETECT_WINDOW: usize = 35;

/// `GST_BUFFER_OFFSET_NONE`: the buffer carries no byte offset.
const BUFFER_OFFSET_NONE: u64 = u64::MAX;

/// The charset override property. Named exactly as the C element names it
/// (`gstsubparse.c:147`), because `parsebin` forwards the application's choice
/// by looking this name up on every element it connects.
const PROP_SUBTITLE_ENCODING: &str = "subtitle-encoding";
/// Deprecated alias for [`PROP_SUBTITLE_ENCODING`].
const PROP_ENCODING_ALIAS: &str = "encoding";
/// The video framerate property, named as the C names it (`gstsubparse.c:155`).
/// Frame-based formats (MicroDVD) need it to convert frame numbers to time when
/// the file carries no rate of its own, and `subtitleoverlay`/`playbin` set it
/// by this name on whatever parser they plugged in, so any other spelling is
/// unreachable from a real pipeline.
const PROP_VIDEO_FPS: &str = "video-fps";
/// How styling is delivered: inline pango markup (default, the C behaviour)
/// or plain text plus a [`CueIrMeta`]. Read at negotiation time, so it has to
/// be set before the element leaves READY.
const PROP_TEXT_FORMAT: &str = "text-format";

#[derive(Debug, Clone)]
struct Settings {
    /// The [`PROP_SUBTITLE_ENCODING`] property: the charset to use when the
    /// input turns out not to be UTF-8.
    encoding: Option<String>,
    /// The `video-fps` used by frame-based formats (MicroDVD) as a default.
    fps_n: u32,
    fps_d: u32,
    /// The [`PROP_TEXT_FORMAT`] property.
    text_format: TextFormat,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            encoding: None,
            fps_n: DEFAULT_FPS_N,
            fps_d: DEFAULT_FPS_D,
            text_format: TextFormat::default(),
        }
    }
}

struct State {
    /// Charset decoder (whole-stream detection with a bounded sniff).
    decoder: Decoder,
    /// Undecoded input bytes: either the held incomplete multi-byte tail or,
    /// once the charset sniff has opened, the undecided window.
    pending: Vec<u8>,
    /// Decoded body that the parser has **not** consumed yet. Everything the
    /// parser reports as consumed is drained, so this holds at most one partial
    /// record, never the stream.
    textbuf: String,

    /// The autodetected format, once known.
    format: Option<Format>,
    /// Set once detection has run out of evidence and found nothing. Latched:
    /// the stream is not a subtitle file, the error has been posted, and
    /// nothing more is parsed or retained.
    unrecognised: bool,
    /// The single parser instance driving this stream, built once the format is
    /// known. It carries the position within a partially-seen record, so it must
    /// live as long as the stream and be rebuilt (not reused) after a flush.
    parser: Option<Box<dyn SubtitleFormat + Send>>,
    /// The text flavour the chosen parser emits.
    output_format: OutputFormat,
    /// Whether downstream wants plain utf8 while our format is pango-markup.
    strip_pango_markup: bool,
    /// Whether `text-format=cue-ir` was in effect when the caps were chosen:
    /// buffers carry plain text plus a [`CueIrMeta`]. Latched at negotiation
    /// so a mid-stream property change cannot contradict the caps.
    ir_mode: bool,
    /// Whether the src caps have been chosen (and are in [`State::caps`]).
    negotiated: bool,
    /// The negotiated src caps, kept so the caps event can be built again: a
    /// flush landing before the first push has to leave them re-sendable.
    caps: Option<gst::Caps>,
    /// Whether downstream still owes the caps event and the codec tag (the C's
    /// `need_tags`). Cleared only once they have actually been pushed.
    need_caps_tags: bool,

    segment: gst::FormattedSegment<gst::ClockTime>,
    segment_seqnum: Option<gst::Seqnum>,
    need_segment: bool,
    /// Byte offset the next input buffer is expected to start at (the C
    /// `self->offset`). A buffer that starts anywhere else is a discontinuity.
    offset: u64,

    /// Whether the decoder has been told the stream ended. It resolves a held
    /// incomplete tail and forces the charset decision, so it must happen
    /// before anything reads `textbuf` at EOS, and exactly once.
    drained: bool,

    flushing: bool,
    /// Bumped by every flush. `process` builds its events and buffers under the
    /// state lock but must release it before pushing (pushing blocks, and
    /// downstream may call back in), so a flushing seek can land in that
    /// window. Re-reading this before each push is what stops the stale
    /// pre-seek segment and cues from being emitted into the new stream.
    generation: u64,
    fps: (u32, u32),
}

impl State {
    fn new(settings: &Settings) -> Self {
        State {
            decoder: Decoder::new(),
            pending: Vec::new(),
            textbuf: String::new(),
            format: None,
            unrecognised: false,
            parser: None,
            output_format: OutputFormat::Utf8,
            strip_pango_markup: false,
            ir_mode: false,
            negotiated: false,
            caps: None,
            need_caps_tags: true,
            segment: gst::FormattedSegment::new(),
            segment_seqnum: None,
            need_segment: true,
            offset: 0,
            drained: false,
            flushing: false,
            generation: 0,
            fps: (settings.fps_n, settings.fps_d),
        }
    }
}

impl Default for State {
    fn default() -> Self {
        State::new(&Settings::default())
    }
}

pub struct SubParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

/// Append `chunk` to the undecoded tail, decode as much as is safely possible,
/// and grow the decoded body. Whatever the decoder did not consume (an
/// incomplete multi-byte tail, or the charset sniff's undecided window) is
/// retained in `state.pending` for the next chunk.
///
/// `at_eos` tells the decoder no more bytes are coming, which resolves a held
/// tail and forces the charset decision. `fallback` is the `subtitle-encoding`
/// property, passed on every call rather than snapshotted, because the C reads
/// its property per converted block and an application may set it after the
/// element is already running.
fn feed_bytes(state: &mut State, chunk: &[u8], at_eos: bool, fallback: Option<&str>) {
    state.pending.extend_from_slice(chunk);
    let mut pending = std::mem::take(&mut state.pending);
    let (text, consumed) = state.decoder.decode(&pending, at_eos, fallback);
    state.textbuf.push_str(&text);
    pending.drain(..consumed);
    state.pending = pending;
}

/// The `GST_TAG_SUBTITLE_CODEC` description, mirroring
/// `gst_sub_parse_get_format_description`.
fn subtitle_codec(format: Format) -> &'static str {
    match format {
        #[cfg(feature = "microdvd")]
        Format::MicroDvd => "MicroDVD",
        #[cfg(feature = "subrip")]
        Format::SubRip => "SubRip",
        #[cfg(feature = "mpsub")]
        Format::MpSub => "MPSub",
        #[cfg(feature = "sami")]
        Format::Sami => "SAMI",
        #[cfg(feature = "tmplayer")]
        Format::TmPlayer => "TMPlayer",
        #[cfg(feature = "mpl2")]
        Format::Mpl2 => "MPL2",
        #[cfg(feature = "subviewer")]
        Format::SubViewer => "SubViewer",
        #[cfg(feature = "dks")]
        Format::Dks => "DKS",
        #[cfg(feature = "webvtt")]
        Format::WebVtt => "WebVTT",
        #[cfg(feature = "qttext")]
        Format::QtText => "QTtext",
        #[cfg(feature = "lrc")]
        Format::Lrc => "LRC",
        #[cfg(feature = "ssa")]
        Format::Ssa => "SubStation Alpha",
    }
}

/// Convert a parser's nanosecond value to a `GstClockTime`.
///
/// `u64::MAX` is `GST_CLOCK_TIME_NONE`, and the parsers use it as exactly that
/// sentinel: SAMI's `TIME_NONE` for a cue with no end, QTtext's
/// `CLOCK_TIME_NONE` on timestamp overflow, TMPlayer's saturating hour field.
/// It reaches a cue's *start* easily enough (SAMI copies its end sentinel into
/// the next cue's start when a `</BODY>` is followed by more `<SYNC>` blocks,
/// which is what two concatenated SAMI documents look like).
///
/// `gst::ClockTime::from_nseconds` panics on that value, so it must never be
/// handed one. An unset time is the correct rendering: downstream reads it as
/// "no timestamp", which is what the sentinel says.
pub(crate) fn clock_time(ns: u64) -> Option<gst::ClockTime> {
    // GST_CLOCK_TIME_NONE == G_MAXUINT64.
    (ns != u64::MAX).then(|| gst::ClockTime::from_nseconds(ns))
}

fn format_field(output: OutputFormat) -> &'static str {
    match output {
        OutputFormat::Utf8 => "utf8",
        OutputFormat::PangoMarkup => "pango-markup",
    }
}

/// The prefix of `body` the format probes may look at, see [`DETECT_WINDOW`].
fn detect_window(body: &str) -> &str {
    let mut end = body.len().min(DETECT_WINDOW);
    // Offset 0 is always a boundary, so this terminates.
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

/// The caps, segment and tag events downstream still owes, in the order the C
/// sends them (`check_initial_events`: caps via `gst_pad_set_caps`, then the
/// segment, then the tags).
///
/// Nothing is cleared here. The events are built under the state lock and
/// pushed without it, so only [`SubParse::push_pending_events`] knows they
/// really went out.
fn pending_events(state: &State) -> Vec<gst::Event> {
    let mut events: Vec<gst::Event> = Vec::new();
    let Some(format) = state.format else {
        return events;
    };

    if state.need_caps_tags {
        let caps = state
            .caps
            .as_ref()
            .expect("caps are negotiated before they are owed");
        events.push(gst::event::Caps::new(caps));
    }
    if state.need_segment {
        events.push(
            gst::event::Segment::builder(&state.segment)
                .seqnum_if_some(state.segment_seqnum)
                .build(),
        );
    }
    if state.need_caps_tags {
        let mut tags = gst::TagList::new();
        {
            let tags = tags.get_mut().unwrap();
            tags.add::<gst::tags::SubtitleCodec>(
                &subtitle_codec(format),
                gst::TagMergeMode::Append,
            );
        }
        events.push(gst::event::Tag::new(tags));
    }
    events
}

impl SubParse {
    /// Decode incoming bytes and try to make progress. If `at_eos`, flush and
    /// emit everything.
    // With no format enabled `Format` is uninhabited, `autodetect::detect`
    // always returns `None`, and the post-detection tail (`state.format.unwrap()`
    // onward) is genuinely unreachable. Silence that only in that (degenerate)
    // configuration; every real subset keeps these lints active.
    #[cfg_attr(
        not(any(
            feature = "subrip",
            feature = "webvtt",
            feature = "ssa",
            feature = "sami",
            feature = "qttext",
            feature = "microdvd",
            feature = "mpl2",
            feature = "subviewer",
            feature = "tmplayer",
            feature = "mpsub",
            feature = "lrc",
            feature = "dks",
        )),
        allow(unreachable_code, unused_variables)
    )]
    fn process(&self, at_eos: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        // The properties must be read BEFORE the state lock is taken: every
        // site that wants both takes them in that order, and they are only
        // deadlock-free while that holds. Only the EOS drain needs `encoding`.
        let (fallback, text_format) = {
            let settings = self.settings.lock().unwrap();
            let fallback = if at_eos {
                settings.encoding.clone()
            } else {
                None
            };
            (fallback, settings.text_format)
        };
        let mut state = self.state.lock().unwrap();

        if state.flushing {
            return Err(gst::FlowError::Flushing);
        }
        // A stream the detector rejected stays rejected. The C errors it out on
        // the buffer it fails to recognise and keeps returning that error for
        // every buffer after it, so there is nothing left to parse or hold.
        if state.unrecognised {
            return Err(gst::FlowError::NotNegotiated);
        }

        // 0. At EOS, tell the decoder so before anything reads `textbuf`: that
        //    is what resolves a held incomplete tail and forces the charset
        //    decision when the sniff window never filled.
        if at_eos && !state.drained {
            feed_bytes(&mut state, &[], true, fallback.as_deref());
            state.drained = true;
            gst::debug!(
                CAT,
                imp = self,
                "Decoded the stream as {}",
                state.decoder.charset()
            );
        }

        // 1. Autodetect the format, once we have enough to decide.
        //
        //    The C guards only on `strlen(textbuf) < 6`, which asks whether any
        //    data has arrived, not whether it is enough to recognise. That is a
        //    real hole: every probe in `autodetect` matches a *line* pattern,
        //    the answer is latched for the whole stream, and a line truncated by
        //    a buffer boundary can match a different format than the same line
        //    whole. `[123][` (the first six bytes of the MPL2 line
        //    `[123][456] ...`) is a valid start for an LRC `[mm:ss.xx]` tag and
        //    detects as LRC, which then parses nothing at all. The C only gets
        //    away with it because `filesrc` hands it 4 KB at a time.
        //
        //    So detection waits for a complete line as well, which makes the
        //    format the element picks independent of how the input was split
        //    into buffers. Two escapes keep that from becoming a hang: EOS
        //    (there will never be a newline), and a body that has produced
        //    [`DETECT_MAX_WAIT`] bytes without one (some file with no line
        //    breaks; no format could yield cues from it anyway, but the C would
        //    still have guessed, so guess too rather than stall).
        //
        //    What the probes are shown is the C's window and nothing more, see
        //    [`DETECT_WINDOW`]. That also bounds the cost of a retry: waiting
        //    for more evidence re-runs detection over 35 bytes, not over the
        //    body.
        if state.format.is_none() {
            let enough = state.textbuf.len() >= 6
                && (state.textbuf.len() >= DETECT_MAX_WAIT || state.textbuf.contains('\n'));
            if !enough && !at_eos {
                return Ok(gst::FlowSuccess::Ok);
            }
            match autodetect::detect(detect_window(&state.textbuf)) {
                Some(fmt) => {
                    state.format = Some(fmt);
                    gst::info!(CAT, imp = self, "Detected format {:?}", fmt);
                }
                None => {
                    // "Nothing matched" and "nothing has matched yet" are
                    // different answers, and only the first one is fatal. The C
                    // cannot tell them apart because it decides on the first
                    // buffer, which is also why a six-byte first buffer makes it
                    // pick a format from an incomplete line. Waiting is bounded
                    // by the same two escapes as the gate above, so the verdict
                    // still arrives: at EOS, or once the body has grown past the
                    // point where more input could change the answer.
                    if !at_eos && state.textbuf.len() < DETECT_MAX_WAIT {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    if state.textbuf.len() < 6 {
                        // The C's "File too small to be a subtitles file": no
                        // format, and no element error either.
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Too little data to detect a subtitle format, nothing to emit"
                        );
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    // Latch the verdict. Without it every later chain call
                    // re-runs detection over a body that only grows, and the
                    // whole file is retained for a parser that will never exist.
                    state.unrecognised = true;
                    state.textbuf = String::new();
                    state.pending = Vec::new();
                    drop(state);
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::WrongType,
                        ["The input is not a valid/supported subtitle file"]
                    );
                    return Err(gst::FlowError::NotNegotiated);
                }
            }
        }
        let format = state.format.unwrap();

        // 2. Build the stream's one parser instance. It is `None` both on the
        //    first pass and after a flush (which keeps the negotiated format but
        //    restarts the byte stream from zero), and either way a fresh parser
        //    is exactly right: its state is a position within this stream.
        if state.parser.is_none() {
            let parser = format.parser();
            state.output_format = parser.output_format();
            state.parser = Some(parser);
        }

        // 3. Choose the src caps (once), then collect whatever sticky events
        //    downstream still owes. The `need_*` flags stay set until the events
        //    are really out, see `push_pending_events`.
        if !state.negotiated {
            let output = state.output_format;
            let (caps, strip) = match self.negotiate(output, text_format) {
                Some(v) => v,
                None => {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Negotiation,
                        ["Could not negotiate caps"]
                    );
                    return Err(gst::FlowError::NotNegotiated);
                }
            };
            state.strip_pango_markup = strip;
            state.ir_mode = text_format == TextFormat::CueIr;
            state.caps = Some(caps);
            state.negotiated = true;
        }
        let events = pending_events(&state);

        // 4. Parse as much of the accumulated body as forms complete records,
        //    then drop the prefix the parser is done with. Draining is what
        //    keeps this linear: without it the same bytes would be re-parsed on
        //    every chain call and `textbuf` would grow to the whole file.
        let ctx = ParseContext {
            fps: Some(state.fps),
        };
        let parsed = {
            // Split the borrow: the parser and the buffer are disjoint fields.
            let State {
                parser, textbuf, ..
            } = &mut *state;
            parser
                .as_mut()
                .expect("built above")
                .parse_incremental(textbuf, &ctx, at_eos)
                .unwrap_or_default()
        };

        debug_assert!(
            parsed.consumed <= state.textbuf.len()
                && state.textbuf.is_char_boundary(parsed.consumed),
            "parser reported consumed={} for a {}-byte body",
            parsed.consumed,
            state.textbuf.len(),
        );
        // Clamp rather than trust: a parser bug must not turn into a panic in a
        // live pipeline, and `String::drain` panics on both an out-of-range end
        // and one inside a character.
        let mut consumed = parsed.consumed.min(state.textbuf.len());
        while consumed > 0 && !state.textbuf.is_char_boundary(consumed) {
            consumed -= 1;
        }
        state.textbuf.drain(..consumed);

        let strip = state.strip_pango_markup;
        let ir_mode = state.ir_mode;
        let output = state.output_format;
        let seg_start = state.segment.start();
        // The styling the stream has declared so far (WebVTT STYLE blocks /
        // SSA style sections). Only the IR consumes it; pango-markup output
        // ignores styling exactly like the C.
        let sheet = if ir_mode {
            state.parser.as_deref().and_then(|p| p.stylesheet())
        } else {
            None
        };
        let ssa_styles = if ir_mode {
            state.parser.as_deref().and_then(|p| p.ssa_styles())
        } else {
            None
        };

        // Each buffer travels with the position it renders, which is published
        // immediately before it is pushed and not a batch ahead of it.
        let mut buffers: Vec<(gst::Buffer, Option<gst::ClockTime>)> = Vec::new();
        for cue in &parsed.cues {
            // Segment clipping (matters after a seek). Drop cues that end
            // before the segment start.
            let cue_start = clock_time(cue.start_ns);
            let cue_end = cue.end_ns.and_then(clock_time);
            if let (Some(seg_start), Some(end)) = (seg_start, cue_end)
                && end < seg_start
            {
                continue;
            }

            // In cue-ir mode the payload is the IR's own plain text, so the
            // buffer text and the meta can never disagree about the content.
            let (text, cue_ir) = if ir_mode {
                let cue_ir = match (ssa_styles, cue.ssa.as_deref()) {
                    // SSA: rebuild the styled cue from the raw dialogue and
                    // the collected style registry.
                    (Some(styles), Some(d)) => {
                        subparse_formats::ssastyle::dialogue_to_ir(d, styles, cue.start_ns)
                    }
                    _ => ir::cue_to_ir(cue, output, sheet),
                };
                (cue_ir.plain_text(), Some(cue_ir))
            } else {
                let mut text = cue.text.clone();
                if strip && output == OutputFormat::PangoMarkup {
                    text = strip_pango_markup(&text);
                }
                (text, None)
            };
            // Never emit trailing newlines (the C strips them too).
            let text = text.trim_end_matches(['\n', '\r']).to_owned();

            let mut buffer = gst::Buffer::from_slice(text.into_bytes());
            {
                let buf = buffer.get_mut().unwrap();
                buf.set_pts(cue_start);
                // A duration needs both ends. With either unusable the buffer
                // gets no duration, which is what an unset `GstClockTime` means.
                if let (Some(start), Some(end_ns)) = (cue_start, cue.end_ns)
                    && let Some(end) = clock_time(end_ns)
                {
                    buf.set_duration(end.saturating_sub(start));
                }
                if let Some(cue_ir) = cue_ir {
                    CueIrMeta::add(buf, cue_ir);
                }
            }
            buffers.push((buffer, cue_start));
        }

        // Everything below pushes, which blocks and lets downstream call back
        // in, so the state lock has to go. A flushing seek landing in that
        // window resets the parse state under us and makes this batch describe
        // a timeline that no longer exists, so each push re-checks that no
        // flush has happened since. Without it a stale pre-seek segment and its
        // cues leak into the stream the seek just started.
        let generation = state.generation;
        drop(state);

        self.push_pending_events(events, generation)?;

        for (buffer, position) in buffers {
            // The position is the cue about to go out, not the last of the
            // batch: the C assigns `segment.position` immediately before every
            // `gst_pad_push` (`gstsubparse.c:1833`), so a POSITION query can
            // never report time that has not been rendered yet. Only a cue with
            // a real timestamp moves it.
            {
                let mut state = self.state.lock().unwrap();
                if state.flushing || state.generation != generation {
                    return Err(gst::FlowError::Flushing);
                }
                if position.is_some() {
                    state.segment.set_position(position);
                }
            }
            self.srcpad.push(buffer)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    /// Push the sticky events downstream owes and only then record that it has
    /// them.
    ///
    /// Clearing the `need_*` flags where the events are *built* is what loses
    /// them: that happens under the state lock, the push happens without it, and
    /// a flush landing in between abandons the batch. The pad keeps CAPS and TAG
    /// stickies across a flush (`gstpad.c:5697-5704`), but only if they ever
    /// reached it.
    fn push_pending_events(
        &self,
        events: Vec<gst::Event>,
        generation: u64,
    ) -> Result<(), gst::FlowError> {
        if events.is_empty() {
            return Ok(());
        }
        for event in events {
            if self.is_stale(generation) {
                return Err(gst::FlowError::Flushing);
            }
            gst::log!(CAT, imp = self, "Pushing event {:?}", event);
            self.srcpad.push_event(event);
        }
        let mut state = self.state.lock().unwrap();
        if state.generation == generation {
            state.need_caps_tags = false;
            state.need_segment = false;
        }
        Ok(())
    }

    /// The C `check_initial_events` (`gstsubparse.c:1710-1755`): get the caps,
    /// segment and tags downstream before letting anything else past them.
    ///
    /// Returns whether downstream has them now. Detection needs bytes and a GAP
    /// can arrive before any have, so a stream whose format is still unknown
    /// answers `false`, and the C drops that GAP rather than sending it ahead of
    /// the caps.
    fn check_initial_events(&self) -> bool {
        let state = self.state.lock().unwrap();
        if state.flushing || !state.negotiated {
            return false;
        }
        let events = pending_events(&state);
        let generation = state.generation;
        drop(state);
        self.push_pending_events(events, generation).is_ok()
    }

    /// Whether a flush has happened since this batch was built under the lock.
    fn is_stale(&self, generation: u64) -> bool {
        let state = self.state.lock().unwrap();
        state.flushing || state.generation != generation
    }

    /// Choose the src caps and whether to strip pango markup, mirroring the C
    /// `gst_sub_parse_negotiate`.
    fn negotiate(
        &self,
        output: OutputFormat,
        text_format: TextFormat,
    ) -> Option<(gst::Caps, bool)> {
        // In cue-ir mode the buffer text is always plain (styling travels in
        // the meta), whatever flavour the parser emits.
        let fmt = match text_format {
            TextFormat::CueIr => "utf8",
            TextFormat::PangoMarkup => format_field(output),
        };
        let preferred = gst::Caps::builder("text/x-raw")
            .field("format", fmt)
            .build();

        let mut caps = match self.srcpad.allowed_caps() {
            Some(caps) if !caps.is_any() && !caps.is_empty() => caps,
            _ => preferred.clone(),
        };

        // The C only intersects with the preferred caps when preferred is utf8.
        if fmt == "utf8" {
            caps = caps.intersect(&preferred);
        }

        caps.fixate();
        if caps.is_empty() {
            return None;
        }

        let out_fmt = caps
            .structure(0)
            .and_then(|s| s.get::<String>("format").ok());
        let strip = out_fmt.as_deref() == Some("utf8") && fmt == "pango-markup";
        if strip {
            gst::info!(CAT, imp = self, "Will convert from pango-markup to utf8");
        }

        Some((caps, strip))
    }

    /// Reset the parse state, keeping what downstream has already been told.
    /// Takes `settings` by reference rather than locking them itself: the
    /// caller already holds the state lock, and taking the settings lock under
    /// it would invert the order every other site uses.
    fn flush(&self, settings: &Settings, state: &mut State) {
        let format = state.format;
        let negotiated = state.negotiated;
        let caps = state.caps.take();
        let strip = state.strip_pango_markup;
        let ir_mode = state.ir_mode;
        let output = state.output_format;
        let segment = state.segment.clone();
        let segment_seqnum = state.segment_seqnum;
        // Must outlive the reset: it is what tells a batch built before this
        // flush not to push itself into the stream that follows it.
        let generation = state.generation + 1;
        *state = State::new(settings);
        state.generation = generation;
        // Keep what has already been negotiated downstream. `need_caps_tags` is
        // deliberately left as the reset put it (armed): a flush can land
        // between the caps and tag events being built and being pushed, and that
        // batch is then abandoned, so the only way to be sure downstream has
        // them is to send them again.
        state.format = format;
        state.negotiated = negotiated;
        state.caps = caps;
        state.strip_pango_markup = strip;
        state.ir_mode = ir_mode;
        state.output_format = output;
        // The segment also survives (the C's FLUSH_STOP resets nothing): a
        // seek stores its target segment here, the seek's flush must not
        // reset the timeline to zero underneath it. `need_segment` is set by
        // the reset, so whatever segment applies is re-pushed either way. The
        // segment carries the position a POSITION query answers, which is why
        // that survives a flush too.
        state.segment = segment;
        state.segment_seqnum = segment_seqnum;
    }

    /// Everything that describes a position within the *byte* stream, dropped.
    ///
    /// This is the C's only mid-stream reset (`feed_textbuf`,
    /// `gstsubparse.c:1597-1608`): the parser state, the undecoded tail (its
    /// adapter) and the decoded body go, while the detected format and the
    /// charset decision stay, because a discontinuity is still the same file.
    fn reset_at_discont(&self, state: &mut State) {
        gst::info!(CAT, imp = self, "Discontinuity, resetting the parser");
        state.parser = None;
        state.textbuf.clear();
        state.pending.clear();
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, imp = self, "Handling buffer {:?}", buffer);

        // Read before the buffer is consumed by the mapping.
        let mut discont = buffer.flags().contains(gst::BufferFlags::DISCONT);
        let offset = buffer.offset();
        let size = buffer.size() as u64;

        let map = buffer.into_mapped_buffer_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        {
            let fallback = self.settings.lock().unwrap().encoding.clone();
            let mut state = self.state.lock().unwrap();
            if state.unrecognised {
                // The format was rejected and the error posted. Retaining or
                // decoding anything further would only grow a body nothing will
                // ever parse.
                return Err(gst::FlowError::NotNegotiated);
            }
            // A buffer that does not continue where the last one ended is a
            // discontinuity even without the flag, exactly as in the C.
            if offset != BUFFER_OFFSET_NONE && offset != state.offset {
                state.offset = offset;
                discont = true;
            }
            if discont {
                self.reset_at_discont(&mut state);
            }
            state.offset = state.offset.saturating_add(size);
            feed_bytes(&mut state, map.as_slice(), false, fallback.as_deref());
        }

        self.process(false)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, imp = self, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(_) => {
                // We send our own caps from the chain function.
                true
            }
            EventView::StreamStart(_) => {
                // A new stream on the same element (an EOS, then STREAM_START
                // and data again, with no flush anywhere). Everything the old
                // stream left behind describes that stream and not this one, and
                // the charset decoder is a one-shot: it has been told the stream
                // ended, so feeding it again is a programming error (see
                // `crate::encoding`).
                //
                // The negotiation is re-armed rather than kept. The new stream is
                // detected from its own bytes and may be a different format
                // altogether, so downstream gets fresh caps and a fresh codec
                // tag, just as it would from an element that had only just
                // started. The segment is re-armed with it, since a stream that
                // starts also sends its own.
                let settings = self.settings.lock().unwrap().clone();
                let mut state = self.state.lock().unwrap();
                let generation = state.generation + 1;
                *state = State::new(&settings);
                state.generation = generation;
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::Gap(_) => {
                // A GAP must not overtake the caps and segment the chain
                // function holds back until it knows the format, so the C sends
                // those first and drops the GAP if it cannot
                // (`gstsubparse.c:1935-1944`). Sparse subtitle pads are fed
                // exactly this way (matroskademux emits sync GAPs), so the
                // pre-negotiation GAP is the normal case rather than a corner
                // one. Dropping is not a failure: the event is consumed either
                // way.
                if self.check_initial_events() {
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                } else {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Dropping a GAP that arrived before the caps could be sent"
                    );
                    true
                }
            }
            EventView::Segment(e) => {
                let seg = e.segment();
                let mut state = self.state.lock().unwrap();
                if seg.format() == gst::Format::Time
                    && let Some(s) = seg.downcast_ref::<gst::ClockTime>()
                {
                    state.segment = s.clone();
                }
                state.segment_seqnum = Some(event.seqnum());
                state.need_segment = true;
                true
            }
            EventView::Eos(_) | EventView::StreamGroupDone(_) => {
                if let Err(err) = self.process(true) {
                    gst::debug!(CAT, imp = self, "Draining at EOS returned {:?}", err);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStart(_) => {
                let mut state = self.state.lock().unwrap();
                state.flushing = true;
                state.generation += 1;
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(_) => {
                let settings = self.settings.lock().unwrap().clone();
                let mut state = self.state.lock().unwrap();
                self.flush(&settings, &mut state);
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, imp = self, "Handling src event {:?}", event);

        match event.view() {
            EventView::Seek(e) => {
                let (rate, flags, start_type, start, stop_type, stop) = e.get();
                let (
                    gst::GenericFormattedValue::Time(start),
                    gst::GenericFormattedValue::Time(stop),
                ) = (start, stop)
                else {
                    gst::warning!(CAT, imp = self, "we only support seeking in TIME format");
                    return false;
                };
                let seqnum = event.seqnum();

                // Forward the seek upstream first: a demuxer that can seek in
                // TIME re-sends data and a fresh segment on its own.
                if gst::Pad::event_default(pad, Some(&*self.obj()), event) {
                    return true;
                }

                // The standalone file case, mirroring the C: a byte source
                // cannot seek in TIME, so seek it back to byte 0, re-parse
                // everything, and clip to the requested segment.
                //
                // The target segment is applied BEFORE the byte seek is pushed.
                // The flushing byte seek restarts the upstream task, and that
                // task can push data (and this element can push cues) the
                // instant FLUSH_STOP is through, which is *before* control
                // returns here. Applying the segment afterwards leaves that
                // window running on whatever segment was in place before, so a
                // second seek emits the first seek's segment and clips its cues
                // to the wrong target. `flush` deliberately preserves the
                // segment across the reset, which is what makes applying it
                // first safe: the seek's own flush cannot wipe it.
                let previous = {
                    let mut state = self.state.lock().unwrap();
                    let previous = (
                        state.segment.clone(),
                        state.segment_seqnum,
                        state.need_segment,
                    );
                    if state
                        .segment
                        .do_seek(rate, flags, start_type, start, stop_type, stop)
                        .is_none()
                    {
                        gst::warning!(CAT, imp = self, "could not apply the seek to our segment");
                        return false;
                    }
                    state.segment_seqnum = Some(seqnum);
                    state.need_segment = true;
                    gst::debug!(CAT, imp = self, "segment after seek: {:?}", state.segment);
                    previous
                };

                let byte_seek = gst::event::Seek::builder(
                    rate,
                    flags,
                    gst::SeekType::Set,
                    Some(gst::format::Bytes::ZERO),
                    gst::SeekType::None,
                    gst::format::Bytes::NONE,
                )
                .seqnum(seqnum)
                .build();
                if !self.sinkpad.push_event(byte_seek) {
                    // The seek failed, so put the timeline back as it was,
                    // `need_segment` included: nothing about the stream
                    // downstream is watching has changed, and re-arming it would
                    // send a second, identical segment for no reason.
                    gst::warning!(CAT, imp = self, "seek to 0 bytes failed");
                    let mut state = self.state.lock().unwrap();
                    (state.segment, state.segment_seqnum, state.need_segment) = previous;
                    return false;
                }
                true
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(q) => {
                if q.format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    // The C answers `self->segment.position`, which is the cue
                    // last pushed or, after a seek, the target `do_seek` stored.
                    q.set(state.segment.position());
                    true
                } else {
                    self.sinkpad.peer_query(query)
                }
            }
            QueryViewMut::Seeking(q) => {
                // Mirrors `gst_sub_parse_src_query` (`gstsubparse.c:220-241`):
                // this element can seek in TIME whenever upstream can seek in
                // BYTES, because that is exactly what a TIME seek is turned into
                // (see `src_event`). Falling through to the default query
                // instead answers "not seekable", and `GstBin` folds that into
                // the whole pipeline's answer.
                let fmt = q.format();
                let mut seekable = false;
                if fmt == gst::Format::Time {
                    let mut peer = gst::query::Seeking::new(gst::Format::Bytes);
                    if self.sinkpad.peer_query(&mut peer) {
                        seekable = peer.result().0;
                    }
                }
                let start = gst::GenericFormattedValue::new(fmt, if seekable { 0 } else { -1 });
                let stop = gst::GenericFormattedValue::new(fmt, -1);
                q.set(seekable, start, stop);
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SubParse {
    const NAME: &'static str = "GstRsSubParse";
    type Type = super::SubParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                SubParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse| parse.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                SubParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse| parse.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                SubParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse| parse.src_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                SubParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse| parse.src_query(pad, query),
                )
            })
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for SubParse {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let blurb = "Charset to assume when the input subtitles are not UTF-8 \
                         (if not set, the GST_SUBTITLE_ENCODING environment \
                         variable is checked, otherwise the charset is detected \
                         from the data)";
            vec![
                // The name the C element uses (gstsubparse.c:147) and, more to
                // the point, the name `parsebin`/`decodebin3` look up on every
                // element they connect in order to forward the application's
                // choice down (gstparsebin.c:2130-2139). Spelling it anything
                // else makes the override unreachable from an autoplugged
                // pipeline, which is where it matters.
                glib::ParamSpecString::builder(PROP_SUBTITLE_ENCODING)
                    .nick("subtitle charset encoding")
                    .blurb(blurb)
                    .build(),
                // Deprecated alias kept so code written against this element's
                // original spelling keeps working. Both names are the same
                // setting.
                glib::ParamSpecString::builder(PROP_ENCODING_ALIAS)
                    .nick("subtitle charset encoding (alias)")
                    .blurb(blurb)
                    .build(),
                // Same name, types, range and default as the C
                // (gstsubparse.c:154-160). `subtitleoverlay` and `playbin` set
                // it on the parser they plug in, which is the only way a
                // frame-based format learns the rate when the file carries none.
                gst::ParamSpecFraction::builder(PROP_VIDEO_FPS)
                    .nick("Video framerate")
                    .blurb(
                        "Framerate of the video stream. This is needed by some \
                         subtitle formats to synchronize subtitles and video \
                         properly. If not set and the subtitle format requires \
                         it subtitles may be out of sync.",
                    )
                    .minimum(gst::Fraction::new(0, 1))
                    .maximum(gst::Fraction::new(i32::MAX, 1))
                    .default_value(gst::Fraction::new(
                        DEFAULT_FPS_N as i32,
                        DEFAULT_FPS_D as i32,
                    ))
                    .readwrite()
                    .build(),
                glib::ParamSpecEnum::builder_with_default(PROP_TEXT_FORMAT, TextFormat::default())
                    .nick("Text format")
                    .blurb(
                        "How styling is delivered: pango-markup puts it inline in \
                     the buffer text (the classic subparse behaviour), cue-ir \
                     pushes plain UTF-8 text with a CueIrMeta carrying \
                     structured styling for custom renderers. Read when the \
                     src caps are chosen, so set it before starting.",
                    )
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            PROP_SUBTITLE_ENCODING | PROP_ENCODING_ALIAS => {
                let mut settings = self.settings.lock().unwrap();
                settings.encoding = value.get().expect("type checked upstream");
            }
            PROP_VIDEO_FPS => {
                let fps = value.get::<gst::Fraction>().expect("type checked upstream");
                // The pspec's range starts at 0/1 and a `GstFraction` denominator
                // is always positive, so neither part can arrive negative.
                let (fps_n, fps_d) = (fps.numer().max(0) as u32, fps.denom().max(1) as u32);
                let mut settings = self.settings.lock().unwrap();
                settings.fps_n = fps_n;
                settings.fps_d = fps_d;
                gst::debug!(CAT, imp = self, "video framerate set to {fps_n}/{fps_d}");
                // The running stream sees it too, as long as it has not started
                // parsing: a rate the file states itself wins over this one, and
                // the parser latches that on its first call (the C keeps the same
                // distinction with `have_internal_fps`).
                let mut state = self.state.lock().unwrap();
                state.fps = (fps_n, fps_d);
            }
            PROP_TEXT_FORMAT => {
                let mut settings = self.settings.lock().unwrap();
                settings.text_format = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            PROP_SUBTITLE_ENCODING | PROP_ENCODING_ALIAS => {
                let settings = self.settings.lock().unwrap();
                settings.encoding.to_value()
            }
            PROP_VIDEO_FPS => {
                let settings = self.settings.lock().unwrap();
                gst::Fraction::new(settings.fps_n as i32, settings.fps_d as i32).to_value()
            }
            PROP_TEXT_FORMAT => {
                let settings = self.settings.lock().unwrap();
                settings.text_format.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for SubParse {}

impl ElementImpl for SubParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Subtitle parser",
                // The C uses Decoder, not Parser. decodebin3 picks the element
                // that turns `application/x-subtitle*` into `text/x-raw` via
                // GST_ELEMENT_FACTORY_TYPE_DECODER, which matches on this word.
                "Codec/Decoder/Subtitle",
                "Parses subtitle (.sub) files into text streams",
                "Marcus Hanestad <marlhan@proton.me>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            use std::str::FromStr;

            let sink_caps = gst::Caps::from_str(
                "application/x-subtitle; application/x-subtitle-sami; \
                 application/x-subtitle-tmplayer; application/x-subtitle-mpl2; \
                 application/x-subtitle-dks; application/x-subtitle-qttext; \
                 application/x-subtitle-lrc; application/x-subtitle-vtt",
            )
            .unwrap();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("text/x-raw")
                .field("format", gst::List::new(["pango-markup", "utf8"]))
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        if transition == gst::StateChange::ReadyToPaused {
            let settings = self.settings.lock().unwrap().clone();
            let mut state = self.state.lock().unwrap();
            *state = State::new(&settings);
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::PausedToReady {
            let settings = self.settings.lock().unwrap().clone();
            let mut state = self.state.lock().unwrap();
            *state = State::new(&settings);
        }

        Ok(ret)
    }
}

/// Strip pango markup down to plain UTF-8, dropping `<...>` tags and unescaping
/// the XML entities. Mirrors the C `strip_pango_markup` (which runs the markup
/// through a `GMarkupParser` and keeps only the text nodes).
fn strip_pango_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // Skip to the matching '>'.
                for c2 in chars.by_ref() {
                    if c2 == '>' {
                        break;
                    }
                }
            }
            '&' => {
                let mut entity = String::new();
                let mut terminated = false;
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2 == ';' {
                        terminated = true;
                        break;
                    }
                    entity.push(c2);
                    if entity.len() > 12 {
                        break;
                    }
                }
                match (terminated, decode_entity(&entity)) {
                    (true, Some(ch)) => out.push(ch),
                    _ => {
                        // Not a recognized entity, keep it verbatim.
                        out.push('&');
                        out.push_str(&entity);
                        if terminated {
                            out.push(';');
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }

    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = entity.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DETECT_WINDOW, decode_entity, detect_window, strip_pango_markup};

    /// The probes see the C's 35 bytes and no more.
    #[test]
    fn the_detection_window_is_the_c_prefix() {
        let short = "1\n00:00:01,000";
        assert_eq!(detect_window(short), short);

        let body = "1\n00:00:01,000 --> 00:00:02,000\nOne\n\nand a great deal more text";
        assert_eq!(detect_window(body).len(), DETECT_WINDOW);
        assert_eq!(detect_window(body), &body[..DETECT_WINDOW]);
    }

    /// The C's `g_strndup` may cut a character in half. Slicing a `&str` there
    /// would panic, so the window shrinks to the boundary below instead.
    #[test]
    fn the_detection_window_never_splits_a_character() {
        // Four bytes of ASCII, then three-byte characters, so byte 35 lands
        // inside one of them (35 - 4 = 31, which is not a multiple of 3).
        let body = format!("[ar:{}", "\u{4f60}".repeat(20));
        assert!(
            !body.is_char_boundary(DETECT_WINDOW),
            "the test body must straddle the limit"
        );
        let window = detect_window(&body);
        assert!(window.len() < DETECT_WINDOW && window.len() >= DETECT_WINDOW - 2);
        assert!(body.starts_with(window));
    }

    /// A stream nothing recognises must be latched, not re-examined: the body
    /// stops being retained (nothing will ever parse it) and every later buffer
    /// is refused, which is what the C does from the buffer it fails on.
    #[test]
    fn an_unrecognised_stream_is_latched_and_not_retained() {
        use gst::prelude::*;
        use gst::subclass::prelude::*;

        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            crate::plugin_register_static().unwrap();
        });

        let mut h = gst_check::Harness::new("rssubparse");
        h.set_src_caps_str("application/x-subtitle");
        let element = h.element().unwrap();
        let subparse = element
            .downcast_ref::<crate::subparse::SubParse>()
            .expect("harness element is our SubParse");
        let imp = subparse.imp();

        // Prose, in buffers large enough that detection has all the evidence it
        // is ever going to get.
        let junk = "this is not a subtitle file, not even a little bit\n".repeat(200);
        assert_eq!(
            h.push(gst::Buffer::from_slice(junk.clone().into_bytes())),
            Err(gst::FlowError::NotNegotiated)
        );
        assert!(imp.state.lock().unwrap().unrecognised);
        assert!(imp.state.lock().unwrap().textbuf.is_empty());

        // More of the same is refused without being decoded or retained.
        assert_eq!(
            h.push(gst::Buffer::from_slice(junk.into_bytes())),
            Err(gst::FlowError::NotNegotiated)
        );
        let state = imp.state.lock().unwrap();
        assert!(state.textbuf.is_empty(), "the body must not be retained");
        assert!(
            state.pending.is_empty(),
            "the undecoded tail must not be retained"
        );
    }

    /// `textbuf` must never grow with the stream: whatever the parser has
    /// consumed has to be drained out of it. Without that the element keeps the
    /// whole file in memory *and* re-parses it, which is the quadratic shape
    /// this bound exists to rule out.
    ///
    /// A partial trailing record is legitimately retained, so the bound is
    /// "a small multiple of a record", not zero.
    #[test]
    fn textbuf_stays_bounded_while_streaming() {
        use gst::prelude::*;
        use gst::subclass::prelude::*;

        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            crate::plugin_register_static().unwrap();
        });

        // ~2 MB of SubRip. Small enough that the assertion below is reached
        // even under the old quadratic parser (which would otherwise time the
        // test out before it could fail on the bound).
        let mut body = String::new();
        for n in 0..8000u64 {
            body.push_str(&format!(
                "{}\n00:{:02}:{:02},000 --> 00:{:02}:{:02},000\n\
                 A reasonably long subtitle line so records are not tiny, number {}\n\
                 And a second visual line for the same cue.\n\n",
                n + 1,
                (2 * n / 60) % 60,
                (2 * n) % 60,
                (2 * n + 1) / 60 % 60,
                (2 * n + 1) % 60,
                n + 1,
            ));
        }

        let mut h = gst_check::Harness::new("rssubparse");
        h.set_src_caps_str("application/x-subtitle");

        let element = h.element().unwrap();
        let subparse = element
            .downcast_ref::<crate::subparse::SubParse>()
            .expect("harness element is our SubParse");
        let imp = subparse.imp();

        // One record is ~190 bytes. 8 KiB leaves room for a partial record plus
        // a whole chunk without being anywhere near "the file".
        const BOUND: usize = 8 * 1024;

        let bytes = body.as_bytes();
        let mut high_water = 0usize;
        for chunk in bytes.chunks(4096) {
            assert_eq!(
                h.push(gst::Buffer::from_slice(chunk.to_vec())),
                Ok(gst::FlowSuccess::Ok)
            );
            while h.try_pull().is_some() {}

            let len = imp.state.lock().unwrap().textbuf.len();
            high_water = high_water.max(len);
            assert!(
                len <= BOUND,
                "textbuf grew to {len} bytes (bound {BOUND}); \
                 the parsed prefix is not being drained"
            );
        }

        h.push_event(gst::event::Eos::new());
        while h.try_pull().is_some() {}
        eprintln!(
            "textbuf high water mark: {high_water} bytes over {} MB",
            body.len() >> 20
        );
    }

    #[test]
    fn strips_simple_tags() {
        assert_eq!(strip_pango_markup("<i>Six</i>"), "Six");
        assert_eq!(strip_pango_markup("<b><i>Eight</i></b>"), "Eight");
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(strip_pango_markup("Rock &amp; Roll"), "Rock & Roll");
        assert_eq!(strip_pango_markup("a &lt; b"), "a < b");
        assert_eq!(
            strip_pango_markup("gave <i>Rock &amp; Roll</i> to"),
            "gave Rock & Roll to"
        );
    }

    #[test]
    fn numeric_entities() {
        assert_eq!(decode_entity("#177"), char::from_u32(177));
        assert_eq!(decode_entity("#xa0"), char::from_u32(0xA0));
    }
}
