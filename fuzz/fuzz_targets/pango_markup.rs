#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Strict parity mode with an accel marker, like pango's own tests.
        if let Ok(parsed) = pango_markup::parse_markup(s, Some('_')) {
            let _ = pango_markup::dump::dump(&parsed);
            let _ = pango_markup::to_cue_ir(&parsed);
        }
        // Tolerant mode must accept anything.
        let parsed = pango_markup::parse_markup_tolerant(s);
        let _ = pango_markup::to_cue_ir(&parsed);
    }
});
