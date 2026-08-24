# Pango markup test corpus

Vendored from pango, `tests/markups/` at commit
`0b96e86efec3706601d7dc02b21c9bf19817c9de` (main, 2026-08).
License: LGPL-2.1-or-later, same as this repository.

`valid-*.markup` parse successfully; the paired `.expected` file is the
exact output of pango's `markup-parse` test driver (accel marker `_`):
flattened text, attribute list dump, per-range font descriptions, and the
accelerator char, separated by `---` lines. `fail-*.markup` must be
rejected; their `.expected` files hold pango's error message and are only
checked for the failure itself here (our messages are similar, not
byte-identical).

`corpus.rs` diffs our `dump::dump()` output byte-for-byte against the
`.expected` files.
