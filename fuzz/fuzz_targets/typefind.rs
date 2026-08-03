// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the typefind detection path. This mirrors what the gst-subparse
// `subparse_typefind` function feeds `autodetect::detect`: take the first <=128
// bytes of arbitrary input (the typefinder peeks 128), lossily decode to a str
// (dependency-free, standing in for the element's one-shot sample decode), and
// run format autodetection. It must never panic on arbitrary input.
fuzz_target!(|data: &[u8]| {
    let sample = &data[..data.len().min(128)];
    let text = String::from_utf8_lossy(sample);
    let _ = subparse_formats::autodetect::detect(&text);
});
