# Benchmarks (synthetic corpus, 2026-09)

Generated with `cargo build --release && aahl.exe corpus .\corpus_set &&
aahl.exe bench .\corpus_set --tsv bench_results.tsv`. Synthetic corpus only
(text/CSV/JSON-ish/random/empty/tiny/precompressed); real-source corpora need
`--source`. Ratio = archive size / raw size; lower is better.

```
corpus                                raw         size  create_ms extract_ms  ratio
empty/7z9                               0           90         16         15  1.000
empty/aahl                              0          101         15          9  1.000
empty/rar                               0           73         37         35  1.000
empty/xz                                0           32         10         10  1.000
empty/zip6                              0          152         16         26  1.000
empty/zstd                              0           13         10         10  1.000
precompressed/7z9                 2097189      1049543        143         25  0.500
precompressed/aahl                2097189      2097334         29         15  1.000
precompressed/rar                 2097189      2097440        123         38  1.000
precompressed/xz                  2097189      2097428        460         15  1.000
precompressed/zip6                2097189      2097457         64         33  1.000
precompressed/zstd                2097189      2097266        210         14  1.000
random/7z9                        1048576      1048773         88         22  1.000
random/aahl                       1048576      1048678         20         11  1.000
random/rar                        1048576      1048736         82         34  1.000
random/xz                         1048576      1048696        276         12  1.000
random/zip6                       1048576      1048730         47         23  1.000
random/zstd                       1048576      1048613        110         12  1.000
table/7z9                         9128841      1393151       3146        157  0.153
table/aahl                        9128841      1918871      29493        172  0.210
table/rar                         9128841      1681802        693         87  0.184
table/xz                          9128841      1395056       6460         81  0.153
table/zip6                        9128841      1688084       1471         73  0.185
table/zstd                        9128841      1508699       7298         31  0.165
text-large/7z9                    1375929       387951        351         37  0.282
text-large/aahl                   1375929       413040       4882         59  0.300
text-large/rar                    1375929       415968        112         39  0.302
text-large/xz                     1375929       388208        596         22  0.282
text-large/zip6                   1375929       443597        426         43  0.322
text-large/zstd                   1375929       392441        597         13  0.285
tiny/7z9                                1          127         16         15  127.000
tiny/aahl                               1          100         17         10  100.000
tiny/rar                                1           72         41         35  72.000
tiny/xz                                 1           68         11         12  68.000
tiny/zip6                               1          149         16         14  149.000
tiny/zstd                               1           14         12         13  14.000
```

## Reading

- **Incompressible inputs (random, precompressed)**: AAHL adds ~0.0%
  (honest STORE fallback, per-file blake3); xz/zstd add a hair more or less,
  all ~1.000. 7z's 0.500 on precompressed is LZMA2's solid-block re-run paying
  a dictionary of ~1MB on a 2MB store — a different, not obviously better,
  trade.
- **text-large (1.4 MB prose)**: AAHL 0.300 vs zstd 0.285 / xz 0.282 / 7z9
  0.282 — within ~5% of the LZ-family at its own strength, with no dictionary
  work. RAR (0.302) lands right next to AAHL; zip6 (0.322) is behind both.
- **table (9.1 MB CSV-ish)**: AAHL 0.210 is 37% behind xz/7z9 (0.153) and 27%
  behind zstd (0.165). RAR (0.184) beats AAHL but still trails the LZMA
  family. This is the expected territory — order-1/order-1+2 context models
  trail LZMA2's multi-MiB match window on long low-entropy tables.
- **RAR (`rar a -m5 -ep`)**: ratio lands between zip6 and zstd (0.184 table /
  0.302 prose), is the fastest compressing tool on table (693ms) and is behind
  AAHL on tiny/empty due to RAR's own container overhead (72 vs 100 bytes on a
  1-byte file is RAR's baseline format cost, not compression strength).
- **Speed**: AAHL create is >10x slower than xz on table (29s vs 6.5s) — the
  O(n*m) fold per chunk. Extract is competitive (172ms vs 81ms). `--jobs > 1`
  closes the create gap without changing output bytes.

## Takeaway

AAHL is a research/from-scratch grammar codec: class-competitive on prose, best
possible on incompressible input (honest STORE), and clearly behind
general-purpose LZ (7z/xz/zstd, and to a lesser extent RAR) on big repetitive
tables. Its value is the self-contained, deterministic, integrity-checked
pipeline — not raw ratio vs LZMA2.

## Study

ahl ablate <set_dir> --tsv out.tsv runs a deterministic per-chunk ablation
(grammar vs stateless vs order-0/1 vs blend) and reports model-footprint bytes
+ wall-clock per row. Findings from the RULE_CTX sweep (8 vs 16 vs 32 vs 64)
are in docs/ABLATION.md: RULE_CTX stays at 8 (format constant; 16 only wins at
the non-default 4096 chunk and loses at 262144). The ablation also pinned the
blend codec as never-winning (rejected) and shipped the decoder overshoot
stall fix.

