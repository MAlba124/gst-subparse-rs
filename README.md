# gst-subparse-rs

A dependency-light, memory-safe Rust reimplementation of GStreamer's `subparse`
plugin: a drop-in replacement covering all the same subtitle formats. The
plugin is named `rssubparse` and registers the `rssubparse` + `rsssaparse`
elements, so it can be installed alongside the C plugin.

- `crates/subparse-formats`: the parsers. **Zero dependencies** (std only),
  one module per format, dependency-free benchmarks, unit tests.
- `crates/gst-subparse`: the GStreamer plugin wrapping them.

See [specs/](specs/) for the in-tree format references.

## Cue IR for custom renderers

Both elements have a `text-format` property. The default (`pango-markup`)
matches the C plugin: styling inline in the buffer text. Setting it to
`cue-ir` instead pushes plain UTF-8 text (`text/x-raw, format=utf8`) with a
`CueIrMeta` attached to every buffer, whose payload is the plain-old-data
`subparse_formats::ir::CueIr` struct (styled spans, colors, fonts,
positioning). A custom renderer — e.g. one built on `parley` + `vello_cpu` —
links `gst-subparse` as an rlib and reads the IR straight off the buffer:

```rust
if let Some(meta) = buffer.meta::<gstrssubparse::cueir::CueIrMeta>() {
    render(meta.ir()); // spans, colors, layout — no markup parsing
}
```

For WebVTT, cue-ir mode goes beyond what the C plugin ever surfaced: `STYLE`
blocks are parsed (`::cue` selectors with classes, voices, ids, `:lang`;
colors, fonts, text-shadow, outline, ruby-position — see
`subparse_formats::vttcss`) and applied to the IR with CSS cascade semantics,
and both the archaic (`T:`/`A:`/...) and modern (`align:center
position:50%`) cue-settings syntaxes fold into the IR's layout. The default
pango-markup output stays byte-identical to the C either way.

[crates/cueir-demo](crates/cueir-demo/) is a runnable end-to-end demo of
exactly that (a standalone crate, not a workspace member): it pipes a subtitle
file through `rssubparse text-format=cue-ir` and renders each cue to a PNG
with parley + vello_cpu. Its `renderer.rs` is the starting point for the fcast
receiver's subtitle renderer.

License: LGPL-2.1-or-later (matches upstream subparse).
