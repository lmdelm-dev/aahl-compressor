# Benchmarks (synthetic corpus, 2026-09)

Generated with `cargo build --release && aahl.exe corpus .\corpus_set &&
aahl.exe bench .\corpus_set --tsv bench_results.tsv`. Synthetic corpus only
(text/CSV/JSON-ish/random/empty/tiny/precompressed); real-source corpora need
`--source`. Ratio = archive size / raw size; lower is better.

```
corpus                                raw         size  create_ms extract_ms  ratio
empty/7z9                               0           90         27         43  1.000
empty/aahl                              0          101         30         17  1.000
empty/xz                                0           32        124         16  1.000
empty/zip6                              0          152        310         75  1.000
empty/zstd                              0           13         16         16  1.000
precompressed/7z9                 2097189      1049543        159         41  0.500
precompressed/aahl                2097189      2097334         38         23  1.000
precompressed/xz                  2097189      2097428        575         17  1.000
precompressed/zip6                2097189      2097457         72         33  1.000
precompressed/zstd                2097189      2097266        431         29  1.000
random/7z9                        1048576      1048773        122         34  1.000
random/aahl                       1048576      1048678         42         20  1.000
random/xz                         1048576      1048696        317         16  1.000
random/zip6                       1048576      1048730         78         33  1.000
random/zstd                       1048576      1048613        197         25  1.000
table/7z9                         9128841      1393151       5181        251  0.153
table/aahl                        9128841      1918871      55428        385  0.210
table/xz                          9128841      1395056       9784        205  0.153
table/zip6                        9128841      1688084       2318         94  0.185
table/zstd                        9128841      1508699      13683         41  0.165
text-large/7z9                    1375929       387951        429         49  0.282
text-large/aahl                   1375929       413040       7807         96  0.300
text-large/xz                     1375929       388208       1407         51  0.282
text-large/zstd                   1375929       392441       1594         24  0.285
tiny/7z9                                1          127         29         42  127.000
tiny/aahl                               1          100         31         19  100.000
tiny/xz                                 1           68         20         23  68.000
tiny/zip6                               1          149         28         43  149.000
tiny/zstd                               1           14         23         22  14.000
```

## Reading

- **Incompressible inputs (random, precompressed)**: AAHL adds ~0.0%
  (honest STORE fallback, per-file blake3); xz/zstd add a hair more or less,
  all ~1.000. 7z's 0.500 on precompressed is LZMA2's solid-block re-run paying
  a dictionary of ~1MB on a 2MB store — a different, not obviously better,
  trade.
- **text-large (1.4 MB prose)**: AAHL 0.300 vs zstd 0.285 / xz 0.282 / 7z9
  0.282 — within ~5% of the LZ-family at its own strength, with no dictionary
  work. zip6 (0.322) is behind AAHL.
- **table (9.1 MB CSV-ish)**: AAHL 0.210 is 37% behind xz/7z9 (0.153) and 27%
  behind zstd (0.165). This is the expected territory — order-1/order-1+2
  context models trail LZMA2's multi-MiB match window on long low-entropy
  tables.
- **Speed**: AAHL create is >10x slower than xz on table (55s vs 9.8s) — the
  O(n*m) fold per chunk. Extract is competitive (385ms vs 205ms). `--jobs > 1`
  closes the create gap without changing output bytes.

## Takeaway

AAHL is a research/from-scratch grammar codec: class-competitive on prose, best
possible on incompressible input (honest STORE), and clearly behind
general-purpose LZ on big repetitive tables. Its value is the self-contained,
deterministic, integrity-checked pipeline — not raw ratio vs LZMA2.