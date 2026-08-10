# cueir-demo

End-to-end demo of the cue-IR path, and the seed of the fcast receiver's
subtitle renderer:

```text
appsrc ! rssubparse text-format=cue-ir ! appsink
       -> CueIrMeta (POD CueIr struct)
       -> parley (text layout) + vello_cpu (raster)
       -> one PNG per cue
```

Run it (from this directory — the crate is deliberately **not** part of the
plugin workspace, since it pulls in the parley/vello_cpu stack):

```sh
cargo run --release                          # embedded WebVTT sample
cargo run --release path/to/subs.srt out/    # any format rssubparse detects
```

Each cue is composited onto a synthetic dark 720p "video frame" and written to
the output directory (default `cueir-demo-output/`).

## What goes where

- `src/renderer.rs` — the piece to lift into the receiver. GStreamer-free:
  takes a `subparse_formats::ir::CueIr` plus a `vello_cpu::RenderContext` and
  draws the styled, positioned cue. Maps span font/size/weight/style,
  underline/strikethrough, colors, span background boxes, letter spacing, and
  the cue-level layout (`position`/`line`/`size`/`align`). Not yet mapped
  (the IR carries them): outline, shadow, ruby annotations, sub/superscript,
  glyph scale, vertical writing, karaoke `reveal_ns`.
- `src/main.rs` — the demo harness: pipeline, `CueIrMeta` extraction, PNG
  output. In the receiver this becomes "on new sample, stash the `CueIr`;
  draw it over the video for buffers whose PTS window covers the clock".

Rendering is modeled on parley's `examples/vello_cpu_render`; `vello_cpu` is
pinned to the same git revision that parley workspace uses.

Known input-side gap: WebVTT `bg_*` background classes don't survive the
(C-faithful) markup pass — its attribute whitelist stops at `_` — so span
backgrounds currently only show up from formats like QTtext (`bgcolor`).
Lifting that is parser-side work; IR, meta and renderer already support it.
