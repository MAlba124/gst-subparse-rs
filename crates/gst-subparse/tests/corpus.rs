// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Real-world corpus files pushed through the `rssubparse` element as raw
//! bytes, so every file crosses the same charset decoder and format parser
//! that production streams do.
//!
//! These corpora used to live in `subparse-formats`, whose parsers take
//! `&str` and so could never see the non-UTF-8 files at all (reading them
//! with `read_to_string` panicked before the parser ran). Charset handling is
//! this crate's job (`src/encoding.rs`), so the files are exercised here,
//! through the element.
//!
//! * `pysrt/` is the test corpus of the pysrt project. It carries the same
//!   text in five encodings (UTF-8 plus BOM-marked UTF-16 LE/BE and UTF-32
//!   LE/BE) and a BOM-less windows-1252 transcode of another file, which pins
//!   the decoder end to end: every variant must yield the reference's cues.
//! * `wptvtt/` is the WebVTT parsing corpus of web-platform-tests, a breadth
//!   smoke test for the WebVTT parser.

use std::path::{Path, PathBuf};
use std::sync::Once;

const PYSRT: &str = "tests/corpus/pysrt";
const WPTVTT: &str = "tests/corpus/wptvtt";

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // The windows-1252 file relies on the statistics fallback, which only
        // runs when no charset is named, so the variable must be unset even
        // when the suite is run with one exported. No test in this binary
        // sets it, so removing it once up front is enough.
        // SAFETY: `Once` blocks every other test until this returns, so
        // nothing is decoding or reading the environment yet.
        unsafe {
            std::env::remove_var("GST_SUBTITLE_ENCODING");
        }
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// A cue as it leaves the element: timestamp, duration and text.
type Cue = (Option<gst::ClockTime>, Option<gst::ClockTime>, String);

/// Push one file's raw bytes through the element and drain the cues. An
/// undetectable file is not an error: the element warns at EOS and emits
/// nothing, so a degenerate input simply comes back empty.
fn run_file(path: &Path) -> Vec<Cue> {
    init();
    let bytes = std::fs::read(path).unwrap();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");
    assert_eq!(
        h.push(gst::Buffer::from_slice(bytes)),
        Ok(gst::FlowSuccess::Ok),
        "push failed for {}",
        path.display(),
    );
    h.push_event(gst::event::Eos::new());

    let mut cues = Vec::new();
    while let Some(buf) = h.try_pull() {
        let text = {
            let map = buf.map_readable().unwrap();
            String::from_utf8(map.as_slice().to_vec())
                .unwrap_or_else(|_| panic!("non-UTF-8 cue text from {}", path.display()))
        };
        cues.push((buf.pts(), buf.duration(), text));
    }
    cues
}

/// Every file in `dir` whose extension is one of `extensions`, sorted for a
/// stable iteration order.
fn corpus_files(dir: &str, extensions: &[&str]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| extensions.contains(&ext.to_string_lossy().as_ref()))
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no corpus files under {dir}");
    files
}

/// Every pysrt file goes through without a panic or a flow error, whatever
/// its encoding.
#[test]
fn pysrt_corpus_smoke() {
    for path in corpus_files(PYSRT, &["srt"]) {
        let _ = run_file(&path);
    }
}

/// The BOM-marked variants all carry the same text as `bom-utf-8.srt`, so
/// they must all produce its cues. UTF-32LE is the case the C plugin gets
/// wrong: its BOM table tests the two-byte UTF-16LE mark first, and the
/// UTF-32LE mark starts with those same two bytes.
#[test]
fn pysrt_bom_variants_match_the_utf8_reference() {
    let reference = run_file(&Path::new(PYSRT).join("bom-utf-8.srt"));
    assert!(!reference.is_empty());
    for variant in [
        "bom-utf-16-le.srt",
        "bom-utf-16-be.srt",
        "bom-utf-32-le.srt",
        "bom-utf-32-be.srt",
    ] {
        assert_eq!(
            run_file(&Path::new(PYSRT).join(variant)),
            reference,
            "{variant} must decode to the same cues as bom-utf-8.srt"
        );
    }
}

/// `windows-1252.srt` is `utf-8.srt` transcoded to cp1252 (and CRLF), with no
/// BOM to declare it, so it exercises the illegal-versus-multibyte statistics
/// and the cp1252 fallback. It must come out as the same cues, accents
/// intact.
#[test]
fn pysrt_windows_1252_matches_the_utf8_reference() {
    let reference = run_file(&Path::new(PYSRT).join("utf-8.srt"));
    assert!(!reference.is_empty());
    assert!(
        reference
            .iter()
            .any(|(_, _, text)| text.contains("ÉVÉNEMENTS")),
        "the reference must contain the accented cue this test keys on"
    );
    assert_eq!(
        run_file(&Path::new(PYSRT).join("windows-1252.srt")),
        reference
    );
}

/// Breadth smoke over the WPT corpus: every file parses without a panic and
/// yields UTF-8 text, and the corpus as a whole yields cues. (The formats
/// crate's old version of this test filtered on the `.vtt` extension and so
/// skipped the two `.webvtt` files; they are included here.)
#[test]
fn wpt_vtt_corpus_smoke() {
    let mut total = 0;
    for path in corpus_files(WPTVTT, &["vtt", "webvtt"]) {
        total += run_file(&path).len();
    }
    assert!(total > 0, "the corpus must produce at least some cues");
}
