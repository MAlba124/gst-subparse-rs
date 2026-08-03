// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

#![no_main]
use libfuzzer_sys::fuzz_target;
use subparse_formats::{ParseContext, SubtitleFormat, formats::SubViewer};

// Fuzz the subviewer parser. It must never panic on arbitrary UTF-8 input.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let mut parser = SubViewer::default();
        let _ = parser.parse(text, &ParseContext::default());
    }
});
