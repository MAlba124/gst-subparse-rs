// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Dependency-free subtitle format parsers for `gst-subparse-rs`.
//!
//! This crate has **no external dependencies** (std only) and knows nothing
//! about GStreamer. It turns a decoded, UTF-8, newline-normalized subtitle body
//! into an ordered list of [`Cue`]s. The `gst-subparse` crate wraps these in
//! GStreamer elements (encoding, buffering, seeking, caps).
//!
//! Parsers follow a simple lex → parse shape. See [`format::SubtitleFormat`].

#![forbid(unsafe_code)]

pub mod autodetect;
pub mod cue;
pub mod format;
pub mod formats;
pub mod ir;

pub use cue::{Cue, CueSettings, OutputFormat, ParseContext, ParseError};
pub use format::{Format, LineScanner, Parsed, SubtitleFormat, parse_with};
