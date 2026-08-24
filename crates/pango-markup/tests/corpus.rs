// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Conformance against pango's own markup test corpus (tests/corpus/).
//!
//! valid-*.markup must produce byte-identical dumps to their .expected
//! files (pango's markup-parse driver output). fail-*.markup must be
//! rejected by the strict parser. The tolerant parser must accept
//! everything without panicking.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn corpus_files(prefix: &str) -> Vec<(String, String, Option<String>)> {
    let mut files: Vec<_> = fs::read_dir(corpus_dir())
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_string();
            if !name.starts_with(prefix) || !name.ends_with(".markup") {
                return None;
            }
            let markup = fs::read_to_string(&path).unwrap();
            let expected = fs::read_to_string(path.with_extension("expected")).ok();
            Some((name, markup, expected))
        })
        .collect();
    files.sort();
    assert!(!files.is_empty());
    files
}

/// First differing line, for a compact failure report.
fn first_diff(got: &str, want: &str) -> String {
    let mut out = String::new();
    for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        if g != w {
            let _ = write!(out, "line {}:\n  got:  {:?}\n  want: {:?}", i + 1, g, w);
            return out;
        }
    }
    let (gn, wn) = (got.lines().count(), want.lines().count());
    let _ = write!(out, "line counts differ: got {} want {}", gn, wn);
    out
}

#[test]
fn valid_corpus_matches_pango_expected_output() {
    let mut failures = Vec::new();
    for (name, markup, expected) in corpus_files("valid") {
        let expected = expected.unwrap_or_else(|| panic!("{name}: missing .expected"));
        match pango_markup::parse_markup(&markup, Some('_')) {
            Err(e) => failures.push(format!("{name}: unexpected parse error: {e}")),
            Ok(parsed) => {
                let got = pango_markup::dump::dump(&parsed);
                if got != expected {
                    failures.push(format!("{name}: {}", first_diff(&got, &expected)));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} corpus mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn fail_corpus_is_rejected() {
    let mut failures = Vec::new();
    for (name, markup, _) in corpus_files("fail") {
        if let Ok(parsed) = pango_markup::parse_markup(&markup, Some('_')) {
            failures.push(format!(
                "{name}: should have failed, parsed text {:?}",
                parsed.text
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} fail-corpus mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn tolerant_never_fails_on_corpus() {
    for (_, markup, _) in corpus_files("") {
        let parsed = pango_markup::parse_markup_tolerant(&markup);
        let _ = pango_markup::to_cue_ir(&parsed);
    }
}

/// Every prefix of every corpus file through both parsers: parse errors are
/// fine, panics are not.
#[test]
fn truncations_never_panic() {
    for (_, markup, _) in corpus_files("") {
        for end in 0..=markup.len() {
            if !markup.is_char_boundary(end) {
                continue;
            }
            let cut = &markup[..end];
            if let Ok(parsed) = pango_markup::parse_markup(cut, Some('_')) {
                let _ = pango_markup::dump::dump(&parsed);
            }
            let parsed = pango_markup::parse_markup_tolerant(cut);
            let _ = pango_markup::to_cue_ir(&parsed);
        }
    }
}
