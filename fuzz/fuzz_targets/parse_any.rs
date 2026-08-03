// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

#![no_main]
use libfuzzer_sys::fuzz_target;
use subparse_formats::{ParseContext, SubtitleFormat, autodetect};

// Fuzz the whole detect-then-parse pipeline the element runs.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Some(fmt) = autodetect::detect(text) {
            let _ = fmt.parser().parse(text, &ParseContext::default());
        }
    }
});
