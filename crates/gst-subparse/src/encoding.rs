// SPDX-FileCopyrightText: 2026 Marcus Hanestad <marlhan@proton.me>
// SPDX-License-Identifier: LGPL-2.1-or-later

//! Charset detection and decoding for the subtitle elements.
//!
//! The C `convert_encoding()` in `gstsubparse.c` decides "is this UTF-8?" **per
//! push buffer** and latches the first "no" for the rest of the stream. That is
//! wrong twice over: a multi-byte character straddling a read boundary is not
//! damage but is treated as such, and one genuinely bad byte condemns every cue
//! in the same buffer, including cues that appear *before* it. This module
//! decides once, from a bounded window of the actual bytes, and repairs damage
//! instead of re-reading the whole stream in another charset.
//!
//! # The decision
//!
//! In order:
//!
//! 1. **A byte-order mark wins outright**, and no statistics are collected at
//!    all. UTF-8, UTF-16 LE/BE and UTF-32 LE/BE are recognised. A UTF-8 BOM is
//!    a declaration that the body is UTF-8, so damaged bytes behind one are
//!    damaged UTF-8 and are repaired, never re-guessed.
//! 2. **Otherwise, clean UTF-8 is kept as UTF-8.** A trailing sequence that is
//!    merely incomplete is held for the next buffer and only counts as damage
//!    once EOS proves no more bytes are coming. A NUL byte is not clean UTF-8
//!    here: glib's `g_utf8_validate()` stops at one, so the C never reads a
//!    stream containing one as UTF-8 either, and this is what lets a BOM-less
//!    UTF-16 file reach the charset its user named. See [`Scan`].
//! 3. **Otherwise the input is either damaged UTF-8 or a legacy 8-bit file**,
//!    and the two are told apart by counting: a charset the user named wins
//!    first (the C consults its property one line after UTF-8 validation
//!    fails, and so do we), else **cp1252 wins only when illegal sequences
//!    outnumber valid multi-byte sequences**. A legacy file has almost no
//!    valid multi-byte sequences and damaged UTF-8 has many, and a file that
//!    merely ends inside a sequence argues for neither. Anything not chosen as
//!    a legacy charset is decoded as UTF-8 with the damaged bytes replaced by
//!    U+FFFD.
//!
//! The rules and the measurements behind them come from fcast's
//! `subtitle-encoding-findings.md` and from the receiver-side
//! `subtitle_transcode` module this replaces. That module only ever saw
//! EXTERNAL subtitle files; the identical bytes arriving as an embedded track
//! got the parser's much weaker guessing. Doing it here makes the two paths
//! agree.
//!
//! # The bounded sniff, and what is latched
//!
//! Whole-file statistics want the whole file, but an element cannot wait for
//! EOS unconditionally or a live stream would never emit a cue. So:
//!
//! * While no illegal sequence has been seen, the stream is **provisionally
//!   UTF-8 and is released as it arrives**, with only an incomplete trailing
//!   sequence held back. A clean UTF-8 file, whatever its script, therefore
//!   streams with no added latency at all and is never buffered.
//! * From the **first illegal sequence** onwards, bytes accumulate instead of
//!   being emitted. Then the charset is decided once, the window is decoded
//!   with it, and the stream continues. Whichever of these comes first forces
//!   the decision:
//!     1. **EOS**, which also resolves a held incomplete tail as damage.
//!     2. **A named charset with one illegal sequence in the window.** That
//!        pair already determines the answer, because a name beats the counts
//!        and more bytes can only add illegal sequences, never remove one, so
//!        there is nothing left to wait for.
//!     3. **[`SNIFF_LIMIT`] bytes** in the window.
//!     4. **[`SNIFF_CALLS`] calls** with the window open. A byte limit bounds
//!        a byte stream and does not bound a framed one: a whole film's
//!        dialogue arriving one line per buffer is tens of kilobytes in total,
//!        so a single bad byte in its first line would otherwise hold every cue
//!        in the track back until EOS.
//! * Counting spans the **whole** stream, not just the buffered window: valid
//!   multi-byte sequences already released still vote. Otherwise a file that
//!   is UTF-8 for 40 KB and then has two stray bytes would be called legacy on
//!   the evidence of those two bytes alone.
//! * Only the first [`SNIFF_LIMIT`] bytes of the window are ever *examined*,
//!   so the decision is a function of the stream's prefix rather than of how
//!   upstream chose to size its reads: 4096-byte buffers and one big buffer
//!   latch the same charset for the same bytes. The rest of the window is
//!   still decoded, it just does not vote. The call bound above is the one
//!   place where framing can still move the decision, and that is the price of
//!   never letting a live stream go silent.
//!
//! The decision is **latched**: once made it applies to the rest of the
//! stream, and damage found later is repaired under it rather than reopening
//! the question. This matches the C's one-way latch in shape, but latches a
//! decision made from real evidence rather than from whichever bytes happened
//! to share a push buffer with the first problem. It is reset by
//! `READY -> PAUSED` and by `FLUSH_STOP`, exactly as the C's is, and it resets
//! itself if bytes arrive after the EOS that ended the stream, since those can
//! only belong to a new one (see [`Decoder::decode`]).
//!
//! # Deliberate divergences from the C
//!
//! This element is otherwise a drop-in replacement, so each one is called out:
//!
//! 1. **Damaged bytes become U+FFFD; the stream is not re-read as
//!    ISO-8859-15.** The C flips the whole buffer, so cues *before* the bad
//!    byte are destroyed too (findings case 2). Only bytes that cannot be
//!    decoded change.
//! 2. **The unnamed fallback is cp1252, not ISO-8859-15.** The two agree
//!    across the Latin range and disagree at 0x80-0x9F, which is exactly where
//!    a real legacy subtitle keeps its curly quotes, ellipsis and en dash.
//!    ISO-8859-15 maps that block to C1 controls, which pango draws as hex
//!    boxes (findings case 4). The five undefined cp1252 slots (0x81, 0x8D,
//!    0x8F, 0x90, 0x9D) become U+FFFD rather than C1 controls for the same
//!    reason, but only when cp1252 was *guessed*: a charset the user named is
//!    honoured verbatim, undefined slots included, because the name is a
//!    statement about the bytes and this rewrite is a repair of a guess. An
//!    unrecognised *name* still falls back to ISO-8859-15 as the C does, and so
//!    does a name WHATWG deliberately refuses to implement, see
//!    [`resolve_encoding`].
//! 3. **A UTF-32LE BOM is read as UTF-32LE.** The C tests the two-byte
//!    UTF-16LE BOM before the four-byte UTF-32LE one
//!    (`gstsubparseelement.c:250-264`), and `FF FE 00 00` starts with
//!    `FF FE`, so the C reads every UTF-32LE file as UTF-16LE. UTF-32BE is
//!    decoded rather than refused.
//! 4. **A UTF-8 BOM plus a bad byte still produces cues.** The C discards the
//!    BOM detection on the first conversion failure, ISO-8859-15's the BOM
//!    itself into visible `ï»¿` text, and then fails format autodetection, so
//!    the whole track errors out (findings case 11).
//! 5. **Naming UTF-8 as the fallback charset is treated as naming nothing.**
//!    It is a statement about bytes that have just been shown not to be UTF-8,
//!    so it carries no information. The C's net behaviour is the same by
//!    accident: `g_convert` fails with EILSEQ and the hardcoded ISO-8859-15
//!    retry runs anyway (findings case 10).
//!
//! # Implementation note
//!
//! The UTF-8 paths deliberately stay on [`std::str::from_utf8`], whose error
//! type distinguishes an *illegal* sequence from a merely *incomplete* one
//! (`error_len()`), which is the same line glibc's iconv draws between EILSEQ
//! and EINVAL and the entire basis of rule 3 above. It is wrapped in [`Scan`],
//! which adds the one thing glib's validation has and Rust's does not (a NUL
//! byte ends the text) and which counting, repairing and releasing all walk, so
//! they can never disagree about which bytes are damaged. `encoding_rs` drives
//! every other charset. Its streaming [`encoding_rs::Decoder`] holds incomplete
//! multi-byte sequences internally, so the buffer-boundary hold generalises to
//! all of them. UTF-32 is not in the WHATWG set `encoding_rs` implements, so it
//! has a small decoder of its own here.

use encoding_rs::{CoderResult, Encoding};

/// How many undecided bytes may pile up before the charset is decided without
/// waiting for EOS.
///
/// Only bytes from the first illegal sequence onwards are ever counted here: a
/// clean UTF-8 stream never buffers, so this bounds the damaged/legacy case
/// alone. The value trades detection quality against how long a live stream
/// can be held silent, and 64 KiB is where those meet:
///
/// * It is 16 times the default `filesrc` read (4096 bytes) and larger than any
///   realistic network chunk, so the window always spans many push-buffer
///   boundaries and a single split character can never be the only evidence.
/// * A complete SubRip for a feature-length film is 30-80 KB, so for a typical
///   whole *file* the window is effectively the whole file, which is exactly
///   the input the receiver-side transcoder used to decide from.
/// * 64 KiB is roughly a thousand cues' worth of text: far more evidence than
///   the illegal-versus-multi-byte count needs to be stable.
/// * It is negligible memory next to a video pipeline, and it is bounded per
///   stream rather than per file, so a pathological input cannot grow it.
pub(crate) const SNIFF_LIMIT: usize = 64 * 1024;

/// How many [`Decoder::decode`] calls the undecided window may survive before
/// the charset is decided anyway.
///
/// [`SNIFF_LIMIT`] bounds the wait for a *byte* stream, where 64 KiB is a
/// fraction of a second of a `filesrc`. It does not bound it for a **framed**
/// one, and that is the case that broke: `rsssaparse` is fed one dialogue line
/// per buffer, and a whole film's dialogue is well under 64 KiB, so one bad byte
/// in the first line withheld every cue in the track until EOS (measured: 1110
/// consecutive silent buffers). Counting calls bounds the wait in the unit a
/// framed stream actually arrives in.
///
/// Eight is small enough to be a few cues of latency at worst, and large enough
/// that the window still spans several push-buffer boundaries: a character split
/// by one of them can never be the only evidence, which is the property
/// [`SNIFF_LIMIT`] was sized for too. A decision forced this early is made from
/// less input than one forced at the byte limit, but from the same rule, and the
/// released prefix still votes, so nothing about *how* the charset is chosen
/// changes here.
pub(crate) const SNIFF_CALLS: usize = 8;

/// The longest byte-order mark recognised (UTF-32's). Nothing is released
/// before this many bytes have arrived, or EOS proves they never will, because
/// the UTF-32BE BOM (`00 00 FE FF`) opens with two bytes that would otherwise
/// look like ordinary ASCII.
const MAX_BOM_LEN: usize = 4;

/// Incremental charset decoder shared by `rssubparse` and `rsssaparse`.
///
/// Feed it the *entire* not-yet-consumed byte tail on every call; it returns
/// the freshly decoded text and how many input bytes it consumed. The caller
/// keeps the unconsumed remainder and prepends it to the next chunk. That
/// remainder is either a held incomplete multi-byte tail or, once the sniff has
/// opened, the undecided window.
pub struct Decoder {
    /// Which decoding path drives the stream right now.
    mode: Mode,
    /// Whether the one-time leading BOM sniff has run.
    bom_sniffed: bool,
    /// Whether a leading U+FEFF has been dropped from the emitted text.
    bom_stripped: bool,
    /// Whether the provisional UTF-8 release is still running. It stops at the
    /// first illegal sequence and never restarts.
    releasing: bool,
    /// Whether the final call (`at_eos`) has already happened.
    finished: bool,
    /// How many calls the undecided window has survived, bounded by
    /// [`SNIFF_CALLS`].
    undecided_calls: usize,
    /// Valid multi-byte sequences seen over the WHOLE stream, released ones
    /// included.
    multibyte: usize,
    /// Illegal SEQUENCES seen over the whole stream. One damaged character is
    /// one of these however many bytes it spans, because it is weighed against
    /// `multibyte`, which counts characters.
    illegal: usize,
}

/// The active decoding path.
enum Mode {
    /// Undecided. Provisionally UTF-8 while [`Decoder::releasing`] holds;
    /// afterwards the caller's buffer is the sniff window.
    Sniffing,
    /// Committed to UTF-8. Illegal sequences become U+FFFD, an incomplete
    /// trailing sequence is held for the next buffer.
    Utf8,
    /// Committed to an `encoding_rs` charset: a BOM-detected UTF-16, a charset
    /// the user named, or cp1252. Incomplete sequences are held inside it.
    Foreign {
        dec: encoding_rs::Decoder,
        /// Whether the five undefined cp1252 slots are rewritten to U+FFFD.
        /// True only for the cp1252 the statistics *guessed*: a charset the
        /// user named is honoured verbatim (module docs, divergence 2).
        repair_undefined: bool,
    },
    /// Committed to UTF-32, which `encoding_rs` does not implement.
    Utf32 { big_endian: bool },
}

impl Mode {
    /// A stable name for logs and tests.
    fn charset(&self) -> &'static str {
        match self {
            Mode::Sniffing => "undecided",
            Mode::Utf8 => "UTF-8",
            Mode::Foreign { dec, .. } => dec.encoding().name(),
            Mode::Utf32 { big_endian: false } => "UTF-32LE",
            Mode::Utf32 { big_endian: true } => "UTF-32BE",
        }
    }
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder")
            .field("charset", &self.mode.charset())
            .field("releasing", &self.releasing)
            .field("multibyte", &self.multibyte)
            .field("illegal", &self.illegal)
            .finish()
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Decoder::new()
    }
}

impl Decoder {
    pub fn new() -> Self {
        Decoder {
            mode: Mode::Sniffing,
            bom_sniffed: false,
            bom_stripped: false,
            releasing: true,
            finished: false,
            undecided_calls: 0,
            multibyte: 0,
            illegal: 0,
        }
    }

    /// The charset this stream was decided to be, or `"undecided"`.
    pub fn charset(&self) -> &'static str {
        self.mode.charset()
    }

    /// Decode as much of `bytes` as is safely possible.
    ///
    /// `at_eos` means "these are the last bytes of the stream": it resolves a
    /// held incomplete tail as damage and forces the charset decision even if
    /// the sniff window never filled. `fallback` is the charset the user named
    /// (the `subtitle-encoding` property), read on every call rather than
    /// snapshotted, because the C reads its property per converted block and an
    /// application may set it after the element is already running.
    ///
    /// Returns `(decoded_text, consumed)`; `bytes[consumed..]` must be retained
    /// by the caller and prepended to the next chunk. After a call with
    /// `at_eos` everything is consumed.
    ///
    /// Calling again after `at_eos` **starts a new stream**: the decoder resets
    /// itself and sniffs the bytes as if it had just been built. An element is
    /// expected to reset the decoder on `STREAM_START` and `FLUSH_STOP` itself,
    /// but it does not get to leave the question open, because a `Foreign`
    /// charset's `encoding_rs` decoder panics if it is used after it has been
    /// told the stream ended, and event orders that reach that are real
    /// (EOS, then `STREAM_START`, then a buffer). Starting over is the only
    /// reading of post-EOS bytes that keeps every cue: they cannot belong to the
    /// stream that just ended.
    pub fn decode(
        &mut self,
        bytes: &[u8],
        at_eos: bool,
        fallback: Option<&str>,
    ) -> (String, usize) {
        if self.finished {
            *self = Decoder::new();
        }
        self.finished |= at_eos;

        if bytes.is_empty() && !at_eos {
            return (String::new(), 0);
        }

        // 1. One-time BOM sniff. A BOM is a declaration, so it decides the
        //    whole stream and no statistics are gathered at all.
        if !self.bom_sniffed {
            if bytes.len() < MAX_BOM_LEN && !at_eos {
                // Too few bytes to tell a UTF-32 BOM from a UTF-16 one, and too
                // few to release safely: the UTF-32BE BOM opens with 00 00.
                return (String::new(), 0);
            }
            self.bom_sniffed = true;
            if let Some((mode, bom_len)) = sniff_bom(bytes) {
                self.mode = mode;
                let (text, used) = self.decode_committed(&bytes[bom_len..], at_eos);
                return (self.strip_bom(text), bom_len + used);
            }
        }

        // 2. Already decided: just decode.
        if !matches!(self.mode, Mode::Sniffing) {
            let (text, used) = self.decode_committed(bytes, at_eos);
            return (self.strip_bom(text), used);
        }

        // 3. Undecided. Release what is provisionally UTF-8, which is
        //    everything up to the first illegal sequence.
        //
        //    Releasing early is only safe because every candidate charset
        //    reads those bytes the same way. A charset the user named that is
        //    NOT ASCII-transparent (UTF-16, UTF-32) breaks that, so the
        //    release is suppressed and everything waits for the decision.
        let mut out = String::new();
        let mut consumed = 0;
        let named = named_charset(fallback);
        let release_is_safe = match named {
            Some(named) => named.ascii_transparent(),
            None => true,
        };
        if self.releasing && release_is_safe {
            let (text, used, illegal_ahead) = self.release(bytes);
            out.push_str(&text);
            consumed = used;
            self.releasing = !illegal_ahead;
        }

        // 4. Decide, once there is no more input, enough of it, or enough calls
        //    have gone by that a framed stream would otherwise stay silent to
        //    the end of the file. See the module docs for all four triggers.
        let window = &bytes[consumed..];
        if !self.releasing || !release_is_safe {
            // Bytes are piling up rather than being released, either because an
            // illegal sequence closed the release or because the charset named
            // is not ASCII-transparent. Only those calls are counted: while the
            // release runs, nothing is being withheld but an incomplete tail.
            self.undecided_calls += 1;
        }
        // A named charset does not depend on the counts, so one illegal
        // sequence already settles the question and waiting can only add
        // latency: more bytes cannot take an illegal sequence away.
        let named_is_settled = named.is_some() && has_illegal(window);
        if at_eos
            || named_is_settled
            || window.len() >= SNIFF_LIMIT
            || self.undecided_calls >= SNIFF_CALLS
        {
            self.mode = self.decide(window, at_eos, fallback);
            let (text, used) = self.decode_committed(window, at_eos);
            out.push_str(&text);
            consumed += used;
        }

        (self.strip_bom(out), consumed)
    }

    /// Emit the provisionally-UTF-8 prefix of `bytes` and count the valid
    /// multi-byte sequences in it. Returns the text, how many bytes it covered,
    /// and whether an *illegal* sequence (as opposed to a merely incomplete
    /// trailing one) follows it, which is what closes the release.
    fn release(&mut self, bytes: &[u8]) -> (String, usize, bool) {
        // One step of the scan is enough: the release stops at the first flaw
        // whatever it is.
        let step = Scan::new(bytes).step().expect("a scan always takes a step");
        self.multibyte += count_multibyte(step.text);
        let illegal_ahead = matches!(step.flaw, Some(Flaw::Illegal { .. }));
        (step.text.to_owned(), step.text.len(), illegal_ahead)
    }

    /// Fold `window` into the running counts and pick the charset. See the
    /// module docs for the rule and for why each step is where it is.
    fn decide(&mut self, window: &[u8], at_eos: bool, fallback: Option<&str>) -> Mode {
        // Only the window's first SNIFF_LIMIT bytes are examined, so the same
        // bytes latch the same charset however upstream sized its reads. Bytes
        // past the clamp are still decoded below, they just do not vote.
        let prefix = &window[..window.len().min(SNIFF_LIMIT)];
        // The clamp can cut a sequence in half, and that cut is a boundary like
        // any other rather than the end of the stream, so EOS does not apply to
        // whatever it truncated.
        let ends_the_stream = at_eos && prefix.len() == window.len();
        let stats = utf8_stats(prefix);
        self.multibyte += stats.multibyte;
        self.illegal += stats.illegal;

        // Clean UTF-8. A tail that is only truncated is damage exclusively when
        // EOS proves nothing more is coming, which is the same distinction
        // iconv makes between EILSEQ and EINVAL.
        if self.illegal == 0 && !(ends_the_stream && stats.truncated > 0) {
            return Mode::Utf8;
        }

        // Not UTF-8, so a charset the user named decides.
        if let Some(named) = named_charset(fallback) {
            return named.into_mode();
        }

        if self.illegal > self.multibyte {
            Mode::Foreign {
                dec: WINDOWS_1252_REPAIRING.new_decoder_without_bom_handling(),
                // Guessed, so its undefined slots are damage to repair.
                repair_undefined: true,
            }
        } else {
            // Damaged UTF-8: keep every byte that decodes and replace the rest.
            Mode::Utf8
        }
    }

    fn decode_committed(&mut self, bytes: &[u8], at_eos: bool) -> (String, usize) {
        match &mut self.mode {
            Mode::Sniffing => unreachable!("decode_committed before a decision"),
            Mode::Utf8 => decode_utf8(bytes, at_eos),
            Mode::Foreign {
                dec,
                repair_undefined,
            } => (
                decode_foreign(dec, bytes, at_eos, *repair_undefined),
                bytes.len(),
            ),
            Mode::Utf32 { big_endian } => decode_utf32(*big_endian, bytes, at_eos),
        }
    }

    /// Drop a leading U+FEFF from the very first non-empty output, matching the
    /// C `gst_sub_parse_gst_convert_to_utf8()`. The BOM *bytes* are already
    /// skipped by [`sniff_bom`]; this catches a second, encoded BOM.
    fn strip_bom(&mut self, mut text: String) -> String {
        if !self.bom_stripped && !text.is_empty() {
            if let Some(rest) = text.strip_prefix('\u{FEFF}') {
                text = rest.to_owned();
            }
            self.bom_stripped = true;
        }
        text
    }
}

/// cp1252 as the statistics' fallback, where the five undefined slots (0x81,
/// 0x8D, 0x8F, 0x90, 0x9D) map to U+FFFD. `encoding_rs`'s WINDOWS_1252 maps them
/// to the C1 controls a strict Latin-1 reading gives them, and C1 controls are
/// precisely the hex boxes this module exists to stop drawing, so they are
/// rewritten after the fact in [`decode_foreign`]. Naming cp1252 gets the
/// unrewritten reading: see `Mode::Foreign::repair_undefined`.
const WINDOWS_1252_REPAIRING: &Encoding = encoding_rs::WINDOWS_1252;

/// The undefined cp1252 code points, as the C1 controls `encoding_rs` decodes
/// them to.
const CP1252_UNDEFINED: [char; 5] = ['\u{81}', '\u{8D}', '\u{8F}', '\u{90}', '\u{9D}'];

/// Recognise a leading byte-order mark and return the mode it selects together
/// with the BOM's length, which the caller skips.
///
/// The UTF-32 marks are tested FIRST: `FF FE 00 00` (UTF-32LE) starts with
/// `FF FE` (UTF-16LE), and the C's table gets this backwards.
fn sniff_bom(bytes: &[u8]) -> Option<(Mode, usize)> {
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        return Some((Mode::Utf32 { big_endian: false }, 4));
    }
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        return Some((Mode::Utf32 { big_endian: true }, 4));
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some((Mode::Utf8, 3));
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some((
            Mode::Foreign {
                dec: encoding_rs::UTF_16LE.new_decoder_without_bom_handling(),
                repair_undefined: false,
            },
            2,
        ));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some((
            Mode::Foreign {
                dec: encoding_rs::UTF_16BE.new_decoder_without_bom_handling(),
                repair_undefined: false,
            },
            2,
        ));
    }
    None
}

/// A charset the user named. Kept separate from [`Mode`] so it can be resolved
/// cheaply, without building a decoder, on the per-buffer safety check.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Named {
    Encoding(&'static Encoding),
    Utf32 { big_endian: bool },
}

impl Named {
    /// Whether bytes 0x00-0x7F decode to themselves, one byte per character.
    /// Only then can the provisional UTF-8 release run, because only then do
    /// this charset and UTF-8 agree on what it releases.
    fn ascii_transparent(self) -> bool {
        match self {
            Named::Encoding(enc) => enc.is_ascii_compatible(),
            Named::Utf32 { .. } => false,
        }
    }

    fn into_mode(self) -> Mode {
        match self {
            // Named, so its undefined slots are the user's business, not damage
            // to repair (module docs, divergence 2).
            Named::Encoding(enc) => Mode::Foreign {
                dec: enc.new_decoder_without_bom_handling(),
                repair_undefined: false,
            },
            Named::Utf32 { big_endian } => Mode::Utf32 { big_endian },
        }
    }
}

/// Which end a label that does not say (`UTF-16`, `UCS-2`, `UTF-32`) is read
/// from when the bytes carry no BOM to settle it.
///
/// RFC 2781 says big-endian. glibc's iconv, which is what the C's `g_convert`
/// calls, says the host's own order, and parity is with the C rather than with
/// the RFC. Verified on x86-64: `printf 'H\0i\0' | iconv -f UTF-16 -t UTF-8`
/// prints `Hi`, while `printf '\0H\0i'` through the same converter prints
/// U+4800 U+6900, and `UCS-2` behaves identically. A file that *does* carry a
/// BOM never reaches here, because [`sniff_bom`] runs before any label is
/// consulted, so this settles the BOM-less case only, which is precisely the
/// case iconv resolves this way.
///
/// `UCS-4` is the exception iconv makes: that one is big-endian whatever the
/// host (`printf '\0\0\0H' | iconv -f UCS-4 -t UTF-8` prints `H`), so it is
/// listed with the explicitly big-endian labels below.
const AMBIGUOUS_BIG_ENDIAN: bool = cfg!(target_endian = "big");

/// The charset the user named, in order the `subtitle-encoding` property then
/// the `GST_SUBTITLE_ENCODING` environment variable, mirroring the C.
///
/// A label with nothing left in it once the iconv suffix is stripped and the
/// rest trimmed ("", "  ", "//TRANSLIT") names nothing, and naming nothing is
/// not the same as naming a charset: it must not shadow the environment
/// variable, and it must not resolve to the unknown-label fallback either. The
/// C only tests for the empty string (`gstsubparse.c:441-446`), so a blank
/// label there reaches `g_convert`, fails, and lands on ISO-8859-15 anyway.
///
/// Naming UTF-8 yields `None` too: it is a statement about bytes just shown not
/// to be UTF-8, so it says nothing and the statistics decide instead. It is
/// still a label, though, so unlike a blank one it does shadow the environment
/// variable, exactly as it does in the C.
fn named_charset(property: Option<&str>) -> Option<Named> {
    let environment = std::env::var("GST_SUBTITLE_ENCODING").ok();
    // iconv permits suffixes like "ISO-8859-15//TRANSLIT". WHATWG does not.
    let core = [property, environment.as_deref()]
        .into_iter()
        .flatten()
        .map(|label| label.split("//").next().unwrap_or(label).trim())
        .find(|core| !core.is_empty())?;
    match core.to_ascii_lowercase().as_str() {
        "utf-32" | "utf32" => {
            return Some(Named::Utf32 {
                big_endian: AMBIGUOUS_BIG_ENDIAN,
            });
        }
        "utf-32le" | "utf32le" | "ucs-4le" | "ucs4le" => {
            return Some(Named::Utf32 { big_endian: false });
        }
        "utf-32be" | "utf32be" | "ucs-4be" | "ucs4be" | "ucs-4" | "ucs4" => {
            return Some(Named::Utf32 { big_endian: true });
        }
        // WHATWG reads a bare "utf-16" as little-endian by fiat, which is a
        // browser's rule and not iconv's, so these are settled here instead of
        // by `Encoding::for_label`.
        "utf-16" | "utf16" | "ucs-2" | "ucs2" => {
            return Some(Named::Encoding(if AMBIGUOUS_BIG_ENDIAN {
                encoding_rs::UTF_16BE
            } else {
                encoding_rs::UTF_16LE
            }));
        }
        _ => {}
    }
    let enc = resolve_encoding(core);
    (enc != encoding_rs::UTF_8).then_some(Named::Encoding(enc))
}

/// What ends a decodable run of UTF-8.
#[derive(Debug, PartialEq, Eq)]
enum Flaw {
    /// `len` bytes that cannot be decoded: a bad byte, a sequence cut short in
    /// the middle of the input, or a NUL. One flaw whatever its length, because
    /// it is weighed against characters.
    Illegal { len: usize },
    /// The input ends inside a sequence that is legal as far as it goes, `len`
    /// bytes of it. Not damage until EOS proves no more bytes are coming, which
    /// is the EILSEQ/EINVAL line glibc's iconv draws and the one the C's
    /// streaming path fails to.
    Incomplete { len: usize },
}

/// One step of a [`Scan`]: text, then whatever stopped it.
struct Step<'a> {
    text: &'a str,
    flaw: Option<Flaw>,
}

/// A left-to-right walk of bytes as UTF-8, alternating decodable text with the
/// flaws between the runs.
///
/// Counting ([`utf8_stats`]), repairing ([`decode_utf8`]) and releasing
/// ([`Decoder::release`]) all walk this, so the statistics can never disagree
/// with the repair about which bytes are damaged.
///
/// It differs from plain [`std::str::from_utf8`] in one way: **a NUL byte is an
/// illegal sequence.** Rust accepts an embedded NUL as a character and glib's
/// `g_utf8_validate()` does not (verified: `g_utf8_validate("1\0\n\0H\0i\0", 8,
/// NULL)` is FALSE), so the C never reads a stream containing one as UTF-8, and
/// a rule this module inherits from the C's ordering has to inherit the C's
/// notion of validity with it. Concretely: a BOM-less UTF-16LE file is half NUL
/// bytes and Rust says every one of them is fine, so without this rule the file
/// "validates", the clean-UTF-8 reading wins before the charset the user named
/// is ever consulted, and the cues come out NUL-laden junk that no format probe
/// can detect. Subtitle text has no legitimate NUL in it either way.
///
/// The walk remembers how far validation has already reached, so input that is
/// dense in flaws (a UTF-16 file has one every other byte) still costs a single
/// pass rather than one pass per flaw.
struct Scan<'a> {
    bytes: &'a [u8],
    /// Where the run the next step yields begins.
    pos: usize,
    /// How far validation has accepted. `bytes[pos..validated]` is legal UTF-8,
    /// NULs aside, and is never revalidated.
    validated: usize,
    /// What ends the validated stretch, once validation has run for it.
    flaw: Option<Flaw>,
    /// Whether the last step yielded a flaw that nothing can follow.
    done: bool,
}

impl<'a> Scan<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Scan {
            bytes,
            pos: 0,
            validated: 0,
            flaw: None,
            done: false,
        }
    }

    /// The next run of decodable text and the flaw that ends it, or `None` once
    /// the input is exhausted. Always takes at least one step, which for empty
    /// input is empty text and no flaw.
    fn step(&mut self) -> Option<Step<'a>> {
        if self.done {
            return None;
        }
        if self.validated <= self.pos {
            // Validation has been walked up to, so ask once for all that is
            // left. Asking again for bytes already accepted is what would make
            // a NUL-dense input quadratic.
            self.flaw = match std::str::from_utf8(&self.bytes[self.pos..]) {
                Ok(_) => {
                    self.validated = self.bytes.len();
                    None
                }
                Err(err) => {
                    self.validated = self.pos + err.valid_up_to();
                    Some(match err.error_len() {
                        Some(len) => Flaw::Illegal { len },
                        None => Flaw::Incomplete {
                            len: self.bytes.len() - self.validated,
                        },
                    })
                }
            };
        }
        // A NUL can only be inside the accepted stretch, since Rust reads it as
        // a character. The search stops at the first one, so a NUL every other
        // byte costs each byte once across all the steps, not once per step.
        let stretch = &self.bytes[self.pos..self.validated];
        Some(match stretch.iter().position(|&b| b == 0) {
            Some(nul) => {
                self.pos += nul + 1;
                Step {
                    text: as_text(&stretch[..nul]),
                    flaw: Some(Flaw::Illegal { len: 1 }),
                }
            }
            None => {
                let flaw = self.flaw.take();
                self.pos = match flaw {
                    Some(Flaw::Illegal { len }) => self.validated + len,
                    // Nothing follows the end of the input, either way.
                    _ => {
                        self.done = true;
                        self.bytes.len()
                    }
                };
                Step {
                    text: as_text(stretch),
                    flaw,
                }
            }
        })
    }
}

/// Bytes a [`Scan`] has accepted, as text.
fn as_text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("the scan only accepts UTF-8")
}

/// Whether `bytes` holds an illegal sequence, which is all a named charset needs
/// to know: see [`Decoder::decode`]. The first flaw settles it, because nothing
/// follows a merely incomplete tail, so one step of the scan is enough.
fn has_illegal(bytes: &[u8]) -> bool {
    matches!(
        Scan::new(bytes).step().and_then(|step| step.flaw),
        Some(Flaw::Illegal { .. })
    )
}

/// Evidence for and against the input having been UTF-8.
#[derive(Debug, Default, PartialEq, Eq)]
struct Stats {
    /// Valid multi-byte sequences.
    multibyte: usize,
    /// ILLEGAL sequences. Only these argue that the input is not UTF-8, and
    /// they are counted one per sequence rather than one per byte, because the
    /// rule they feed weighs them against `multibyte`, which counts characters.
    /// A 4-byte emoji cut in half is one argument for another charset, not
    /// three.
    illegal: usize,
    /// Bytes of the single incomplete sequence the input may end with. A file
    /// that merely ends inside a character is not evidence of another charset.
    truncated: usize,
}

fn utf8_stats(bytes: &[u8]) -> Stats {
    let mut stats = Stats::default();
    let mut scan = Scan::new(bytes);
    while let Some(step) = scan.step() {
        stats.multibyte += count_multibyte(step.text);
        match step.flaw {
            None => {}
            Some(Flaw::Illegal { .. }) => stats.illegal += 1,
            Some(Flaw::Incomplete { len }) => stats.truncated = len,
        }
    }
    stats
}

fn count_multibyte(text: &str) -> usize {
    text.chars().filter(|c| c.len_utf8() > 1).count()
}

/// Decode UTF-8, replacing each undecodable sequence with U+FFFD.
///
/// An incomplete trailing sequence is held for the next buffer unless `at_eos`,
/// where it is damage like any other. This is what keeps a multi-byte character
/// split across a push-buffer boundary invisible.
fn decode_utf8(bytes: &[u8], at_eos: bool) -> (String, usize) {
    let mut out = String::with_capacity(bytes.len());
    let mut consumed = 0;
    let mut scan = Scan::new(bytes);
    while let Some(step) = scan.step() {
        out.push_str(step.text);
        consumed += step.text.len();
        match step.flaw {
            None => {}
            Some(Flaw::Illegal { len }) => {
                out.push(char::REPLACEMENT_CHARACTER);
                consumed += len;
            }
            // An incomplete tail is damage once nothing more is coming...
            Some(Flaw::Incomplete { len }) if at_eos => {
                out.push(char::REPLACEMENT_CHARACTER);
                consumed += len;
            }
            // ...and until then it is held for the next buffer, which is what
            // keeps a character split by a push boundary invisible.
            Some(Flaw::Incomplete { .. }) => {}
        }
    }
    (out, consumed)
}

/// Feed `bytes` to a persistent `encoding_rs` decoder. Any incomplete trailing
/// multi-byte sequence is retained inside it for the next call, so everything
/// is reported as consumed.
///
/// `repair_undefined` rewrites cp1252's five undefined slots, and is set only
/// for the cp1252 the statistics guessed: see [`WINDOWS_1252_REPAIRING`].
fn decode_foreign(
    dec: &mut encoding_rs::Decoder,
    bytes: &[u8],
    at_eos: bool,
    repair_undefined: bool,
) -> String {
    let mut out = String::new();
    let mut pos = 0;
    loop {
        let remaining = &bytes[pos..];
        // decode_to_string requires pre-reserved capacity. Size it to the
        // worst-case UTF-8 expansion of the remaining input.
        let need = dec
            .max_utf8_buffer_length(remaining.len())
            .unwrap_or(remaining.len());
        out.reserve(need.max(1));
        let (result, read, _replaced) = dec.decode_to_string(remaining, &mut out, at_eos);
        pos += read;
        match result {
            CoderResult::InputEmpty => break,
            CoderResult::OutputFull => continue,
        }
    }
    if repair_undefined && out.chars().any(|c| CP1252_UNDEFINED.contains(&c)) {
        // See WINDOWS_1252_REPAIRING: in a charset nobody named, an undefined
        // slot is damage, and drawing it as a hex box is the artifact this
        // module removes.
        out = out
            .chars()
            .map(|c| {
                if CP1252_UNDEFINED.contains(&c) {
                    char::REPLACEMENT_CHARACTER
                } else {
                    c
                }
            })
            .collect();
    }
    out
}

/// Decode UTF-32, which is not in the WHATWG set `encoding_rs` implements.
/// Values that are not Unicode scalars become U+FFFD; a trailing partial unit
/// is held unless `at_eos`.
fn decode_utf32(big_endian: bool, bytes: &[u8], at_eos: bool) -> (String, usize) {
    let (units, tail) = bytes.as_chunks::<4>();
    let whole = bytes.len() - tail.len();
    let mut out = String::with_capacity(units.len());
    for unit in units {
        let raw = if big_endian {
            u32::from_be_bytes(*unit)
        } else {
            u32::from_le_bytes(*unit)
        };
        out.push(char::from_u32(raw).unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    if tail.is_empty() {
        (out, bytes.len())
    } else if at_eos {
        out.push(char::REPLACEMENT_CHARACTER);
        (out, bytes.len())
    } else {
        (out, whole)
    }
}

/// One-shot decode of a typefind detection sample (the first `<=128` peeked
/// bytes), reusing the streaming [`Decoder`] so BOM detection and the charset
/// decision behave exactly as the element's do.
///
/// This mirrors the C `gst_sub_parse_type_find`, which BOM-converts and/or
/// UTF-8-validates the peeked bytes before handing the text to autodetection.
/// `fallback` is the charset the user named, and the typefind passes `None`
/// because no element exists yet to carry the property. The environment
/// variable still applies, which is the C's behaviour too and not an accident of
/// this signature: `gst_sub_parse_type_find` reads `GST_SUBTITLE_ENCODING`
/// itself, in the same order, when the peeked bytes fail UTF-8 validation
/// (`gstsubparseelement.c:355-375`). Since a name only ever applies to bytes
/// that are not UTF-8, a sample that is ASCII (which is what the format probes
/// are anchored on) is unaffected by any value it may hold, and a value that
/// resolves to nothing recognisable lands on ISO-8859-15, which leaves that
/// ASCII alone. So no environment setting can stop a format being detected.
///
/// The sample is treated as a complete unit (`at_eos`), so the charset is
/// decided from it rather than deferred. The format probes are start-anchored,
/// so a U+FFFD standing in for a character the 128-byte window cut in half
/// cannot change the detection.
pub(crate) fn decode_sample(bytes: &[u8], fallback: Option<&str>) -> String {
    Decoder::new().decode(bytes, true, fallback).0
}

/// Resolve a gst/iconv encoding *label* to an `encoding_rs` [`Encoding`]. The
/// label arrives with its iconv suffix already stripped and trimmed, and is
/// never blank, because [`named_charset`] resolves those to no charset at all.
///
/// `Encoding::for_label_no_replacement` already accepts most WHATWG labels
/// case-insensitively (and iconv shares many of them, such as `ISO-8859-15`,
/// `windows-1252`, `Shift_JIS`, `GBK`, …). A thin normaliser covers a few common
/// gst/iconv spellings WHATWG rejects. An unknown *name* falls back to
/// ISO-8859-15, exactly like the C ("... assume ISO-8859-15"). That is the
/// fallback for an unrecognised label only. The fallback for unrecognised
/// *bytes* is cp1252, see the module docs.
///
/// The `_no_replacement` in the lookup is load-bearing. Plain
/// `Encoding::for_label` resolves six perfectly legal iconv labels
/// (`iso-2022-kr`, `csiso2022kr`, `iso-2022-cn`, `iso-2022-cn-ext`,
/// `hz-gb-2312` and `replacement` itself) to `encoding_rs::REPLACEMENT`, whose
/// decoder emits ONE U+FFFD for the entire stream and nothing else. That is
/// deliberate in a browser, where those charsets are an attack surface, and it
/// would mean naming a charset destroys the track, which is the one thing naming
/// a charset must never do. `encoding_rs` simply does not implement them, so
/// they get the answer any other label it does not implement gets, and the C's
/// iconv would have decoded them.
fn resolve_encoding(label: &str) -> &'static Encoding {
    Encoding::for_label_no_replacement(label.as_bytes())
        .or_else(|| Encoding::for_label_no_replacement(normalize_label(label).as_bytes()))
        .unwrap_or(encoding_rs::ISO_8859_15)
}

/// Map a handful of common gst/iconv encoding spellings to their canonical
/// WHATWG label. Consulted only when the raw label was not recognised. The
/// labels that do not state an endianness are settled in [`named_charset`]
/// instead, since WHATWG and iconv disagree about them.
fn normalize_label(label: &str) -> String {
    match label.trim().to_ascii_lowercase().as_str() {
        "utf16le" | "ucs-2le" => "utf-16le",
        "utf16be" | "ucs-2be" => "utf-16be",
        "iso8859-15" | "iso_8859_15" | "latin-9" | "latin9" => "iso-8859-15",
        "shift-jis" | "sjis" | "cp932" => "shift_jis",
        "cp936" | "gb2312" => "gbk",
        "cp1251" => "windows-1251",
        "cp1252" => "windows-1252",
        other => return other.to_owned(),
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that mutate `GST_SUBTITLE_ENCODING`, since cargo
    /// runs unit tests as threads in one process and `set_var` is process-wide.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `body` with `GST_SUBTITLE_ENCODING` set to `value` (or unset), and
    /// restores whatever was there before.
    fn with_env<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("GST_SUBTITLE_ENCODING").ok();
        // SAFETY: ENV_LOCK keeps every other env-touching test out.
        unsafe {
            match value {
                Some(value) => std::env::set_var("GST_SUBTITLE_ENCODING", value),
                None => std::env::remove_var("GST_SUBTITLE_ENCODING"),
            }
        }
        let out = body();
        unsafe {
            match previous {
                Some(previous) => std::env::set_var("GST_SUBTITLE_ENCODING", previous),
                None => std::env::remove_var("GST_SUBTITLE_ENCODING"),
            }
        }
        out
    }

    /// Drive the decoder the way the elements do: keep an undecoded `pending`
    /// tail, append each incoming chunk, decode, drain what was consumed.
    struct Feeder {
        dec: Decoder,
        encoding: Option<String>,
        pending: Vec<u8>,
        out: String,
    }

    impl Feeder {
        fn new() -> Self {
            Feeder::with_encoding(None)
        }

        fn with_encoding(encoding: Option<&str>) -> Self {
            Feeder {
                dec: Decoder::new(),
                encoding: encoding.map(str::to_owned),
                pending: Vec::new(),
                out: String::new(),
            }
        }

        fn push(&mut self, chunk: &[u8]) {
            self.feed(chunk, false);
        }

        /// Push the last chunk and signal EOS, which is what makes the decoder
        /// resolve a held tail and decide if it has not already.
        fn finish(&mut self) -> &str {
            self.feed(&[], true);
            &self.out
        }

        fn feed(&mut self, chunk: &[u8], at_eos: bool) {
            self.pending.extend_from_slice(chunk);
            let pending = std::mem::take(&mut self.pending);
            let (text, consumed) = self.dec.decode(&pending, at_eos, self.encoding.as_deref());
            self.out.push_str(&text);
            self.pending = pending[consumed..].to_vec();
        }

        /// Bytes currently held back, either as an incomplete tail or as the
        /// undecided sniff window.
        fn held(&self) -> usize {
            self.pending.len()
        }
    }

    /// Decode `bytes` in one shot with `label` named, as the element would for
    /// a complete small file.
    fn decode_all(bytes: &[u8], label: Option<&str>) -> String {
        let mut f = Feeder::with_encoding(label);
        f.push(bytes);
        f.finish().to_owned()
    }

    /// Encode `s` as UTF-16 (little- or big-endian) code units, optional BOM.
    fn utf16(s: &str, little_endian: bool, bom: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if bom {
            v.extend_from_slice(if little_endian {
                &[0xFF, 0xFE]
            } else {
                &[0xFE, 0xFF]
            });
        }
        for u in s.encode_utf16() {
            v.extend_from_slice(&if little_endian {
                u.to_le_bytes()
            } else {
                u.to_be_bytes()
            });
        }
        v
    }

    /// Encode `s` as UTF-32 with a BOM.
    fn utf32(s: &str, little_endian: bool) -> Vec<u8> {
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

    const ZWSP: &[u8] = &[0xE2, 0x80, 0x8B]; // U+200B ZERO WIDTH SPACE

    // ------------------------------------------------------- clean streaming

    #[test]
    fn ascii_passthrough() {
        let mut f = Feeder::new();
        f.push(b"hello world");
        assert_eq!(f.out, "hello world");
        assert_eq!(f.held(), 0);
    }

    #[test]
    fn whole_multibyte_in_one_chunk() {
        let mut f = Feeder::new();
        let mut buf = b"What".to_vec();
        buf.extend_from_slice(ZWSP);
        buf.extend_from_slice(b"is");
        f.push(&buf);
        assert_eq!(f.out, "What\u{200B}is");
        assert_eq!(f.held(), 0);
    }

    /// The bounded sniff must not become store-and-forward: a clean UTF-8
    /// stream is released as it arrives, whatever its script, and nothing is
    /// ever buffered for it.
    #[test]
    fn clean_utf8_is_released_without_waiting_for_eos() {
        let mut f = Feeder::new();
        f.push("caf\u{e9} \u{4f60}\u{597d} \u{1f600}".as_bytes());
        assert_eq!(f.out, "caf\u{e9} \u{4f60}\u{597d} \u{1f600}");
        assert_eq!(f.held(), 0, "clean UTF-8 must never be buffered");
    }

    #[test]
    fn multibyte_split_holds_incomplete_tail() {
        // Split the 3-byte ZWSP after its first byte. The tail must be held,
        // not decoded/mangled, until the rest arrives.
        let mut f = Feeder::new();
        f.push(b"What\xe2"); // "What" + first byte of ZWSP
        assert_eq!(f.out, "What");
        assert_eq!(f.held(), 1, "the lone 0xE2 must be held");

        f.push(b"\x80"); // second byte of ZWSP, still incomplete
        assert_eq!(f.out, "What");
        assert_eq!(f.held(), 2);

        f.push(b"\x8bis"); // third byte completes ZWSP, then more text
        assert_eq!(f.out, "What\u{200B}is");
        assert_eq!(f.held(), 0);
    }

    #[test]
    fn every_split_point_yields_identical_text() {
        // Cutting anywhere inside the multi-byte char must not change output.
        let mut whole = b"What".to_vec();
        whole.extend_from_slice(ZWSP);
        whole.extend_from_slice(b"is");
        whole.extend_from_slice(ZWSP);
        whole.extend_from_slice(b"this??");
        let expected = "What\u{200B}is\u{200B}this??";

        for split in 0..whole.len() {
            let mut f = Feeder::new();
            f.push(&whole[..split]);
            f.push(&whole[split..]);
            assert_eq!(f.finish(), expected, "failed at split offset {split}");
            assert_eq!(f.held(), 0, "nothing should remain held at split {split}");
        }
    }

    // ------------------------------------------------------------------ BOMs

    #[test]
    fn strips_leading_utf8_bom() {
        let mut f = Feeder::new();
        f.push(b"\xef\xbb\xbf1\n00:00:00,000");
        assert_eq!(f.out, "1\n00:00:00,000");
    }

    #[test]
    fn bom_split_across_chunks_is_still_stripped() {
        let mut f = Feeder::new();
        f.push(b"\xef"); // first BOM byte, too few to sniff
        assert_eq!(f.held(), 1);
        f.push(b"\xbb\xbfHi");
        assert_eq!(f.out, "Hi");
    }

    #[test]
    fn utf16le_bom_decodes() {
        let mut f = Feeder::new();
        f.push(&utf16("Héllo — 1", true, true));
        assert_eq!(f.out, "Héllo — 1");
        assert_eq!(f.held(), 0);
    }

    #[test]
    fn utf16be_bom_decodes() {
        let mut f = Feeder::new();
        f.push(&utf16("Héllo — 1", false, true));
        assert_eq!(f.out, "Héllo — 1");
        assert_eq!(f.held(), 0);
    }

    #[test]
    fn utf16le_split_mid_unit_holds_internally() {
        // Includes a non-BMP char (surrogate pair) so splitting mid-unit and
        // mid-surrogate-pair both exercise encoding_rs's internal hold.
        let text = "Hé𝄞!";
        let whole = utf16(text, true, true);

        for split in 1..whole.len() {
            let mut f = Feeder::new();
            f.push(&whole[..split]);
            f.push(&whole[split..]);
            assert_eq!(f.finish(), text, "failed at split offset {split}");
            assert_eq!(f.held(), 0, "encoding_rs holds partials internally");
        }
    }

    /// The trap the C falls into: `FF FE 00 00` (UTF-32LE) starts with
    /// `FF FE` (UTF-16LE), so a table that tests the shorter mark first reads
    /// every UTF-32LE file as UTF-16LE. Both endiannesses, cut at every offset.
    #[test]
    fn utf32_boms_are_not_mistaken_for_utf16() {
        let text = "Hé𝄞 — 1";
        for little_endian in [true, false] {
            let whole = utf32(text, little_endian);
            assert_eq!(decode_all(&whole, None), text, "le={little_endian}");
            for split in 1..whole.len() {
                let mut f = Feeder::new();
                f.push(&whole[..split]);
                f.push(&whole[split..]);
                assert_eq!(
                    f.finish(),
                    text,
                    "le={little_endian} failed at split offset {split}"
                );
                assert_eq!(f.held(), 0);
            }
        }
    }

    /// A UTF-32 value outside the Unicode scalar range is damage, not a reason
    /// to abandon the declared encoding.
    #[test]
    fn utf32_out_of_range_units_become_the_replacement_character() {
        let mut bytes = utf32("ab", true);
        bytes.extend_from_slice(&0x0011_0000u32.to_le_bytes());
        bytes.extend_from_slice(&u32::from('c').to_le_bytes());
        assert_eq!(decode_all(&bytes, None), "ab\u{FFFD}c");
    }

    /// A UTF-8 BOM declares the body UTF-8, so damage behind it is repaired
    /// rather than treated as evidence of another charset. The C discards the
    /// detection instead and fails the whole track (findings case 11).
    #[test]
    fn a_utf8_bom_makes_damage_a_repair_not_a_re_guess() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("caf\u{e9} ".as_bytes());
        bytes.push(0xFF);
        bytes.extend_from_slice(" ok".as_bytes());
        assert_eq!(decode_all(&bytes, None), "caf\u{e9} \u{FFFD} ok");
    }

    // -------------------------------------- damaged UTF-8 vs a legacy 8-bit

    /// The discriminator, stated directly: illegal sequences against valid
    /// multi-byte ones.
    #[test]
    fn the_statistic_tells_damage_from_a_legacy_file() {
        // Many valid multi-byte sequences, one stray byte: damaged UTF-8.
        let mut damaged = "caf\u{e9} na\u{ef}ve \u{fc}ber".as_bytes().to_vec();
        damaged.push(0xFF);
        let stats = utf8_stats(&damaged);
        assert_eq!(stats.multibyte, 3);
        assert_eq!(stats.illegal, 1);
        assert_eq!(stats.truncated, 0);
        assert!(stats.illegal <= stats.multibyte, "reads as damaged UTF-8");

        // Almost no valid multi-byte sequences, many stray bytes: legacy.
        let legacy = [0x93u8, b'C', b'a', b'f', 0xE9, 0x94, 0x85, 0x96];
        let stats = utf8_stats(&legacy);
        assert!(
            stats.illegal > stats.multibyte,
            "reads as legacy: {stats:?}"
        );

        // Ending inside a sequence argues for neither.
        let mut truncated = b"ends here".to_vec();
        truncated.extend_from_slice(&ZWSP[..2]);
        let stats = utf8_stats(&truncated);
        assert_eq!(stats.illegal, 0);
        assert_eq!(stats.truncated, 2);
    }

    /// Illegal SEQUENCES are counted, not illegal bytes, because the other side
    /// of the scale counts characters. A single truncated 4-byte emoji is three
    /// bytes and one flaw, and counting its bytes used to outvote the two clean
    /// accented characters around it and flip the whole file to cp1252, which
    /// mojibakes every one of them.
    #[test]
    fn one_truncated_emoji_does_not_flip_a_utf8_file_to_cp1252() {
        with_env(None, || {
            let mut bytes = "caf\u{e9} ".as_bytes().to_vec();
            bytes.extend_from_slice(&"\u{1F600}".as_bytes()[..3]); // cut short
            bytes.extend_from_slice(" na\u{ef}ve".as_bytes());

            let stats = utf8_stats(&bytes);
            assert_eq!(stats.illegal, 1, "one flaw, not three bytes: {stats:?}");
            assert_eq!(stats.multibyte, 2);

            let mut f = Feeder::new();
            f.push(&bytes);
            assert_eq!(f.finish(), "caf\u{e9} \u{FFFD} na\u{ef}ve");
            assert_eq!(f.dec.charset(), "UTF-8");
        });
    }

    /// A NUL is damage, not text. Rust's `from_utf8` accepts one and glib's
    /// `g_utf8_validate()` does not, and the C's ordering (validate, then
    /// consult the property) only makes sense with the C's notion of validity.
    #[test]
    fn an_embedded_nul_is_damage_not_text() {
        with_env(None, || {
            // Two clean accented characters outvote the one flaw, so the file
            // stays UTF-8 and only the NUL is replaced.
            let mut bytes = "caf\u{e9}".as_bytes().to_vec();
            bytes.push(0);
            bytes.extend_from_slice("na\u{ef}ve".as_bytes());
            let stats = utf8_stats(&bytes);
            assert_eq!(stats.illegal, 1, "the NUL is a flaw: {stats:?}");
            assert_eq!(decode_all(&bytes, None), "caf\u{e9}\u{FFFD}na\u{ef}ve");
        });
    }

    /// The case the release-suppression machinery exists for, and used to lose:
    /// a BOM-less UTF-16LE file whose bytes all "validate" as UTF-8 because Rust
    /// counts a NUL as a character. The clean-UTF-8 reading won before the named
    /// charset was ever consulted, the cues came out NUL-laden junk, and format
    /// detection then found nothing at all. The C decodes this correctly.
    #[test]
    fn a_bomless_utf16_file_decodes_as_the_charset_it_was_named() {
        with_env(None, || {
            let text = "1\n00:00:01,000 --> 00:00:02,000\nHi\n";
            let le = utf16(text, true, false);
            assert!(
                std::str::from_utf8(&le).is_ok(),
                "the premise: Rust reads these bytes as valid UTF-8"
            );
            assert_eq!(decode_all(&le, Some("UTF-16LE")), text);
            assert_eq!(
                decode_all(&utf16(text, false, false), Some("UTF-16BE")),
                text
            );

            // Line by line, which is how a framed stream arrives, and the shape
            // that also has to reach the decision without waiting for EOS.
            let mut f = Feeder::with_encoding(Some("UTF-16LE"));
            for line in text.split_inclusive('\n') {
                f.push(&utf16(line, true, false));
            }
            assert_eq!(f.finish(), text);
            assert_eq!(f.dec.charset(), "UTF-16LE");
        });
    }

    /// ...and the same rule keeps the C's forgiveness: an ASCII file with UTF-16
    /// named is still ASCII, because it is honestly valid UTF-8 and there is no
    /// NUL in it to say otherwise. `g_utf8_validate()` passes it too, so the C
    /// never consults the name either.
    #[test]
    fn a_named_utf16_does_not_reinterpret_an_ascii_file() {
        with_env(None, || {
            let ascii = b"1\n00:00:01,000 --> 00:00:02,000\nplain\n\n";
            for label in ["UTF-16LE", "UTF-16BE", "UTF-16", "UTF-32LE"] {
                assert_eq!(
                    decode_all(ascii, Some(label)),
                    String::from_utf8_lossy(ascii),
                    "label {label}"
                );
            }
        });
    }

    /// One stray byte in an otherwise clean file damages only itself. The C
    /// latches the whole buffer onto ISO-8859-15, destroying cues that come
    /// BEFORE the bad byte (findings case 2).
    #[test]
    fn one_invalid_byte_damages_only_itself() {
        with_env(None, || {
            let mut bytes = "What\u{200b} is\u{200b} ".as_bytes().to_vec();
            bytes.push(0xFF);
            bytes.extend_from_slice("this?? caf\u{e9}".as_bytes());
            assert_eq!(
                decode_all(&bytes, None),
                "What\u{200b} is\u{200b} \u{FFFD}this?? caf\u{e9}"
            );
        });
    }

    /// A legacy file's punctuation block. cp1252 and ISO-8859-15 agree on the
    /// accents; only cp1252 gets 0x80-0x9F right, and that is where a real
    /// legacy subtitle keeps its curly quotes, ellipsis and en dash.
    #[test]
    fn legacy_punctuation_decodes_as_cp1252_not_iso8859_15() {
        with_env(None, || {
            let bytes = [0x93u8, b'C', b'a', b'f', 0xE9, 0x94, 0x85, 0x96];
            assert_eq!(
                decode_all(&bytes, None),
                "\u{201C}Caf\u{e9}\u{201D}\u{2026}\u{2013}"
            );
        });
    }

    /// The five undefined cp1252 slots are damage when cp1252 was GUESSED, and
    /// ordinary content when the user named the charset: a name is a statement
    /// about the bytes, and rewriting under it would be inventing damage the
    /// user said was not there. Both paths, one byte at a time.
    #[test]
    fn undefined_cp1252_slots_are_repaired_only_for_the_guess() {
        for byte in [0x81u8, 0x8D, 0x8F, 0x90, 0x9D] {
            let bytes = [b'x', byte, b'y', 0x93];
            // Guessed: the slot is a hex box waiting to happen, so U+FFFD.
            with_env(None, || {
                assert_eq!(
                    decode_all(&bytes, None),
                    "x\u{FFFD}y\u{201C}".to_string(),
                    "guessed, byte {byte:#x}"
                );
            });
            // Named, through the property and through the environment, and by
            // the "iso-8859-1" label WHATWG also resolves to cp1252: verbatim,
            // C1 control and all.
            let verbatim = format!("x{}y\u{201C}", char::from_u32(u32::from(byte)).unwrap());
            for label in ["windows-1252", "cp1252", "ISO-8859-1"] {
                assert_eq!(
                    decode_all(&bytes, Some(label)),
                    verbatim,
                    "named {label}, byte {byte:#x}"
                );
                with_env(Some(label), || {
                    assert_eq!(
                        decode_all(&bytes, None),
                        verbatim,
                        "named {label} in the environment, byte {byte:#x}"
                    );
                });
            }
        }
    }

    /// Genuinely mixed input. Whichever reading explains more of the file wins,
    /// and the other side's bytes become U+FFFD instead of flipping the file.
    #[test]
    fn mixed_input_keeps_the_majority_reading() {
        with_env(None, || {
            // Six valid multi-byte sequences against two illegal bytes.
            let mut majority_utf8 = "caf\u{e9} na\u{ef}ve \u{fc}ber accents \u{e9}\u{e8}\u{fc} "
                .as_bytes()
                .to_vec();
            majority_utf8.extend_from_slice(&[0x93, b'q', b'u', b'o', b't', b'e', 0x94]);
            assert_eq!(
                decode_all(&majority_utf8, None),
                "caf\u{e9} na\u{ef}ve \u{fc}ber accents \u{e9}\u{e8}\u{fc} \u{FFFD}quote\u{FFFD}"
            );

            // Two illegal sequences (0x93, then 0xE9 0x94 which opens a
            // three-byte sequence a space cuts short) against one accidentally
            // valid pair.
            let majority_legacy = [0x93u8, b'C', b'a', b'f', 0xE9, 0x94, b' ', 0xC3, 0xA9];
            assert_eq!(
                decode_all(&majority_legacy, None),
                "\u{201C}Caf\u{e9}\u{201D} \u{c3}\u{a9}"
            );
        });
    }

    /// The counting spans the whole stream, not just the buffered window: the
    /// valid sequences arrive in one buffer and the damage in another, and a
    /// window-only count would call the second buffer legacy.
    #[test]
    fn evidence_is_pooled_across_chunks() {
        with_env(None, || {
            let mut f = Feeder::new();
            f.push("caf\u{e9} na\u{ef}ve \u{fc}ber \u{e9}\u{e8}\u{fc}".as_bytes());
            f.push(&[0x93, b'!']);
            assert_eq!(
                f.finish(),
                "caf\u{e9} na\u{ef}ve \u{fc}ber \u{e9}\u{e8}\u{fc}\u{FFFD}!"
            );
        });
    }

    /// A truncated trailing sequence at EOS is damage, and with nothing named
    /// it is repaired rather than flipping the file to a legacy charset.
    #[test]
    fn a_truncated_tail_at_eos_is_repaired() {
        with_env(None, || {
            let mut f = Feeder::new();
            f.push(b"Truncated\xe2");
            assert_eq!(f.held(), 1);
            assert_eq!(f.finish(), "Truncated\u{FFFD}");
        });
    }

    /// The same file with a charset NAMED: the user's choice wins over the
    /// repair, exactly as the C consults its property once UTF-8 validation
    /// fails. This is what `test_srt_utf8_truncated_at_eos` pins end to end.
    #[test]
    fn a_named_charset_wins_over_the_repair() {
        with_env(Some("ISO-8859-15"), || {
            let mut f = Feeder::new();
            f.push(b"Truncated\xe2");
            assert_eq!(f.held(), 1);
            // 0xE2 in ISO-8859-15 is U+00E2 'â'.
            assert_eq!(f.finish(), "Truncated\u{00E2}");
        });
    }

    /// ...but only over bytes that are not UTF-8. Valid UTF-8 is never
    /// re-read, which is the C's ordering too.
    #[test]
    fn a_named_charset_never_overrides_valid_utf8() {
        with_env(Some("windows-1251"), || {
            assert_eq!(
                decode_all("caf\u{e9} \u{4f60}".as_bytes(), None),
                "caf\u{e9} \u{4f60}"
            );
        });
    }

    /// Naming UTF-8 as the fallback for bytes just shown not to be UTF-8 says
    /// nothing, so the statistics decide as if nothing were named. The C's net
    /// behaviour is the same by accident (findings case 10).
    #[test]
    fn naming_utf8_as_the_fallback_is_the_same_as_naming_nothing() {
        let legacy = [0x93u8, b'C', b'a', b'f', 0xE9, 0x94];
        let named = with_env(Some("UTF-8"), || decode_all(&legacy, None));
        let unnamed = with_env(None, || decode_all(&legacy, None));
        assert_eq!(named, unnamed);
        assert_eq!(named, "\u{201C}Caf\u{e9}\u{201D}");
    }

    /// The decision is latched: damage found after it is repaired under the
    /// charset already chosen, not used to reopen the question. Even a run of
    /// bytes that would have voted the other way on its own cannot flip it.
    #[test]
    fn the_decision_is_latched_for_the_rest_of_the_stream() {
        with_env(None, || {
            let mut f = Feeder::new();
            // Plenty of valid multi-byte evidence...
            f.push("caf\u{e9} na\u{ef}ve \u{fc}ber ".repeat(8).as_bytes());
            // ...then one stray byte, which opens the sniff window...
            f.push(&[0xFF]);
            assert_eq!(f.dec.charset(), "undecided");
            // ...and enough filler to reach the limit, which forces the call.
            f.push(&vec![b'x'; SNIFF_LIMIT]);
            assert_eq!(f.dec.charset(), "UTF-8", "24 multi-byte against 1 illegal");

            // A pile of legacy bytes now: repaired under the latched decision,
            // never re-guessed as cp1252.
            f.push(&[0x93, 0x94, 0x85, 0x96, 0x93, 0x94]);
            assert_eq!(f.dec.charset(), "UTF-8");
            assert!(
                f.finish().ends_with(&"\u{FFFD}".repeat(6)),
                "got {:?}",
                &f.out[f.out.len() - 20..]
            );
        });
    }

    /// The sniff is bounded: once [`SNIFF_LIMIT`] undecided bytes have piled
    /// up the charset is decided without waiting for EOS, so a stream that
    /// never ends still produces text.
    #[test]
    fn the_sniff_decides_at_the_limit_without_eos() {
        with_env(None, || {
            let mut f = Feeder::new();
            f.push(&[0x93]); // opens the window
            assert_eq!(f.out, "");
            assert_eq!(f.held(), 1);
            f.push(&vec![b'x'; SNIFF_LIMIT]);
            assert_eq!(f.held(), 0, "the window must have been decided and drained");
            assert!(f.out.starts_with('\u{201C}'), "got {:?}", &f.out[..8]);
            assert_eq!(f.dec.charset(), "windows-1252");
        });
    }

    /// The byte limit alone does not bound a FRAMED stream. `rsssaparse` is fed
    /// one dialogue line per buffer and a whole film's dialogue is well under
    /// [`SNIFF_LIMIT`], so one bad byte in the first line used to withhold every
    /// cue in the track until EOS (measured: 1110 consecutive silent buffers).
    #[test]
    fn a_sparse_stream_decides_within_a_bounded_number_of_buffers() {
        with_env(None, || {
            let mut f = Feeder::new();
            // The bad byte arrives in the first line, the way a legacy file's
            // first accent does.
            f.push(b"Ren\xe9e speaks\n");
            assert_eq!(f.dec.charset(), "undecided");
            // Only the clean prefix was released, so the line is unusable until
            // the charset is settled.
            assert_eq!(f.out, "Ren");
            assert!(f.held() > 0, "the window is open");

            let mut buffers = 1;
            while f.dec.charset() == "undecided" {
                f.push(format!("line {buffers}\n").as_bytes());
                buffers += 1;
                assert!(
                    buffers <= SNIFF_CALLS,
                    "still undecided after {buffers} buffers, and EOS may never come"
                );
            }
            // cp1252 (one flaw, no clean multi-byte sequence to outvote it), so
            // the accent is the one the file meant and every ASCII line is
            // untouched.
            assert_eq!(f.dec.charset(), "windows-1252");
            assert!(f.out.starts_with("Ren\u{e9}e speaks\n"), "got {:?}", f.out);
            assert_eq!(f.held(), 0, "the window must have been drained");

            // ...and the rest of the track streams from there with no further
            // latency at all.
            f.push(b"and on\n");
            assert!(f.out.ends_with("and on\n"), "got {:?}", f.out);
        });
    }

    /// A named charset does not depend on the counts, so there is nothing to
    /// wait for once the window holds one illegal sequence: the decision lands
    /// on that very call rather than after [`SNIFF_CALLS`] of them.
    #[test]
    fn a_named_charset_settles_the_window_at_the_first_illegal_sequence() {
        with_env(None, || {
            let mut f = Feeder::with_encoding(Some("windows-1251"));
            f.push(&[0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
            assert_eq!(f.dec.charset(), "windows-1251");
            assert_eq!(f.out, "Привет");
            assert_eq!(f.held(), 0);
        });
    }

    /// The decision is a function of the stream's PREFIX, not of how upstream
    /// sized its reads: only the first [`SNIFF_LIMIT`] bytes of the window vote.
    /// Feeding the identical bytes as one buffer and as 4096-byte reads used to
    /// latch different charsets and therefore produce different text.
    #[test]
    fn the_decision_ignores_bytes_past_the_sniff_limit() {
        with_env(None, || {
            // One flaw, then a filler of the sniff limit, then a run of clean
            // multi-byte characters that would outvote the flaw if it could be
            // seen. It cannot: it is past the clamp.
            let mut bytes = vec![0x93];
            bytes.extend_from_slice(&vec![b'x'; SNIFF_LIMIT]);
            bytes.extend_from_slice("caf\u{e9} ".repeat(64).as_bytes());

            let one_buffer = decode_all(&bytes, None);
            assert!(
                one_buffer.starts_with('\u{201C}'),
                "the prefix decides, so cp1252: {:?}",
                &one_buffer[..8]
            );

            let mut chunked = Feeder::new();
            for chunk in bytes.chunks(4096) {
                chunked.push(chunk);
            }
            assert_eq!(
                chunked.finish(),
                one_buffer,
                "4096-byte reads must latch what one big buffer latches"
            );
        });
    }

    // ----------------------------------------------------- named charsets

    #[test]
    fn iso8859_15_euro_sign() {
        // 0xA4 is EURO in ISO-8859-15 (not the currency sign of ISO-8859-1).
        assert_eq!(decode_all(&[0xA4], Some("ISO-8859-15")), "\u{20AC}");
        assert_eq!(decode_all(&[0xA4], Some("ISO-8859-1")), "\u{00A4}");
    }

    #[test]
    fn shift_jis_via_env_split_mid_char() {
        // The Shift_JIS bytes for "日本" are 日 = 93 FA, 本 = 96 7B. These bytes
        // are invalid UTF-8, so the statistics send them to the named charset.
        with_env(Some("Shift_JIS"), || {
            let bytes = [0x93u8, 0xFA, 0x96, 0x7B];
            for split in 1..bytes.len() {
                let mut f = Feeder::new();
                f.push(&bytes[..split]);
                f.push(&bytes[split..]);
                assert_eq!(f.finish(), "日本", "failed at split offset {split}");
            }
        });
    }

    #[test]
    fn windows_1251_via_property() {
        // "Привет" in windows-1251 (single-byte Cyrillic), selected through the
        // property. No statistic could place this file; naming it is the point.
        let bytes = [0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        assert_eq!(decode_all(&bytes, Some("windows-1251")), "Привет");
    }

    /// The property is read on every call, not snapshotted when the decoder is
    /// built, because the C reads its property per converted block and an
    /// application may set it after the element is already running.
    #[test]
    fn the_named_charset_is_read_at_decision_time() {
        with_env(None, || {
            let bytes = [0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
            let mut f = Feeder::new();
            f.push(&bytes);
            assert_eq!(f.out, "", "the window is open, nothing decided yet");
            // Set only now, after the bytes have already arrived.
            f.encoding = Some("windows-1251".to_owned());
            assert_eq!(f.finish(), "Привет");
        });
    }

    #[test]
    fn unknown_label_falls_back_to_iso8859_15() {
        // An unrecognised label must decode like ISO-8859-15, not panic.
        assert_eq!(
            decode_all(&[0xA4], Some("definitely-not-an-encoding")),
            "\u{20AC}"
        );
    }

    /// `Encoding::for_label` resolves six legal iconv labels to `REPLACEMENT`,
    /// whose decoder emits ONE U+FFFD for the whole stream and nothing else, so
    /// naming one of them destroyed the track. They are labels `encoding_rs`
    /// does not implement, so they get the unknown-label fallback instead.
    #[test]
    fn a_replacement_mapped_label_falls_back_to_iso8859_15() {
        with_env(None, || {
            for label in [
                "iso-2022-kr",
                "csISO2022KR",
                "iso-2022-cn",
                "iso-2022-cn-ext",
                "hz-gb-2312",
                "replacement",
            ] {
                // 0xA4 is EURO in ISO-8859-15, and one whole cue's worth of text
                // survives around it.
                let mut f = Feeder::with_encoding(Some(label));
                f.push(b"Price: \xA4 5\n");
                assert_eq!(f.finish(), "Price: \u{20AC} 5\n", "label {label}");
                assert_eq!(f.dec.charset(), "ISO-8859-15", "label {label}");
            }
        });
    }

    /// A label with nothing left in it once the iconv suffix is stripped and the
    /// rest trimmed names nothing at all. It used to name ISO-8859-15 and, worse,
    /// to shadow `GST_SUBTITLE_ENCODING` while doing so.
    #[test]
    fn a_blank_label_names_nothing() {
        let cyrillic = [0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        for blank in ["", "   ", "//TRANSLIT", " //IGNORE"] {
            // The environment is still consulted, exactly as it is when the
            // property was never set at all.
            with_env(Some("windows-1251"), || {
                assert_eq!(
                    decode_all(&cyrillic, Some(blank)),
                    "Привет",
                    "property {blank:?}"
                );
            });
            // And with nothing named anywhere, the statistics decide rather than
            // the unknown-label fallback: these bytes are cp1252's, not
            // ISO-8859-15's (which would read 0xF0 as 'ð').
            with_env(None, || {
                assert_eq!(
                    decode_all(&cyrillic, Some(blank)),
                    "\u{cf}\u{f0}\u{e8}\u{e2}\u{e5}\u{f2}",
                    "property {blank:?}"
                );
            });
        }
        // Naming UTF-8 is naming nothing too, but it IS a label, so it still
        // shadows the environment the way the C's non-empty test does.
        with_env(Some("windows-1251"), || {
            assert_eq!(
                decode_all(&cyrillic, Some("UTF-8")),
                "\u{cf}\u{f0}\u{e8}\u{e2}\u{e5}\u{f2}"
            );
        });
    }

    /// A label that does not say which end comes first is read the way iconv
    /// reads it, since the C hands the label to `g_convert`, which is iconv.
    /// That is the host's byte order for `UTF-16`, `UCS-2` and `UTF-32` (NOT
    /// RFC 2781's big-endian), and big-endian for `UCS-4`. See
    /// [`AMBIGUOUS_BIG_ENDIAN`] for the `iconv` runs behind this.
    #[test]
    fn an_endianness_free_label_is_read_the_way_iconv_reads_it() {
        with_env(None, || {
            let text = "H\u{e9}llo";
            let host_utf16 = utf16(text, !AMBIGUOUS_BIG_ENDIAN, false);
            for label in ["UTF-16", "utf16", "UCS-2", "ucs2"] {
                assert_eq!(decode_all(&host_utf16, Some(label)), text, "label {label}");
            }

            let units = |big_endian: bool| -> Vec<u8> {
                text.chars()
                    .flat_map(|c| {
                        let raw = c as u32;
                        if big_endian {
                            raw.to_be_bytes()
                        } else {
                            raw.to_le_bytes()
                        }
                    })
                    .collect()
            };
            assert_eq!(
                decode_all(&units(AMBIGUOUS_BIG_ENDIAN), Some("UTF-32")),
                text
            );
            // UCS-4 is iconv's exception: big-endian whatever the host.
            assert_eq!(decode_all(&units(true), Some("UCS-4")), text);
        });
    }

    /// The typefind sample reads `GST_SUBTITLE_ENCODING`, which is what the C's
    /// `gst_sub_parse_type_find` does too (`gstsubparseelement.c:355-375`). What
    /// it must never do is let that variable stop a format being detected.
    #[test]
    fn the_typefind_sample_reads_the_environment_like_the_c_does() {
        let cyrillic = [0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        with_env(Some("windows-1251"), || {
            assert_eq!(decode_sample(&cyrillic, None), "Привет");
        });
        // A sample the probes can read is ASCII, and no named charset touches
        // ASCII: it is honestly UTF-8, so the name is never consulted. Even a
        // value that resolves to nothing recognisable is harmless, because the
        // unknown-label fallback is ASCII-transparent as well.
        let vtt = b"WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n";
        for label in [
            "definitely-not-an-encoding",
            "UTF-16LE",
            "iso-2022-kr",
            "  ",
        ] {
            with_env(Some(label), || {
                assert_eq!(
                    decode_sample(vtt, None),
                    String::from_utf8_lossy(vtt),
                    "environment {label:?}"
                );
            });
        }
    }

    #[test]
    fn utf32_can_be_named() {
        let text = "Hé!";
        let mut le: Vec<u8> = Vec::new();
        for c in text.chars() {
            le.extend_from_slice(&(c as u32).to_le_bytes());
        }
        assert_eq!(decode_all(&le, Some("UTF-32LE")), text);
    }

    // ------------------------------------------------------------ degenerate

    /// Bytes after the EOS that ended the stream start a new one. An element is
    /// expected to reset the decoder itself, but it must not be able to crash
    /// the process by not doing so: a `Foreign` charset's `encoding_rs` decoder
    /// panics ("Must not use a decoder that has finished") when it is used after
    /// being told the stream ended, and EOS then STREAM_START then a buffer is a
    /// real event order.
    #[test]
    fn decoding_after_eos_starts_a_new_stream() {
        with_env(None, || {
            let cyrillic = [0xCFu8, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
            let mut dec = Decoder::new();
            let (first, consumed) = dec.decode(&cyrillic, true, Some("windows-1251"));
            assert_eq!(first, "Привет");
            assert_eq!(consumed, cyrillic.len());
            assert_eq!(dec.charset(), "windows-1251");

            // The same decoder, one stream later.
            let (second, consumed) = dec.decode(&cyrillic, true, Some("windows-1251"));
            assert_eq!(second, "Привет", "the second stream must decode too");
            assert_eq!(consumed, cyrillic.len());

            // A new stream means a new decision, a new BOM sniff and a new BOM
            // strip, not the last stream's.
            let mut dec = Decoder::new();
            assert_eq!(dec.decode(b"\xef\xbb\xbffirst", true, None).0, "first");
            assert_eq!(dec.decode(b"\xef\xbb\xbfsecond", true, None).0, "second");
            assert_eq!(dec.charset(), "UTF-8");
            // Including the charset: this stream is UTF-16LE where the last was
            // UTF-8, and the latch must not have survived.
            assert_eq!(
                dec.decode(&utf16("third", true, true), true, None).0,
                "third"
            );
            assert_eq!(dec.charset(), "UTF-16LE");
        });
    }

    #[test]
    fn empty_input_decodes_to_nothing() {
        let mut f = Feeder::new();
        assert_eq!(f.finish(), "");
        assert_eq!(f.held(), 0);
    }

    #[test]
    fn a_file_valid_in_every_candidate_encoding_decodes_identically() {
        let ascii = b"1\n00:00:01,000 --> 00:00:02,000\nplain\n\n";
        for label in [
            None,
            Some("windows-1252"),
            Some("ISO-8859-15"),
            Some("UTF-8"),
        ] {
            assert_eq!(
                decode_all(ascii, label),
                String::from_utf8_lossy(ascii),
                "label {label:?}"
            );
        }
    }
}
