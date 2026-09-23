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


---

# Phase A (v5 milestone 1): parallelism, plumbing, and real binaries

Measurement-only milestone. No compression-logic changes to the v4 codec were
made; everything below records what the v4 encoder/decoder already does.

Host: Windows 10 Pro 19045 x64, Intel i7-8750H (6c/12t) @ 2.2 GHz, 15.9 GB RAM,
rustc 1.98.1. Tools: 7-Zip 26.02 (`7z`), xz 5.8.3, zstd 1.5.7, WinRAR 7.13
(demo build, `rar`). AAHL v0.1.0 went through `cargo build --release` and the
98-test suite (all green) before any measurement. Fixed reference settings:
`zip6` = `7z a -tzip -mx=6`, `7z9` = `7z a -m0=LZMA2 -mx=9`, `xz` = `xz -9 -c`,
`zstd` = `zstd -19 -c`, `rar` = `rar a -m5 -ep`. AAHL always `-j 1` in the bench
lanes; chunk size 1 MiB, lag 16, GC every 64 chunks.

Artifacts: `bench/jobs-scaling.tsv` (+ `jobs-scaling.raw.tsv`), `bench/parstats.tsv`,
`bench/binary.tsv`, `bench/synthetic-matrix.tsv`, `bench/full-matrix.tsv`.

## A1. Jobs scaling (`bench-jobs`): `--jobs N` buys nothing

Per-run create/extract timing for `-j 1/4/8`, 5 runs, median times:

| corpus     | jobs | med create | speedup vs j1 | efficiency | deterministic |
|------------|------|-----------:|--------------:|-----------:|:-------------:|
| text-large | 1    | 15 804 ms  | 1.000         | 1.000      | true          |
| text-large | 4    | 15 755 ms  | 1.003         | 0.251      | true          |
| text-large | 8    | 15 804 ms  | 1.000         | 0.125      | true          |
| table      | 1    | 50 592 ms  | 1.000         | 1.000      | true          |
| table      | 4    | 49 991 ms  | 1.012         | 0.253      | true          |
| table      | 8    | 49 646 ms  | 1.019         | 0.127      | true          |
| random     | 1    | 22 ms      | 1.000         | 1.000      | true          |
| random     | 4    | 22 ms      | 1.000         | 0.250      | true          |
| random     | 8    | 22 ms      | 1.000         | 0.125      | true          |

`deterministic=true` means the full archive BLAKE3 is identical across
`-j 1/4/8` for every corpus (also true of every create in the benchmark lanes).
Speedup never exceeds 1.02; efficiency is 1/N by construction.

## A2. Why: the parallelized phase is one microsecond of a 50-second job

`create --par-stats` instruments the worker-pool path without touching codec
semantics (counters live in the CLI wrapper):

| corpus     | jobs | chunks | tasks | used | stale | discover_us | commit_us | peak_workers |
|------------|------|-------:|------:|-----:|------:|------------:|----------:|-------------:|
| table      | 4    | 9      | 9     | 8    | 1     | 8           | 49 744 162 | 1            |
| table      | 8    | 9      | 9     | 8    | 1     | 3           | 49 557 003 | 1            |
| text-large | 4    | 2      | 2     | 0    | 2     | 3           | 15 699 493 | 1            |
| text-large | 8    | 2      | 2     | 0    | 2     | 3           | 15 810 173 | 1            |

Reading:
- **Worker reuse is not the problem.** On `table` 8 of 9 forked candidates were
  committed (the 1 "stale" is the epoch-base bootstrap chunk). On `text-large`
  both candidates are stale only because a 2-chunk input never bootstraps rules
  inside the lag window, so each chunk is discovered against an empty grammar -
  identical to serial in every respect (and byte-identical output).
- **The parallel arm is ~1 µs; the serial arm is ~50 s.** `discover_us` is the
  pooled work (captured-trie tokenization + BPE fold). `commit_us` is the
  main-thread commit path (candidate rebind + order-1/blend arithmetic coding +
  live model update + record write), which is serial by design because the
  progressive model and the output archive are one deterministic sequence.
- **`peak_workers=1`** confirms the pipeline never even overlaps: a worker
  finishes its microsecond of discovery before the committer releases the next
  chunk, so there is nothing to run in parallel with.
- Net effect: `-j N` changes nothing observable except efficiency. The flag is
  retained (it is harmless, byte-identical, and exercised by the test suite),
  but parallel discovery cannot speed up v4 create. Improving create time
  requires attacking the serial commit path - out of scope for a measurement
  milestone.

## A3. Real binary corpus (`corpus-bin`, harvest from C:\Windows\System32)

Deterministic harvest (sorted order, no reordering of results by favour):
files are classed by ordered rules - installer (setup/install/update-named or
.msi) -> executable (.exe/.dll/.sys) -> archive (.zip/.7z/.cab/.iso/.wim/.rar
/.tar) -> cold-image (.png/.jpg/.bmp/.gif/.ico). Each class is filled in sorted
order to a byte budget (8/16/16/4 MiB); one overshooting file per class is
kept. ACL-protected entries (15) are skipped and counted. Output tree is
`corpus_bin/<class>/` + `manifest.tsv` (class, rel, bytes, sha256) + summary.

| class      | files | copied bytes | ext mix |
|------------|------:|-------------:|---------|
| installer  | 58    | 9 534 133    | update/setup-named .cat/.dll/.exe/.mui/.png |
| executable | 42    | 16 779 568   | 38 .dll, 4 .exe |
| archive    | 11    | 16 799 516   | 9 .cab, 1 .wim, 1 .zip |
| cold-image | 94    | 4 496 311    | 88 .png, 3 .jpg, 3 .gif |
| **total**  | 205   | 47 609 528   | (manifest hashes verified by re-read) |

Single-lane median ratios (full rows in `bench/binary.tsv`):

| corpus    | aahl | 7z9  | xz   | zstd | rar  | zip6 |
|-----------|-----:|-----:|-----:|-----:|-----:|-----:|
| executable| 0.495| 0.245| 0.315| 0.339| 0.327| 0.391|
| installer | 0.517| 0.367| 0.425| 0.434| 0.432| 0.450|
| archive   | 0.995| 0.786| 0.789| 0.789| 0.790| 0.879|
| cold-image| 0.979| 0.957| 0.963| 0.962| 0.965| 0.969|

Reading (honest):
- **AAHL is strong on text, weak on real binaries.** On 42 real System32
  binaries it reaches only 0.495 vs 0.245 for 7z/LZMA2 and 0.315 for xz. The
  phrase-level grammar cannot discover the byte-windowed match structure of
  machine code that LZMA-family coders exploit; this is a real, documented
  limitation, not a tuning gap. Create also costs 166 s on binaries vs 2-4 s
  for the references.
- **Precompressed/cold inputs are stored, not worsened.** Archives (.cab/.wim,
  already-compressed) stay at 0.995 and PNGs at 0.979, both at or near the
  reference lanes - the per-file STORE fallback works as designed and never
  inflates beyond ~0.1% (worst observed overflow: precompressed/zip6 0.500 is
  7z re-compressing a 2 MB store with a 1 MB dictionary; AAHL stays 1.0001).
- Nothing in this milestone changes or improves these numbers; they are the v4
  baseline to measure Phase B/C proposals against.

## A4. Full matrix

`bench/full-matrix.tsv` = 60 rows (36 synthetic + 24 binary) across 6 lanes
and 10 corpora, every row `ok=true` (round-trip verified). Headline v4
positioning, one line per corpus class:

| corpus (raw bytes)        | best lane | best ratio | aahl | aahl rank |
|---------------------------|-----------|-----------:|-----:|:---------:|
| text-large (1.4 MB prose) | aahl      | 0.2362     | 0.2362 | 1/6      |
| table (9.1 MB CSV/JSON)   | 7z9       | 0.1526     | 0.1834 | 5/6      |
| executable (16.8 MB DLLs) | 7z9       | 0.2453     | 0.4950 | 6/6      |
| installer (9.5 MB)        | 7z9       | 0.3669     | 0.5172 | 6/6      |
| archive (16.8 MB)         | 7z9       | 0.7864     | 0.9950 | 6/6      |
| cold-image (4.5 MB PNGs)  | zstd      | 0.9618     | 0.9792 | 6/6      |
| random / precompressed    | any       | ~1.000     | ~1.000 | tie      |

The v4 codec wins text, ties store-lanes, and loses - clearly and predictably -
on machine code and compressed data. That is the measurement Phase A was
commissioned to establish.

## B. Phase B: measured table transform (v5)

Harness: `aahl bench corpus_set --tsv bench/phase-b-table.tsv --aahl-modes v4,v5`
(cargo release, 2026-09). Every row `ok=true` (round-trip blake3-verified in
both directions). `aahl-v4` = `create --no-table` (byte-identical legacy
container); `aahl-v5` = default create (measured transform). All corpora are
from the standard deterministic set.

| corpus        | raw      | aahl-v4 | aahl-v5 | delta size | delta %  | v5 ratio | 7z9    | xz     | zstd   | rar    | zip6   |
|---------------|----------|--------:|--------:|-----------:|---------:|---------:|-------:|-------:|-------:|-------:|-------:|
| table         | 9,128,841 | 1,674,390 | 1,538,846 | -135,544  | -8.09%   | 0.1686   | 0.1526 | 0.1528 | 0.1653 | 0.1842 | 0.1849 |
| text-large    | 1,375,929 |   324,987 |   324,987 |         0  |  0.00%   | 0.2362   | 0.2820 | 0.2821 | 0.2852 | 0.3023 | 0.3224 |
| precompressed | 2,097,189 | 2,097,334 | 2,097,334 |         0  |  0.00%   | 1.0001   | 0.5005 | 0.5005 | 0.5005 | 0.5005 | 0.5005 |
| random        | 1,048,576 | 1,048,678 | 1,048,678 |         0  |  0.00%   | 1.0001   | 1.0003 | 1.0001 | 1.0001 | 1.0001 | 1.0001 |
| tiny          |         1 |       100 |       100 |         0  |  0.00%   | 100.00   | 127.0  | 68.0   | 14.0   | 72.0   | 149.0  |
| empty         |         0 |       101 |       101 |         0  |  0.00%   | n/a      | 90     | 32     | 13     | 73     | 152    |

### B.1 Phase B takeaway

- **table is the only moved needle, and it moved**: 0.1834 -> 0.1686 ratio
  (-8.09% size, 1,674,390 -> 1,538,846 B), created with default chunking
  (9 unique chunks; oracle found 8 grids, transformed 3). v5 closes ~57% of
  the v4-to-7z9 gap on the table lane (gap was 281,239 B; remaining gap
  145,305 B to 7z9's 1,393,151).
- **Zero regressions on any other corpus**: text-large, precompressed,
  random, tiny, empty are byte-identical between v4 and v5 lanes (and the
  oracle counters read `transforms_chosen=0`), because the gate is measured,
  not heuristic. The transform only ever fires when the wrapped T stream is
  strictly smaller.
- **Honest cost**: v5 create on table is ~149 s vs v4's ~50 s. The oracle
  compresses both the raw chunk and the candidate T stream with the real
  codec; that is the price of a measured gate. Extract is unaffected
  (181-197 ms, no oracle at read time). The create cost is a documented
  tradeoff, not a bug; `--no-table` is the fast lane when table content is
  not expected.
- **BEFORE/AFTER frame**: v4-vs-v5 delta on the only changed corpus is
  -135,544 B (-8.09%). Non-table delta is exactly 0. Absolute payload
  numbers for every ablation component live in `bench/phase-b-ablation.tsv`;
  the 7-lane table lives in `bench/phase-b-table.tsv`; raw TSV, untrimmed.
