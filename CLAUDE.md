# Fork policy

This is a permanent fork of immunant/c2rust, maintained for the linux-rs
project (a from-scratch C-to-Rust translation of parts of the Linux
kernel). Two standing policies govern how changes are made here:

## Not going upstream

This fork is not staged for upstream contribution to immunant/c2rust.
Fixes should still be scoped and tested as if they might be portable
(clean, well-isolated, not linux-rs-specific hacks) — but there is no
active intent or plan to send PRs upstream. Don't frame commit messages,
issue text, or planning docs around "eventually upstream this."

Every real bug fix should still land as a real commit on this fork's
`master` (not just patched locally and forgotten), and should be tested
carefully to confirm it's a genuine upstream-shared bug rather than
something linux-rs's own usage pattern introduced.

## Additive, opt-in changes only

Fork-specific behavior changes default to OFF, gated behind an explicit
opt-in flag (the existing convention is `--enable-rule=all`), not a
change to default behavior. Stock `c2rust transpile` with no flags stays
byte-for-byte identical to upstream behavior.

**Exception:** strict, unconditional correctness fixes (e.g. a crash or
an AST-exporter bug that drops an entire file) don't need a flag — there
is no "default behavior" worth preserving when the alternative is a
crash or silent data loss.
