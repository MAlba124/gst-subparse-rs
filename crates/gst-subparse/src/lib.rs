// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! A Rust re-implementation of the upstream GStreamer `subparse` plugin.
//!
//! It registers two elements, `rs`-prefixed so they can coexist with the C
//! plugin's `subparse`/`ssaparse`:
//!
//! * `rssubparse` autodetects the subtitle format, decodes the charset, and
//!   pushes timed `text/x-raw` buffers. Mirrors the C `GstSubParse`.
//! * `rsssaparse` parses SSA/ASS subtitles into pango-markup. Mirrors the C
//!   `GstSsaParse`.
//!
//! The actual format parsers live in the dependency-free `subparse-formats`
//! crate. This crate only wraps them in GStreamer elements (buffering, charset
//! decoding, caps negotiation, seeking, EOS handling).
//!
//! The element structure is modeled on gst-plugins-rs' `closedcaption`
//! `scc_parse`/`mcc_parse` elements.

use gst::glib;

mod encoding;
#[cfg(feature = "ssa")]
mod ssaparse;
pub(crate) mod subparse;
mod typefind;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    subparse::register(plugin)?;
    #[cfg(feature = "ssa")]
    ssaparse::register(plugin)?;
    typefind::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    rssubparse,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "LGPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
