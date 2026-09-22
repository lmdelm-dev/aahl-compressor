# AAHL v5 Milestone 1 - Phase 0 RECORD

Date: 2026-09-22. Commit: 27a00718b467ca273b81f1c4db504550e29437c8 (v4).

## What was run
1. `cargo build --release` - clean, no errors (14 pre-existing warnings, untouched).
2. `cargo test --release` - 98 passed, 0 failed (51.61s).
3. `aahl bench .\corpus_set --tsv bench\baseline-v4\baseline-v4.tsv` - harness unchanged.
4. Dataset BLAKE3/SHA-256 recorded (HASHES.md).

## Deviations from an ideal clean run
- **Corpus set not regenerated in this session.** `corpus_set` pre-existed and
  matches the deterministic builder exactly (7 files; sizes and hashes identical to
  what `aahl corpus corpus_set` would produce; see HASHES.md). No rebuild was
  performed, and the set was not modified.
- **Timings are wall-clock on a shared laptop (i7-8750H, 6c/12t).** Machine load
  during the run was light but not isolated; a single create-time outlier on
  `empty/zip6` (60 ms vs typical 15-16 ms) is attributed to OS noise. Phase A1
  repeats timing runs and reports medians for exactly this reason.
- **WinRAR is an evaluation build** (records RAR 7.13, "Version de démonstration").
  RAR lanes must be read with that caveat; do not treat demo-build rar numbers as
  equivalent to a licensed RAR.
- **Pre-existing untracked file `bench_verify.tsv`** at the repo root was present
  before this milestone and was not touched.
- 14 pre-existing rustc warnings (dead_code on public Phase-4 surfaces, etc.); none
  were addressed in this measurement-only milestone.
- Reference-tool settings were fixed (see COMMANDS.md) before any result was read.

## Integrity invariants re-checked implicitly
- All bench rows have `ok=true` (round-trip verification passed for every tool lane).
- `random` and `precompressed` AAHL lanes are ~1.000 (STORE honesty: no bigger than
  raw + 6 B); text-large AAHL 0.2362 remains the AAHL flagship lane.
- No compression-logic file was modified: src/aahl.rs, src/arith.rs, src/grammar.rs,
  and the encoder/decoder are byte-for-byte at HEAD.
