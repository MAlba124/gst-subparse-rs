# gst-subparse-rs

A dependency-light, memory-safe Rust reimplementation of GStreamer's `subparse`
plugin: a drop-in replacement covering all the same subtitle formats. The
plugin is named `rssubparse` and registers the `rssubparse` + `rsssaparse`
elements, so it can be installed alongside the C plugin.

- `crates/subparse-formats`: the parsers. **Zero dependencies** (std only),
  one module per format, dependency-free benchmarks, unit tests.
- `crates/gst-subparse`: the GStreamer plugin wrapping them.

See [specs/](specs/) for the in-tree format references.

License: LGPL-2.1-or-later (matches upstream subparse).
