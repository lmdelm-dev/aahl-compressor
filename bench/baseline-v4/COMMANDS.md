# AAHL v5 Milestone 1 - Phase 0 exact command lines

All commands run from the repo root `C:\Users\Pc\aahl` on 2026-09-22.

## 1. Build (release)
```
cargo build --release
```

## 2. Test (release)
```
cargo test --release
```
Result: 98 passed; 0 failed; 0 ignored (finished in 51.61s).

## 3. Baseline bench (harness unchanged, synthetic corpus set)
```
.\target\release\aahl.exe bench .\corpus_set --tsv bench\baseline-v4\baseline-v4.tsv
```
Full console output captured in `bench-output.log` in this directory.

The bench harness (src/bench.rs, unchanged for Phase 0) runs, per corpus subdir:
- `aahl create <work>/aahl.aahl <files...>`  (defaults: 1 MiB chunk, lag 16, GC 64, jobs=1 serial)
- `aahl extract <work>/aahl.aahl <work>/out`
- reference lanes with FIXED settings:
  - zip6:   `7z a -tzip -mx=6 <out> <files...>`
  - 7z9:    `7z a -m0=LZMA2 -mx=9 <out> <files...>`
  - xz:     `xz -9 -c <files...>` (concatenated stream, stdout captured)
  - zstd:   `zstd -19 -c <files...>` (concatenated stream, stdout captured)
  - rar:    `rar a -m5 -ep <out> <files...>`
Every lane is round-trip verified (blake3 file-by-file for aahl/7z/rar; decode + compare
against concatenated input for the stream codecs) before a row is recorded.

These reference settings were fixed before measurement and were not changed after
seeing results.

## 4. Dataset hashes
```
python (blake3 + hashlib)  ->  per-file and set-concat hashes, see HASHES.md
PowerShell Get-FileHash -Algorithm SHA256  ->  cross-check of SHA-256 values
```

## 5. Environment record
See ENV.md in this directory.
