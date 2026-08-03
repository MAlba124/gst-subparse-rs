// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Format parser implementations. Each module is dependency-free (std only)
//! and owns exactly one format, so they can be developed independently.
//!
//! Each module (and its re-exported parser struct) is gated behind its
//! per-format Cargo feature, so a disabled format is compiled out entirely.

#[cfg(feature = "dks")]
pub mod dks;
#[cfg(feature = "lrc")]
pub mod lrc;
#[cfg(feature = "microdvd")]
pub mod microdvd;
#[cfg(feature = "mpl2")]
pub mod mpl2;
#[cfg(feature = "mpsub")]
pub mod mpsub;
#[cfg(feature = "qttext")]
pub mod qttext;
#[cfg(feature = "sami")]
pub mod sami;
#[cfg(feature = "ssa")]
pub mod ssa;
#[cfg(feature = "subrip")]
pub mod subrip;
#[cfg(feature = "subviewer")]
pub mod subviewer;
#[cfg(feature = "tmplayer")]
pub mod tmplayer;
#[cfg(feature = "webvtt")]
pub mod webvtt;

#[cfg(feature = "dks")]
pub use dks::Dks;
#[cfg(feature = "lrc")]
pub use lrc::Lrc;
#[cfg(feature = "microdvd")]
pub use microdvd::MicroDvd;
#[cfg(feature = "mpl2")]
pub use mpl2::Mpl2;
#[cfg(feature = "mpsub")]
pub use mpsub::MpSub;
#[cfg(feature = "qttext")]
pub use qttext::QtText;
#[cfg(feature = "sami")]
pub use sami::Sami;
#[cfg(feature = "ssa")]
pub use ssa::Ssa;
#[cfg(feature = "subrip")]
pub use subrip::SubRip;
#[cfg(feature = "subviewer")]
pub use subviewer::SubViewer;
#[cfg(feature = "tmplayer")]
pub use tmplayer::TmPlayer;
#[cfg(feature = "webvtt")]
pub use webvtt::WebVtt;
