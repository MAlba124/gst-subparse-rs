# gst-subparse-rs

A dependency-light, memory-safe Rust reimplementation of GStreamer's `subparse`
plugin: a drop-in replacement covering all the same subtitle formats. The
plugin is named `rssubparse` and registers the `rssubparse` + `rsssaparse`
elements, so it can be installed alongside the C plugin.

- `crates/subparse-formats`: the parsers. **Zero dependencies** (std only),
  one module per format, dependency-free benchmarks, unit tests.
- `crates/pango-markup`: a std-only reimplementation of `pango_parse_markup`
  (full `<span>` vocabulary, GMarkup XML subset, attribute-list semantics),
  verified byte-for-byte against pango's own test corpus (vendored under
  `crates/pango-markup/tests/corpus/`). See below.
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
position:50%`) cue-settings syntaxes fold into the IR's layout. Inline
timestamps (`<00:00:00.200>`, shown literally by the C) become per-span
karaoke reveal times.

For SSA/ASS, cue-ir mode reads the `[V4(+) Styles]` definitions (from the
file or from a container's `codec_data`) and the `{\...}` override tags the C
strips: colors, fonts, bold/italic/underline/strikeout, outline and shadow,
`\an`/`\pos` placement, margins, karaoke timing as per-span reveal times —
see `subparse_formats::ssastyle`.

For SubRip, cue-ir mode keeps the `<font color|face|size>` tags the C
deletes and honours `{\an8}`-style positioning blocks (shown literally by
the C) as the cue anchor.

The default pango-markup output stays byte-identical to the C in every case.

[crates/cueir-demo](crates/cueir-demo/) is a runnable end-to-end demo of
exactly that (a standalone crate, not a workspace member): it pipes a subtitle
file through `rssubparse text-format=cue-ir` and renders each cue to a PNG
with parley + vello_cpu. Its `renderer.rs` is the starting point for the fcast
receiver's subtitle renderer.

## Pango markup without pango

Not every `text/x-raw, format=pango-markup` buffer comes from this plugin:
`matroskademux` emits it for embedded SubRip tracks, the C `ssaparse` for
SSA, and so on. `crates/pango-markup` (re-exported as
`gstrssubparse::pango_markup`) lets a renderer consume those streams without
linking pango:

```rust
let ir = gstrssubparse::pango_markup::markup_to_cue_ir(text); // never fails
```

`parse_markup` is strict `pango_parse_markup` parity (same accepts, same
rejects, same attribute lists, proven against pango's corpus);
`markup_to_cue_ir` uses the tolerant mode instead, which degrades locally
(unknown tags become transparent, bad values drop, broken syntax stays
literal text) — the right behavior for subtitles from the wild. Together
with cue-ir mode this removes the receiver's last hard pango dependency.

License: LGPL-2.1-or-later (matches upstream subparse).
