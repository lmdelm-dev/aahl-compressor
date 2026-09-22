# Benchmarks (synthetic corpus, 2026-09)

Generated with `cargo build --release && aahl.exe corpus .\corpus_set &&
aahl.exe bench .\corpus_set --tsv bench_results.tsv`. Synthetic corpus only
(text/CSV/JSON-ish/random/empty/tiny/precompressed); real-source corpora need
`--source`. Shipped defaults: v4 token model, 1 MiB chunks, lag 16, GC every
64 chunks, serial discovery. Ratio = archive size / raw size; lower is better.

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
table/aahl                        9128841      1674390      60816        240  0.183
table/rar                         9128841      1681802        693         87  0.184
table/xz                          9128841      1395056       6460         81  0.153
table/zip6                        9128841      1688084       1471         73  0.185
table/zstd                        9128841      1508699       7298         31  0.165
text-large/7z9                    1375929       387951        351         37  0.282
text-large/aahl                   1375929       324987      24707         53  0.236
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
  a dictionary of ~1MB on a 2MB store - a different, not obviously better,
  trade.
- **text-large (1.4 MB prose)**: AAHL 0.236 outpasses **all** reference
  lanes - ~19-21% smaller than xz 0.282, 7z9 0.282, and zstd 0.285, and
  ~28% smaller than rar (0.302). This is the flagship lane and the v4
  hot-rule model + 1 MiB default move it from 0.300 (pre-v4, 64 KiB default)
  to a clear first place with no dictionary work.
- **table (9.1 MB CSV-ish)**: AAHL 0.183 is a 13% improvement over the
  pre-v4 default (0.210) and edges rar (0.184), but remains ~11% behind zstd
  (0.165) and ~16% behind xz/7z9 (0.153). The residual is structural: table
  rows are near-unique and their rule->rule followers are near-uniform, so
  neither the v4 exact contexts nor deeper folding can close the gap to the
  LZMA family's windowed match coding (see docs/DESIGN.md, section on v4).
  This is the known, documented limitation; closing it needs a table-specific
  transform (row-format detection / columnar separation), which is out of
  scope for the current codec.
- **tiny**: AAHL 100.000 fixed overhead matches container minimums; zstd's
  14 is a stream header trick on a 1-byte input. Not a compression claim.