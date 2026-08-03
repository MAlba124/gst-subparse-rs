// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Typefind registration for the subtitle formats `subparse` handles.
//!
//! Ports the C `gst_sub_parse_type_find` (`gstsubparseelement.c`) so
//! `decodebin` / `playbin` autoplug the `subparse` element on subtitle files.
//! The C typefinder:
//!
//! 1. peeks the first 128 bytes (or fewer, if that is all there is; gives up on
//!    an empty stream),
//! 2. decodes that sample (BOM detect -> convert, else UTF-8 as-is, else convert
//!    via `GST_SUBTITLE_ENCODING`/ISO-8859-15), here reusing the element's
//!    [`crate::encoding`] decoder via [`crate::encoding::decode_sample`],
//! 3. runs [`autodetect::detect`] on the decoded sample, and
//! 4. suggests the detected format's static media-type caps with
//!    [`gst::TypeFindProbability::Maximum`] (or suggests nothing on no match).
//!
//! Registered at [`gst::Rank::MARGINAL`], mirroring the C
//! `GST_TYPE_FIND_REGISTER_DEFINE(subparse, ...)`.

use gst::glib;

use subparse_formats::{Format, autodetect};

use crate::encoding;

/// The number of leading bytes the C typefinder inspects.
const SAMPLE_LEN: u32 = 128;

/// The extensions the C registers the typefinder for.
const EXTENSIONS: &str = "srt,sub,mpsub,mdvd,smi,txt,dks,vtt";

/// Every media type this typefinder can suggest, used as the factory's
/// `possible_caps`. This is the same set as the `subparse` element's sink-pad
/// template, so a suggested caps always intersects the template and `decodebin`
/// can link the two.
fn possible_caps() -> gst::Caps {
    use std::str::FromStr;
    gst::Caps::from_str(
        "application/x-subtitle; application/x-subtitle-sami; \
         application/x-subtitle-tmplayer; application/x-subtitle-mpl2; \
         application/x-subtitle-dks; application/x-subtitle-qttext; \
         application/x-subtitle-lrc; application/x-subtitle-vtt",
    )
    .expect("static possible-caps string is valid")
}

/// Map a detected [`Format`] to the media-type caps the C typefind suggests.
///
/// The mapping is exactly [`Format::media_type`], which matches the C `*_CAPS`
/// one-for-one (SubRip/MicroDVD/MPSub/SubViewer -> `application/x-subtitle`,
/// TMPlayer -> `-tmplayer`, MPL2 -> `-mpl2`, and so on). `autodetect::detect`
/// never returns [`Format::Ssa`] (SSA is a separate element), so the `-ssa`
/// type never appears here.
fn caps_for(format: Format) -> gst::Caps {
    gst::Caps::new_empty_simple(format.media_type())
}

/// The pure detect-and-map step: decode the sample, autodetect the format and
/// return the caps to suggest (or `None` when nothing is recognised). Factored
/// out so it can be unit-tested without a typefind harness.
pub(crate) fn detect_sample(sample: &[u8]) -> Option<gst::Caps> {
    if sample.is_empty() {
        return None;
    }
    let text = encoding::decode_sample(sample, None);
    let format = autodetect::detect(&text)?;
    Some(caps_for(format))
}

/// The typefind function itself. Peeks up to 128 bytes (with the short-data
/// fallback), runs [`detect_sample`], and suggests the caps at maximum
/// probability.
fn type_find(tf: &mut gst::TypeFind) {
    // Use the first 128 bytes for detection, if available.
    let sample = match tf.peek(0, SAMPLE_LEN) {
        Some(data) => data.to_vec(),
        None => {
            // Fewer than 128 bytes: detect using whatever is available.
            let len = match tf.length() {
                Some(len) if len > 0 => len.min(SAMPLE_LEN as u64) as u32,
                _ => return,
            };
            match tf.peek(0, len) {
                Some(data) => data.to_vec(),
                None => return,
            }
        }
    };

    if let Some(caps) = detect_sample(&sample) {
        tf.suggest(gst::TypeFindProbability::Maximum, &caps);
    }
}

/// Register the `subparse_typefind` factory on `plugin`.
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::TypeFind::register(
        Some(plugin),
        "rssubparse_typefind",
        gst::Rank::MARGINAL,
        Some(EXTENSIONS),
        Some(&possible_caps()),
        type_find,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| gst::init().unwrap());
    }

    /// The suggested media type for a decoded sample, or `None`.
    fn media_type_of(sample: &[u8]) -> Option<String> {
        init();
        detect_sample(sample).map(|caps| caps.structure(0).unwrap().name().to_string())
    }

    // One positive detection per format, mirroring the C detection cases (and
    // the `autodetect` unit tests), asserting the *caps* the typefind suggests.

    #[cfg(feature = "microdvd")]
    #[test]
    fn microdvd_maps_to_generic_subtitle() {
        assert_eq!(
            media_type_of(b"{1}{1}12.500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n").as_deref(),
            Some("application/x-subtitle")
        );
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn subrip_maps_to_generic_subtitle() {
        assert_eq!(
            media_type_of(b"1\n00:00:01,000 --> 00:00:02,000\nOne\n\n").as_deref(),
            Some("application/x-subtitle")
        );
    }

    #[cfg(feature = "mpsub")]
    #[test]
    fn mpsub_maps_to_generic_subtitle() {
        assert_eq!(
            media_type_of(b"FORMAT=TIME\n\n0.00 3.00\nHello world\n").as_deref(),
            Some("application/x-subtitle")
        );
    }

    #[cfg(feature = "subviewer")]
    #[test]
    fn subviewer_maps_to_generic_subtitle() {
        assert_eq!(
            media_type_of(
                b"[INFORMATION]\n[TITLE]xxx\n[END INFORMATION]\n00:00:41.00,00:00:44.40\nHi\n"
            )
            .as_deref(),
            Some("application/x-subtitle")
        );
    }

    #[cfg(feature = "sami")]
    #[test]
    fn sami_maps_to_sami() {
        assert_eq!(
            media_type_of(b"<SAMI>\n<BODY>\n<SYNC Start=1000>Hi</SYNC>\n</BODY>\n</SAMI>\n")
                .as_deref(),
            Some("application/x-subtitle-sami")
        );
    }

    #[cfg(feature = "tmplayer")]
    #[test]
    fn tmplayer_maps_to_its_own_type() {
        // The C maps TMPlayer to TMP_CAPS, not the generic subtitle caps.
        assert_eq!(
            media_type_of(b"00:00:10:This is the Earth|when...\n00:00:13:\n").as_deref(),
            Some("application/x-subtitle-tmplayer")
        );
    }

    #[cfg(feature = "mpl2")]
    #[test]
    fn mpl2_maps_to_its_own_type() {
        // The C maps MPL2 to MPL2_CAPS, not the generic subtitle caps.
        assert_eq!(
            media_type_of(b"[123][456] This is the Earth at a time|when...\n").as_deref(),
            Some("application/x-subtitle-mpl2")
        );
    }

    #[cfg(feature = "dks")]
    #[test]
    fn dks_maps_to_dks() {
        assert_eq!(
            media_type_of(b"[00:00:07]THERE IS A PLACE ON EARTH[br]...\n[00:00:12]\n").as_deref(),
            Some("application/x-subtitle-dks")
        );
    }

    #[cfg(feature = "qttext")]
    #[test]
    fn qttext_maps_to_qttext() {
        assert_eq!(
            media_type_of(b"{QTtext}{timeScale:100}\n[00:00:00.00]\nHello\n").as_deref(),
            Some("application/x-subtitle-qttext")
        );
    }

    #[cfg(feature = "lrc")]
    #[test]
    fn lrc_maps_to_lrc() {
        assert_eq!(
            media_type_of(b"[ar:123]\n[ti:Title]\n[00:02.23]Line 1\n").as_deref(),
            Some("application/x-subtitle-lrc")
        );
    }

    #[cfg(feature = "webvtt")]
    #[test]
    fn webvtt_maps_to_vtt() {
        assert_eq!(
            media_type_of(b"WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHi\n").as_deref(),
            Some("application/x-subtitle-vtt")
        );
    }

    #[test]
    fn unknown_suggests_nothing() {
        assert_eq!(
            media_type_of(b"Just some random text\nnothing here\n"),
            None
        );
        // Empty sample: give up, exactly like the C `data_len == 0` early return.
        assert_eq!(media_type_of(b""), None);
    }

    #[cfg(feature = "subrip")]
    #[test]
    fn non_utf8_iso8859_15_sample_still_detected() {
        // A SubRip cue with an ISO-8859-15 'é' (0xE9), invalid UTF-8. The sample
        // decode must fall back and still detect SubRip, as the C does.
        let mut sample = b"1\n00:00:01,000 --> 00:00:02,000\nCaf".to_vec();
        sample.push(0xE9); // 'é' in ISO-8859-15
        sample.extend_from_slice(b"\n\n");
        assert_eq!(
            media_type_of(&sample).as_deref(),
            Some("application/x-subtitle")
        );
    }

    // Gated on there being at least one detectable format (SSA is a separate
    // element and never suggested here), so the `fmts` vec is never unused.
    #[cfg(any(
        feature = "subrip",
        feature = "microdvd",
        feature = "mpsub",
        feature = "subviewer",
        feature = "sami",
        feature = "tmplayer",
        feature = "mpl2",
        feature = "dks",
        feature = "qttext",
        feature = "lrc",
        feature = "webvtt",
    ))]
    #[test]
    fn every_suggestable_caps_intersects_the_possible_caps() {
        init();
        let possible = possible_caps();
        let mut fmts: Vec<Format> = Vec::new();
        #[cfg(feature = "subrip")]
        fmts.push(Format::SubRip);
        #[cfg(feature = "microdvd")]
        fmts.push(Format::MicroDvd);
        #[cfg(feature = "mpsub")]
        fmts.push(Format::MpSub);
        #[cfg(feature = "subviewer")]
        fmts.push(Format::SubViewer);
        #[cfg(feature = "sami")]
        fmts.push(Format::Sami);
        #[cfg(feature = "tmplayer")]
        fmts.push(Format::TmPlayer);
        #[cfg(feature = "mpl2")]
        fmts.push(Format::Mpl2);
        #[cfg(feature = "dks")]
        fmts.push(Format::Dks);
        #[cfg(feature = "qttext")]
        fmts.push(Format::QtText);
        #[cfg(feature = "lrc")]
        fmts.push(Format::Lrc);
        #[cfg(feature = "webvtt")]
        fmts.push(Format::WebVtt);
        for fmt in fmts {
            assert!(
                !caps_for(fmt).intersect(&possible).is_empty(),
                "{fmt:?} caps must intersect the possible-caps / sink template"
            );
        }
    }
}
