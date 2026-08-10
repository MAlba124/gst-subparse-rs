// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! The `ssaparse` element.
//!
//! Mirrors the upstream C `GstSsaParse`, which accepts exactly one input shape:
//! SSA/ASS embedded in a container. There, the demuxer hands the codec-private
//! init section over in the caps' `codec_data` and then pushes one *dialogue
//! field row* per buffer (`0,0,Default,,0,0,0,,Hello world`), with no `[Events]`
//! section, no `Dialogue:` keyword and the timing on the buffer rather than in
//! the line. Without `codec_data` the C refuses the caps outright ("Only SSA
//! subtitles embedded in containers are supported").
//!
//! This element therefore has two modes, chosen by the CAPS event:
//!
//! * **Framed** (`codec_data` present): the C's model, and the only one a
//!   demuxer produces. Each buffer is one complete row. Its text field is
//!   extracted by [`subparse_formats::formats::ssa::dialogue_to_pango_markup`],
//!   a port of `gst_ssa_parse_push_line`, and pushed with that buffer's PTS and
//!   duration.
//! * **Whole-file** (no `codec_data`): an intentional extension over the C, for
//!   standalone `.ass`/`.ssa` files whose `Dialogue:` lines carry their own
//!   timing. The body goes to the `subparse_formats::formats::ssa` `[Events]`
//!   parser. Like `rssubparse`, it holds **one** parser instance for the whole
//!   stream and feeds it incrementally (see [`crate::subparse`] for why):
//!   every byte is parsed once and `textbuf` holds at most a partial line.
//!
//! Charset handling differs between the two for the same reason. A framed buffer
//! is a complete unit, so it is decoded on its own with no tail carried into the
//! next one. A whole-file body is a byte stream, so there the decoder holds an
//! incomplete multi-byte tail across buffers.
//!
//! Neither mode looks at `GST_BUFFER_FLAG_DISCONT`, which is what the C's
//! `ssaparse` does (its `subparse` flushes parser state on one, `ssaparse` never
//! reads the flag). Framed buffers are independent, so there is nothing a
//! discontinuity could invalidate, and the whole-file mode gets its restarts
//! from FLUSH_STOP and STREAM_START.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::{Arc, LazyLock, Mutex};

use subparse_formats::formats::ssa::{dialogue_to_pango_markup, framed_dialogue};
use subparse_formats::ir::{self, CueIr};
use subparse_formats::ssastyle::{self, SsaStyles};
use subparse_formats::{Format, ParseContext, SubtitleFormat};

use crate::cueir::{CueIrMeta, TextFormat};
use crate::encoding::Decoder;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rsssaparse",
        gst::DebugColorFlags::empty(),
        Some("SSA/ASS subtitle parser element"),
    )
});

/// The header `gst_ssa_parse_setcaps` requires the `codec_data` init section to
/// contain, searched for anywhere in it (after an optional UTF-8 BOM).
const INIT_HEADER: &[u8] = b"[Script Info]";

/// The UTF-8 byte-order mark, skipped at the head of the init section.
const BOM_UTF8: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// See [`crate::cueir::TextFormat`]; read when the src caps are chosen.
const PROP_TEXT_FORMAT: &str = "text-format";

#[derive(Debug, Clone, Copy, Default)]
struct Settings {
    text_format: TextFormat,
}

struct State {
    /// Container-framed input: one dialogue field row per buffer, timed by the
    /// buffer. Set by the CAPS event and therefore kept across a stream restart,
    /// which does not re-negotiate.
    framed: bool,
    /// Style registry parsed from the `codec_data` init section (framed mode
    /// only; the whole-file parser collects its own). Consumed by the
    /// `cue-ir` path; from the caps, so it survives a stream restart.
    styles: Option<Arc<SsaStyles>>,
    /// The charset a previous framed buffer was decided to be, or `None` while
    /// every buffer so far read as UTF-8. Handed to the following buffers as
    /// their fallback so one stream cannot decode two ways.
    charset_latch: Option<&'static str>,

    decoder: Decoder,
    pending: Vec<u8>,
    /// Decoded body the parser has not consumed yet: at most a partial line.
    textbuf: String,
    /// The single parser instance driving this stream. Built lazily so a flush
    /// (which resets the state) restarts it from the top of the file.
    parser: Option<Box<dyn SubtitleFormat + Send>>,

    negotiated: bool,
    /// Whether `text-format=cue-ir` was in effect when the caps were chosen:
    /// buffers carry plain text plus a [`CueIrMeta`]. Latched with
    /// `negotiated` so a mid-stream property change cannot contradict the
    /// caps, and it survives whatever `negotiated` survives.
    ir_mode: bool,
    segment: gst::FormattedSegment<gst::ClockTime>,
    segment_seqnum: Option<gst::Seqnum>,
    need_segment: bool,

    /// Whether the decoder has been told the stream ended, which resolves a
    /// held incomplete tail and forces the charset decision.
    drained: bool,
    flushing: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            framed: false,
            styles: None,
            charset_latch: None,
            decoder: Decoder::new(),
            pending: Vec::new(),
            textbuf: String::new(),
            parser: None,
            negotiated: false,
            ir_mode: false,
            segment: gst::FormattedSegment::new(),
            segment_seqnum: None,
            need_segment: true,
            drained: false,
            flushing: false,
        }
    }
}

impl State {
    /// Drop everything that belongs to the stream just ended, keeping only what
    /// the CAPS event configured.
    ///
    /// A STREAM_START means the pad is starting over, and the state it starts
    /// over from must be a *fresh* one: the charset decoder refuses to be fed
    /// after it has been finished at EOS (it is a one-shot at that point), and
    /// the format parser's state is a position within the stream that just
    /// ended. `framed` and `styles` survive because they come from the caps,
    /// and the caps are not re-sent for a new stream on the same pad.
    fn restart(&mut self) {
        let framed = self.framed;
        let styles = self.styles.take();
        *self = State::default();
        self.framed = framed;
        self.styles = styles;
    }
}

/// See `subparse::imp::feed_bytes`. `rsssaparse` mirrors the C `ssaparse`,
/// which has no charset property of its own, so the only override reaching the
/// decoder is `GST_SUBTITLE_ENCODING`.
fn feed_bytes(state: &mut State, chunk: &[u8], at_eos: bool) {
    state.pending.extend_from_slice(chunk);
    let mut pending = std::mem::take(&mut state.pending);
    let (text, consumed) = state.decoder.decode(&pending, at_eos, None);
    state.textbuf.push_str(&text);
    pending.drain(..consumed);
    state.pending = pending;
}

/// Decode one container frame, on its own.
///
/// A framed buffer is a complete dialogue row, so there is nothing to carry into
/// the next one: a truncated multi-byte sequence at its end is damage now, not a
/// character split across a push-buffer boundary. Hence a fresh [`Decoder`] per
/// buffer, finished immediately, which is the shape of the C's per-block
/// conversion.
///
/// What does carry over is the *decision*. Once a buffer has been decided to be
/// something other than UTF-8, that charset is passed to the following buffers
/// as their fallback, so a stream cannot decode two ways. Latching UTF-8 would
/// say nothing: the fallback is only ever consulted for bytes that are not valid
/// UTF-8 (see `encoding::named_charset`), which is also why an all-UTF-8 stream
/// never latches anything at all.
fn decode_frame(state: &mut State, bytes: &[u8]) -> String {
    let mut decoder = Decoder::new();
    let (text, _consumed) = decoder.decode(bytes, true, state.charset_latch);
    if state.charset_latch.is_none() {
        let decided = decoder.charset();
        // "undecided" cannot survive a finished decode, but the latch must not
        // hold a label that would resolve to some other charset if it ever did.
        if !matches!(decided, "UTF-8" | "undecided") {
            state.charset_latch = Some(decided);
        }
    }
    text
}

pub struct SsaParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl SsaParse {
    /// Handle the CAPS event, mirroring `gst_ssa_parse_setcaps`.
    ///
    /// `codec_data` is what tells the two input shapes apart, so this is where
    /// the mode is decided.
    ///
    /// Nothing is read *out* of the init section. The C validates it (a UTF-8
    /// prefix containing `[Script Info]`), keeps it behind a `FIXME: parse
    /// initial section`, and never looks at it again. In particular it does not
    /// take the field order from the section's `[Events]` `Format:` line: a
    /// framed row's columns are fixed at `ReadOrder,Layer,Style,Name,MarginL,
    /// MarginR,MarginV,Effect,Text`, which is the 8 commas the text extraction
    /// walks past.
    ///
    /// Where this does part with the C: an init section that is missing or has
    /// no `[Script Info]` only warns here, rather than rejecting the caps and
    /// with them the whole track. Nothing depends on the section, and a remux
    /// that stripped the header still delivers parsable rows.
    fn setcaps(&self, caps: &gst::CapsRef) {
        let codec_data = caps
            .structure(0)
            .and_then(|s| s.get::<gst::Buffer>("codec_data").ok());

        let Some(codec_data) = codec_data else {
            gst::debug!(
                CAT,
                imp = self,
                "No codec_data, treating the input as a whole SSA/ASS file body"
            );
            self.state.lock().unwrap().framed = false;
            return;
        };

        let styles = match codec_data.map_readable() {
            Ok(map) => {
                let bytes = map.as_slice();
                let bytes = bytes.strip_prefix(&BOM_UTF8).unwrap_or(bytes);
                if !bytes.windows(INIT_HEADER.len()).any(|w| w == INIT_HEADER) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Invalid init section: no Script Info header"
                    );
                }
                // The C keeps the section as UTF-8 text, truncated at the first
                // byte that is not, and never reads it back ("FIXME: parse
                // initial section"). We log it like its GST_LOG does — and,
                // for the cue-ir path, actually parse the [Script Info] and
                // [V4(+) Styles] sections out of it.
                let init = match std::str::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(err) => {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Init section is not valid UTF-8. Problem at byte offset {}",
                            err.valid_up_to()
                        );
                        std::str::from_utf8(&bytes[..err.valid_up_to()])
                            .expect("valid_up_to() bytes are UTF-8")
                    }
                };
                gst::log!(CAT, imp = self, "Init section:\n{}", init);
                Some(Arc::new(SsaStyles::parse(init)))
            }
            Err(_) => {
                gst::warning!(CAT, imp = self, "Failed to map codec_data readable");
                None
            }
        };

        gst::debug!(CAT, imp = self, "codec_data present, input is framed");
        let mut state = self.state.lock().unwrap();
        state.framed = true;
        state.styles = styles;
    }

    /// The events downstream still has to be told about, in the order they have
    /// to be sent: caps, segment, then tags.
    ///
    /// The C sends its caps from `setcaps` and its tag list from the first chain
    /// call. Both come from here instead, so a stream that never produces a cue
    /// never produces events either, and so the two modes cannot disagree.
    fn pending_events(&self, state: &mut State, text_format: TextFormat) -> Vec<gst::Event> {
        let mut events: Vec<gst::Event> = Vec::new();

        if !state.negotiated {
            // In cue-ir mode the buffer text is plain; styling travels in the
            // CueIrMeta.
            let format = match text_format {
                TextFormat::PangoMarkup => "pango-markup",
                TextFormat::CueIr => "utf8",
            };
            let caps = gst::Caps::builder("text/x-raw")
                .field("format", format)
                .build();
            events.push(gst::event::Caps::new(&caps));
            state.negotiated = true;
            state.ir_mode = text_format == TextFormat::CueIr;

            if state.need_segment {
                events.push(
                    gst::event::Segment::builder(&state.segment)
                        .seqnum_if_some(state.segment_seqnum)
                        .build(),
                );
                state.need_segment = false;
            }

            let mut tags = gst::TagList::new();
            {
                let tags = tags.get_mut().unwrap();
                tags.add::<gst::tags::SubtitleCodec>(
                    &"SubStation Alpha",
                    gst::TagMergeMode::Append,
                );
            }
            events.push(gst::event::Tag::new(tags));
        } else if state.need_segment {
            events.push(
                gst::event::Segment::builder(&state.segment)
                    .seqnum_if_some(state.segment_seqnum)
                    .build(),
            );
            state.need_segment = false;
        }

        events
    }

    /// One container-framed buffer: one dialogue field row, timed by the buffer.
    ///
    /// Ports `gst_ssa_parse_chain` plus `gst_ssa_parse_push_line`. The text is
    /// everything past the row's 8th comma, with `{...}` override blocks removed,
    /// the `\N`/`\n`/`\h` escapes translated and the result markup-escaped, all
    /// of which is [`dialogue_to_pango_markup`].
    fn chain_framed(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Read the timing before the buffer is consumed by the mapping: it is
        // the only place a framed row's timing exists.
        let pts = buffer.pts();
        let duration = buffer.duration();

        let map = buffer.into_mapped_buffer_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        // Settings before state, the order every other site uses.
        let text_format = self.settings.lock().unwrap().text_format;
        let (events, text, ir_mode, styles) = {
            let mut state = self.state.lock().unwrap();
            if state.flushing {
                return Err(gst::FlowError::Flushing);
            }
            let events = self.pending_events(&mut state, text_format);
            let text = decode_frame(&mut state, map.as_slice());
            (events, text, state.ir_mode, state.styles.clone())
        };

        // The row is one line and its terminator is not part of the text. The C
        // takes the buffer verbatim, which leaves a muxer's trailing newline in
        // the output. This element never emits trailing newlines (see the
        // whole-file path below and the C `subparse`, which strips them too).
        let line = text.trim_end_matches(['\n', '\r']);

        let markup = if line.is_empty() {
            // The C's `empty_text` case, which it reaches on a zero-size buffer
            // and reports as an element warning. Logged rather than posted for
            // the same reason as the empty text field below.
            gst::debug!(CAT, imp = self, "Received empty subtitle");
            None
        } else {
            match dialogue_to_pango_markup(line) {
                // Nothing to show. Karaoke rows whose text is nothing but timing
                // override codes end up here, which is common enough that it is
                // not worth a bus message. The C would push an empty buffer.
                Some(markup) if markup.trim().is_empty() => {
                    gst::debug!(CAT, imp = self, "Dropping empty dialogue at {:?}", pts);
                    None
                }
                Some(markup) => Some(markup),
                None => {
                    // Fewer than 8 commas: not a dialogue row at all. The C
                    // returns GST_FLOW_ERROR here and then swallows it by
                    // pushing a segment to "advance time without sending
                    // anything", which would reset the timeline downstream is
                    // running on. Dropping the row has the same visible effect
                    // without that.
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Dropping malformed dialogue row (fewer than 8 fields): {:?}",
                        line
                    );
                    None
                }
            }
        };

        for event in events {
            gst::log!(CAT, imp = self, "Pushing event {:?}", event);
            self.srcpad.push_event(event);
        }

        let Some(markup) = markup else {
            return Ok(gst::FlowSuccess::Ok);
        };

        // The trailing-newline rule again: `\N` at the end of a row translates
        // to " \n".
        let markup = markup.trim_end_matches(['\n', '\r']);
        let (text, cue_ir) = if ir_mode {
            // The styled path: the raw row (override blocks intact) plus the
            // codec_data style registry. `markup` being `Some` means the row
            // had its 8 commas, so `framed_dialogue` cannot fail; the
            // fallback keeps a plain IR coming out even if it somehow does.
            let cue_ir = match framed_dialogue(line) {
                Some(d) => {
                    let start_ns = pts.map(gst::ClockTime::nseconds).unwrap_or(0);
                    let default_styles = SsaStyles::default();
                    let styles = styles.as_deref().unwrap_or(&default_styles);
                    ssastyle::dialogue_to_ir(&d, styles, start_ns)
                }
                None => CueIr::from_pango_markup(markup),
            };
            (cue_ir.plain_text(), Some(cue_ir))
        } else {
            (markup.to_owned(), None)
        };
        let mut buffer = gst::Buffer::from_slice(text.into_bytes());
        {
            let buf = buffer.get_mut().unwrap();
            buf.set_pts(pts);
            buf.set_duration(duration);
            if let Some(cue_ir) = cue_ir {
                CueIrMeta::add(buf, cue_ir);
            }
        }
        gst::log!(
            CAT,
            imp = self,
            "Pushing buffer with pts {:?} and duration {:?}",
            pts,
            duration
        );
        self.srcpad.push(buffer)
    }

    /// Whole-file mode: parse the accumulated `[Events]` body and push its cues.
    fn process(&self, at_eos: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Settings before state, the order every other site uses.
        let text_format = self.settings.lock().unwrap().text_format;
        let mut state = self.state.lock().unwrap();

        if state.flushing {
            return Err(gst::FlowError::Flushing);
        }

        // At EOS, finish the decoder before anything reads `textbuf`.
        if at_eos && !state.drained {
            feed_bytes(&mut state, &[], true);
            state.drained = true;
        }

        let events = self.pending_events(&mut state, text_format);

        // Parse whatever forms complete `Dialogue:` lines, then drop the prefix
        // the parser is done with. The drain is what keeps this linear.
        if state.parser.is_none() {
            state.parser = Some(Format::Ssa.parser());
        }
        let ctx = ParseContext::default();
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
        // Clamp rather than trust: `String::drain` panics on an out-of-range end
        // or one inside a character, and a parser bug must not kill a pipeline.
        let mut consumed = parsed.consumed.min(state.textbuf.len());
        while consumed > 0 && !state.textbuf.is_char_boundary(consumed) {
            consumed -= 1;
        }
        state.textbuf.drain(..consumed);

        let ir_mode = state.ir_mode;
        // The parser collects [Script Info] and [V4(+) Styles] as they stream
        // by; each cue carries its dialogue extras (raw text, style name,
        // margins). Together they rebuild the styled cue.
        let ssa_styles = if ir_mode {
            state.parser.as_deref().and_then(|p| p.ssa_styles())
        } else {
            None
        };
        let mut buffers: Vec<gst::Buffer> = Vec::new();
        for cue in &parsed.cues {
            let (text, cue_ir) = if ir_mode {
                let cue_ir = match (ssa_styles, cue.ssa.as_deref()) {
                    (Some(styles), Some(d)) => ssastyle::dialogue_to_ir(d, styles, cue.start_ns),
                    // No extras (shouldn't happen for SSA): fall back to the
                    // stripped pango-flavoured text.
                    _ => ir::cue_to_ir(cue, subparse_formats::OutputFormat::PangoMarkup, None),
                };
                (cue_ir.plain_text(), Some(cue_ir))
            } else {
                (cue.text.trim_end_matches(['\n', '\r']).to_owned(), None)
            };
            let mut buffer = gst::Buffer::from_slice(text.into_bytes());
            {
                let buf = buffer.get_mut().unwrap();
                // `u64::MAX` is GST_CLOCK_TIME_NONE, which `from_nseconds`
                // panics on. See `subparse::imp::clock_time`.
                let start = crate::subparse::imp::clock_time(cue.start_ns);
                buf.set_pts(start);
                if let (Some(start), Some(end_ns)) = (start, cue.end_ns)
                    && let Some(end) = crate::subparse::imp::clock_time(end_ns)
                {
                    buf.set_duration(end.saturating_sub(start));
                }
                if let Some(cue_ir) = cue_ir {
                    CueIrMeta::add(buf, cue_ir);
                }
            }
            buffers.push(buffer);
        }

        drop(state);

        for event in events {
            self.srcpad.push_event(event);
        }
        for buffer in buffers {
            self.srcpad.push(buffer)?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Read the mode out and release the lock: both paths below take it
        // again, and this one is not reentrant.
        let framed = self.state.lock().unwrap().framed;
        if framed {
            return self.chain_framed(buffer);
        }

        let map = buffer.into_mapped_buffer_readable().map_err(|_| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map buffer readable"]
            );
            gst::FlowError::Error
        })?;

        {
            let mut state = self.state.lock().unwrap();
            if state.drained {
                // Data after EOS with no STREAM_START between the two is an
                // upstream contract violation, but the charset decoder has been
                // finished and must not be fed again, so restart the stream
                // rather than let that be a panic.
                gst::warning!(
                    CAT,
                    imp = self,
                    "Buffer after EOS, restarting the parse from the top"
                );
                state.restart();
            }
            feed_bytes(&mut state, map.as_slice(), false);
        }

        self.process(false)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, imp = self, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(e) => {
                // We send our own caps from the chain function, but this is
                // where the input shape is read off them.
                self.setcaps(e.caps());
                true
            }
            EventView::StreamStart(_) => {
                // A new stream on the same pad. Everything the old one left
                // behind is stale, including the finished charset decoder, which
                // panics if it is fed again.
                self.state.lock().unwrap().restart();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
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
                // Framed input has nothing to drain: every buffer was a
                // complete row, decoded and pushed on its own. The lock is
                // released before the drain, which takes it again.
                let framed = self.state.lock().unwrap().framed;
                if !framed && let Err(err) = self.process(true) {
                    gst::debug!(CAT, imp = self, "Draining at EOS returned {:?}", err);
                }
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStart(_) => {
                self.state.lock().unwrap().flushing = true;
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                let negotiated = state.negotiated;
                let ir_mode = state.ir_mode;
                state.restart();
                state.negotiated = negotiated;
                state.ir_mode = ir_mode;
                drop(state);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SsaParse {
    const NAME: &'static str = "GstRsSsaParse";
    type Type = super::SsaParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                SsaParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse| parse.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                SsaParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse| parse.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ).build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for SsaParse {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder_with_default(PROP_TEXT_FORMAT, TextFormat::default())
                    .nick("Text format")
                    .blurb(
                        "How styling is delivered: pango-markup puts it inline in \
                     the buffer text (the classic ssaparse behaviour), cue-ir \
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
            PROP_TEXT_FORMAT => {
                let mut settings = self.settings.lock().unwrap();
                settings.text_format = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
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

impl GstObjectImpl for SsaParse {}

impl ElementImpl for SsaParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "SSA Subtitle Parser",
                // Decoder, not Parser, mirroring the C so that decodebin3's
                // GST_ELEMENT_FACTORY_TYPE_DECODER selection keeps working.
                "Codec/Decoder/Subtitle",
                "Parses SSA/ASS subtitle streams",
                "Marcus Hanestad <marlhan@proton.me>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            use std::str::FromStr;

            let sink_caps = gst::Caps::from_str("application/x-ssa; application/x-ass").unwrap();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            // The C advertises pango-markup only; utf8 is what `text-format=cue-ir`
            // pushes (plain text plus a CueIrMeta).
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
        if matches!(
            transition,
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady
        ) {
            // A full reset, `framed` included: PAUSED_TO_READY is where the C
            // drops its init section and clears its own `framed` flag, and the
            // caps arrive again before any data does.
            *self.state.lock().unwrap() = State::default();
        }

        self.parent_change_state(transition)
    }
}
