// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Sink-side subtitle cue state for the fcast receiver: which cue is on
//! screen right now ([`CueEngine`]) and what it looks like (the
//! `fvid-cue-raster` worker) — **parley + vello_cpu edition**.
//!
//! This is a drop-in replacement for `fcast-video/src/cue.rs`, written in the
//! `gst-subparse-rs` tree so it can be developed and tested without touching
//! the fcast checkout. The scheduling half (pending queue, active cue,
//! running-time evaluation, dirty/repaint signalling, worker handoff) is
//! lifted from the pango version essentially verbatim — it was already
//! renderer-agnostic. What changed:
//!
//! * **No pango/cairo/fontconfig.** Text layout is parley, rasterization is
//!   vello_cpu, both pure Rust. The worker thread, the raster cache and the
//!   never-block contract are unchanged.
//! * **Input is the cue IR, not markup.** With `rssubparse`/`rsssaparse`
//!   switched to `text-format=cue-ir`, every buffer carries a `CueIrMeta`
//!   whose [`CueIr`] describes styled spans (colors, fonts, bold/italic,
//!   outline, shadow, ruby, karaoke reveal times) and cue-level layout
//!   (WebVTT line/position/align, SSA anchor/origin/margins). The engine
//!   accepts that IR directly — and still accepts plain utf8 or the classic
//!   pango-markup text, which it parses with `CueIr::from_pango_markup`
//!   (the markup subset those elements emit; no pango involved).
//! * **Karaoke works.** Spans carry absolute reveal times; the engine
//!   re-keys the raster as the frame clock passes each step, so `\k`
//!   syllables and WebVTT inline timestamps paint on progressively.
//! * **The house style is configurable.** [`CueEngine::set_style`] takes a
//!   [`CueStyle`] (font, outline, a rounded/feathered readability box —
//!   see [`CueStyle::boxed`]) at any time, e.g. from a user settings menu;
//!   the active cue re-rasters immediately, paused included. The subtitle's
//!   own styling still overrides the corresponding house fields.
//!
//! ## Porting checklist (fcast side)
//!
//! 1. Replace the `Overlay`/`OverlaySpace` shim at the bottom of this file
//!    with `use crate::video::{Overlay, OverlaySpace};` — the fields match.
//! 2. `fcast-video/Cargo.toml`: drop `pango`/`pangocairo`/`cairo-rs`, add
//!    `subparse-formats` (path/git), `parley = "0.11"`, `peniko = "0.6"` and
//!    the pinned `vello_cpu` revision (see `cueir-demo/Cargo.toml`;
//!    `default-features = false, features = ["std", "text", "u8_pipeline"]`).
//!    `parking_lot`/`smallvec`/`tracing` are already there.
//! 3. Set `text-format=cue-ir` on the subtitle parser elements and build
//!    [`CueInput`]s from the buffers the text pad delivers:
//!
//!    ```ignore
//!    let content = match buffer.meta::<gstrssubparse::cueir::CueIrMeta>() {
//!        Some(meta) => CueContent::ir(meta.ir().clone()),
//!        // Caps say which of the two string formats this pad carries.
//!        None if format_is_pango_markup => CueContent::pango_markup(text),
//!        None => CueContent::plain(text),
//!    };
//!    engine.submit(CueInput {
//!        content,
//!        start_rt,
//!        end_rt,
//!        // The buffer's pts anchors karaoke reveal times; None disables
//!        // per-syllable stepping and shows the whole cue at once.
//!        pts_start: buffer.pts(),
//!    });
//!    ```
//!
//! 4. Feed [`CueEngine::set_video_rect`] wherever the sink recomputes the
//!    scaled destination rect (resize/rotation/aspect change), alongside the
//!    `set_canvas` call it already makes. It anchors positioned cues to the
//!    picture and sizes text against the picture height; without it the
//!    window doubles as the frame, which is what the pango version did.
//! 5. The first-use warm-up lesson still applies (ASSERTION-LANDMINES §6):
//!    call [`CueEngine::warm`] at sink construction. Parley's font
//!    enumeration is far cheaper than fontconfig's, but it is not free and
//!    it must not run on a streaming thread.
//!
//! Timing semantics are unchanged from the pango version: lifted from
//! `fcasttextoverlay`'s `wait_for_text_buf` as the pure functions
//! [`cue_is_too_old`] and [`cue_is_in_future`].

use std::{
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamilyName, FontWeight, GenericFamily, GlyphRun,
    LayoutContext, LineHeight, PositionedLayoutItem, StyleProperty,
};
use peniko::Color;
use smallvec::SmallVec;
use subparse_formats::ir::{self, CueIr};
use tracing::{debug, info, warn};
use vello_cpu::{
    Glyph, Pixmap, RenderContext,
    kurbo::{Affine, Cap, Join, Rect, Stroke, Vec2},
};

/// Cap on cues waiting for their turn. Cue cadence is roughly one per second,
/// so 16 is many seconds of lookahead; overflowing it means the producer is
/// running away and the oldest (least useful) cue is dropped, counted.
const PENDING_LIMIT: usize = 16;

/// Rasters kept around after they stop being active. Slightly larger than the
/// pango version's 8: a karaoke cue occupies one slot per reveal step, and
/// the steps of the active cue should not evict each other.
const RASTER_CACHE_LIMIT: usize = 16;

/// Refuse to allocate a raster larger than this in either dimension.
const MAX_RASTER_PX: i32 = 8192;

/// The rectangle the video actually occupies inside the window (after
/// aspect-ratio scaling), in window coordinates. The sink computes this for
/// rendering anyway; feeding it to [`CueEngine::set_video_rect`] is what
/// anchors positioned cues (SSA `\pos`, WebVTT `line:`/`position:`) to the
/// *picture* rather than the window, and sizes text against the picture
/// height. Without it, the whole window doubles as the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

// -- house style ----------------------------------------------------------------

/// Straight-alpha RGBA, toolkit-agnostic.
pub type Rgba = [u8; 4];

/// How cue text is presented when (and wherever) the subtitle file itself
/// says nothing: the *house style*. Set it with [`CueEngine::set_style`] —
/// e.g. from a user settings menu — and the active cue re-rasterizes.
/// Everything the IR specifies (colors, fonts, per-cue outline/shadow,
/// positioning) still overrides the corresponding field here.
///
/// All fractions of the font size are em-like (`0.5` = half the font size);
/// the font size itself is a fraction of the canvas height, so cues scale
/// with the real display.
#[derive(Debug, Clone, PartialEq)]
pub struct CueStyle {
    /// Font family; `None` = the platform's sans-serif.
    pub font_family: Option<String>,
    /// CSS-style weight (400 normal, 700 bold).
    pub font_weight: f32,
    /// Font size as a fraction of canvas height.
    pub font_height_fraction: f32,
    /// Never smaller than this, however small the window gets.
    pub min_font_px: f32,
    /// Wrap width as a fraction of canvas width.
    pub wrap_width_fraction: f32,
    /// Distance from the bottom edge, as a fraction of canvas height.
    pub bottom_margin_fraction: f32,
    /// Whether *default-placed* subtitles (no positioning in the file) may
    /// sit in the window's letterbox bars instead of covering the picture
    /// (mpv's `sub-use-margins`, and what the pango version effectively
    /// did). Cues the file positions explicitly always track the video
    /// rectangle regardless.
    pub use_window_margins: bool,
    /// Stroked border behind the glyphs; `None` = no outline.
    pub outline: Option<OutlineStyle>,
    /// Box painted behind the whole cue; `None` = no box. When the subtitle
    /// itself asks for a cue background (SSA `BorderStyle=3`, WebVTT
    /// `::cue { background }`) that color wins, drawn with this box's
    /// geometry (or square and snug when this is `None`).
    pub background: Option<BackgroundStyle>,
}

/// A stroked border around the glyph edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlineStyle {
    pub color: Rgba,
    /// Stroke width as a fraction of the font size.
    pub width_fraction: f32,
}

/// The readability box behind the cue text.
///
/// Note this is a *tint*, not frosted glass: the raster is composited over
/// the video later, so a true backdrop blur cannot happen here (the video
/// pixels do not exist at raster time) — it belongs to the GPU compositor.
/// `edge_softness` gives the CPU-side approximation: a gaussian-feathered
/// rim instead of a hard edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundStyle {
    pub color: Rgba,
    /// Corner radius as a fraction of the font size (`0.0` = square).
    pub corner_radius: f32,
    /// Space between the text's ink and the box edge, as a fraction of the
    /// font size.
    pub padding: f32,
    /// Gaussian feathering of the box edge, as a fraction of the font size
    /// (`0.0` = hard edge).
    pub edge_softness: f32,
}

impl Default for CueStyle {
    /// The classic look, matching the pango renderer this module replaced:
    /// bold white text on a rounded black outline, no box.
    fn default() -> Self {
        Self {
            font_family: None,
            font_weight: 700.0,
            font_height_fraction: 0.045,
            min_font_px: 12.0,
            wrap_width_fraction: 0.90,
            bottom_margin_fraction: 0.04,
            use_window_margins: true,
            outline: Some(OutlineStyle {
                color: [0, 0, 0, 217],
                width_fraction: 0.14,
            }),
            background: None,
        }
    }
}

impl CueStyle {
    /// The boxed-captions look: a slightly transparent black rounded box
    /// behind the text for readability on busy video, no outline.
    pub fn boxed() -> Self {
        Self {
            outline: None,
            background: Some(BackgroundStyle {
                color: [0, 0, 0, 160],
                corner_radius: 0.35,
                padding: 0.45,
                edge_softness: 0.0,
            }),
            ..Self::default()
        }
    }
}

// -- cue content --------------------------------------------------------------

/// What a cue says and how it is styled. All three variants normalise to a
/// [`CueIr`] at construction, so the engine and the worker only ever see the
/// IR.
///
/// Identity is the IR itself, structurally: same IR ⇒ same pixels, which is
/// what the raster cache keys on. (`CueIr` holds `f32`s and so has no lawful
/// `Eq`/`Hash`; nothing here needs them — the cache is a bounded LRU `Vec`
/// with a linear scan, so `PartialEq` is all a lookup takes.)
#[derive(Debug, Clone)]
pub struct CueContent {
    ir: Arc<CueIr>,
}

impl CueContent {
    /// The styled IR straight off a buffer's `CueIrMeta`
    /// (`text-format=cue-ir`). This is the production path.
    pub fn ir(ir: CueIr) -> Self {
        Self { ir: Arc::new(ir) }
    }

    /// Pango-markup text (`rssubparse`/`rsssaparse` in their default mode).
    /// Parsed by `subparse-formats`' own parser for the markup subset those
    /// elements emit — no pango involved, and bad markup degrades to styled-
    /// as-plain text instead of being dropped.
    pub fn pango_markup(text: impl AsRef<str>) -> Self {
        Self::ir(CueIr::from_pango_markup(text.as_ref()))
    }

    /// Plain UTF-8 (`format=utf8`; the test sources).
    pub fn plain(text: impl AsRef<str>) -> Self {
        Self::ir(CueIr::from_plain_text(text.as_ref()))
    }

    /// The unstyled text, for logs and tests.
    pub fn plain_text(&self) -> String {
        self.ir.plain_text()
    }
}

impl PartialEq for CueContent {
    fn eq(&self, other: &Self) -> bool {
        // Pointer equality first: a re-shown cue is usually the same Arc.
        Arc::ptr_eq(&self.ir, &other.ir) || self.ir == other.ir
    }
}

/// One cue, already converted to running time by the producer.
#[derive(Debug, Clone, PartialEq)]
pub struct CueInput {
    pub content: CueContent,
    pub start_rt: gst::ClockTime,
    /// `None` means open-ended: the cue stays active until superseded or
    /// cleared.
    pub end_rt: Option<gst::ClockTime>,
    /// The cue's presentation start (the text buffer's pts). Karaoke reveal
    /// times in the IR are absolute on that timeline; this anchors them to
    /// `start_rt`. `None` disables reveal stepping (the whole cue shows at
    /// once), which is always safe.
    pub pts_start: Option<gst::ClockTime>,
}

/// A cue no longer covers a frame once the frame's running time has reached
/// the cue's end.
///
/// This is `fcasttextoverlay.rs`'s too-old pop verbatim
/// (`text_running_time_end <= vid_running_time`), with an open-ended cue (no
/// end) never expiring.
pub fn cue_is_too_old(end_rt: Option<gst::ClockTime>, frame_rt: gst::ClockTime) -> bool {
    end_rt.is_some_and(|end| end <= frame_rt)
}

/// A cue has not begun while the frame's running time is before its start.
pub fn cue_is_in_future(start_rt: gst::ClockTime, frame_rt: gst::ClockTime) -> bool {
    start_rt > frame_rt
}

/// The reveal steps of a cue, as running times: sorted, deduplicated, and
/// anchored to `start_rt` by the pts the reveal times are absolute against.
/// Empty when the cue has no karaoke (the common case) or no pts anchor.
fn reveal_steps(
    ir: &CueIr,
    start_rt: gst::ClockTime,
    pts_start: Option<gst::ClockTime>,
) -> Vec<gst::ClockTime> {
    let Some(pts) = pts_start else {
        return Vec::new();
    };
    let mut steps: Vec<u64> = ir
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .filter_map(|s| s.reveal_ns)
        .collect();
    steps.sort_unstable();
    steps.dedup();
    steps
        .into_iter()
        .map(|ns| {
            // Offset from the cue's own start; a reveal at/before the start
            // is step zero (visible immediately).
            let offset = ns.saturating_sub(pts.nseconds());
            start_rt + gst::ClockTime::from_nseconds(offset)
        })
        .collect()
}

/// How many reveal thresholds a span at `reveal_ns` sits behind, i.e. its
/// rank in the sorted step list. Rank 0 means "visible from the start".
fn reveal_rank(ir_steps: &[u64], reveal_ns: Option<u64>) -> usize {
    match reveal_ns {
        None => 0,
        Some(ns) => ir_steps.partition_point(|s| *s < ns) + 1,
    }
}

// -- rasters --------------------------------------------------------------------

/// A rendered cue: tightly packed RGBA with straight (non-premultiplied)
/// alpha, placed in window coordinates.
#[derive(Debug)]
pub struct Raster {
    /// Shared with every [`Overlay`] built from this raster: a cue strip is
    /// megabytes, and `overlays_for` runs per displayed frame — cloning the
    /// bytes there (as the pango version did) was the module's only heavy
    /// per-frame cost.
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
}

impl Raster {
    /// Cheap: the pixel buffer is refcount-shared with the overlay, so this
    /// is a handful of scalar copies per frame, not a memcpy.
    fn to_overlay(&self) -> Overlay {
        Overlay {
            pixels: self.pixels.clone(),
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            render_width: self.width,
            render_height: self.height,
            // Window space: the raster was laid out at display resolution,
            // so it must not be scaled (or rotated) with the video.
            space: OverlaySpace::Window,
        }
    }

    /// Texture dimensions, for tests and diagnostics.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Placement in window coordinates, for tests and diagnostics.
    pub fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// The RGBA bytes, for tests and diagnostics.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

/// What a raster is fully determined by: the cue's IR, the canvas it is laid
/// out against, and — for karaoke — how many reveal steps have passed. Same
/// key ⇒ byte-identical pixels, which is what makes the cache sound.
///
/// Equality is structural over the IR (with an `Arc` pointer fast path via
/// [`CueContent`]'s semantics); the cache is a linear-scan `Vec`, so no
/// `Hash`/`Eq` is needed and `CueIr`'s floats never have to pretend they
/// have either.
#[derive(Debug, Clone)]
struct RasterKey {
    ir: Arc<CueIr>,
    style: Arc<CueStyle>,
    canvas: (u32, u32),
    video_rect: Option<VideoRect>,
    /// Number of reveal thresholds at or before the frame clock (0 = only
    /// the un-timed spans are visible). Always 0 for non-karaoke cues.
    step: usize,
}

impl PartialEq for RasterKey {
    fn eq(&self, other: &Self) -> bool {
        self.canvas == other.canvas
            && self.video_rect == other.video_rect
            && self.step == other.step
            && (Arc::ptr_eq(&self.ir, &other.ir) || self.ir == other.ir)
            && (Arc::ptr_eq(&self.style, &other.style) || self.style == other.style)
    }
}

#[derive(Debug)]
enum RasterState {
    /// Requested (or requestable); no pixels yet. The frame renders bare and
    /// the completion signal repaints — the engine never waits.
    Pending,
    Ready(Arc<Raster>),
    /// The worker could not produce pixels (empty text, absurd size).
    /// Remembered so the cue is not re-requested every frame.
    Failed,
}

#[derive(Debug)]
struct Active {
    cue: CueInput,
    key: RasterKey,
    raster: RasterState,
    /// Reveal thresholds as running times (see [`reveal_steps`]); empty for
    /// the non-karaoke common case.
    steps: Vec<gst::ClockTime>,
}

#[derive(Default)]
struct State {
    /// Cues waiting for their window, ordered by `start_rt`.
    pending: VecDeque<CueInput>,
    active: Option<Active>,
    /// Display size the rasters are laid out against.
    canvas: (u32, u32),
    /// Where the video sits inside that display (see [`VideoRect`]).
    video_rect: Option<VideoRect>,
    /// The house style rasters are drawn with (see [`CueStyle`]).
    style: Arc<CueStyle>,
    /// The video segment as captured by the sink, for pts → running time.
    video_segment: Option<gst::Segment>,
    /// Running time of the most recently shown frame. While paused this is
    /// frozen, and it is what a newly arriving cue is evaluated against.
    last_shown_rt: Option<gst::ClockTime>,
}

type OnChange = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    cache: Mutex<RasterCache>,
    worker: Mutex<Option<WorkerHandle>>,
    on_change: Mutex<Option<OnChange>>,
    dirty: AtomicBool,
    dropped: AtomicU64,
    /// Font warm-up cost in nanoseconds; 0 until the worker has warmed.
    warm_nanos: AtomicU64,
    /// How long each raster the WORKER produced took, newest last, bounded.
    /// Cache hits never reach the worker and are deliberately not in here.
    raster_latencies: Mutex<VecDeque<Duration>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(handle) = self.worker.lock().take() {
            handle.stop();
        }
    }
}

/// Sink-side cue scheduler. Cheap to clone (an `Arc` handle); every method is
/// non-blocking.
#[derive(Clone, Default)]
pub struct CueEngine {
    shared: Arc<Shared>,
}

impl CueEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedule a cue. Called from the text delivery thread; never blocks and
    /// never rasterizes inline.
    pub fn submit(&self, cue: CueInput) {
        let mut changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();

            // Insert by start time (delivery is normally in order; a re-send
            // after a seek may not be).
            let at = state
                .pending
                .iter()
                .rposition(|queued| queued.start_rt <= cue.start_rt)
                .map_or(0, |idx| idx + 1);
            state.pending.insert(at, cue);

            while state.pending.len() > PENDING_LIMIT {
                let dropped = state.pending.pop_front();
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                warn!(
                    ?dropped,
                    total = self.shared.dropped.load(Ordering::Relaxed),
                    "cue backlog full, dropping the oldest cue"
                );
            }

            // A cue that covers the frame already on screen becomes visible
            // without a new frame: this is the paused path.
            changed = match state.last_shown_rt {
                Some(rt) => evaluate(&mut state, rt),
                None => false,
            };
            let (want, filled) = self.resolve_raster(&mut state);
            changed |= filled;
            fetch = want;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
    }

    /// Drop everything scheduled and everything showing. The raster cache is
    /// deliberately kept: a clear is usually a prelude to re-delivery of the
    /// same cues (a flushing seek, a track restart).
    pub fn clear(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            state.pending.clear();
            state.active.take().is_some()
        };
        if changed {
            self.mark_changed();
        }
    }

    /// Set the display size cues are laid out against, from the sink's
    /// `window-resolution` property.
    ///
    /// A zero dimension is ignored: a window mid-create or mid-minimize
    /// reports 0x0 and there is nothing to lay out against.
    pub fn set_canvas(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            debug!(width, height, "ignoring zero canvas size");
            return;
        }

        let changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();
            if state.canvas == (width, height) {
                return;
            }
            state.canvas = (width, height);

            // The active cue's raster was laid out against the old size.
            if let Some(active) = state.active.as_mut() {
                active.key.canvas = (width, height);
                active.raster = RasterState::Pending;
            }
            // Old-canvas rasters can never match again; drop them now.
            self.shared
                .cache
                .lock()
                .prune(|key| key.canvas == (width, height));
            let (want, filled) = self.resolve_raster(&mut state);
            fetch = want;
            changed = filled;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
    }

    /// Record the video segment the sink is running, so frame pts can be
    /// turned into the running time cues are scheduled in.
    pub fn set_video_segment(&self, segment: &gst::Segment) {
        self.shared.state.lock().video_segment = Some(segment.clone());
    }

    /// Set the rectangle the video occupies inside the window, in window
    /// coordinates (`None` = unknown; the whole window then doubles as the
    /// frame). Update it wherever the sink recomputes its scaled destination
    /// rect — resize, rotation, aspect change. Positioned cues track it;
    /// default-placed subtitles may still use the window bars (see
    /// [`CueStyle::use_window_margins`]).
    pub fn set_video_rect(&self, rect: Option<VideoRect>) {
        if let Some(r) = rect
            && (r.width == 0 || r.height == 0)
        {
            debug!(?rect, "ignoring zero-sized video rect");
            return;
        }

        let changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();
            if state.video_rect == rect {
                return;
            }
            state.video_rect = rect;

            // The active cue's raster was laid out against the old frame.
            if let Some(active) = state.active.as_mut() {
                active.key.video_rect = rect;
                active.raster = RasterState::Pending;
            }
            self.shared.cache.lock().prune(|key| key.video_rect == rect);
            let (want, filled) = self.resolve_raster(&mut state);
            fetch = want;
            changed = filled;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
    }

    /// Change the house style (see [`CueStyle`]); the active cue re-rasters.
    /// Callable at any time from any thread — a user toggling "subtitle
    /// background" in a settings menu lands here, including while paused.
    pub fn set_style(&self, style: CueStyle) {
        let changed;
        let fetch;
        {
            let mut state = self.shared.state.lock();
            if *state.style == style {
                return;
            }
            let style = Arc::new(style);
            state.style = style.clone();

            // The active cue's raster was drawn with the old style.
            if let Some(active) = state.active.as_mut() {
                active.key.style = style.clone();
                active.raster = RasterState::Pending;
            }
            self.shared
                .cache
                .lock()
                .prune(|key| Arc::ptr_eq(&key.style, &style) || *key.style == *style);
            let (want, filled) = self.resolve_raster(&mut state);
            fetch = want;
            changed = filled;
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
    }

    /// FLUSH_STOP: both sides of the comparison are invalid — cues from
    /// before the flush must not be shown after it, and the timeline anchor
    /// is gone.
    pub fn flush(&self) {
        let changed = {
            let mut state = self.shared.state.lock();
            state.pending.clear();
            state.video_segment = None;
            state.last_shown_rt = None;
            state.active.take().is_some()
        };
        if changed {
            self.mark_changed();
        }
    }

    /// STREAM_START: a new stream's segment is about to arrive; forget the
    /// old timeline anchor. Scheduled cues are left alone — dropping them is
    /// the producer's decision.
    pub fn reset_timeline(&self) {
        let mut state = self.shared.state.lock();
        state.video_segment = None;
        state.last_shown_rt = None;
    }

    /// Running time of a frame with this pts, under the captured video
    /// segment.
    pub fn video_running_time(&self, pts: Option<gst::ClockTime>) -> Option<gst::ClockTime> {
        let pts = pts?;
        let state = self.shared.state.lock();
        let segment = state.video_segment.as_ref()?;
        match segment.to_running_time(pts) {
            gst::GenericFormattedValue::Time(time) => time,
            _ => None,
        }
    }

    /// The overlays a frame at `frame_rt` should carry. Called per frame from
    /// the sink's streaming thread.
    ///
    /// `None` means the frame has no usable running time (no segment yet, no
    /// pts): the cue state is left exactly as it is and whatever is on screen
    /// stays there, since there is no information to schedule against.
    pub fn overlays_for(&self, frame_rt: Option<gst::ClockTime>) -> SmallVec<[Overlay; 1]> {
        let mut changed = false;
        let fetch;
        let overlays;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = frame_rt {
                state.last_shown_rt = Some(rt);
                changed = evaluate(&mut state, rt);
            }
            let (want, filled) = self.resolve_raster(&mut state);
            changed |= filled;
            fetch = want;
            overlays = active_overlays(&state);
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
        overlays
    }

    /// The overlays for the frame already on screen, re-evaluated against the
    /// frozen `last_shown_rt`. This is the paused path: read from the render
    /// thread, it produces the same answer `overlays_for` would without
    /// needing a frame to flow.
    pub fn current_overlays(&self) -> SmallVec<[Overlay; 1]> {
        let mut changed = false;
        let fetch;
        let overlays;
        {
            let mut state = self.shared.state.lock();
            if let Some(rt) = state.last_shown_rt {
                changed = evaluate(&mut state, rt);
            }
            let (want, filled) = self.resolve_raster(&mut state);
            changed |= filled;
            fetch = want;
            overlays = active_overlays(&state);
        }

        if let Some(request) = fetch {
            self.request_raster(request);
        }
        if changed {
            self.mark_changed();
        }
        overlays
    }

    /// Whether the overlay set changed since the last call; clears the flag.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// Called when the overlay set changes without a frame flowing: raster
    /// completion, activation/expiry, clear. Invoked from the raster worker
    /// or from whichever thread submitted, never with an engine lock held.
    pub fn set_on_change(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.shared.on_change.lock() = Some(Arc::new(callback));
    }

    /// Cues dropped because the pending list was full.
    pub fn dropped_cues(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Rasters currently held in the cache.
    pub fn cached_rasters(&self) -> usize {
        self.shared.cache.lock().len()
    }

    /// Start the raster worker and have it load its fonts now.
    ///
    /// Parley's font collection is much cheaper to build than a fontconfig
    /// fontmap, but "much cheaper" is still not "free": it happens here, on
    /// the dedicated thread, at sink construction — never on a streaming or
    /// event-loop thread, and never in the middle of a cue.
    pub fn warm(&self) {
        let inbox = self.worker_inbox();
        let mut slot = inbox.slot.lock();
        slot.warm = true;
        inbox.cv.notify_all();
    }

    /// What the last rasters cost, in order, from the request reaching the
    /// worker to the pixels being ready. Cache hits are excluded by
    /// construction — they never reach the worker.
    pub fn raster_latencies(&self) -> Vec<Duration> {
        self.shared
            .raster_latencies
            .lock()
            .iter()
            .copied()
            .collect()
    }

    /// How long the font warm-up took, once it has finished.
    pub fn warm_up_time(&self) -> Option<Duration> {
        match self.shared.warm_nanos.load(Ordering::Acquire) {
            0 => None,
            nanos => Some(Duration::from_nanos(nanos)),
        }
    }

    /// Cache lookup for the active cue's raster. Returns the key to hand to
    /// the worker (a miss, or a karaoke prefetch) and whether the active
    /// raster was filled from cache. Must be called with `state` locked;
    /// takes the cache lock underneath it, which is the only order this pair
    /// is ever taken in.
    fn resolve_raster(&self, state: &mut State) -> (Option<RasterKey>, bool) {
        let Some(active) = state.active.as_mut() else {
            return (None, false);
        };
        match active.raster {
            RasterState::Pending => {
                if let Some(raster) = self.shared.cache.lock().get(&active.key) {
                    active.raster = RasterState::Ready(raster);
                    (None, true)
                } else {
                    (Some(active.key.clone()), false)
                }
            }
            // Karaoke: while the current step shows, warm the next one so
            // crossing a reveal threshold is a cache hit instead of a raster
            // latency. `publish` files a prefetch under its own key without
            // touching what is on screen. (Once cached, this is one short
            // cache probe per frame, only for karaoke cues.)
            RasterState::Ready(_) if active.key.step < active.steps.len() => {
                let next = RasterKey {
                    step: active.key.step + 1,
                    ..active.key.clone()
                };
                if self.shared.cache.lock().get(&next).is_none() {
                    (Some(next), false)
                } else {
                    (None, false)
                }
            }
            _ => (None, false),
        }
    }

    fn request_raster(&self, request: RasterKey) {
        let inbox = self.worker_inbox();
        let mut slot = inbox.slot.lock();
        // Latest-wins: an older outstanding request is for a cue that is no
        // longer active, so nobody is waiting for it.
        slot.request = Some((request, Instant::now()));
        inbox.cv.notify_all();
    }

    /// The worker is spawned on first use — a sink that never shows a cue
    /// never pays for a thread.
    fn worker_inbox(&self) -> Arc<Inbox> {
        let mut worker = self.shared.worker.lock();
        if let Some(handle) = worker.as_ref() {
            return handle.inbox.clone();
        }
        let inbox = Arc::new(Inbox::default());
        let weak = Arc::downgrade(&self.shared);
        let thread_inbox = inbox.clone();
        let spawned = std::thread::Builder::new()
            .name("fvid-cue-raster".to_owned())
            .spawn(move || worker_main(weak, thread_inbox));
        match spawned {
            Ok(_) => {
                *worker = Some(WorkerHandle {
                    inbox: inbox.clone(),
                })
            }
            Err(err) => warn!(%err, "failed to spawn the cue raster thread"),
        }
        inbox
    }

    fn mark_changed(&self) {
        self.shared.dirty.store(true, Ordering::Release);
        let callback = self.shared.on_change.lock().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
}

fn active_overlays(state: &State) -> SmallVec<[Overlay; 1]> {
    let mut overlays = SmallVec::new();
    if let Some(Active {
        raster: RasterState::Ready(raster),
        ..
    }) = state.active.as_ref()
    {
        overlays.push(raster.to_overlay());
    }
    overlays
}

/// Advance the schedule to `rt`. Returns whether what should be on screen
/// changed (a different cue, or the same karaoke cue crossing a reveal step).
///
/// Single-active, latest-start-wins, exactly as before: cues that started and
/// ended between two frames are dropped without ever being shown.
fn evaluate(state: &mut State, rt: gst::ClockTime) -> bool {
    let mut candidate = None;
    while let Some(next) = state.pending.front() {
        if cue_is_in_future(next.start_rt, rt) {
            break;
        }
        let cue = state
            .pending
            .pop_front()
            .expect("front() just returned a cue");
        if cue_is_too_old(cue.end_rt, rt) {
            debug!(?cue, %rt, "cue expired before it could be shown");
            continue;
        }
        candidate = Some(cue);
    }

    let canvas = state.canvas;
    let video_rect = state.video_rect;
    let mut changed = match candidate {
        Some(cue) => {
            if state.active.as_ref().is_some_and(|act| act.cue == cue) {
                false
            } else {
                let steps = reveal_steps(&cue.content.ir, cue.start_rt, cue.pts_start);
                state.active = Some(Active {
                    key: RasterKey {
                        ir: cue.content.ir.clone(),
                        style: state.style.clone(),
                        canvas,
                        video_rect,
                        step: 0,
                    },
                    steps,
                    cue,
                    raster: RasterState::Pending,
                });
                true
            }
        }
        None => {
            let expired = state
                .active
                .as_ref()
                .is_some_and(|act| cue_is_too_old(act.cue.end_rt, rt));
            if expired {
                state.active = None;
            }
            expired
        }
    };

    // Karaoke: the raster is keyed on how many reveal steps the clock has
    // passed; crossing one re-keys it (usually a cache hit after the first
    // pass through the cue).
    if let Some(active) = state.active.as_mut()
        && !active.steps.is_empty()
    {
        let step = active.steps.partition_point(|s| *s <= rt);
        if step != active.key.step {
            active.key.step = step;
            active.raster = RasterState::Pending;
            changed = true;
        }
    }

    changed
}

/// Most-recently-used-last, capacity [`RASTER_CACHE_LIMIT`].
#[derive(Default)]
struct RasterCache {
    entries: Vec<(RasterKey, Arc<Raster>)>,
}

impl RasterCache {
    fn get(&mut self, key: &RasterKey) -> Option<Arc<Raster>> {
        let idx = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(idx);
        let raster = entry.1.clone();
        self.entries.push(entry);
        Some(raster)
    }

    fn insert(&mut self, key: RasterKey, raster: Arc<Raster>) {
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(idx);
        }
        self.entries.push((key, raster));
        while self.entries.len() > RASTER_CACHE_LIMIT {
            self.entries.remove(0);
        }
    }

    /// Drop every entry whose key fails `keep`. Called when the canvas,
    /// style or video rect changes: entries keyed on the old value can never
    /// match again, and at megabytes per raster they should not sit in
    /// memory until sixteen new inserts push them out.
    fn prune(&mut self, keep: impl Fn(&RasterKey) -> bool) {
        self.entries.retain(|(key, _)| keep(key));
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

// -- the raster worker -----------------------------------------------------------

#[derive(Default)]
struct Slot {
    /// Latest-wins: only the newest request matters, older ones are stale by
    /// construction. The key carries everything the worker needs (the IR it
    /// depicts, the canvas, the reveal step); the instant is when the request
    /// was made, so the worker can report what the wait cost.
    request: Option<(RasterKey, Instant)>,
    warm: bool,
    quit: bool,
}

#[derive(Default)]
struct Inbox {
    slot: Mutex<Slot>,
    cv: Condvar,
}

struct WorkerHandle {
    inbox: Arc<Inbox>,
}

impl WorkerHandle {
    fn stop(&self) {
        let mut slot = self.inbox.slot.lock();
        slot.quit = true;
        self.inbox.cv.notify_all();
    }
}

fn worker_main(shared: Weak<Shared>, inbox: Arc<Inbox>) {
    // Built on this thread, on first use, and kept for its lifetime: parley's
    // contexts cache font data and layouts, and the (cheap but not free)
    // font enumeration happens here rather than on a streaming thread.
    let mut ctx: Option<RasterCtx> = None;

    loop {
        let (request, warm) = {
            let mut slot = inbox.slot.lock();
            while slot.request.is_none() && !slot.warm && !slot.quit {
                inbox.cv.wait(&mut slot);
            }
            if slot.quit {
                break;
            }
            (slot.request.take(), std::mem::take(&mut slot.warm))
        };

        if warm {
            let started = Instant::now();
            let ctx = ctx.get_or_insert_with(RasterCtx::new);
            ctx.warm();
            let elapsed = started.elapsed();
            info!(?elapsed, "cue raster font stack warmed");
            if let Some(shared) = shared.upgrade() {
                shared
                    .warm_nanos
                    .store(elapsed.as_nanos().max(1) as u64, Ordering::Release);
            }
        }

        let Some((request, requested_at)) = request else {
            continue;
        };
        let Some(shared) = shared.upgrade() else {
            break;
        };

        // The engine re-requests a Pending key every frame while a raster is
        // in flight, so after publishing, the slot often holds a stale copy
        // of the request just completed. Serve it from the cache instead of
        // rendering the same pixels twice (which used to halve worker
        // throughput exactly when rasters were slowest). Cache hits are not
        // recorded in the latency window: it measures the rasterizer.
        let cached = shared.cache.lock().get(&request);
        if let Some(raster) = cached {
            if publish(&shared, request, Some(raster)) {
                let engine = CueEngine { shared };
                engine.mark_changed();
            }
            continue;
        }

        let ctx = ctx.get_or_insert_with(RasterCtx::new);
        let raster = ctx.render(&request).map(Arc::new);
        record_raster_latency(&shared, requested_at.elapsed());
        if publish(&shared, request, raster) {
            let engine = CueEngine { shared };
            engine.mark_changed();
        }
    }
}

/// How many raster costs are kept for [`CueEngine::raster_latencies`].
const RASTER_LATENCY_WINDOW: usize = 256;

fn record_raster_latency(shared: &Arc<Shared>, cost: Duration) {
    let mut latencies = shared.raster_latencies.lock();
    if latencies.len() == RASTER_LATENCY_WINDOW {
        latencies.pop_front();
    }
    latencies.push_back(cost);
}

/// Hand a finished raster to the engine. Returns whether it changed what is
/// on screen (a raster for a cue that has since been replaced only warms the
/// cache). Takes the state lock without holding the inbox lock — the worker
/// must never hold both, since the engine takes them in the opposite order.
fn publish(shared: &Arc<Shared>, key: RasterKey, raster: Option<Arc<Raster>>) -> bool {
    if let Some(raster) = raster.as_ref() {
        shared.cache.lock().insert(key.clone(), raster.clone());
    }

    let mut state = shared.state.lock();
    let Some(active) = state.active.as_mut() else {
        return false;
    };
    if active.key != key || !matches!(active.raster, RasterState::Pending) {
        return false;
    }
    active.raster = match raster {
        Some(raster) => RasterState::Ready(raster),
        None => RasterState::Failed,
    };
    matches!(active.raster, RasterState::Ready(_))
}

// -- the parley/vello rasterizer ----------------------------------------------------

/// Parley brush for cue text: fill color, optional background box, the
/// outline stroke, and whether this span has been revealed yet (karaoke).
#[derive(Clone, Debug, PartialEq)]
struct CueBrush {
    fg: Color,
    bg: Option<Color>,
    /// `(color, width_px)`; every cue has one (the house style's if the IR
    /// says nothing).
    outline: (Color, f32),
    /// `(color, dx_px, dy_px)` drop shadow, when the IR sets one.
    shadow: Option<(Color, f32, f32)>,
    /// Unrevealed karaoke spans still occupy their space (layout must not
    /// reflow as syllables appear) but paint nothing.
    revealed: bool,
}

impl Default for CueBrush {
    fn default() -> Self {
        Self {
            fg: Color::WHITE,
            bg: None,
            outline: (Color::from_rgba8(0, 0, 0, 217), 1.0),
            shadow: None,
            revealed: true,
        }
    }
}

/// The worker's layout/render state. Both contexts cache aggressively, so
/// one long-lived instance per worker thread.
struct RasterCtx {
    font_cx: FontContext,
    layout_cx: LayoutContext<CueBrush>,
    /// The render surface, reused across rasters of the same size (karaoke
    /// steps in particular): keeps vello's glyph outline/hinting cache warm
    /// (`reset()` retains it) and avoids two large allocations per raster.
    /// `render_to_pixmap` clears before writing, so reuse is safe.
    surface: Option<((u16, u16), RenderContext, Pixmap)>,
}

impl RasterCtx {
    fn new() -> Self {
        Self {
            font_cx: FontContext::new(),
            layout_cx: LayoutContext::new(),
            surface: None,
        }
    }

    /// The reusable `(RenderContext, Pixmap)` for a `dims`-sized raster.
    fn surface(&mut self, dims: (u16, u16)) -> (&mut RenderContext, &mut Pixmap) {
        let reusable = matches!(&self.surface, Some((have, _, _)) if *have == dims);
        if !reusable {
            let (w, h) = dims;
            self.surface = Some((dims, RenderContext::new(w, h), Pixmap::new(w, h)));
        }
        let (_, rc, pixmap) = self.surface.as_mut().expect("just ensured");
        if reusable {
            rc.reset();
        }
        (rc, pixmap)
    }

    /// Force the font stack to actually load: a throwaway cue, laid out and
    /// rasterized exactly like a real one.
    fn warm(&mut self) {
        let request = RasterKey {
            ir: Arc::new(CueIr::from_plain_text("Warming the font stack")),
            style: Arc::new(CueStyle::default()),
            canvas: (640, 360),
            video_rect: None,
            step: 0,
        };
        if self.render(&request).is_none() {
            warn!("cue raster warm-up produced no pixels");
        }
    }

    fn render(&mut self, request: &RasterKey) -> Option<Raster> {
        let ir = &*request.ir;
        let house = &*request.style;
        let (canvas_w, canvas_h) = request.canvas;
        if canvas_w == 0 || canvas_h == 0 {
            return None;
        }
        let (cw, ch) = (canvas_w as f32, canvas_h as f32);
        // The *frame*: where the picture sits in the window. Everything the
        // subtitle file expresses (positions, margins, SSA font sizes) is
        // relative to it; without a known rect the window doubles as it.
        let frame = match request.video_rect {
            Some(r) => Frame {
                x: r.x as f32,
                y: r.y as f32,
                w: r.width as f32,
                h: r.height as f32,
            },
            None => Frame {
                x: 0.0,
                y: 0.0,
                w: cw,
                h: ch,
            },
        };

        // Text scales with the picture, not the window: a pillarboxed video
        // should not get subtitles sized for the full screen.
        let base_px = (frame.h * house.font_height_fraction).max(house.min_font_px);
        // Cue box width from the IR's `size` (WebVTT), else the house wrap.
        let size_pct = ir
            .layout
            .size
            .unwrap_or(house.wrap_width_fraction * 100.0)
            .clamp(1.0, 100.0);
        let wrap_px = (frame.w * size_pct / 100.0).max(1.0);

        // Which reveal ranks are visible at this step (see `reveal_rank`).
        let mut step_ns: Vec<u64> = ir
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.reveal_ns)
            .collect();
        step_ns.sort_unstable();
        step_ns.dedup();

        // Flatten the cue into one string plus per-span byte ranges, the
        // shape parley's ranged builder styles with.
        let mut text = String::new();
        let mut spans: Vec<(std::ops::Range<usize>, &ir::Span)> = Vec::new();
        for (i, line) in ir.lines.iter().enumerate() {
            if i != 0 {
                text.push('\n');
            }
            for span in &line.spans {
                let start = text.len();
                text.push_str(&span.text);
                spans.push((start..text.len(), span));
            }
        }
        if text.trim().is_empty() {
            debug!("cue laid out to nothing");
            return None;
        }

        let base = &ir.base;
        let house_outline = house
            .outline
            .map(|o| (rgba(o.color), (base_px * o.width_fraction).max(1.0)))
            .unwrap_or((Color::TRANSPARENT, 0.0));
        let base_brush = CueBrush {
            fg: base.foreground.map(color).unwrap_or(Color::WHITE),
            bg: base.background.map(color),
            outline: base
                .outline
                .map(|o| (color(o.color), pt_to_px(o.width).max(1.0)))
                .unwrap_or(house_outline),
            shadow: base
                .shadow
                .map(|s| (color(s.color), pt_to_px(s.dx), pt_to_px(s.dy))),
            revealed: true,
        };

        let mut b = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, &text, 1.0, true);
        b.push_default(StyleProperty::Brush(base_brush.clone()));
        b.push_default(GenericFamily::SansSerif);
        b.push_default(LineHeight::FontSizeRelative(1.2));
        b.push_default(StyleProperty::FontSize(font_px(
            base.font_size,
            base_px,
            frame.h,
        )));
        match (base.font_family.as_deref(), house.font_family.as_deref()) {
            (Some(family), _) | (None, Some(family)) => {
                b.push_default(FontFamilyName::Named(family.into()));
            }
            (None, None) => {}
        }
        if let Some(style) = base.font_style {
            b.push_default(font_style(style));
        }
        // The house weight (bold by default); the IR (styles, <b>, CSS)
        // overrides it.
        b.push_default(FontWeight::new(
            base.font_weight.map_or(house.font_weight, f32::from),
        ));
        if base.underline == Some(true) {
            b.push_default(StyleProperty::Underline(true));
        }
        if base.strikethrough == Some(true) {
            b.push_default(StyleProperty::Strikethrough(true));
        }

        for (range, span) in &spans {
            let s = &span.style;
            let revealed = reveal_rank(&step_ns, span.reveal_ns) <= request.step;
            if !revealed
                || s.foreground.is_some()
                || s.background.is_some()
                || s.outline.is_some()
                || s.shadow.is_some()
            {
                let brush = CueBrush {
                    fg: s.foreground.map(color).unwrap_or(base_brush.fg),
                    bg: s.background.map(color).or(base_brush.bg),
                    outline: s
                        .outline
                        .map(|o| (color(o.color), pt_to_px(o.width).max(1.0)))
                        .unwrap_or(base_brush.outline),
                    shadow: s
                        .shadow
                        .map(|sh| (color(sh.color), pt_to_px(sh.dx), pt_to_px(sh.dy)))
                        .or(base_brush.shadow),
                    revealed,
                };
                b.push(StyleProperty::Brush(brush), range.clone());
            }
            if let Some(style) = s.font_style {
                b.push(font_style(style), range.clone());
            }
            if let Some(weight) = s.font_weight {
                b.push(FontWeight::new(weight as f32), range.clone());
            }
            if let Some(underline) = s.underline {
                b.push(StyleProperty::Underline(underline), range.clone());
            }
            if let Some(strikethrough) = s.strikethrough {
                b.push(StyleProperty::Strikethrough(strikethrough), range.clone());
            }
            if s.font_size.is_some() {
                b.push(
                    StyleProperty::FontSize(font_px(s.font_size, base_px, frame.h)),
                    range.clone(),
                );
            }
            if let Some(family) = s.font_family.as_deref() {
                b.push(FontFamilyName::Named(family.into()), range.clone());
            }
            if let Some(spacing) = s.letter_spacing {
                b.push(
                    StyleProperty::LetterSpacing(pt_to_px(spacing)),
                    range.clone(),
                );
            }
        }

        let mut playout = b.build(&text);
        playout.break_all_lines(Some(wrap_px));
        playout.align(alignment(ir.layout.align), AlignmentOptions::default());

        // Tight ink extents: alignment offsets lines within `wrap_px`, so the
        // raster hugs the widest line rather than the whole wrap box.
        let lh = playout.height();
        let (mut ink_x0, mut ink_x1) = (f32::MAX, 0.0f32);
        for line in playout.lines() {
            let m = line.metrics();
            ink_x0 = ink_x0.min(m.offset);
            ink_x1 = ink_x1.max(m.offset + m.advance - m.trailing_whitespace);
        }
        if ink_x0 >= ink_x1 {
            debug!("cue laid out to nothing");
            return None;
        }

        // Padding must cover whatever paints outside the glyph boxes: the
        // widest outline and the farthest shadow reach.
        let mut reach = base_brush.outline.1;
        let mut consider = |brush: &CueBrush| {
            reach = reach.max(brush.outline.1);
            if let Some((_, dx, dy)) = brush.shadow {
                reach = reach.max(dx.abs()).max(dy.abs());
            }
        };
        consider(&base_brush);
        for line in playout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(run) = item {
                    consider(&run.style().brush);
                }
            }
        }
        // The box geometry (from the house style; the subtitle's own cue
        // background reuses it, square and snug when there is none).
        let box_pad = house.background.map_or(reach, |b| b.padding * base_px);
        let box_radius = house.background.map_or(0.0, |b| b.corner_radius * base_px);
        let box_soft = house.background.map_or(0.0, |b| b.edge_softness * base_px);
        // Padding must cover whatever paints outside the ink: outline and
        // shadow reach, the box padding, and the box's feathered rim
        // (~3 standard deviations to fade out).
        let pad = reach.max(box_pad + 3.0 * box_soft).ceil() + 1.0;

        let surface_w = ((ink_x1 - ink_x0).ceil() as i32) + 2 * pad as i32;
        let surface_h = (lh.ceil() as i32) + 2 * pad as i32;
        if surface_w > MAX_RASTER_PX || surface_h > MAX_RASTER_PX {
            warn!(surface_w, surface_h, "cue raster too large, skipping");
            return None;
        }

        let (mut rc, pixmap) = self.surface((surface_w as u16, surface_h as u16));
        rc.set_transform(Affine::translate(Vec2::new(
            (pad - ink_x0) as f64,
            pad as f64,
        )));

        // Paint order, whole layout at a time so nothing overdraws a
        // neighbouring run: cue box, span boxes, shadows, outlines, fills
        // (with decorations).
        //
        // The subtitle's own cue background (SSA BorderStyle=3, WebVTT
        // ::cue { background }) wins over the house box color; the house
        // geometry applies either way.
        let box_color = ir
            .layout
            .background
            .map(color)
            .or_else(|| house.background.map(|b| rgba(b.color)));
        if let Some(bg) = box_color {
            rc.set_paint(bg);
            let rect = Rect::new(
                (ink_x0 - box_pad) as f64,
                -(box_pad as f64),
                (ink_x1 + box_pad) as f64,
                (lh + box_pad) as f64,
            );
            if box_soft > 0.0 {
                rc.fill_blurred_rounded_rect(&rect, box_radius, box_soft);
            } else if box_radius > 0.0 {
                use vello_cpu::kurbo::{RoundedRect, Shape};
                rc.fill_path(&RoundedRect::from_rect(rect, box_radius as f64).to_path(0.1));
            } else {
                rc.fill_rect(&rect);
            }
        }
        for line in playout.lines() {
            let m = line.metrics();
            let (top, bottom) = (
                (m.baseline - m.ascent) as f64,
                (m.baseline + m.descent) as f64,
            );
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let brush = glyph_run.style().brush.clone();
                if !brush.revealed {
                    continue;
                }
                if let Some(bg) = brush.bg {
                    rc.set_paint(bg);
                    rc.fill_rect(&Rect::new(
                        glyph_run.offset() as f64,
                        top,
                        (glyph_run.offset() + glyph_run.advance()) as f64,
                        bottom,
                    ));
                }
            }
        }
        for_each_revealed_run(&playout, |glyph_run| {
            if let Some((shadow, dx, dy)) = glyph_run.style().brush.shadow {
                rc.set_paint(shadow);
                draw_glyphs(rc, glyph_run, Pass::Fill, Vec2::new(dx as f64, dy as f64));
            }
        });
        for_each_revealed_run(&playout, |glyph_run| {
            let (outline, width) = glyph_run.style().brush.outline;
            if width > 0.0 {
                rc.set_paint(outline);
                rc.set_stroke(
                    Stroke::new(width as f64)
                        .with_join(Join::Round)
                        .with_caps(Cap::Round),
                );
                draw_glyphs(rc, glyph_run, Pass::Stroke, Vec2::ZERO);
            }
        });
        for_each_revealed_run(&playout, |glyph_run| {
            let brush = glyph_run.style().brush.clone();
            rc.set_paint(brush.fg);
            draw_glyphs(rc, glyph_run, Pass::Fill, Vec2::ZERO);

            let run = glyph_run.run();
            let style = glyph_run.style();
            if let Some(decoration) = &style.underline {
                let offset = decoration.offset.unwrap_or(run.metrics().underline_offset);
                let size = decoration.size.unwrap_or(run.metrics().underline_size);
                draw_decoration(rc, glyph_run, brush.fg, offset, size);
            }
            if let Some(decoration) = &style.strikethrough {
                let offset = decoration
                    .offset
                    .unwrap_or(run.metrics().strikethrough_offset);
                let size = decoration.size.unwrap_or(run.metrics().strikethrough_size);
                draw_decoration(rc, glyph_run, brush.fg, offset, size);
            }
        });

        rc.flush();
        rc.render_to_pixmap(pixmap);
        let pixels = Arc::new(premul_to_straight_rgba(pixmap.data_as_u8_slice()));

        let (x, y) = place(
            ir,
            house,
            (cw, ch),
            frame,
            surface_w as f32,
            surface_h as f32,
        );
        Some(Raster {
            pixels,
            width: surface_w as u32,
            height: surface_h as u32,
            x,
            y,
        })
    }
}

/// Which vello pass `draw_glyphs` runs.
enum Pass {
    Fill,
    Stroke,
}

/// One glyph run through vello's glyph pipeline, optionally offset (shadows).
fn draw_glyphs(
    rc: &mut RenderContext,
    glyph_run: &GlyphRun<'_, CueBrush>,
    pass: Pass,
    offset: Vec2,
) {
    let run = glyph_run.run();
    let builder = rc
        .glyph_run(run.font())
        .font_size(run.font_size())
        .hint(true)
        .normalized_coords(run.normalized_coords());
    let glyphs = glyph_run.positioned_glyphs().map(|g| Glyph {
        id: g.id,
        x: g.x + offset.x as f32,
        y: g.y + offset.y as f32,
    });
    match pass {
        Pass::Fill => builder.fill_glyphs(glyphs),
        Pass::Stroke => builder.stroke_glyphs(glyphs),
    }
}

/// Iterate the revealed glyph runs of the whole layout, in order.
fn for_each_revealed_run<'a>(
    layout: &'a parley::Layout<CueBrush>,
    mut f: impl FnMut(&GlyphRun<'a, CueBrush>),
) {
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            if glyph_run.style().brush.revealed {
                f(&glyph_run);
            }
        }
    }
}

/// A decoration (underline/strikethrough) is a filled rectangle across the
/// run's advance.
fn draw_decoration(
    rc: &mut RenderContext,
    glyph_run: &GlyphRun<'_, CueBrush>,
    color: Color,
    offset: f32,
    size: f32,
) {
    rc.set_paint(color);
    let y = (glyph_run.baseline() - offset) as f64;
    let x = glyph_run.offset() as f64;
    rc.fill_rect(&Rect::new(
        x,
        y,
        x + glyph_run.advance() as f64,
        y + size as f64,
    ));
}

/// The rectangle the file's coordinates are relative to (the video rect, or
/// the whole window when it is unknown), in window coordinates.
#[derive(Debug, Clone, Copy)]
struct Frame {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

/// Place the finished raster, in window coordinates.
///
/// Everything the subtitle file expresses is resolved inside the *frame*
/// (the picture): an explicit SSA origin (`\pos`) pins the anchor point
/// exactly, then WebVTT `position`/`line` percentages, then the anchor's own
/// frame region with the IR margins. A cue the file says nothing about is
/// house policy: bottom-center of the frame — or of the *window* when
/// [`CueStyle::use_window_margins`] is set, letting default subtitles sit in
/// the letterbox bars instead of covering the picture (the margin itself
/// stays proportional to the picture so the look does not change with the
/// bars).
fn place(
    ir: &CueIr,
    house: &CueStyle,
    (cw, ch): (f32, f32),
    frame: Frame,
    w: f32,
    h: f32,
) -> (i32, i32) {
    let anchor = ir.layout.anchor.unwrap_or(ir::Anchor::BottomCenter);
    let (col, row) = anchor_cell(anchor);
    let l = &ir.layout;
    // "The file said nothing": no explicit placement of any kind. Note the
    // anchor alone (SSA alignment, {\an8}) keeps frame placement — a
    // top-anchored cue belongs over the picture's top, not the window's.
    let positioned = l.origin.is_some()
        || l.position.is_some()
        || l.line.is_some()
        || l.margins.is_some()
        || l.anchor.is_some();
    let window_margins = house.use_window_margins && !positioned;

    let x = if let Some((ox, _)) = l.origin {
        frame.x + frame.w * ox / 100.0 - w * col
    } else if let Some(p) = l.position {
        frame.x + frame.w * p / 100.0 - w / 2.0
    } else {
        // Centered on the picture (which is centered in the window anyway).
        frame.x + (frame.w - w) / 2.0
    };
    let y = if let Some((_, oy)) = l.origin {
        frame.y + frame.h * oy / 100.0 - h * row
    } else if let Some(ir::LinePosition::Percent(p)) = l.line {
        frame.y + frame.h * p / 100.0
    } else {
        // Margin proportional to the picture, applied to the frame or —
        // for unpositioned cues under the window-margins policy — to the
        // window, whose bottom bar it may then use.
        let mv = l
            .margins
            .map(|m| m.vertical)
            .filter(|v| *v > 0.0)
            .map(|v| v / 100.0)
            .unwrap_or(house.bottom_margin_fraction)
            * frame.h;
        let (top, bottom) = if window_margins {
            (mv, ch - mv - h)
        } else {
            (frame.y + mv, frame.y + frame.h - mv - h)
        };
        match anchor_row(anchor) {
            AnchorRow::Top => top,
            AnchorRow::Center => frame.y + (frame.h - h) / 2.0,
            AnchorRow::Bottom => bottom,
        }
    };

    // Whatever was asked for, the raster must land inside the window.
    (
        (x.clamp(0.0, (cw - w).max(0.0))) as i32,
        (y.clamp(0.0, (ch - h).max(0.0))) as i32,
    )
}

enum AnchorRow {
    Top,
    Center,
    Bottom,
}

fn anchor_row(a: ir::Anchor) -> AnchorRow {
    use ir::Anchor::*;
    match a {
        TopLeft | TopCenter | TopRight => AnchorRow::Top,
        CenterLeft | Center | CenterRight => AnchorRow::Center,
        BottomLeft | BottomCenter | BottomRight => AnchorRow::Bottom,
    }
}

/// The anchor's fractional cell: `(column, row)` with `0.0` = left/top,
/// `0.5` = center, `1.0` = right/bottom — the fraction of the cue box that
/// sits before the anchor point.
fn anchor_cell(a: ir::Anchor) -> (f32, f32) {
    use ir::Anchor::*;
    match a {
        TopLeft => (0.0, 0.0),
        TopCenter => (0.5, 0.0),
        TopRight => (1.0, 0.0),
        CenterLeft => (0.0, 0.5),
        Center => (0.5, 0.5),
        CenterRight => (1.0, 0.5),
        BottomLeft => (0.0, 1.0),
        BottomCenter => (0.5, 1.0),
        BottomRight => (1.0, 1.0),
    }
}

fn color(c: ir::Color) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn rgba(c: Rgba) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], c[3])
}

fn pt_to_px(pt: f32) -> f32 {
    pt * 96.0 / 72.0
}

/// IR font size → pixels: absolute points via the CSS factor, scales against
/// the house base size, frame-height percents (SSA) against the canvas.
fn font_px(size: Option<ir::FontSize>, base_px: f32, canvas_h: f32) -> f32 {
    match size {
        Some(ir::FontSize::Points(pt)) => pt_to_px(pt),
        Some(ir::FontSize::Scale(s)) => base_px * s,
        Some(ir::FontSize::FrameHeightPercent(p)) => canvas_h * p / 100.0,
        None => base_px,
    }
}

fn font_style(style: ir::FontStyle) -> parley::FontStyle {
    match style {
        ir::FontStyle::Normal => parley::FontStyle::Normal,
        ir::FontStyle::Italic => parley::FontStyle::Italic,
        ir::FontStyle::Oblique => parley::FontStyle::Oblique(None),
    }
}

fn alignment(align: Option<ir::TextAlign>) -> Alignment {
    match align {
        // Subtitles center by default.
        None | Some(ir::TextAlign::Center) => Alignment::Center,
        Some(ir::TextAlign::Start) => Alignment::Start,
        Some(ir::TextAlign::End) => Alignment::End,
        Some(ir::TextAlign::Left) => Alignment::Left,
        Some(ir::TextAlign::Right) => Alignment::Right,
    }
}

/// `⌈255/a⌉` in 16.16 fixed point, per alpha value: `(v * RECIP[a]) >> 16`
/// rounds to within 1 LSB of `v * 255 / a` without a per-pixel division.
static UNPREMUL_RECIP: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut a = 1usize;
    while a < 256 {
        table[a] = ((255u32 << 16) + (a as u32) / 2) / (a as u32);
        a += 1;
    }
    table
};

/// vello_cpu's pixmap is premultiplied RGBA; overlays are tightly packed
/// straight-alpha RGBA (`Overlay::pixels`, uploaded as `PL_ALPHA_INDEPENDENT`).
///
/// This runs over every pixel of every raster (~20% of a raster's cost
/// before it was tuned), so the two dominant alpha populations are
/// fast-pathed — fully transparent padding and fully opaque glyph
/// interiors — and the remainder uses the reciprocal table instead of three
/// integer divisions per pixel.
fn premul_to_straight_rgba(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.as_chunks::<4>().0 {
        let alpha = px[3];
        match alpha {
            0 => out.extend_from_slice(&[0, 0, 0, 0]),
            255 => out.extend_from_slice(px),
            _ => {
                let recip = UNPREMUL_RECIP[alpha as usize];
                let unpremultiply =
                    |value: u8| -> u8 { ((value as u32 * recip + (1 << 15)) >> 16).min(255) as u8 };
                out.push(unpremultiply(px[0]));
                out.push(unpremultiply(px[1]));
                out.push(unpremultiply(px[2]));
                out.push(alpha);
            }
        }
    }
    out
}

// -- PORT SHIM ---------------------------------------------------------------------
// fcast: delete this section and `use crate::video::{Overlay, OverlaySpace};`
// instead. The fields match `fcast-video/src/video.rs` one-for-one, EXCEPT
// `pixels`, which is `Arc<Vec<u8>>` here rather than `Vec<u8>`: a cue strip
// is megabytes and an Overlay is built per displayed frame, so the buffer is
// refcount-shared with the raster instead of memcpy'd. Make the same change
// in fcast's `Overlay` — its upload path only reads `&pixels[..]`, which
// derefs identically.

/// A composited overlay for one frame (mirror of `crate::video::Overlay`).
#[derive(Debug, Clone)]
pub struct Overlay {
    pub pixels: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub render_width: u32,
    pub render_height: u32,
    pub space: OverlaySpace,
}

/// Mirror of `crate::video::OverlaySpace` (only the variant cues use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySpace {
    /// Window coordinates: composited after video scaling/rotation.
    Window,
}

// -- tests -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use subparse_formats::ir::{Color as IrColor, CueIr};

    fn ms(value: u64) -> gst::ClockTime {
        gst::ClockTime::from_mseconds(value)
    }

    fn cue(text: &str, start: u64, duration: u64) -> CueInput {
        CueInput {
            content: CueContent::plain(text),
            start_rt: ms(start),
            end_rt: Some(ms(start + duration)),
            pts_start: Some(ms(start)),
        }
    }

    /// The engine's view of what is showing, without needing a raster.
    fn showing(engine: &CueEngine) -> Option<String> {
        engine
            .shared
            .state
            .lock()
            .active
            .as_ref()
            .map(|active| active.cue.content.plain_text())
    }

    fn advance(engine: &CueEngine, rt: gst::ClockTime) -> Option<String> {
        engine.overlays_for(Some(rt));
        showing(engine)
    }

    fn wait_for<F: Fn() -> bool>(condition: F) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn ready_overlay(engine: &CueEngine) -> Overlay {
        assert!(
            wait_for(|| !engine.current_overlays().is_empty()),
            "raster never became ready"
        );
        engine.current_overlays().into_iter().next().unwrap()
    }

    // ---- scheduling (renderer-independent, matching the pango version) ----

    #[test]
    fn cue_shows_inside_its_window_and_expires() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("hello", 1_000, 1_000));
        assert_eq!(advance(&engine, ms(500)), None);
        assert_eq!(advance(&engine, ms(1_000)), Some("hello".into()));
        assert_eq!(advance(&engine, ms(1_999)), Some("hello".into()));
        assert_eq!(advance(&engine, ms(2_000)), None);
    }

    #[test]
    fn newest_cue_wins_and_flush_clears() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("one", 0, 10_000));
        engine.submit(cue("two", 1_000, 10_000));
        assert_eq!(advance(&engine, ms(1_500)), Some("two".into()));
        engine.flush();
        assert_eq!(showing(&engine), None);
    }

    // ---- rasterization -----------------------------------------------------

    #[test]
    fn plain_cue_produces_visible_pixels() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("hello world", 0, 5_000));
        engine.overlays_for(Some(ms(100)));

        let overlay = ready_overlay(&engine);
        assert!(overlay.width > 0 && overlay.height > 0);
        assert_eq!(
            overlay.pixels.len(),
            (overlay.width * overlay.height * 4) as usize
        );
        // Some white-ish fill and some dark outline must have been painted.
        let mut white = 0usize;
        let mut dark = 0usize;
        for px in overlay.pixels.chunks_exact(4) {
            if px[3] > 200 {
                if px[0] > 200 && px[1] > 200 && px[2] > 200 {
                    white += 1;
                } else if px[0] < 60 && px[1] < 60 && px[2] < 60 {
                    dark += 1;
                }
            }
        }
        assert!(white > 50, "expected a filled cue, got {white} white px");
        assert!(dark > 50, "expected an outline, got {dark} dark px");
        // Bottom strip, horizontally centred-ish.
        assert!(overlay.y as u32 > 360 / 2);
    }

    #[test]
    fn styled_ir_colors_the_fill() {
        // A red cue via the IR (what a <font color> / STYLE block produces).
        let mut ir = CueIr::from_plain_text("red text");
        ir.base.foreground = Some(IrColor::rgb(255, 0, 0));
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(CueInput {
            content: CueContent::ir(ir),
            start_rt: ms(0),
            end_rt: None,
            pts_start: None,
        });
        engine.overlays_for(Some(ms(100)));

        let overlay = ready_overlay(&engine);
        let red = overlay
            .pixels
            .chunks_exact(4)
            .filter(|px| px[3] > 200 && px[0] > 180 && px[1] < 60 && px[2] < 60)
            .count();
        assert!(red > 50, "expected red fill pixels, got {red}");
    }

    #[test]
    fn karaoke_steps_rekey_the_raster() {
        // Two spans: the second reveals at +500ms.
        let mut ir = CueIr::from_plain_text("la la");
        ir.lines[0].spans[0].text = "la ".into();
        let mut second = ir.lines[0].spans[0].clone();
        second.text = "la".into();
        second.reveal_ns = Some(ms(500).nseconds());
        ir.lines[0].spans.push(second);

        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(CueInput {
            content: CueContent::ir(ir),
            start_rt: ms(0),
            end_rt: Some(ms(2_000)),
            pts_start: Some(gst::ClockTime::ZERO),
        });

        engine.overlays_for(Some(ms(100)));
        let before = ready_overlay(&engine);
        engine.overlays_for(Some(ms(600)));
        assert!(
            wait_for(|| {
                engine
                    .current_overlays()
                    .first()
                    .is_some_and(|after| after.pixels != before.pixels)
            }),
            "crossing the reveal step must change the raster"
        );
        // Layout must not reflow: same geometry, more ink.
        let after = engine.current_overlays().into_iter().next().unwrap();
        assert_eq!((after.width, after.height), (before.width, before.height));
        let painted = |o: &Overlay| o.pixels.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(painted(&after) > painted(&before));
    }

    #[test]
    fn boxed_style_draws_a_rounded_translucent_background() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.set_style(CueStyle::boxed());
        engine.submit(cue("boxed", 0, 5_000));
        engine.overlays_for(Some(ms(100)));

        let overlay = ready_overlay(&engine);
        let px = |x: u32, y: u32| -> [u8; 4] {
            let at = ((y * overlay.width + x) * 4) as usize;
            overlay.pixels[at..at + 4].try_into().unwrap()
        };
        // The very corner is outside the rounded rect: transparent.
        assert_eq!(px(0, 0)[3], 0, "rounded corner must stay transparent");
        // Just inside the left edge at mid-height (between the box edge and
        // the text ink): the translucent black tint, not opaque, not clear.
        let edge = px(4, overlay.height / 2);
        assert!(
            edge[3] > 100 && edge[3] < 220 && edge[0] < 30,
            "expected the translucent black box inside the edge, got {edge:?}"
        );
        // The text itself still fills white somewhere.
        assert!(
            overlay
                .pixels
                .chunks_exact(4)
                .any(|px| px[3] > 200 && px[0] > 200 && px[1] > 200 && px[2] > 200),
            "expected white glyph fill inside the box"
        );
    }

    #[test]
    fn set_style_rerasters_the_active_cue() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("styled live", 0, 10_000));
        engine.overlays_for(Some(ms(100)));
        let before = ready_overlay(&engine);

        // The paused path: no frame flows, the style change alone repaints.
        engine.set_style(CueStyle::boxed());
        assert!(
            wait_for(|| {
                engine
                    .current_overlays()
                    .first()
                    .is_some_and(|after| after.pixels != before.pixels)
            }),
            "a style change must re-raster the active cue"
        );
    }

    #[test]
    fn positioned_cues_track_the_video_rect() {
        // A 2x-letterboxed-and-pillarboxed picture inside a 640x360 window.
        let rect = VideoRect {
            x: 160,
            y: 90,
            width: 320,
            height: 180,
        };
        let mut ir = CueIr::from_plain_text("sign");
        ir.layout.origin = Some((50.0, 50.0)); // SSA \pos at picture center

        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.set_video_rect(Some(rect));
        engine.submit(CueInput {
            content: CueContent::ir(ir),
            start_rt: ms(0),
            end_rt: None,
            pts_start: None,
        });
        engine.overlays_for(Some(ms(100)));

        let overlay = ready_overlay(&engine);
        // Default anchor is bottom-center: the anchor point sits at the
        // *picture's* center (320, 180 in window coords), not the window's.
        let anchor_x = overlay.x + overlay.width as i32 / 2;
        let anchor_y = overlay.y + overlay.height as i32;
        assert!(
            (anchor_x - 320).abs() <= 2 && (anchor_y - 180).abs() <= 2,
            "expected the \\pos anchor at the picture center (320,180), \
             got ({anchor_x},{anchor_y})"
        );
    }

    #[test]
    fn default_cues_may_use_the_window_bars() {
        // Letterboxed: 45px bars above and below the picture.
        let rect = VideoRect {
            x: 0,
            y: 45,
            width: 640,
            height: 270,
        };
        let submit_and_bottom = |style: CueStyle| -> i32 {
            let engine = CueEngine::new();
            engine.set_canvas(640, 360);
            engine.set_video_rect(Some(rect));
            engine.set_style(style);
            engine.submit(cue("plain subtitle", 0, 5_000));
            engine.overlays_for(Some(ms(100)));
            let overlay = ready_overlay(&engine);
            overlay.y + overlay.height as i32
        };

        // Default policy (use_window_margins): the cue sits in the bottom
        // bar, below the picture's bottom edge at y=315.
        assert!(
            submit_and_bottom(CueStyle::default()) > 315,
            "window-margins policy must use the letterbox bar"
        );
        // Opting out keeps it over the picture.
        let mut over_video = CueStyle::default();
        over_video.use_window_margins = false;
        assert!(
            submit_and_bottom(over_video) <= 315,
            "without window margins the cue must stay on the picture"
        );
    }

    #[test]
    fn cache_serves_repeat_cues() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("again", 0, 1_000));
        engine.overlays_for(Some(ms(100)));
        ready_overlay(&engine);
        let latencies = engine.raster_latencies().len();

        // Same text later: served from cache, the worker sees nothing.
        engine.overlays_for(Some(ms(1_000))); // expires the first
        engine.submit(cue("again", 2_000, 1_000));
        engine.overlays_for(Some(ms(2_100)));
        assert!(
            !engine.current_overlays().is_empty(),
            "cache hit is instant"
        );
        assert_eq!(engine.raster_latencies().len(), latencies);
    }

    #[test]
    fn karaoke_next_step_is_prefetched() {
        let mut ir = CueIr::from_plain_text("la la");
        ir.lines[0].spans[0].text = "la ".into();
        let mut second = ir.lines[0].spans[0].clone();
        second.text = "la".into();
        second.reveal_ns = Some(ms(500).nseconds());
        ir.lines[0].spans.push(second);

        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(CueInput {
            content: CueContent::ir(ir),
            start_rt: ms(0),
            end_rt: Some(ms(2_000)),
            pts_start: Some(gst::ClockTime::ZERO),
        });
        engine.overlays_for(Some(ms(100)));
        ready_overlay(&engine);

        // While step 0 shows, frames keep flowing and the next step lands in
        // the cache speculatively.
        assert!(
            wait_for(|| {
                engine.overlays_for(Some(ms(150)));
                engine.cached_rasters() >= 2
            }),
            "step 1 must be prefetched while step 0 is on screen"
        );
        let worker_renders = engine.raster_latencies().len();

        // Crossing the reveal threshold is now a cache hit: the raster is
        // there the same frame, and the worker renders nothing new.
        engine.overlays_for(Some(ms(600)));
        assert!(!engine.current_overlays().is_empty());
        assert_eq!(engine.raster_latencies().len(), worker_renders);
    }

    #[test]
    fn resize_prunes_stale_rasters() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("resized", 0, 10_000));
        engine.overlays_for(Some(ms(100)));
        ready_overlay(&engine);
        assert!(engine.cached_rasters() >= 1);

        // The old-canvas raster can never be used again: gone immediately,
        // and the cue re-rasters at the new size.
        engine.set_canvas(1280, 720);
        assert_eq!(engine.cached_rasters(), 0);
        assert!(
            wait_for(|| !engine.current_overlays().is_empty()),
            "the active cue must re-raster at the new size"
        );
    }

    #[test]
    fn unpremultiply_matches_the_reference_formula() {
        // The LUT must stay within 1 LSB of the exact rounded division for
        // every (value, alpha) pair, and be exact on the fast paths.
        for alpha in 0..=255u32 {
            for value in 0..=255u32 {
                let out = premul_to_straight_rgba(&[value as u8, 0, 0, alpha as u8]);
                let expect = match alpha {
                    0 => 0,
                    a => ((value * 255 + a / 2) / a).min(255),
                };
                let got = out[0] as u32;
                assert!(
                    got.abs_diff(expect) <= 1,
                    "v={value} a={alpha}: got {got}, want {expect}"
                );
                if alpha == 255 || alpha == 0 {
                    assert_eq!(got, expect, "fast paths must be exact");
                }
                assert_eq!(out[3], alpha as u8);
            }
        }
    }

    #[test]
    fn empty_cue_fails_without_wedging() {
        let engine = CueEngine::new();
        engine.set_canvas(640, 360);
        engine.submit(cue("   ", 0, 1_000));
        engine.overlays_for(Some(ms(100)));
        assert!(wait_for(|| {
            matches!(
                engine.shared.state.lock().active.as_ref(),
                Some(Active {
                    raster: RasterState::Failed,
                    ..
                })
            )
        }));
        assert!(engine.current_overlays().is_empty());
    }

    #[test]
    fn pango_markup_content_styles_spans() {
        // The classic element output still works, parsed without pango.
        let content = CueContent::pango_markup("<i>Hello</i> &amp; more");
        assert_eq!(content.plain_text(), "Hello & more");
    }
}
