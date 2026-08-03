// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//
// Element-level integration tests for the `subparse` element, ported from the
// upstream C check suite
// (gst-plugins-base/tests/check/elements/subparse.c), INCLUDING the
// split-buffer ZWSP tests that fcast used to carry as a C patch
// (subparse-hold-incomplete-utf8-tail.patch, retired in favour of this
// element).
//
// Note that these push-buffer tests exercise the whole element AND the pure
// `subparse-formats` parsers + autodetect. They therefore only pass once those
// sibling crates are implemented (the scaffold parsers currently return empty
// vecs and `autodetect::detect` returns `None`). The charset partial-UTF-8 hold
// (the critical piece of the element crate) is validated independently and
// unconditionally by the unit tests in `src/encoding.rs`.

use std::sync::Once;

use gst::prelude::*;

const S: u64 = 1_000_000_000; // one second, in nanoseconds
const MS: u64 = 1_000_000; // one millisecond, in nanoseconds

/// U+200B ZERO WIDTH SPACE, a three-byte UTF-8 sequence (0xE2 0x80 0x8B).
const ZWSP: &str = "\u{200B}";

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // The charset tests below pin the behaviour with NOTHING configured, so
        // they have to see an unset variable even when the suite is run with one
        // exported. `Once` blocks every other test until this is through, so
        // nothing can be decoding while it runs.
        // SAFETY: no other thread of this binary is running a pipeline or
        // reading the environment at this point.
        unsafe {
            std::env::remove_var(ENCODING_VAR);
        }
        gst::init().unwrap();
        gstrssubparse::plugin_register_static().unwrap();
    });
}

/// The environment variable the decoder consults for a charset the user named,
/// one step after the `subtitle-encoding` property.
const ENCODING_VAR: &str = "GST_SUBTITLE_ENCODING";

/// [`ENCODING_VAR`] is process-global and the decoder reads it at the moment it
/// decides a charset, so a test that sets it must be the only charset-sensitive
/// test running while it does. Everything that either sets the variable or
/// depends on it being unset takes this lock.
static ENCODING_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_encoding_env() -> std::sync::MutexGuard<'static, ()> {
    // A failing test poisons the lock and the tests after it are still
    // meaningful, so the poison is ignored rather than cascaded.
    ENCODING_ENV.lock().unwrap_or_else(|err| err.into_inner())
}

/// Sets [`ENCODING_VAR`] for as long as it is alive and restores the previous
/// value, so a panicking test cannot leak it into the next one. The caller must
/// hold [`lock_encoding_env`].
struct EncodingEnv(Option<std::ffi::OsString>);

impl EncodingEnv {
    fn set(value: &str) -> Self {
        let previous = std::env::var_os(ENCODING_VAR);
        // SAFETY: the caller holds the encoding-env lock, so no other
        // charset-sensitive test in this binary is running.
        unsafe {
            std::env::set_var(ENCODING_VAR, value);
        }
        Self(previous)
    }
}

impl Drop for EncodingEnv {
    fn drop(&mut self) {
        // SAFETY: as in `set`, the encoding-env lock is still held.
        unsafe {
            match self.0.take() {
                Some(previous) => std::env::set_var(ENCODING_VAR, previous),
                None => std::env::remove_var(ENCODING_VAR),
            }
        }
    }
}

struct Chunk {
    input: String,
    from: u64,
    to: Option<u64>,
    out: String,
}

fn chunk(input: &str, from: u64, to: Option<u64>, out: &str) -> Chunk {
    Chunk {
        input: input.to_owned(),
        from,
        to,
        out: out.to_owned(),
    }
}

fn buffer(bytes: &[u8]) -> gst::Buffer {
    gst::Buffer::from_slice(bytes.to_vec())
}

fn text_of(buf: &gst::Buffer) -> String {
    let map = buf.map_readable().unwrap();
    String::from_utf8_lossy(map.as_slice()).into_owned()
}

/// Push each `input` as a buffer, EOS, then drain all output buffers. Returns
/// the buffers and the negotiated sink-pad caps.
fn run(inputs: &[&str], sink_caps: Option<&str>) -> (Vec<gst::Buffer>, Option<gst::Caps>) {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");
    if let Some(caps) = sink_caps {
        h.set_sink_caps_str(caps);
    }

    for input in inputs {
        assert_eq!(h.push(buffer(input.as_bytes())), Ok(gst::FlowSuccess::Ok));
    }
    h.push_event(gst::event::Eos::new());

    let mut buffers = Vec::new();
    while let Some(buf) = h.try_pull() {
        buffers.push(buf);
    }
    let caps = h.sinkpad().and_then(|p| p.current_caps());
    (buffers, caps)
}

/// The common assertion loop, checking buffer count, per-buffer
/// timestamp/duration/text, and the negotiated `format` field.
fn assert_chunks(chunks: &[Chunk], expected_format: &str) {
    let inputs: Vec<&str> = chunks.iter().map(|c| c.input.as_str()).collect();
    let (buffers, caps) = run(&inputs, None);

    assert_eq!(
        buffers.len(),
        chunks.len(),
        "expected {} output buffers, got {}",
        chunks.len(),
        buffers.len()
    );

    for (buf, chunk) in buffers.iter().zip(chunks) {
        assert_eq!(buf.pts(), Some(gst::ClockTime::from_nseconds(chunk.from)));
        if let Some(to) = chunk.to {
            assert_eq!(
                buf.duration(),
                Some(gst::ClockTime::from_nseconds(to - chunk.from))
            );
        }
        let text = text_of(buf);
        assert!(!text.ends_with('\n'), "output must not end in a newline");
        assert_eq!(text, chunk.out);
    }

    let caps = caps.expect("caps must be negotiated");
    let s = caps.structure(0).unwrap();
    assert_eq!(s.name(), "text/x-raw");
    assert_eq!(s.get::<String>("format").unwrap(), expected_format);
}

// ---------------------------------------------------------------------------
// SubRip
// ---------------------------------------------------------------------------

fn srt_input() -> Vec<Chunk> {
    vec![
        chunk(
            "1\n00:00:01,000 --> 00:00:02,000\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "2\n00:00:02,000 --> 00:00:03,000\nTwo\n\n",
            2 * S,
            Some(3 * S),
            "Two",
        ),
        chunk(
            "3\n00:00:03,000 --> 00:00:04,000\nThree\n\n",
            3 * S,
            Some(4 * S),
            "Three",
        ),
        chunk(
            "4\n00:00:04,000 --> 00:00:05,000\nFour\n\n",
            4 * S,
            Some(5 * S),
            "Four",
        ),
        chunk(
            "5\n00:00:05,000 --> 00:00:06,000\nFive\n\n",
            5 * S,
            Some(6 * S),
            "Five",
        ),
        // markup should be preserved
        chunk(
            "6\n00:00:06,000 --> 00:00:07,000\n<i>Six</i>\n\n",
            6 * S,
            Some(7 * S),
            "<i>Six</i>",
        ),
        // open markup tags should be closed
        chunk(
            "7\n00:00:07,000 --> 00:00:08,000\n<i>Seven\n\n",
            7 * S,
            Some(8 * S),
            "<i>Seven</i>",
        ),
        chunk(
            "8\n00:00:08,000 --> 00:00:09,000\n<b><i>Eight\n\n",
            8 * S,
            Some(9 * S),
            "<b><i>Eight</i></b>",
        ),
        // broken markup should be fixed
        chunk(
            "9\n00:00:09,000 --> 00:00:10,000\n</b>\n\n",
            9 * S,
            Some(10 * S),
            "",
        ),
        chunk(
            "10\n00:00:10,000 --> 00:00:11,000\n</b></i>\n\n",
            10 * S,
            Some(11 * S),
            "",
        ),
        chunk(
            "11\n00:00:11,000 --> 00:00:12,000\n<i>xyz</b></i>\n\n",
            11 * S,
            Some(12 * S),
            "<i>xyz</i>",
        ),
        chunk(
            "12\n00:00:12,000 --> 00:00:13,000\n<i>xyz</b>\n\n",
            12 * S,
            Some(13 * S),
            "<i>xyz</i>",
        ),
        // skip a few chunk numbers here, the numbers shouldn't matter
        chunk(
            "24\n00:01:00,000 --> 00:02:00,000\nYep, still here\n\n",
            60 * S,
            Some(120 * S),
            "Yep, still here",
        ),
        // escaping, but allowed markup stays intact
        chunk(
            "25\n00:03:00,000 --> 00:04:00,000\ngave <i>Rock & Roll</i> to\n\n",
            180 * S,
            Some(240 * S),
            "gave <i>Rock &amp; Roll</i> to",
        ),
        chunk(
            "26\n00:04:00,000 --> 00:05:00,000\n<i>Rock & Roll</i>\n\n",
            240 * S,
            Some(300 * S),
            "<i>Rock &amp; Roll</i>",
        ),
        chunk(
            "27\n00:06:00,000 --> 00:08:00,000\nRock & Roll\n\n",
            360 * S,
            Some(480 * S),
            "Rock &amp; Roll",
        ),
        chunk(
            "28\n00:10:00,000 --> 00:11:00,000\n<font \"#0000FF\"><joj>This is </xxx>in blue but <5</font>\n\n",
            600 * S,
            Some(660 * S),
            "This is in blue but &lt;5",
        ),
        // closing tag with a space
        chunk(
            "29\n00:11:00,000 --> 00:12:00,000\n<i>italics</ i>\n\n",
            660 * S,
            Some(720 * S),
            "<i>italics</i>",
        ),
        // unrecognised closing tag should be escaped
        chunk(
            "30\n00:12:00,000 --> 00:12:01,000\n<i>italics</ x>\n\n",
            720 * S,
            Some(721 * S),
            "<i>italics&lt;/ x&gt;</i>",
        ),
    ]
}

#[test]
fn test_srt() {
    assert_chunks(&srt_input(), "pango-markup");

    // starts with chunk number 0 (not exactly according to spec)
    let srt0 = vec![
        chunk(
            "0\n00:00:01,000 --> 00:00:02,000\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:02,000 --> 00:00:03,000\nTwo\n\n",
            2 * S,
            Some(3 * S),
            "Two",
        ),
        chunk(
            "2\n00:00:03,000 --> 00:00:04,000\nThree\n\n",
            3 * S,
            Some(4 * S),
            "Three",
        ),
    ];
    assert_chunks(&srt0, "pango-markup");

    // spaces instead of doubled zeroes
    let srt1 = vec![
        chunk(
            "1\n 0: 0:26, 26 --> 0: 0:28, 17\nI cant see.\n\n",
            26 * S + 26 * MS,
            Some(28 * S + 17 * MS),
            "I cant see.",
        ),
        chunk(
            "2\n 0: 0:30, 30 --> 0: 0:33, 22\nI really cant see.\n\n",
            30 * S + 30 * MS,
            Some(33 * S + 22 * MS),
            "I really cant see.",
        ),
        chunk(
            "3\n 0: 0:40, 40 --> 0: 0:44, 44\nI still cant see anything.\n\n",
            40 * S + 40 * MS,
            Some(44 * S + 44 * MS),
            "I still cant see anything.",
        ),
    ];
    assert_chunks(&srt1, "pango-markup");

    // UTF-8 BOM at the start
    let srt2 = vec![chunk(
        "\u{feff}1\n00:00:00,000 --> 00:00:03,50\nJust testing.\n\n",
        0,
        Some(3 * S + 500 * MS),
        "Just testing.",
    )];
    assert_chunks(&srt2, "pango-markup");

    // fewer than three post-comma digits, plus extra spaces
    let srt3 = vec![
        chunk(
            "0\n00:00:01,0 --> 00:00:02,0\nOne\n\n",
            1000 * MS,
            Some(2000 * MS),
            "One",
        ),
        chunk(
            "1\n00:00:02,5   --> 00:00:03,  5 \nTwo\n\n",
            2500 * MS,
            Some(3005 * MS),
            "Two",
        ),
        chunk(
            "2\n00:00:03, 9 --> 00:00:04,0   \nThree\n\n",
            3090 * MS,
            Some(4000 * MS),
            "Three",
        ),
    ];
    assert_chunks(&srt3, "pango-markup");

    // WebVTT-ish inline tags in SRT
    let srt4 = vec![
        chunk(
            "1\n00:00:01,000 --> 00:00:02,000\n<v>some text\n\n",
            S,
            Some(2 * S),
            "some text",
        ),
        chunk(
            "1\n00:00:01,000 --> 00:00:02,000\n<b.loud>some text\n\n",
            S,
            Some(2 * S),
            "<b>some text</b>",
        ),
        chunk(
            "1\n00:00:01,000 --> 00:00:02,000\n<ruby>base text<rt>annotation</rt></ruby>\n\n",
            S,
            Some(2 * S),
            "base textannotation",
        ),
    ];
    assert_chunks(&srt4, "pango-markup");

    // no newline at the end
    let srt6 = vec![chunk(
        "1\n00:00:01,000 --> 00:00:02,000\nLast cue, no newline at the end",
        S,
        Some(2 * S),
        "Last cue, no newline at the end",
    )];
    assert_chunks(&srt6, "pango-markup");
}

// ---------------------------------------------------------------------------
// WebVTT
// ---------------------------------------------------------------------------

fn assert_vtt(chunks: &[Chunk]) {
    // WebVTT input is the SRT-shaped body with a "WEBVTT FILE\n" preamble.
    let inputs: Vec<String> = chunks
        .iter()
        .map(|c| format!("WEBVTT FILE\n{}", c.input))
        .collect();
    let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    let (buffers, caps) = run(&refs, None);

    assert_eq!(buffers.len(), chunks.len());
    for (buf, chunk) in buffers.iter().zip(chunks) {
        assert_eq!(buf.pts(), Some(gst::ClockTime::from_nseconds(chunk.from)));
        if let Some(to) = chunk.to {
            assert_eq!(
                buf.duration(),
                Some(gst::ClockTime::from_nseconds(to - chunk.from))
            );
        }
        assert_eq!(text_of(buf), chunk.out);
    }
    let caps = caps.expect("caps");
    let s = caps.structure(0).unwrap();
    assert_eq!(s.get::<String>("format").unwrap(), "pango-markup");
}

#[test]
fn test_webvtt() {
    let vtt = vec![
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical T:50%\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical   T:50%\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical\tT:50%\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 D:vertical-lr\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 L:-123\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 L:123\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 L:12%\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 L:12% S:35% A:start\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 A:middle\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000 A:end\nOne\n\n",
            S,
            Some(2 * S),
            "One",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\nOne & Two\n\n",
            S,
            Some(2 * S),
            "One &amp; Two",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\nOne < Two\n\n",
            S,
            Some(2 * S),
            "One &lt; Two",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\n<v Spoke>Live long and prosper\n\n",
            S,
            Some(2 * S),
            "<v Spoke>Live long and prosper</v>",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\n<v The Joker>HAHAHA\n\n",
            S,
            Some(2 * S),
            "<v The Joker>HAHAHA</v>",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\n<c.someclass>some text\n\n",
            S,
            Some(2 * S),
            "<c.someclass>some text</c>",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\n<b.loud>some text\n\n",
            S,
            Some(2 * S),
            "<b.loud>some text</b>",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:02.000\n<ruby>base text<rt>annotation</rt></ruby>\n\n",
            S,
            Some(2 * S),
            "<ruby>base text<rt>annotation</rt></ruby>",
        ),
        chunk(
            "1\n00:00:01.000 --> 00:00:03.000\nOne... <00:00:00,200>Two... <00:00:00,500>Three...\n\n",
            S,
            Some(3 * S),
            "One... &lt;00:00:00,200&gt;Two... &lt;00:00:00,500&gt;Three...",
        ),
        chunk(
            "1\n00:00:02.000 --> 00:00:03.000\nHello\nWorld\n\n",
            2 * S,
            Some(3 * S),
            "Hello\nWorld",
        ),
    ];
    assert_vtt(&vtt);

    // no hour component
    let vtt1 = vec![chunk(
        "1\n00:01.000 --> 00:02.000 D:vertical T:50%\nNo hour component\n\n",
        S,
        Some(2 * S),
        "No hour component",
    )];
    assert_vtt(&vtt1);

    // no newline at the end
    let vtt2 = vec![chunk(
        "1\n00:00:01,000 --> 00:00:02,000\nLast cue, no newline at the end",
        S,
        Some(2 * S),
        "Last cue, no newline at the end",
    )];
    assert_vtt(&vtt2);

    // wrong multi-character closing tags before the end of the line
    let vtt3 = vec![
        chunk(
            "1\n00:00:00,000 --> 00:00:01,000\n<ruby>Hello!</ruby>World!\n\n",
            0,
            Some(S),
            "<ruby>Hello!</ruby>World!",
        ),
        chunk(
            "1\n00:00:01,000 --> 00:00:02,000\n<ruby>Hello!</i></ruby>World!\n\n",
            S,
            Some(2 * S),
            "<ruby>Hello!</ruby>World!",
        ),
        chunk(
            "1\n00:00:02,000 --> 00:00:03,000\n<i>World!</ruby></i>Hello!\n\n",
            2 * S,
            Some(3 * S),
            "<i>World!</i>Hello!",
        ),
    ];
    assert_vtt(&vtt3);
}

// ---------------------------------------------------------------------------
// TMPlayer
// ---------------------------------------------------------------------------

#[test]
fn test_tmplayer_multiline() {
    let chunks = vec![
        chunk(
            "00:00:10,1=This is the Earth at a time\n00:00:10,2=when the dinosaurs roamed...\n00:00:13,1=\n",
            10 * S,
            Some(13 * S),
            "This is the Earth at a time\nwhen the dinosaurs roamed...",
        ),
        chunk(
            "00:00:14,1=a lush and fertile planet.\n00:00:16,1=\n",
            14 * S,
            Some(16 * S),
            "a lush and fertile planet.",
        ),
    ];
    assert_chunks(&chunks, "utf8");
}

#[test]
fn test_tmplayer_style3b() {
    // Also tests the max-duration clamp (third chunk clipped to 5s).
    let chunks = vec![
        chunk(
            "0:00:10:This is the Earth at a time|when the dinosaurs roamed...\n",
            10 * S,
            Some(14 * S),
            "This is the Earth at a time\nwhen the dinosaurs roamed...",
        ),
        chunk(
            "0:00:14:a lush and fertile planet.\n",
            14 * S,
            Some(16 * S),
            "a lush and fertile planet.",
        ),
        chunk(
            "0:00:16:And they liked it a lot.\n",
            16 * S,
            Some((16 + 5) * S),
            "And they liked it a lot.",
        ),
        chunk("0:00:30:Last line.", 30 * S, None, "Last line."),
    ];
    assert_chunks(&chunks, "utf8");
}

// ---------------------------------------------------------------------------
// MicroDVD
// ---------------------------------------------------------------------------

#[test]
fn test_microdvd_with_fps() {
    let chunks = vec![
        chunk(
            "{1}{1}12.500\n{100}{200}- Hi, Eddie.|- Hiya, Scotty.\n",
            8 * S,
            Some(16 * S),
            "<span>- Hi, Eddie.</span>\n<span>- Hiya, Scotty.</span>",
        ),
        chunk(
            "{1250}{1350}- Cold enough for you?|- Well, I'm only faintly alive. It's 25 below\n",
            100 * S,
            Some(108 * S),
            "<span>- Cold enough for you?</span>\n<span>- Well, I&apos;m only faintly alive. It&apos;s 25 below</span>",
        ),
    ];
    assert_chunks(&chunks, "pango-markup");
}

#[test]
fn test_microdvd_with_italics() {
    let chunks = vec![chunk(
        "{1}{1}25.000 movie info: XVID  608x256 25.0fps 699.0 MB|/SubEdit b.4060(http://subedit.com.pl)/\n{100}{200}/italics/|not italics\n",
        4 * S,
        Some(8 * S),
        "<span style=\"italic\">italics</span>\n<span>not italics</span>",
    )];
    assert_chunks(&chunks, "pango-markup");
}

// ---------------------------------------------------------------------------
// MPL2
// ---------------------------------------------------------------------------

#[test]
fn test_mpl2() {
    let chunks = vec![
        chunk(
            "[123][456] This is the Earth at a time|when the dinosaurs roamed\n",
            (123 * S) / 10,
            Some((456 * S) / 10),
            "This is the Earth at a time\nwhen the dinosaurs roamed",
        ),
        chunk(
            "[1234][5678]a lush and fertile planet.\n",
            (1234 * S) / 10,
            Some((5678 * S) / 10),
            "a lush and fertile planet.",
        ),
        chunk(
            "[12345][27890] /Italic|Normal\n",
            (12345 * S) / 10,
            Some((27890 * S) / 10),
            "<i>Italic</i>\nNormal",
        ),
        chunk(
            "[32345][37890]/Italic|/Italic\n",
            (32345 * S) / 10,
            Some((37890 * S) / 10),
            "<i>Italic</i>\n<i>Italic</i>",
        ),
        chunk(
            "[42345][47890] Normal|/Italic",
            (42345 * S) / 10,
            Some((47890 * S) / 10),
            "Normal\n<i>Italic</i>",
        ),
    ];
    assert_chunks(&chunks, "pango-markup");
}

// ---------------------------------------------------------------------------
// SubViewer
// ---------------------------------------------------------------------------

#[test]
fn test_subviewer() {
    let chunks = vec![
        chunk(
            "[INFORMATION]\n[TITLE]xxxxxxxxxx\n[AUTHOR]xxxxxxxx\n[SOURCE]xxxxxxxxxxxxxxxx\n[FILEPATH]\n[DELAY]0\n[COMMENT]\n[END INFORMATION]\n[SUBTITLE]\n[COLF]&HFFFFFF,[STYLE]bd,[SIZE]18,[FONT]Arial\n00:00:41.00,00:00:44.40\nThe Age of Gods was closing.\nEternity had come to an end.\n\n",
            41 * S,
            Some(44 * S + 40 * MS),
            "The Age of Gods was closing.\nEternity had come to an end.",
        ),
        chunk(
            "00:00:55.00,00:00:58.40\nThe heavens shook as the armies\nof Falis, God of Light...\n\n",
            55 * S,
            Some(58 * S + 40 * MS),
            "The heavens shook as the armies\nof Falis, God of Light...",
        ),
    ];
    assert_chunks(&chunks, "utf8");
}

#[test]
fn test_subviewer2() {
    let chunks = vec![
        chunk(
            "[INFORMATION]\n[TITLE]xxxxxxxxxx\n[AUTHOR]xxxxxxxxxx\n[SOURCE]xxxxxxxxxx\n[PRG]\n[FILEPATH]\n[DELAY]0\n[CD TRACK]0\n[COMMENT]\n[END INFORMATION]\n[SUBTITLE]\n[COLF]&H00FFFF,[STYLE]no,[SIZE]12,[FONT]Courier New\n00:00:07.00,00:00:11.91\nTHERE IS A PLACE ON EARTH WHERE IT[br]IS STILL THE MORNING OF LIFE...\n\n",
            7 * S,
            Some(11 * S + 91 * MS),
            "THERE IS A PLACE ON EARTH WHERE IT\nIS STILL THE MORNING OF LIFE...",
        ),
        chunk(
            "00:00:12.48,00:00:15.17\nAND THE GREAT HERDS RUN FREE.[br]SO WHAT?!\n\n",
            12 * S + 48 * MS,
            Some(15 * S + 17 * MS),
            "AND THE GREAT HERDS RUN FREE.\nSO WHAT?!",
        ),
    ];
    assert_chunks(&chunks, "utf8");
}

// ---------------------------------------------------------------------------
// DKS
// ---------------------------------------------------------------------------

#[test]
fn test_dks() {
    let chunks = vec![
        chunk(
            "[00:00:07]THERE IS A PLACE ON EARTH WHERE IT[br]IS STILL THE MORNING OF LIFE...\n[00:00:12]\n",
            7 * S,
            Some(12 * S),
            "THERE IS A PLACE ON EARTH WHERE IT\nIS STILL THE MORNING OF LIFE...",
        ),
        chunk(
            "[00:00:13]AND THE GREAT HERDS RUN FREE.[br]SO WHAT?!\n[00:00:15]\n",
            13 * S,
            Some(15 * S),
            "AND THE GREAT HERDS RUN FREE.\nSO WHAT?!",
        ),
    ];
    assert_chunks(&chunks, "utf8");
}

// ---------------------------------------------------------------------------
// SAMI
// ---------------------------------------------------------------------------

#[test]
fn test_sami() {
    let chunks = vec![
        chunk(
            "<SAMI>\n<HEAD>\n    <TITLE>Subtitle</TITLE>\n    <STYLE TYPE=\"text/css\">\n    <!--\n        P {margin-left:8pt; margin-right:8pt; margin-bottom:2pt; margin-top:2pt; text-align:center; font-size:12pt; font-weight:normal; color:black;}\n        .CC {Name:English; lang:en-AU; SAMIType:CC;}\n        #STDPrn {Name:Standard Print;}\n        #LargePrn {Name:Large Print; font-size:24pt;}\n        #SmallPrn {Name:Small Print; font-size:16pt;}\n    -->\n    </Style>\n</HEAD>\n<BODY>\n    <SYNC Start=1000>\n        <P Class=CC>\n            This is a comment.<br>\n            This is a second comment.\n",
            1000 * MS,
            Some(2000 * MS),
            "This is a comment.\nThis is a second comment.",
        ),
        chunk(
            "    <SYNC Start=2000>\n        <P Class=CC>\n            This is a third comment.<br>\n            This is a fourth comment.\n</BODY>\n</SAMI>\n",
            2000 * MS,
            None,
            "This is a third comment.\nThis is a fourth comment.",
        ),
    ];
    assert_chunks(&chunks, "pango-markup");
}

#[test]
fn test_sami_html_entities() {
    let chunks = vec![
        chunk(
            "<SAMI>\n<BODY>\n    <SYNC Start=1000>\n        <P Class=CC>\n            &nbsp; &plusmn; &acute;\n",
            1000 * MS,
            Some(2000 * MS),
            "\u{a0} \u{b1} \u{b4}",
        ),
        chunk(
            "    <SYNC Start=2000>\n        <P Class=CC>\n            &Alpha; &omega;\n",
            2000 * MS,
            Some(3000 * MS),
            "\u{391} \u{3c9}",
        ),
        chunk(
            "    <SYNC Start=3000>\n        <P Class=CC>\n            &#xa0; &#177; &#180;\n</BODY>\n</SAMI>\n",
            3000 * MS,
            None,
            "\u{a0} \u{b1} \u{b4}",
        ),
    ];
    assert_chunks(&chunks, "pango-markup");
}

// ---------------------------------------------------------------------------
// LRC
// ---------------------------------------------------------------------------

#[test]
fn test_lrc() {
    let chunks = vec![
        chunk(
            "[ar:123]\n[ti:Title]\n[al:Album]\n[00:02.23]Line 1\n",
            2230 * MS,
            None,
            "Line 1",
        ),
        chunk("[00:05.10]Line 2\n", 5100 * MS, None, "Line 2"),
        chunk("[00:06.123]Line 3\n", 6123 * MS, None, "Line 3"),
    ];
    assert_chunks(&chunks, "utf8");
}

// ---------------------------------------------------------------------------
// Raw (pango-markup -> utf8) conversion, negotiated via downstream caps
// ---------------------------------------------------------------------------

#[test]
fn test_raw_conversion() {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");
    h.set_sink_caps_str("text/x-raw, format=utf8");

    // srt_input[5], "<i>Six</i>", becomes "Six" after stripping.
    let buf = buffer(b"6\n00:00:06,000 --> 00:00:07,000\n<i>Six</i>\n\n");
    let out = h.push_and_pull(buf).expect("push_and_pull");

    assert_eq!(text_of(&out), "Six");
}

// ---------------------------------------------------------------------------
// Split-buffer UTF-8 tests (ported from the hold-incomplete-utf8-tail patch)
// ---------------------------------------------------------------------------

/// Push `input` cut into buffers at the byte offsets in `offsets`, then join the
/// text of every output buffer, each followed by '|', so that a different number
/// of buffers is a different string (exactly like the C helper).
fn split_test(input: &[u8], offsets: &[usize], sink_encoding: Option<&str>) -> String {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");
    if let Some(enc) = sink_encoding {
        h.set_sink_caps_str(enc);
    }

    let mut start = 0usize;
    for i in 0..=offsets.len() {
        let stop = if i < offsets.len() {
            offsets[i]
        } else {
            input.len()
        };
        assert!(stop > start);
        assert_eq!(
            h.push(buffer(&input[start..stop])),
            Ok(gst::FlowSuccess::Ok)
        );
        start = stop;
    }
    h.push_event(gst::event::Eos::new());

    let mut out = String::new();
    while let Some(buf) = h.try_pull() {
        out.push_str(&text_of(&buf));
        out.push('|');
    }
    out
}

#[test]
fn test_srt_utf8_split_across_buffers() {
    let srt = format!("1\n00:00:01,000 --> 00:00:02,000\nWhat{ZWSP}is{ZWSP}this??\n\n");
    let srt = srt.as_bytes();
    let expected = format!("What{ZWSP}is{ZWSP}this??|");

    // whole thing in one buffer
    assert_eq!(split_test(srt, &[], None), expected);

    // a buffer boundary in the middle of the first ZWSP must not change output
    let offset = find_subslice(srt, ZWSP.as_bytes()).unwrap();
    for split in (offset + 1)..(offset + 3) {
        assert_eq!(
            split_test(srt, &[split], None),
            expected,
            "split at {split}"
        );
    }
}

#[test]
fn test_srt_utf8_split_before_eos() {
    let srt = format!("1\n00:00:01,000 --> 00:00:02,000\nAlmost the end{ZWSP}\n\n");
    let srt = srt.as_bytes();
    let expected = format!("Almost the end{ZWSP}|");

    assert_eq!(split_test(srt, &[], None), expected);

    // the rest of the sequence only arrives in the very last buffer before EOS
    let offset = find_subslice(srt, ZWSP.as_bytes()).unwrap();
    for split in (offset + 1)..(offset + 3) {
        assert_eq!(
            split_test(srt, &[split], None),
            expected,
            "split at {split}"
        );
    }
}

#[test]
fn test_srt_utf8_truncated_at_eos() {
    // A file that really ends in the middle of a character. The last cue must
    // still be pushed, the stray byte converted from the fallback encoding.
    let srt = b"1\n00:00:01,000 --> 00:00:02,000\nTruncated\xe2";

    init();
    let _lock = lock_encoding_env();
    let _env = EncodingEnv::set("ISO-8859-15");
    let out = split_test(srt, &[], None);

    // 0xE2 in ISO-8859-15 is U+00E2 'â' (UTF-8 0xC3 0xA2). Naming a charset is
    // what puts it there: with nothing named the truncated tail is repaired to
    // U+FFFD instead, see the charset matrix.
    assert_eq!(out, "Truncated\u{00E2}|");
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Typefind (autoplug detection path)
// ---------------------------------------------------------------------------

/// Run *only* the registered `subparse_typefind` factory over `sample` and
/// return the caps it suggests (and the probability). We look the factory up by
/// name and call it directly, so the result is deterministic and does not depend
/// on the core typefinders `gst::init()` also registers.
fn run_subparse_typefind(sample: &[u8]) -> (gst::TypeFindProbability, Option<gst::Caps>) {
    init();
    let factory = gst::TypeFindFactory::factories()
        .into_iter()
        .find(|f| f.name() == "rssubparse_typefind")
        .expect("rssubparse_typefind factory must be registered by the plugin");

    let mut tf = gst::SliceTypeFind::new(sample);
    factory.call_function(&mut tf);
    (
        tf.probability.unwrap_or(gst::TypeFindProbability::None),
        tf.caps,
    )
}

fn typefind_media_type(sample: &[u8]) -> Option<String> {
    let (prob, caps) = run_subparse_typefind(sample);
    caps.map(|caps| {
        // Every suggestion is at maximum probability (C GST_TYPE_FIND_MAXIMUM).
        assert_eq!(prob, gst::TypeFindProbability::Maximum);
        caps.structure(0).unwrap().name().to_string()
    })
}

#[test]
fn test_typefind_registered_and_detects_formats() {
    // SubRip -> generic subtitle caps.
    assert_eq!(
        typefind_media_type(b"1\n00:00:01,000 --> 00:00:02,000\nOne\n\n").as_deref(),
        Some("application/x-subtitle")
    );
    // MPL2 -> its own caps (proves the media_type() fix end-to-end).
    assert_eq!(
        typefind_media_type(b"[123][456] This is the Earth at a time|when...\n").as_deref(),
        Some("application/x-subtitle-mpl2")
    );
    // TMPlayer -> its own caps (the other media_type() fix).
    assert_eq!(
        typefind_media_type(b"00:00:10:This is the Earth|when...\n00:00:13:\n").as_deref(),
        Some("application/x-subtitle-tmplayer")
    );
    // WebVTT -> vtt caps.
    assert_eq!(
        typefind_media_type(b"WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHi\n").as_deref(),
        Some("application/x-subtitle-vtt")
    );
    // Unrecognised -> no suggestion.
    assert_eq!(typefind_media_type(b"not a subtitle file at all\n"), None);
}

#[test]
fn test_typefind_caps_link_to_subparse_sink() {
    // A suggested caps must intersect the subparse element's sink-pad template,
    // otherwise decodebin could not link the two.
    init();
    let subparse = gst::ElementFactory::make("rssubparse").build().unwrap();
    let sink_templ = subparse
        .pad_template("sink")
        .expect("rssubparse has a sink pad template");
    let sink_caps = sink_templ.caps();

    let (_prob, caps) = run_subparse_typefind(b"[123][456] hi|there\n");
    let caps = caps.expect("MPL2 sample is detected");
    assert!(
        !caps.intersect(sink_caps).is_empty(),
        "typefind caps {caps:?} must intersect the subparse sink template {sink_caps:?}"
    );
}

// ---------------------------------------------------------------------------
// Seeking (the standalone-file byte-seek fallback)
// ---------------------------------------------------------------------------
//
// A TIME seek arriving on the src pad is first forwarded upstream; a byte
// source like filesrc refuses it, and the element then mirrors the C: it
// seeks upstream back to byte 0, re-parses everything, and clips the output
// to the requested segment. gst_check's harness accepts every upstream event,
// which would always take the forward path, so these tests run a real
// filesrc.

/// What the downstream pad saw, in order. Only the entries after the last
/// FLUSH_STOP belong to the current seek.
#[derive(Debug)]
enum SeekRec {
    Buffer(Option<u64>, String),
    Segment(Option<u64>),
    FlushStop,
}

/// Five one-second-spaced cues: `CueK` shown at K s .. K+0.5 s.
fn five_cue_srt() -> String {
    (1..=5)
        .map(|k| format!("{k}\n00:00:0{k},000 --> 00:00:0{k},500\nCue{k}\n\n"))
        .collect()
}

/// `filesrc ! rssubparse ! fakesink` over `content`, prerolled, then one
/// flushing TIME seek per entry of `seeks` (preroll awaited in between), then
/// played to EOS. Returns everything the sink pad saw.
fn play_file_with_seeks(name: &str, content: &str, seeks: &[u64]) -> Vec<SeekRec> {
    init();
    let path = std::env::temp_dir().join(format!("rssubparse-{name}-{}.srt", std::process::id()));
    std::fs::write(&path, content).unwrap();

    let pipeline = gst::Pipeline::new();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let subparse = gst::ElementFactory::make("rssubparse").build().unwrap();
    let fakesink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    pipeline.add_many([&filesrc, &subparse, &fakesink]).unwrap();
    gst::Element::link_many([&filesrc, &subparse, &fakesink]).unwrap();

    let records = std::sync::Arc::new(std::sync::Mutex::new(Vec::<SeekRec>::new()));
    let recorder = records.clone();
    fakesink
        .static_pad("sink")
        .unwrap()
        .add_probe(
            gst::PadProbeType::BUFFER
                | gst::PadProbeType::EVENT_DOWNSTREAM
                | gst::PadProbeType::EVENT_FLUSH,
            move |_, info| {
                let mut records = recorder.lock().unwrap();
                match &info.data {
                    Some(gst::PadProbeData::Buffer(buf)) => {
                        let text = buf
                            .map_readable()
                            .map(|m| String::from_utf8_lossy(m.as_slice()).into_owned())
                            .unwrap_or_default();
                        records.push(SeekRec::Buffer(buf.pts().map(|t| t.nseconds()), text));
                    }
                    Some(gst::PadProbeData::Event(event)) => match event.view() {
                        gst::EventView::Segment(e) => {
                            let start = e
                                .segment()
                                .downcast_ref::<gst::ClockTime>()
                                .and_then(|s| s.start())
                                .map(|t| t.nseconds());
                            records.push(SeekRec::Segment(start));
                        }
                        gst::EventView::FlushStop(_) => records.push(SeekRec::FlushStop),
                        _ => {}
                    },
                    _ => {}
                }
                gst::PadProbeReturn::Ok
            },
        )
        .unwrap();

    pipeline.set_state(gst::State::Paused).unwrap();
    let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(10));
    res.expect("preroll");

    for &target in seeks {
        assert!(
            pipeline.send_event(gst::event::Seek::new(
                1.0,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                gst::ClockTime::from_nseconds(target),
                gst::SeekType::None,
                gst::ClockTime::NONE,
            )),
            "the TIME seek to {target} was refused"
        );
        let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(10));
        res.expect("re-preroll after the seek");
    }

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(10),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => {}
        other => panic!("expected EOS, got {other:?}"),
    }
    pipeline.set_state(gst::State::Null).unwrap();
    let _ = std::fs::remove_file(&path);

    // The probe closure keeps its own reference alive, so move the data out.
    let mut records = records.lock().unwrap();
    std::mem::take(&mut *records)
}

/// The entries after the last FLUSH_STOP: the current seek's segment and
/// buffers.
fn after_last_flush(records: Vec<SeekRec>) -> Vec<SeekRec> {
    let cut = records
        .iter()
        .rposition(|r| matches!(r, SeekRec::FlushStop))
        .map(|i| i + 1)
        .unwrap_or(0);
    records.into_iter().skip(cut).collect()
}

#[test]
fn test_time_seek_on_a_file_source_clips_to_the_target() {
    let records = play_file_with_seeks("seekclip", &five_cue_srt(), &[3 * S]);
    let after = after_last_flush(records);

    // The re-pushed segment starts at the seek target, not at zero.
    assert!(
        matches!(after.first(), Some(SeekRec::Segment(Some(start))) if *start == 3 * S),
        "expected a segment starting at 3s first, got {after:?}"
    );
    // Cues fully before the target are clipped away; the rest survive.
    let cues: Vec<&str> = after
        .iter()
        .filter_map(|r| match r {
            SeekRec::Buffer(_, text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(cues, ["Cue3", "Cue4", "Cue5"], "records: {after:?}");
    for rec in &after {
        if let SeekRec::Buffer(pts, text) = rec {
            assert!(
                pts.is_some_and(|pts| pts >= 3 * S),
                "cue {text} sits before the sought segment: {after:?}"
            );
        }
    }
}

#[test]
fn test_a_second_seek_realigns_the_preserved_segment() {
    // 3s then back to 2s: the second do_seek lands on the segment the first
    // one stored (and the flushes in between must not reset it).
    let records = play_file_with_seeks("seektwice", &five_cue_srt(), &[3 * S, 2 * S]);
    let after = after_last_flush(records);

    assert!(
        matches!(after.first(), Some(SeekRec::Segment(Some(start))) if *start == 2 * S),
        "expected a segment starting at 2s first, got {after:?}"
    );
    let cues: Vec<&str> = after
        .iter()
        .filter_map(|r| match r {
            SeekRec::Buffer(_, text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(cues, ["Cue2", "Cue3", "Cue4", "Cue5"], "records: {after:?}");
}

#[test]
fn test_a_seek_past_every_cue_ends_with_no_output() {
    let records = play_file_with_seeks("seekpast", &five_cue_srt(), &[30 * S]);
    let after = after_last_flush(records);
    let cues: Vec<&str> = after
        .iter()
        .filter_map(|r| match r {
            SeekRec::Buffer(_, text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        cues.is_empty(),
        "no cue survives a past-the-end seek: {after:?}"
    );
}

#[test]
fn test_non_time_seeks_are_refused() {
    init();
    let subparse = gst::ElementFactory::make("rssubparse").build().unwrap();
    let byte_seek = gst::event::Seek::new(
        1.0,
        gst::SeekFlags::FLUSH,
        gst::SeekType::Set,
        Some(gst::format::Bytes::ZERO),
        gst::SeekType::None,
        gst::format::Bytes::NONE,
    );
    assert!(
        !subparse.static_pad("src").unwrap().send_event(byte_seek),
        "a BYTES seek must be refused (the C only supports TIME)"
    );
}

// ---------------------------------------------------------------------------
// Whole-file-aware charset detection
// ---------------------------------------------------------------------------
//
// The matrix below covers every case measured in fcast's
// `subtitle-encoding-findings.md` plus every case its receiver-side
// `subtitle_transcode` module carried its own unit tests for. The point of
// moving that detection in here is that an EXTERNAL subtitle file and the
// identical bytes arriving as an EMBEDDED track must decode the same way, so
// the rules are asserted against the element rather than against a helper that
// only external files ever reach.
//
// Assertions are on the joined cue text, one '|' after each cue, exactly like
// `split_test`, so "a different number of cues" is a different string.

/// One SubRip cue wrapping `text`, the smallest thing the element will parse.
fn one_cue(text: &str) -> String {
    format!("1\n00:00:01,000 --> 00:00:02,000\n{text}\n\n")
}

fn utf16_bytes(s: &str, little_endian: bool) -> Vec<u8> {
    let mut v = Vec::from(if little_endian {
        [0xFF, 0xFE]
    } else {
        [0xFE, 0xFF]
    });
    for u in s.encode_utf16() {
        v.extend_from_slice(&if little_endian {
            u.to_le_bytes()
        } else {
            u.to_be_bytes()
        });
    }
    v
}

fn utf32_bytes(s: &str, little_endian: bool) -> Vec<u8> {
    let mut v = Vec::from(if little_endian {
        [0xFF, 0xFE, 0x00, 0x00]
    } else {
        [0x00, 0x00, 0xFE, 0xFF]
    });
    for c in s.chars() {
        let u = c as u32;
        v.extend_from_slice(&if little_endian {
            u.to_le_bytes()
        } else {
            u.to_be_bytes()
        });
    }
    v
}

/// The charset property `parsebin`/`decodebin3` forward to every parser they
/// autoplug (`gstparsebin.c:2130-2139`), and the name the C `subparse` uses
/// (`gstsubparse.c:147`). A parser that spells it differently can only be
/// configured by hand, which nothing in an autoplugged pipeline does.
const CHARSET_PROPERTY: &str = "subtitle-encoding";

/// Push `input` cut at `splits`, with `encoding` set on [`CHARSET_PROPERTY`],
/// and return the joined cue text.
fn charset_run(input: &[u8], splits: &[usize], encoding: Option<&str>) -> String {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");
    if let Some(enc) = encoding {
        let element = h.element().expect("the harness wraps an element");
        assert!(
            element.find_property(CHARSET_PROPERTY).is_some(),
            "rssubparse must expose `{CHARSET_PROPERTY}`: that is the property \
             parsebin forwards, so any other name is a no-op in production"
        );
        element.set_property(CHARSET_PROPERTY, enc);
    }

    let mut start = 0usize;
    for i in 0..=splits.len() {
        let stop = if i < splits.len() {
            splits[i]
        } else {
            input.len()
        };
        assert!(stop >= start, "splits must be ascending and in range");
        if stop > start {
            assert_eq!(
                h.push(buffer(&input[start..stop])),
                Ok(gst::FlowSuccess::Ok)
            );
        }
        start = stop;
    }
    h.push_event(gst::event::Eos::new());

    let mut out = String::new();
    while let Some(buf) = h.try_pull() {
        out.push_str(&text_of(&buf));
        out.push('|');
    }
    out
}

struct CharsetCase {
    name: &'static str,
    input: Vec<u8>,
    splits: Vec<usize>,
    encoding: Option<&'static str>,
    expect: String,
}

fn charset_case(
    name: &'static str,
    input: Vec<u8>,
    splits: Vec<usize>,
    encoding: Option<&'static str>,
    expect: String,
) -> CharsetCase {
    CharsetCase {
        name,
        input,
        splits,
        encoding,
        expect,
    }
}

/// Text exercising the characters a charset fallback mangles: a ZWSP (the
/// three-byte sequence that straddles read boundaries in the field), an accent
/// that ISO-8859-15 and cp1252 agree on, and a CJK glyph.
const MIXED: &str = "What\u{200b} is caf\u{e9} \u{4f60}?";

fn charset_matrix() -> Vec<CharsetCase> {
    let mut cases = Vec::new();
    let want = format!("{MIXED}|");

    // -- every supported encoding, BOM and BOM-less ------------------------
    cases.push(charset_case(
        "clean utf-8, no bom",
        one_cue(MIXED).into_bytes(),
        vec![],
        None,
        want.clone(),
    ));

    let mut utf8_bom = vec![0xEF, 0xBB, 0xBF];
    utf8_bom.extend_from_slice(one_cue(MIXED).as_bytes());
    cases.push(charset_case(
        "utf-8 with bom",
        utf8_bom,
        vec![],
        None,
        want.clone(),
    ));

    cases.push(charset_case(
        "utf-16le with bom",
        utf16_bytes(&one_cue(MIXED), true),
        vec![],
        None,
        want.clone(),
    ));
    cases.push(charset_case(
        "utf-16be with bom",
        utf16_bytes(&one_cue(MIXED), false),
        vec![],
        None,
        want.clone(),
    ));
    // The trap: a UTF-32LE BOM (FF FE 00 00) STARTS WITH the UTF-16LE BOM
    // (FF FE). The C tests UTF-16LE at len >= 2 before UTF-32LE at len >= 4
    // (gstsubparseelement.c:250-264), so it reads a UTF-32LE file as UTF-16LE.
    cases.push(charset_case(
        "utf-32le with bom",
        utf32_bytes(&one_cue(MIXED), true),
        vec![],
        None,
        want.clone(),
    ));
    cases.push(charset_case(
        "utf-32be with bom",
        utf32_bytes(&one_cue(MIXED), false),
        vec![],
        None,
        want.clone(),
    ));

    // -- damaged utf-8 vs a legacy 8-bit file ------------------------------
    // One stray byte in an otherwise clean UTF-8 file. Valid multi-byte
    // sequences (3) outnumber illegal ones (1), so the file IS UTF-8 and the
    // one byte is replaced. The C latches the whole read onto ISO-8859-15,
    // destroying cues that come BEFORE the bad byte.
    let mut one_bad = one_cue(MIXED).into_bytes();
    let at = find_subslice(&one_bad, b"is").unwrap();
    one_bad.insert(at, 0xFF);
    cases.push(charset_case(
        "one invalid byte in utf-8 is replaced, not latched",
        one_bad,
        vec![],
        None,
        "What\u{200b} \u{FFFD}is caf\u{e9} \u{4f60}?|".to_string(),
    ));

    // The same, behind a UTF-8 BOM. A BOM is a declaration, so damaged bytes
    // behind it are damaged UTF-8, never another charset. The C discards the
    // detection, ISO-8859-15's the BOM into visible text, and then fails
    // format autodetection outright (findings case 11).
    let mut bom_bad = vec![0xEF, 0xBB, 0xBF];
    bom_bad.extend_from_slice(one_cue(MIXED).as_bytes());
    let at = find_subslice(&bom_bad, b"is").unwrap();
    bom_bad.insert(at, 0xFF);
    cases.push(charset_case(
        "utf-8 bom plus an invalid byte still yields cues",
        bom_bad,
        vec![],
        None,
        "What\u{200b} \u{FFFD}is caf\u{e9} \u{4f60}?|".to_string(),
    ));

    // A genuine legacy file. cp1252 and ISO-8859-15 agree on the accents and
    // disagree on 0x80-0x9F, which is exactly where the "smart" punctuation of
    // a real-world legacy subtitle lives. ISO-8859-15 maps that block to C1
    // controls, which pango draws as hex boxes and which reach the cue as
    // numeric character references.
    let mut legacy = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    legacy.extend_from_slice(&[0x93, b'C', b'a', b'f', 0xE9, 0x94, 0x85, 0x96]);
    legacy.extend_from_slice(b"\n\n");
    cases.push(charset_case(
        "legacy cp1252 punctuation beats the iso-8859-15 guess",
        legacy,
        vec![],
        None,
        "\u{201C}Caf\u{e9}\u{201D}\u{2026}\u{2013}|".to_owned(),
    ));

    // The five undefined cp1252 slots become U+FFFD rather than the C1
    // controls a strict ISO-8859-1 reading gives them.
    let mut undefined = b"1\n00:00:01,000 --> 00:00:02,000\nx".to_vec();
    undefined.push(0x81);
    undefined.extend_from_slice(b"y\n\n");
    cases.push(charset_case(
        "undefined cp1252 slots become u+fffd",
        undefined,
        vec![],
        None,
        "x\u{FFFD}y|".to_owned(),
    ));

    // Mixed damage, majority UTF-8: six valid multi-byte sequences against two
    // illegal bytes, so the file is UTF-8 and the two bytes are replaced.
    let mut mixed_utf8 =
        one_cue("caf\u{e9} na\u{ef}ve \u{fc}ber plus plenty of accents \u{e9}\u{e8}\u{fc}")
            .into_bytes();
    mixed_utf8.extend_from_slice(b"2\n00:00:02,000 --> 00:00:03,000\n");
    mixed_utf8.extend_from_slice(&[0x93, b'q', b'u', b'o', b't', b'e', 0x94]);
    mixed_utf8.extend_from_slice(b"\n\n");
    cases.push(charset_case(
        "mixed damage, majority utf-8, is repaired",
        mixed_utf8,
        vec![],
        None,
        "caf\u{e9} na\u{ef}ve \u{fc}ber plus plenty of accents \u{e9}\u{e8}\u{fc}|\u{FFFD}quote\u{FFFD}|"
            .to_owned(),
    ));

    // Mixed damage, majority legacy: three illegal bytes against one
    // accidentally valid multi-byte pair, so it stays a legacy file.
    let mut mixed_legacy = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    mixed_legacy.extend_from_slice(&[0x93, b'C', b'a', b'f', 0xE9, 0x94, b' ', 0xC3, 0xA9]);
    mixed_legacy.extend_from_slice(b"\n\n");
    cases.push(charset_case(
        "mixed damage, majority legacy, stays cp1252",
        mixed_legacy,
        vec![],
        None,
        "\u{201C}Caf\u{e9}\u{201D} \u{c3}\u{a9}|".to_owned(),
    ));

    // A truncated multi-byte sequence at the very end of the file is damage
    // too, and with nothing named it is repaired rather than flipping the
    // whole file to a legacy charset.
    let mut truncated = b"1\n00:00:01,000 --> 00:00:02,000\nends with a cut ".to_vec();
    truncated.extend_from_slice(&[0xE2, 0x80]);
    cases.push(charset_case(
        "a truncated tail at eos is repaired, not re-guessed",
        truncated,
        vec![],
        None,
        "ends with a cut \u{FFFD}|".to_owned(),
    ));

    // -- the read-boundary split -------------------------------------------
    // A multi-byte character cut in half by a push-buffer boundary. Fed as two
    // buffers, which is what a 4096-byte filesrc read or a network chunk does.
    let split_me = one_cue(MIXED).into_bytes();
    let zwsp = find_subslice(&split_me, "\u{200b}".as_bytes()).unwrap();
    for cut in 1..3 {
        cases.push(charset_case(
            "multi-byte character split across a push buffer",
            split_me.clone(),
            vec![zwsp + cut],
            None,
            want.clone(),
        ));
    }
    // And the same for a BOM'd stream, where the BOM itself is cut in half.
    let mut bom_split = vec![0xEF, 0xBB, 0xBF];
    bom_split.extend_from_slice(one_cue(MIXED).as_bytes());
    for cut in 1..3 {
        cases.push(charset_case(
            "utf-8 bom split across a push buffer",
            bom_split.clone(),
            vec![cut],
            None,
            want.clone(),
        ));
    }
    let utf16_split = utf16_bytes(&one_cue(MIXED), true);
    cases.push(charset_case(
        "utf-16le bom split across a push buffer",
        utf16_split.clone(),
        vec![1],
        None,
        want.clone(),
    ));
    cases.push(charset_case(
        "utf-16le unit split across a push buffer",
        utf16_split,
        vec![5],
        None,
        want.clone(),
    ));
    let utf32_split = utf32_bytes(&one_cue(MIXED), true);
    cases.push(charset_case(
        "utf-32le bom split across a push buffer",
        utf32_split.clone(),
        vec![2],
        None,
        want.clone(),
    ));
    cases.push(charset_case(
        "utf-32le unit split across a push buffer",
        utf32_split,
        vec![9],
        None,
        want.clone(),
    ));

    // The evidence must be pooled across buffers: the valid multi-byte
    // sequences arrive in the first buffer and the single illegal byte in the
    // second, so a per-buffer decision would call the second buffer legacy.
    let mut pooled = one_cue(MIXED).into_bytes();
    let tail = pooled.len();
    pooled.extend_from_slice(b"2\n00:00:02,000 --> 00:00:03,000\nlate");
    pooled.push(0xFF);
    pooled.extend_from_slice(b"damage\n\n");
    cases.push(charset_case(
        "evidence is pooled across push buffers",
        pooled,
        vec![tail],
        None,
        format!("{MIXED}|late\u{FFFD}damage|"),
    ));

    // -- an explicit charset -----------------------------------------------
    // A Cyrillic file no statistic could place: naming the charset must work,
    // and it must work through the property name parsebin forwards.
    let mut cyrillic = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    cyrillic.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
    cyrillic.extend_from_slice(b"\n\n");
    cases.push(charset_case(
        "an explicit charset decodes a file no statistic could place",
        cyrillic.clone(),
        vec![],
        Some("windows-1251"),
        "\u{41F}\u{440}\u{438}\u{432}\u{435}\u{442}|".to_owned(),
    ));
    // ...and without it, the cp1252 default reading, which proves the property
    // above is not a no-op.
    cases.push(charset_case(
        "the same file with nothing named falls back to cp1252",
        cyrillic,
        vec![],
        None,
        "\u{cf}\u{f0}\u{e8}\u{e2}\u{e5}\u{f2}|".to_owned(),
    ));
    // An explicit charset must NOT override a file that is honestly UTF-8: the
    // C consults the property only after UTF-8 validation fails, and so do we.
    cases.push(charset_case(
        "an explicit charset does not override valid utf-8",
        one_cue(MIXED).into_bytes(),
        vec![],
        Some("windows-1251"),
        want.clone(),
    ));

    // -- degenerate inputs --------------------------------------------------
    cases.push(charset_case(
        "empty input yields no cues and no error",
        Vec::new(),
        vec![],
        None,
        String::new(),
    ));
    cases.push(charset_case(
        "a file valid in every candidate encoding decodes identically",
        one_cue("plain ascii only").into_bytes(),
        vec![],
        None,
        "plain ascii only|".to_owned(),
    ));
    cases.push(charset_case(
        "the same ascii file with a legacy charset named",
        one_cue("plain ascii only").into_bytes(),
        vec![],
        Some("windows-1252"),
        "plain ascii only|".to_owned(),
    ));

    cases
}

#[test]
fn test_charset_matrix() {
    init();
    // Every case here pins the behaviour with no charset named, so no other
    // test may have GST_SUBTITLE_ENCODING set while these run.
    let _lock = lock_encoding_env();
    let mut failures = Vec::new();
    for case in charset_matrix() {
        let got = charset_run(&case.input, &case.splits, case.encoding);
        if got != case.expect {
            failures.push(format!(
                "  {}\n    splits={:?} encoding={:?}\n    want {:?}\n    got  {:?}",
                case.name, case.splits, case.encoding, case.expect, got
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} charset case(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The confirmed production bug: the C `subparse` names this property
/// `subtitle-encoding` (`gstsubparse.c:147`) and `parsebin` looks up exactly
/// that name on every element it connects, setting it if present
/// (`gstparsebin.c:2130-2139`). A parser that spells it `encoding` is
/// configurable only by hand, so the manual override is a SILENT NO-OP in any
/// autoplugged pipeline.
#[test]
fn test_the_property_parsebin_forwards_takes_effect() {
    init();
    // The property must beat the statistics, so nothing else may be naming a
    // charset through the environment while this runs.
    let _lock = lock_encoding_env();
    let element = gst::ElementFactory::make("rssubparse").build().unwrap();
    let pspec = element
        .find_property(CHARSET_PROPERTY)
        .unwrap_or_else(|| panic!("rssubparse must expose `{CHARSET_PROPERTY}`"));
    assert_eq!(pspec.value_type(), gst::glib::Type::STRING);

    // It round-trips...
    element.set_property(CHARSET_PROPERTY, "windows-1251");
    assert_eq!(
        element
            .property::<Option<String>>(CHARSET_PROPERTY)
            .as_deref(),
        Some("windows-1251")
    );

    // ...and it decides the decoding, which is the part a name mismatch loses.
    let mut cyrillic = b"1\n00:00:01,000 --> 00:00:02,000\n".to_vec();
    cyrillic.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
    cyrillic.extend_from_slice(b"\n\n");
    assert_eq!(
        charset_run(&cyrillic, &[], Some("windows-1251")),
        "\u{41F}\u{440}\u{438}\u{432}\u{435}\u{442}|"
    );
}

/// The bounded sniff must not turn the element into a store-and-forward
/// buffer: a cue whose bytes are unambiguous has to reach the src pad without
/// waiting for EOS, or a live subtitle stream would never show anything.
#[test]
fn test_cues_are_emitted_before_eos() {
    init();
    for text in ["ascii only", "caf\u{e9} accented", "\u{4f60}\u{597d} cjk"] {
        let mut h = gst_check::Harness::new("rssubparse");
        h.set_src_caps_str("application/x-subtitle");
        assert_eq!(
            h.push(buffer(one_cue(text).as_bytes())),
            Ok(gst::FlowSuccess::Ok)
        );
        let buf = h
            .try_pull()
            .unwrap_or_else(|| panic!("{text:?}: a complete cue must not wait for EOS"));
        assert_eq!(text_of(&buf), text);
    }
}

// ---------------------------------------------------------------------------
// Format detection: the window, and what happens when nothing matches
// ---------------------------------------------------------------------------

/// The element's own bus, so the messages it posts can be read back. A harness
/// element has no parent, and `gst_element_post_message` then delivers to the
/// element's own bus, which is otherwise unset and drops everything.
fn harness_with_bus(caps: &str) -> (gst_check::Harness, gst::Bus) {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    let bus = gst::Bus::new();
    h.element()
        .expect("the harness wraps an element")
        .set_bus(Some(&bus));
    h.set_src_caps_str(caps);
    (h, bus)
}

/// The first ERROR message on `bus`, if any.
fn pop_error(bus: &gst::Bus) -> Option<gst::glib::Error> {
    let msg = bus.pop_filtered(&[gst::MessageType::Error])?;
    match msg.view() {
        gst::MessageView::Error(err) => Some(err.error()),
        _ => unreachable!("filtered on ERROR"),
    }
}

/// A stream no format recognises is an error, not silence: the C raises
/// `GST_ELEMENT_ERROR (STREAM, WRONG_TYPE)` and fails the flow
/// (`gstsubparse.c:1571-1576`), which is what makes a pipeline give up instead
/// of playing nothing. It used to be swallowed here, and the whole body was
/// retained and re-examined on every buffer while it was.
#[test]
fn test_an_unrecognised_stream_errors_like_the_c() {
    let (mut h, bus) = harness_with_bus("application/x-subtitle");

    // Prose. Not a subtitle format, and long enough that detection has all the
    // evidence more input could give it.
    let junk = "this is not a subtitle file, not even a little bit\n".repeat(200);
    assert_eq!(
        h.push(buffer(junk.as_bytes())),
        Err(gst::FlowError::NotNegotiated),
        "an unrecognised stream must fail the flow"
    );

    let err = pop_error(&bus).expect("the element must post an error");
    assert!(
        err.matches(gst::StreamError::WrongType),
        "expected STREAM/WRONG_TYPE like the C, got {err:?}"
    );
    assert!(h.try_pull().is_none(), "nothing may be emitted");

    // The verdict is latched: the error is posted once and every buffer after
    // it is refused without being parsed.
    assert_eq!(
        h.push(buffer(junk.as_bytes())),
        Err(gst::FlowError::NotNegotiated)
    );
    assert!(
        pop_error(&bus).is_none(),
        "the error must not be posted again for every buffer"
    );
}

/// A file too small to be anything is not an error, matching the C's
/// "File too small to be a subtitles file" branch, which returns without
/// raising one (`gstsubparse.c:1505-1508`).
#[test]
fn test_a_tiny_stream_is_not_an_error() {
    let (mut h, bus) = harness_with_bus("application/x-subtitle");
    assert_eq!(h.push(buffer(b"hi\n")), Ok(gst::FlowSuccess::Ok));
    h.push_event(gst::event::Eos::new());
    assert!(h.try_pull().is_none());
    assert!(pop_error(&bus).is_none(), "a tiny file must not error out");
}

/// Detection sees the C's 35-byte prefix and nothing else
/// (`g_strndup (self->textbuf->str, 35)`, `gstsubparse.c:1510`).
///
/// Both directions matter. LRC demands that every line but the last be
/// LRC-shaped, so a body whose later stanzas are separated by a blank line is
/// rejected when the whole body is examined and accepted when only the head is,
/// which is what the C does. And the `strstr` probes are unanchored, so a marker
/// deep inside a file must not beat the format its head actually is.
#[test]
fn test_detection_looks_only_at_the_c_window() {
    // The blank line between the stanzas sits past byte 35, so the C detects
    // LRC here and parses both stanzas.
    let lrc = "[ar:Some Artist Name]\n\
               [00:01.00]First lyric line\n\
               \n\
               [00:05.00]Second stanza lyric\n";
    assert!(
        lrc.find("\n\n").expect("the body has a blank line") > 35,
        "the blank line has to sit outside the window for this to test anything"
    );
    let (buffers, caps) = run(&[lrc], None);
    let texts: Vec<String> = buffers.iter().map(text_of).collect();
    assert_eq!(
        texts,
        ["First lyric line", "Second stanza lyric"],
        "a multi-stanza LRC file must detect and parse (the blank line is \
         outside the C's detection window)"
    );
    assert_eq!(
        caps.and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok())),
        Some("utf8".to_owned()),
        "LRC negotiates plain utf8"
    );

    // `<SAMI>` deep inside the cue text must not turn a SubRip file into SAMI.
    let srt = "1\n00:00:01,000 --> 00:00:02,000\n\
               the words <SAMI> and <sami> in a cue\n\n";
    let (buffers, caps) = run(&[srt], None);
    assert_eq!(
        caps.and_then(|c| c.structure(0).and_then(|s| s.get::<String>("format").ok())),
        Some("pango-markup".to_owned()),
        "SubRip, whose head it is, negotiates pango-markup"
    );
    assert_eq!(buffers.len(), 1);
    assert_eq!(buffers[0].pts(), Some(gst::ClockTime::from_nseconds(S)));
    // The SubRip parser drops the tags it does not allow, which is what these
    // two markers are to it. A SAMI parser would have made a document of them.
    assert_eq!(text_of(&buffers[0]), "the words  and  in a cue");

    // The same marker in a body that is not a subtitle file at all. Reaching
    // deep enough to find it is what used to turn prose into a SAMI stream that
    // parses to nothing.
    let prose = format!(
        "Just a paragraph of prose with no cues in it.\n{}and a stray <SAMI> mention.\n",
        "filler line\n".repeat(20)
    );
    let (mut h, bus) = harness_with_bus("application/x-subtitle");
    assert_eq!(
        h.push(buffer(prose.as_bytes())),
        Ok(gst::FlowSuccess::Ok),
        "a short body waits for more evidence before giving a verdict"
    );
    h.push_event(gst::event::Eos::new());
    assert!(h.try_pull().is_none());
    assert!(
        pop_error(&bus).is_some_and(|err| err.matches(gst::StreamError::WrongType)),
        "prose that merely mentions <SAMI> is not a subtitle file"
    );
}

// ---------------------------------------------------------------------------
// A second stream on the same element
// ---------------------------------------------------------------------------

/// EOS, then STREAM_START and data again, with no flush anywhere. A demuxer
/// restarting a stream sends exactly this, and it used to reach a charset
/// decoder that had already been told the stream ended: a panic (turned into an
/// element error by `catch_panic_pad_function`) in a debug build, and a finished
/// decoder plus a stale parser in a release one.
#[test]
fn test_a_second_stream_on_the_same_element() {
    let (mut h, bus) = harness_with_bus("application/x-subtitle");

    assert_eq!(
        h.push(buffer(one_cue("first stream").as_bytes())),
        Ok(gst::FlowSuccess::Ok)
    );
    h.push_event(gst::event::Eos::new());

    // The new stream's events, in the order a real one sends them.
    assert!(h.push_event(gst::event::StreamStart::new("second")));
    assert!(h.push_event(gst::event::Caps::new(
        &gst::Caps::builder("application/x-subtitle").build()
    )));
    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );

    // A different format, so a latched one from the first stream would show.
    let second = "[123][456] second stream\n";
    assert_eq!(
        h.push(buffer(second.as_bytes())),
        Ok(gst::FlowSuccess::Ok),
        "the second stream must not error the flow"
    );
    h.push_event(gst::event::Eos::new());

    let mut texts = Vec::new();
    while let Some(buf) = h.try_pull() {
        texts.push(text_of(&buf));
    }
    assert_eq!(texts, ["first stream", "second stream"]);
    assert!(
        pop_error(&bus).is_none(),
        "a stream restart must not post an error"
    );
}

// ---------------------------------------------------------------------------
// GAP
// ---------------------------------------------------------------------------

/// Sparse subtitle pads are driven by GAPs (matroskademux emits them to keep
/// the pad's time moving), and one arriving before the first buffer must not
/// overtake the caps and segment this element holds back until it knows the
/// format. The C routes GAP through `check_initial_events` and drops it when
/// those cannot be sent yet (`gstsubparse.c:1935-1944`).
#[test]
fn test_a_gap_never_overtakes_the_caps_and_segment() {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");

    // Nothing has been parsed, so there is no format and no caps to send.
    assert!(h.push_event(gst::event::Gap::new(
        gst::ClockTime::ZERO,
        gst::ClockTime::from_seconds(1),
    )));
    // Now negotiate, by way of an actual cue.
    assert_eq!(
        h.push(buffer(one_cue("hi").as_bytes())),
        Ok(gst::FlowSuccess::Ok)
    );
    // A GAP after negotiation is ordinary and must be forwarded.
    assert!(h.push_event(gst::event::Gap::new(
        gst::ClockTime::from_seconds(2),
        gst::ClockTime::from_seconds(1),
    )));

    let (mut caps, mut segment, mut gaps) = (false, false, 0);
    while let Some(event) = h.try_pull_event() {
        match event.view() {
            gst::EventView::Caps(_) => caps = true,
            gst::EventView::Segment(_) => segment = true,
            gst::EventView::Gap(_) => {
                assert!(
                    caps && segment,
                    "a GAP reached downstream before the caps and segment"
                );
                gaps += 1;
            }
            _ => {}
        }
    }
    assert_eq!(gaps, 1, "the GAP sent after negotiation must be forwarded");
}

// ---------------------------------------------------------------------------
// Caps and tags across a flush
// ---------------------------------------------------------------------------

/// The caps and the `SUBTITLE_CODEC` tag are built under the state lock and
/// pushed without it, so a flush can land in between. The batch is then
/// abandoned, and marking the negotiation done where the events were *built*
/// loses both for the rest of the stream: downstream ends up with cues and no
/// caps at all.
#[test]
fn test_caps_and_tags_survive_a_flush_before_the_first_push() {
    use std::sync::atomic::{AtomicBool, Ordering};

    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");
    let element = h.element().expect("the harness wraps an element");
    let srcpad = element.static_pad("src").unwrap();
    let sinkpad = element.static_pad("sink").unwrap();

    // Flush the element the instant its first caps event reaches the pad, and
    // drop that event, so neither the caps nor the tag behind it get out.
    let once = std::sync::Arc::new(AtomicBool::new(false));
    srcpad
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            let Some(gst::PadProbeData::Event(event)) = &info.data else {
                return gst::PadProbeReturn::Ok;
            };
            if !matches!(event.view(), gst::EventView::Caps(_)) || once.swap(true, Ordering::SeqCst)
            {
                return gst::PadProbeReturn::Ok;
            }
            sinkpad.send_event(gst::event::FlushStart::new());
            sinkpad.send_event(gst::event::FlushStop::new(true));
            gst::PadProbeReturn::Drop
        })
        .unwrap();

    // This cue dies with the flush (its bytes are flushed with it).
    let _ = h.push(buffer(one_cue("flushed away").as_bytes()));
    // FLUSH_STOP drops the segment sticky event, so upstream sends a new one
    // before data resumes, exactly as it does after a flushing seek.
    assert!(
        h.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
            gst::ClockTime,
        >::new()))
    );
    // Data resumes.
    assert_eq!(
        h.push(buffer(one_cue("after the flush").as_bytes())),
        Ok(gst::FlowSuccess::Ok)
    );

    let mut caps = None;
    let mut codec = None;
    while let Some(event) = h.try_pull_event() {
        match event.view() {
            gst::EventView::Caps(e) => caps = Some(e.caps_owned()),
            gst::EventView::Tag(e) => {
                codec = e
                    .tag()
                    .get::<gst::tags::SubtitleCodec>()
                    .map(|t| t.get().to_owned());
            }
            _ => {}
        }
    }
    let caps = caps.expect("the caps must be sent again after the flush");
    assert_eq!(
        caps.structure(0).unwrap().get::<String>("format").unwrap(),
        "pango-markup"
    );
    assert_eq!(codec.as_deref(), Some("SubRip"), "the codec tag too");
    assert_eq!(
        h.try_pull().map(|buf| text_of(&buf)).as_deref(),
        Some("after the flush")
    );
}

// ---------------------------------------------------------------------------
// video-fps
// ---------------------------------------------------------------------------

/// The C's `video-fps` property (`gstsubparse.c:154-160`). Frame-based formats
/// have no other way to know the rate when the file does not state one, and
/// `subtitleoverlay`/`playbin` set it by this exact name on the parser they
/// plug in, so a missing property is silently out-of-sync subtitles.
#[test]
fn test_video_fps_times_frame_based_subtitles() {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    let element = h.element().expect("the harness wraps an element");
    let pspec = element
        .find_property("video-fps")
        .expect("rssubparse must expose `video-fps`");
    assert_eq!(pspec.value_type(), gst::Fraction::static_type());
    assert_eq!(
        element.property::<gst::Fraction>("video-fps"),
        gst::Fraction::new(24000, 1001),
        "the C's default"
    );

    h.set_src_caps_str("application/x-subtitle");
    // Set once the stream is already running, which is when `subtitleoverlay`
    // sets it: the video caps that state the rate arrive after the parser has
    // been plugged in.
    element.set_property("video-fps", gst::Fraction::new(25, 1));
    assert_eq!(
        element.property::<gst::Fraction>("video-fps"),
        gst::Fraction::new(25, 1)
    );
    // MicroDVD with no `{1}{1}<fps>` header, so the property decides: frame 100
    // is 4 s at 25 fps (it would be 4.170 s at the default 24000/1001).
    assert_eq!(
        h.push(buffer(b"{100}{200}Hello\n")),
        Ok(gst::FlowSuccess::Ok)
    );
    h.push_event(gst::event::Eos::new());
    let buf = h.try_pull().expect("one cue");
    assert_eq!(buf.pts(), Some(gst::ClockTime::from_nseconds(4 * S)));
    assert_eq!(buf.duration(), Some(gst::ClockTime::from_nseconds(4 * S)));
}

// ---------------------------------------------------------------------------
// Discontinuities
// ---------------------------------------------------------------------------

/// DISCONT, or a buffer that does not continue where the last one ended, is the
/// C's only mid-stream reset: parser state, the buffered body and the adapter
/// all go (`gstsubparse.c:1589-1608`). Without it the half-seen record from
/// before the discontinuity is glued to the bytes after it.
#[test]
fn test_a_discontinuity_resets_the_parser() {
    init();
    for by_offset in [false, true] {
        let mut h = gst_check::Harness::new("rssubparse");
        h.set_src_caps_str("application/x-subtitle");

        // Half a cue: a timestamp line and text, with no terminating blank line,
        // so the parser is left mid-record.
        let half = b"1\n00:00:01,000 --> 00:00:02,000\nhalf a cue, never finished";
        assert_eq!(h.push(buffer(half)), Ok(gst::FlowSuccess::Ok));

        // The stream restarts from the top.
        let restart = b"2\n00:00:05,000 --> 00:00:06,000\nsecond\n\n";
        let mut buf = gst::Buffer::from_slice(restart.to_vec());
        {
            let b = buf.get_mut().unwrap();
            if by_offset {
                // No flag, just an offset that is not where the last buffer
                // ended, which the C treats as a discontinuity all the same.
                b.set_offset(4096);
            } else {
                b.set_flags(gst::BufferFlags::DISCONT);
            }
        }
        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
        h.push_event(gst::event::Eos::new());

        let mut cues = Vec::new();
        while let Some(buf) = h.try_pull() {
            cues.push((buf.pts().map(|t| t.nseconds()), text_of(&buf)));
        }
        assert_eq!(
            cues,
            [(Some(5 * S), "second".to_owned())],
            "discontinuity by {}: the abandoned record must not merge with what \
             follows it",
            if by_offset { "offset" } else { "flag" }
        );
    }
}

/// The offsets a byte source actually sends (0, 4096, 8192, ...) are continuous
/// and must NOT be read as discontinuities, or every buffer would reset the
/// parser and no record spanning two buffers could ever complete.
#[test]
fn test_continuous_offsets_are_not_discontinuities() {
    init();
    let mut h = gst_check::Harness::new("rssubparse");
    h.set_src_caps_str("application/x-subtitle");

    let body = one_cue("one record, many buffers");
    let bytes = body.as_bytes();
    let mut offset = 0u64;
    for chunk in bytes.chunks(8) {
        let mut buf = gst::Buffer::from_slice(chunk.to_vec());
        buf.get_mut().unwrap().set_offset(offset);
        offset += chunk.len() as u64;
        assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    }
    h.push_event(gst::event::Eos::new());
    assert_eq!(
        h.try_pull().map(|buf| text_of(&buf)).as_deref(),
        Some("one record, many buffers")
    );
}

// ---------------------------------------------------------------------------
// Upstream-driven behaviour: the SEEKING query, the byte-seek fallback and the
// position
// ---------------------------------------------------------------------------
//
// `gst_check`'s harness answers every upstream event and query itself, always
// successfully, which is exactly what the tests below need to control: whether
// upstream can seek, whether it can answer a SEEKING query, and what it says.
// So they wire their own pads around the element instead.

/// What the pad below the element saw, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Saw {
    /// A cue: its timestamp, the position the element answered *while* it was
    /// being pushed, and its text.
    Buffer(Option<u64>, Option<u64>, String),
    Caps,
    Segment(Option<u64>),
    Other,
}

/// One element with a hand-built pad on each side.
struct Rig {
    element: gst::Element,
    src: gst::Pad,
    /// Held for as long as the rig lives. A pad keeps only a weak reference to
    /// its peer, so dropping this one silently unlinks the element's src pad and
    /// everything it pushes goes nowhere.
    sink: gst::Pad,
    /// Whether upstream honours a TIME seek. A demuxer does, a byte source does
    /// not, and it is the second case the element has a fallback for.
    time_seeks: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether upstream honours the BYTES seek that fallback sends.
    byte_seeks: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether upstream answers a BYTES SEEKING query, and with what.
    answers_seeking: std::sync::Arc<std::sync::atomic::AtomicBool>,
    byte_seekable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    saw: std::sync::Arc<std::sync::Mutex<Vec<Saw>>>,
}

impl Rig {
    fn new() -> Self {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        init();
        let element = gst::ElementFactory::make("rssubparse").build().unwrap();
        let time_seeks = Arc::new(AtomicBool::new(false));
        let byte_seeks = Arc::new(AtomicBool::new(true));
        let answers_seeking = Arc::new(AtomicBool::new(true));
        let byte_seekable = Arc::new(AtomicBool::new(true));
        let saw = Arc::new(std::sync::Mutex::new(Vec::new()));

        let src = {
            let (time_seeks, byte_seeks) = (time_seeks.clone(), byte_seeks.clone());
            let (answers, seekable) = (answers_seeking.clone(), byte_seekable.clone());
            gst::Pad::builder(gst::PadDirection::Src)
                .name("rig-src")
                .event_function(move |_pad, _parent, event| match event.view() {
                    gst::EventView::Seek(e) => {
                        let (_rate, _flags, _start_type, start, _stop_type, _stop) = e.get();
                        if matches!(start, gst::GenericFormattedValue::Bytes(_)) {
                            byte_seeks.load(Ordering::SeqCst)
                        } else {
                            time_seeks.load(Ordering::SeqCst)
                        }
                    }
                    _ => true,
                })
                .query_function(move |_pad, _parent, query| match query.view_mut() {
                    gst::QueryViewMut::Seeking(q) => {
                        if !answers.load(Ordering::SeqCst) || q.format() != gst::Format::Bytes {
                            return false;
                        }
                        q.set(
                            seekable.load(Ordering::SeqCst),
                            gst::format::Bytes::ZERO,
                            gst::format::Bytes::NONE,
                        );
                        true
                    }
                    _ => false,
                })
                .build()
        };

        let sink = {
            let (buffers, events) = (saw.clone(), saw.clone());
            gst::Pad::builder(gst::PadDirection::Sink)
                .name("rig-sink")
                .chain_function(move |pad, _parent, buffer| {
                    // Asked from here, the answer describes the cue in flight.
                    let mut query = gst::query::Position::new(gst::Format::Time);
                    let position = match pad.peer_query(&mut query) {
                        true => match query.result() {
                            gst::GenericFormattedValue::Time(t) => t.map(|t| t.nseconds()),
                            _ => None,
                        },
                        false => None,
                    };
                    let text = buffer
                        .map_readable()
                        .map(|m| String::from_utf8_lossy(m.as_slice()).into_owned())
                        .unwrap_or_default();
                    buffers.lock().unwrap().push(Saw::Buffer(
                        buffer.pts().map(|t| t.nseconds()),
                        position,
                        text,
                    ));
                    Ok(gst::FlowSuccess::Ok)
                })
                .event_function(move |_pad, _parent, event| {
                    let seen = match event.view() {
                        gst::EventView::Caps(_) => Saw::Caps,
                        gst::EventView::Segment(e) => Saw::Segment(
                            e.segment()
                                .downcast_ref::<gst::ClockTime>()
                                .and_then(|s| s.start())
                                .map(|t| t.nseconds()),
                        ),
                        _ => Saw::Other,
                    };
                    events.lock().unwrap().push(seen);
                    true
                })
                .build()
        };

        src.link(&element.static_pad("sink").unwrap()).unwrap();
        element.static_pad("src").unwrap().link(&sink).unwrap();
        src.set_active(true).unwrap();
        sink.set_active(true).unwrap();
        element.set_state(gst::State::Playing).unwrap();

        // The events any real upstream starts with.
        assert!(src.push_event(gst::event::StreamStart::new("rig")));
        assert!(src.push_event(gst::event::Caps::new(
            &gst::Caps::builder("application/x-subtitle").build()
        )));
        assert!(
            src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );

        Rig {
            element,
            src,
            sink,
            time_seeks,
            byte_seeks,
            answers_seeking,
            byte_seekable,
            saw,
        }
    }

    fn push(&self, body: &str) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.src.push(buffer(body.as_bytes()))
    }

    fn srcpad(&self) -> gst::Pad {
        self.element.static_pad("src").unwrap()
    }

    fn saw(&self) -> Vec<Saw> {
        self.saw.lock().unwrap().clone()
    }

    fn forget(&self) {
        self.saw.lock().unwrap().clear();
    }

    /// A flushing TIME seek sent to the element's src pad, as a player sends it.
    fn seek(&self, target: u64) -> bool {
        self.srcpad().send_event(gst::event::Seek::new(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            gst::ClockTime::from_nseconds(target),
            gst::SeekType::None,
            gst::ClockTime::NONE,
        ))
    }

    fn position(&self) -> Option<u64> {
        let mut query = gst::query::Position::new(gst::Format::Time);
        assert!(
            self.srcpad().query(&mut query),
            "the element must answer a TIME POSITION query itself"
        );
        match query.result() {
            gst::GenericFormattedValue::Time(t) => t.map(|t| t.nseconds()),
            other => panic!("a TIME position query answered in {other:?}"),
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.element.set_state(gst::State::Null);
        let _ = self.src.set_active(false);
        let _ = self.sink.set_active(false);
    }
}

/// TIME seekability is upstream's BYTES seekability, because a TIME seek is
/// served by seeking upstream to byte 0 and re-parsing
/// (`gst_sub_parse_src_query`, `gstsubparse.c:220-241`). Answering the default
/// "not seekable" instead is folded by `GstBin` into the whole pipeline's
/// answer, so a plain `filesrc ! subparse` pipeline reports unseekable.
#[test]
fn test_the_seeking_query_answers_time_from_upstream_bytes() {
    use std::sync::atomic::Ordering;

    let rig = Rig::new();

    let mut query = gst::query::Seeking::new(gst::Format::Time);
    assert!(
        rig.srcpad().query(&mut query),
        "the element must handle SEEKING itself"
    );
    let (seekable, start, stop) = query.result();
    assert!(seekable, "a byte-seekable upstream makes us TIME-seekable");
    assert_eq!(
        start,
        gst::GenericFormattedValue::Time(Some(gst::ClockTime::ZERO))
    );
    assert_eq!(stop, gst::GenericFormattedValue::Time(None));

    // An upstream that cannot seek in bytes: handled, and not seekable.
    rig.byte_seekable.store(false, Ordering::SeqCst);
    let mut query = gst::query::Seeking::new(gst::Format::Time);
    assert!(rig.srcpad().query(&mut query));
    assert!(!query.result().0);

    // The same when upstream will not answer the question at all.
    rig.answers_seeking.store(false, Ordering::SeqCst);
    let mut query = gst::query::Seeking::new(gst::Format::Time);
    assert!(rig.srcpad().query(&mut query));
    assert!(!query.result().0);
}

/// A seek nothing upstream can serve must leave the stream exactly as it was.
/// Re-arming the segment while restoring it sends downstream a second,
/// identical segment for a seek that never happened.
#[test]
fn test_a_failed_byte_seek_does_not_resend_the_segment() {
    use std::sync::atomic::Ordering;

    let rig = Rig::new();
    assert_eq!(rig.push(&one_cue("first")), Ok(gst::FlowSuccess::Ok));
    assert!(
        rig.saw().contains(&Saw::Segment(Some(0))),
        "the element's own segment goes out with the first cue: {:?}",
        rig.saw()
    );

    // An upstream that seeks in TIME itself (a demuxer): the seek is forwarded
    // and this element changes nothing, so nothing is re-sent either.
    rig.time_seeks.store(true, Ordering::SeqCst);
    assert!(rig.seek(3 * S), "a TIME seek upstream serves must succeed");
    rig.forget();
    assert_eq!(
        rig.push(&one_cue("after a forwarded seek")),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(
        !rig.saw().iter().any(|s| matches!(s, Saw::Segment(_))),
        "a seek served upstream must not make the element re-send its segment: {:?}",
        rig.saw()
    );

    // Now neither the TIME seek nor the BYTES seek it falls back to is honoured.
    rig.time_seeks.store(false, Ordering::SeqCst);
    rig.byte_seeks.store(false, Ordering::SeqCst);
    assert!(
        !rig.seek(3 * S),
        "a seek nothing upstream can serve must be refused"
    );

    rig.forget();
    assert_eq!(rig.push(&one_cue("second")), Ok(gst::FlowSuccess::Ok));
    assert!(
        !rig.saw().iter().any(|s| matches!(s, Saw::Segment(_))),
        "a failed seek must not make the element re-send its segment: {:?}",
        rig.saw()
    );
}

/// After a seek, POSITION is the seek target, which is what `gst_segment_do_seek`
/// stores and what the C answers from (`self->segment.position`). The port kept
/// its own field that only cue pushes ever wrote, so a seek left it unset and
/// the query answered NONE.
#[test]
fn test_position_after_a_seek_answers_the_target() {
    let rig = Rig::new();
    // Nothing is parsed here, so only the seek can have set the position.
    assert!(rig.seek(30 * S), "upstream honours the byte seek");
    assert_eq!(rig.position(), Some(30 * S));
}

/// POSITION during a batch of cues is the cue being pushed, not the last cue of
/// the batch: the C assigns `segment.position` immediately before every push
/// (`gstsubparse.c:1833`). Reporting the batch's last cue tells downstream about
/// time that has not been rendered yet.
#[test]
fn test_position_advances_one_cue_at_a_time() {
    let rig = Rig::new();
    // Three complete cues in a single buffer, so they are parsed as one batch.
    let body: String = (1..=3)
        .map(|k| format!("{k}\n00:00:0{k},000 --> 00:00:0{k},500\nCue{k}\n\n"))
        .collect();
    assert_eq!(rig.push(&body), Ok(gst::FlowSuccess::Ok));

    let cues: Vec<Saw> = rig
        .saw()
        .into_iter()
        .filter(|s| matches!(s, Saw::Buffer(..)))
        .collect();
    assert_eq!(cues.len(), 3, "{cues:?}");
    for cue in &cues {
        let Saw::Buffer(pts, position, text) = cue else {
            unreachable!("filtered on buffers")
        };
        assert_eq!(
            position, pts,
            "while {text} was in flight the element reported position {position:?}, \
             not its own timestamp {pts:?}"
        );
    }
}
