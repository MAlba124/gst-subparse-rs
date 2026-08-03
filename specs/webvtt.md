# WebVTT format reference

## Authoritative source

- W3C **WebVTT: The Web Video Text Tracks Format**, <https://www.w3.org/TR/webvtt1/>
  (timestamp grammar: <https://www.w3.org/TR/webvtt1/#webvtt-timestamp>).
- Historical WHATWG note referenced by the C: `http://www.whatwg.org/specs/web-apps/current-work/webvtt.html`.

The notes below are our own distilled summary. No spec text is reproduced.

## What we implement: the subparse *subset*

We match the upstream GStreamer `subparse` element, **not** the full W3C spec.
C reference: `gst-plugins-base/gst/subparse/gstsubparse.c`, function
`parse_webvtt` (plus `parse_webvtt_cue_settings`, `parse_subrip_time`, and the
`parse_subrip` state-2 text pipeline it reuses). Output is `text/x-raw,
format=pango-markup`.

subparse treats WebVTT as "SubRip with dotted timings, cue settings, and a
larger tag whitelist". It is a lenient, line-oriented state machine. It does
**not** build the WebVTT node tree, regions, styles, or cue-timeline the real
spec describes.

### Line model and EOS flush

The element splits input into `\n`-terminated lines (a trailing `\r` is
dropped, so CRLF works). A line without a terminating `\n` is held until more
data arrives. At EOS the element pushes a synthetic `"\n\n"` so the final cue
is flushed even without a trailing blank line. Our parser reconstructs this by
splitting on `\n`, stripping a trailing `\r` per segment, and appending one
extra empty line.

### State machine (per line)

- **Seeking** (C states 0 and 1, both look for the timing line): a line
  containing `" --> "` whose two sides parse as timestamps starts a cue.
  Everything else (the `WEBVTT` signature line, cue identifiers, `NOTE`
  comments, `STYLE`/`REGION` blocks, blank lines) is silently ignored. There
  is no signature validation and no `NOTE`-block handling. Those lines simply
  never contain `" --> "`.
- **Collecting text** (C state 2): subsequent lines are appended (joined with
  `\n`) until a blank line ends the cue and emits it.

### Timestamps (`parse_subrip_time`)

- Grammar accepted: `HH:MM:SS.mmm` and the hour-less `MM:SS.mmm`. Both `.` and
  `,` work as the fractional separator (`.` is normalised to `,`).
- Interior spaces are turned into `0`. The fractional part is right-padded or
  truncated to exactly three digits (milliseconds). Examples: `,5` → 500 ms,
  `,05` → 50 ms, `,1234` → 123 ms.
- A missing fractional separator (no `,`/`.`) is a parse failure → the whole
  timing line is rejected and the cue dropped (recovery continues).
- Timing: `t = (H*3600 + M*60 + S) * 1e9 + ms * 1e6` nanoseconds
  (`GST_SECOND = 1e9`, `GST_MSECOND = 1e6`).
- **Field parsing** is one `sscanf "%u:%u:%u,%u"` (falling back to `"%u:%u,%u"`),
  which means per field: leading whitespace is skipped (so a tab parses where a
  space would have become a `0`), `strtoul` takes an optional sign, and the field
  must be **fully** consumed because the format has a literal after it, so
  `00:00:01x,000` is rejected. The fractional field is the *last* conversion, so
  nothing needs to follow it and junk after its digits is not read. That last
  point is load-bearing: the ' ' → `0` munging pulls the cue settings into the
  fractional field, so `00:00:02.5 A:start` becomes `00:00:02,50A:start` → `50A`
  → 50 ms. Rejecting it would silently drop the cue **and its text**.
- Fields land in the C's `guint`, and the seconds sum is computed in 32 bits, so
  an absurd hour/minute field wraps rather than saturating into the far future
  (`99999999:00,000` is 1705032644 s). A far-future cue would otherwise
  suppress every cue after it via the guard below.
- **Monotonic guard:** a timing line is accepted only if
  `previous_cue_end <= new_end`. In the element `ParserState.start_time` is
  advanced by each cue's duration after it is pushed, so the guard compares the
  previous cue's end against the new cue's end. We mirror this with `prev_end`.
- A **reversed** time line (`00:00:05.000 --> 00:00:02.000`) passes that guard
  and leaves the C with an underflowed duration, which `start_time += duration`
  then folds back to the parsed end. We emit the cue with its end clamped up to
  its start (the `end_ns >= start_ns` invariant `Cue` documents) but feed the
  guard the **parsed** end, so a following well-formed cue is still accepted.

### Cue settings (`parse_webvtt_cue_settings`)

Settings are the whitespace-separated tokens after the first space following
the end timestamp. Split on space **or** tab (empty tokens ignored):

| token    | field filled (`CueSettings`) | notes                                                           |
|----------|------------------------------|-----------------------------------------------------------------|
| `T:<n>%` | `text_position` (`u8`)       | sscanf `T:%hd`; `%` not strictly required                       |
| `S:<n>%` | `text_size` (`u8`)           | sscanf `S:%hd`                                                  |
| `L:<n>%` | `line_position` (`u8`)       | only the `%`-suffixed form                                      |
| `L:<n>`  | *(none)*                     | C stores a signed `line_number`; no `CueSettings` field         |
| `D:<x>`  | `vertical` (`String`)        | value is everything after `D:` (e.g. `vertical`, `vertical-lr`) |
| `A:<x>`  | `alignment` (`String`)       | value is everything after `A:` (e.g. `start`, `middle`, `end`)  |

Note: numeric values are parsed as a wrapping `i16` then cast to `u8`, exactly
like the C's `(guint8)` casts.

**Important divergence from the C's *observable* behaviour:** upstream parses
these settings into `ParserState` and then **discards them**. They never reach
the output buffer or caps. We instead surface them on `Cue.settings`. Absent
settings are represented as `None` (the C uses `0`/`""` internally). This is a
richer API, not a parity break, because the C output is identical either way.

### Text → Pango markup pipeline (state-2 tail of `parse_subrip`)

Applied in order to the accumulated cue text:

1. `g_markup_escape_text`, escape `& < > ' "` (to `&amp; &lt; &gt; &apos;
   &quot;`, the named references current GLib emits) plus GLib's C0/C1
   control-character set to `&#xNN;`.
2. `subrip_unescape_formatting`, un-escape `&lt;tag&gt;` back to real `<tag>`
   for the whitelist, **case-sensitively**. WebVTT whitelist:
   `i, b, c, u, v, ruby, rt`. WebVTT allows tag *attributes*, so characters
   after the tag name (`alnum . space tab ( )`) up to `&gt;` are preserved
   (e.g. `<v Spoke>`, `<c.someclass>`). Disallowed tags stay escaped.
3. `subrip_remove_unhandled_tags`, delete any still-escaped `&lt;…&gt;` whose
   first name character is an ASCII letter (e.g. `<font>`). Escaped inline
   timestamps like `&lt;00:00:00,200&gt;` start with a digit, so they are kept
   escaped. The C looks for the closing `&gt;` from every escaped `<`, i.e. it
   rescans to the end of the buffer per `&lt;`. We scan once with a forward-only
   cursor instead, since a cue's length is bounded by nothing (see
   `tests/tag_scan_linearity.rs`).
4. `strip_trailing_newlines`, drop trailing `\n` (keeping ≥ 1 char).
5. `subrip_fix_up_markup`, balance tags by adding missing closing tags at the
   end and dropping closing tags that were never opened (name match here is ASCII
   **case-insensitive**). Handles the "wrong multi-character closing tag" cases,
   e.g. `<ruby>Hello!</i></ruby>World!` → `<ruby>Hello!</ruby>World!`.

## Parity test corpus

The C's `test_webvtt` cases (`gst-plugins-base/tests/check/elements/subparse.c`,
arrays `webvtt_input`, `webvtt_input1..3`) are ported verbatim into
`crates/subparse-formats/src/formats/webvtt.rs` (`#[cfg(test)]`), each chunk
prefixed with the `WEBVTT FILE\n` header exactly as `test_vtt_do_test` does,
plus additional coverage for settings parsing, timestamp edge cases, the
monotonic guard, CRLF, and tag balancing.
