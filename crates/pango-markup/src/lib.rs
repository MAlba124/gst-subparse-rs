// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! A dependency-free reimplementation of Pango's markup parser.
//!
//! [`parse_markup`] is `pango_parse_markup`: same vocabulary (`<span>` with
//! the full attribute set, `b i s u tt big small sub sup`), same GMarkup
//! XML subset, same attribute-list semantics, verified against pango's own
//! test corpus (see `tests/corpus/`). [`parse_markup_tolerant`] is the mode
//! subtitle pipelines want: it never rejects input, degrading locally
//! instead (unknown tags become transparent, bad values are dropped,
//! malformed syntax stays literal text, unclosed tags close at end of
//! input).
//!
//! [`markup_to_cue_ir`] parses straight into
//! [`subparse_formats::ir::CueIr`], letting a renderer consume
//! `text/x-raw, format=pango-markup` buffers (from `matroskademux`,
//! `ssaparse`, C `subparse`, ...) without linking pango.
//!
//! Like `subparse-formats`, this crate is std-only. It knows nothing about
//! GStreamer.

#![forbid(unsafe_code)]

pub mod attr;
pub mod color;
pub mod dump;
pub mod fontdesc;
pub mod ir;
pub mod markup;
pub mod tolerant;
pub mod xml;

pub use ir::{markup_to_cue_ir, to_cue_ir};
pub use markup::{Parsed, parse_markup, parse_markup_tolerant};
pub use xml::XmlError;
