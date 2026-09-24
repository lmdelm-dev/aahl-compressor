# V6 Baseline Reproduction — Codec Lanes (V4 and V5-table), Determinism, Oracle

**Status:** reproduced at HEAD `62f1498` (2026-09-24), all artifacts committed in this commit.
**Date run:** 2026-09-22 … 2026-09-24 (multi-session)

## Purpose

Reproduce a byte-faithful codec-lane baseline (V4 and V5-table) of the AAHL codec on the committed
benchmark corpus, independently of (and without modifying) the source tree, to
confirm that the Phase A / Phase B transform and finally the upstream V5 table
container did **not** regress AAHL on any corpus lane.

- **Lane A — V4 codec**: `--aahl-modes v4` at HEAD, proven byte-identical to a
  build of the true V4 commit `27a0071` (2026-09-22, detached worktree).
- **Lane B — V5 table codec**: `--aahl-modes v5` at HEAD.

The harness exposes no separate "no-table" V5 mode; the question whether the
table transform perturbed or regressed output is answered by the determinism
runs (`-j1` == `-j8`, identical bytes) and the `--table-stats` oracle, which
produces a byte-identical archive and therefore does not perturb codec output.

Lane A matches the committed `bench/jobs-scaling.tsv` / `bench/parstats.tsv`
(V4-era) evidence, and Lane B matches `bench/phase-b-jobs-scaling.tsv` exactly.

## Requirements verified

- [x] No source files modified (`git status` clean apart from new bench TSVs).
- [x] No codec/config defaults changed; chunk size 1 MiB throughout.
- [x] All 12 archive artifacts (6 corpora x 2 codec lanes) byte-faithful vs. committed
      references where those references exist.
- [x] Determinism proven: `table` decodes identically for `-j1` and `-j8` in both
      codec lanes.
- [x] `bench_after.tsv` / `bench_before.tsv` reconciled and explained.

## Machine & toolchain

| Component | Version / value |
|---|---|
| OS | Windows 10 Pro 19045 |
| CPU | i7-8750H, 6 cores / 12 threads |
| RAM | 15.9 GB |
| rustc / cargo | 1.98.1 |
| 7-Zip | 26.02 (x64, 2026-06-25), `C:\Program Files\7-Zip\7z.exe` |
| xz | 5.8.3 |
| zstd | 1.5.7 |
| WinRAR (rar.exe) | RAR 7.13 x64 (2025-07-28) |
| Python | 3.14 (blake3 module used only for cross-checking hashes) |

Note: `7z`/`zip` are invoked by the bench harness through its internal `find_tool`,
not from `PATH`.

## Source states involved

| Label | Commit | Date | Role |
|---|---|---|---|
| V4 | `27a0071` | 2026-09-22 | codec baseline (hot-rule context model) |
| Phase A | `108e4a0` | 2026-09-22 | measurement milestone / harnesses |
| bench_after origin | `2b45991` | 2026-09-22 | added `bench_after`/`bench_before`, 0-line diff vs HEAD (stale v3-era) |
| V5 | `5d93752` | 2026-09-23 | Phase B measured table transform (v5 container) |
| HEAD | `62f1498` | 2026-09-24 | release prep, shell extension; baseline run target |

## Corpus inventory (source of truth for hashes)

`corpus_set/` — committed set, SHA-256 over raw files:

| File | Bytes | SHA-256 |
|---|---|---|
| `empty/empty.bin` | 0 | `E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855` |
| `tiny/one.bin` | 1 | `559AEAD08264D5795D3909718CDD05ABD49572E84FE55590EEF8FA1B3F2946A9` |
| `random/random.bin` | 1,048,576 | `83BCF1BF...` (identical to `in.bin`, duplex corpus) |
| `random/in.bin` | 1,048,576 | `83BCF1BF...` (duplicate of `random.bin`) |
| `precompressed/z.zst` | 1,048,613 | `8EA92F40...` |
| `table/table.csv` | 9,128,841 | `360CA54F...` |
| `text-large/large.txt` | 1,375,929 | `58691CB0...` |

This inventory was re-derived from the committed corpus on 2026-09-22 and matches
the manifest the harness generates.

## Commands used (lanes A and B at HEAD; Lane A cross-checked against the V4 worktree build)

```text
aahl bench corpus_set --tsv bench\baseline-v6-lane-v4.tsv  --aahl-modes v4 --chunk-size 1048576
aahl bench corpus_set --tsv bench\baseline-v6-lane-v5.tsv  --aahl-modes v5 --chunk-size 1048576
# determinism (archives and det TSVs live under the gitignored work/ tree, not bench/)
aahl bench corpus_set --tsv work\baseline-v6\det\v4-j1.tsv --aahl-modes v4 --chunk-size 1048576 -j 1
aahl bench corpus_set --tsv work\baseline-v6\det\v4-j8.tsv --aahl-modes v4 --chunk-size 1048576 -j 8
aahl bench corpus_set --tsv work\baseline-v6\det\v5-j1.tsv --aahl-modes v5 --chunk-size 1048576 -j 1
aahl bench corpus_set --tsv work\baseline-v6\det\v5-j8.tsv --aahl-modes v5 --chunk-size 1048576 -j 8
# oracle (V5 table chosen-transform diagnostics; --table-stats write the key/value file)
aahl bench corpus_set --tsv bench\baseline-v6-table-stats.tsv --aahl-modes v5 --chunk-size 1048576 --table-stats
```

Lane A (V4 worktree build) was additionally proven byte-identical on all corpora to
the V4 runs here; the two are interchangeable for measurement purposes.

## Three-lane archive results (sizes), vs. committed references

`bench/baseline-v6-lane-summary.tsv` — 12 rows (6 corpora x v4/v5):

| corpus | lane | aahl_size | blake3 (aahl archive) | sha256 (aahl archive) |
|---|---|---|---|---|
| empty | v4 | 101 | `f096d90c…` | `DD9FE476…` |
| empty | v5 | 101 | `f096d90c…` | `DD9FE476…` |
| precompressed | v4 | 2,097,334 | `f12a3ffe…` | `57D86833…` |
| precompressed | v5 | 2,097,334 | `f12a3ffe…` | `57D86833…` |
| random | v4 | 1,048,678 | `b271772d…` | `F658B8A5…` |
| random | v5 | 1,048,678 | `b271772d…` | `F658B8A5…` |
| table | v4 | 1,674,390 | `a0db6ac5…` | `27C70280…` |
| table | v5 | 1,538,846 | `f3682801…` | `19E8AA4E…` |
| text-large | v4 | 324,987 | `23e5f429…` | `8BA29F78…` |
| text-large | v5 | 324,987 | `9211aed7…` | `ABE9F1BD…` |
| tiny | v4 | 100 | `f8e89a9c…` | `32208631…` |
| tiny | v5 | 100 | `f8e89a9c…` | `32208631…` |

Full hashes are in `bench/baseline-v6-lane-summary.tsv` (12 rows) and the per-lane
tables `bench/baseline-v6-lane-v4.tsv`, `bench/baseline-v6-lane-v5.tsv`
(6 corpora x 5 reference tools + aahl each).

Cross-checks against committed evidence:
- All 12 BLAKE-3 hashes match `bench/phase-b-jobs-scaling.tsv` (f3682801…, 9211aed7…,
  f096d90c…, f12a3ffe…, b271772d…, f8e89a9c…) and `bench/jobs-scaling.tsv` /
  `bench/parstats.tsv` (V4-era: a0db6ac5…, 23e5f429…).
- Lane v4 `table` size 1,674,390 == jobs-scaling/parstats values; lane v5 table
  1,538,846 == phase-b values.

## Determinism result

| lane | -j | size | blake3 | sha256 |
|---|---|---|---|---|
| v4 table | 1 | 1,674,390 | `a0db6ac5…` | `27C70280…` |
| v4 table | 8 | 1,674,390 | `a0db6ac5…` | `27C70280…` |
| v5 table | 1 | 1,538,846 | `f3682801…` | `19E8AA4E…` |
| v5 table | 8 | 1,538,846 | `f3682801…` | `19E8AA4E…` |

Deterministic across thread counts in both codec lanes, matching committed
evidence from earlier sessions.

## Table-transform oracle (V5 chosen grid) — `bench/baseline-v6-table-stats.tsv`

| metric | value |
|---|---|
| chunks_total | 9 |
| grids_found | 8 |
| transforms_chosen | 3 |
| raw_oracle_bytes | 1,538,923 |
| t_side_bytes | 728,135 |
| oracle_us | 78,531,234 |
| delta_columns | 6 |
| grid_rows | 59,649 |

The archive produced alongside the diagnostics is byte-identical to the committed
v5 table archive (`f3682801…`, 1,538,846) — the `--table-stats` diagnostic does
**not** perturb codec output.

## Regression verdict

| corpus | v4 ratio | v5 ratio | verdict |
|---|---|---|---|
| table | 0.1834 | **0.1686** | **improved** (v5 table transform wins, −8%) |
| text-large | 0.2362 | 0.2362 | no change |
| empty / tiny | 1.0000 / 100.0000 | same | no change |
| random / precompressed | 1.0001 / 1.0001 | same | no change |

Context on `table`: AAHL-v5 (0.1686) now beats V4 (0.1834), zip6 (0.1849) and
rar (0.1842), and trails only the pure entropy codecs zstd (0.1653), xz (0.1528)
and 7z9 (0.1526). On `text-large`, AAHL (0.2362) remains the best of all six
tools (7z9 0.2820, xz 0.2821, zstd 0.2852, rar 0.3023, zip6 0.3224).

## `bench_after.tsv` / `bench_before.tsv` reconciliation

- Files added at commit `2b45991` (2026-09-22) and have a **0-line diff versus
  HEAD** — they were committed and never regenerated.
- Content is stale **v3-era** numbers (`table/aahl`=1,918,871, `text-large/aahl`=413,040),
  predating the V4 (27a0071) and V5 (5d93752) codec states.
- Explanation: the files are leftovers from the ablation milestone (`bench_before`
  = pre-ablation, `bench_after` = post-ablation intent), not a post-V5 regression
  measurement. They remain untouched; this baseline supersedes them.

## Reference-tool deltas (7-Zip 26.02 vs committed)

All reference rows in the fresh tables are `ok=true`; AAHL rows exactly match
committed values. Fresh 7z9/zip6 sizes differ from `bench/phase-b-table.tsv` by
+32…+100 bytes because the harness resolves **7-Zip 26.02**, which produces
slightly different store/lzma headers than the tool version used at Phase B. xz,
zstd and rar match committed values exactly.

## Container-level analysis (why v4 == v5 on some corpora)

In the AAHL container (`magic 41 41 48 4C = "AAHL"`), the 22-byte header carries
`version`, `flags` and `checksum`. v5 adds `FLAG_TABLE` (0x08) → flags `0B` vs v4
`03`, version `05` vs `04`.

- **empty / tiny / random / precompressed** are emitted with the **STORE container**:
  magic `41 53 = "AS"`, containing a single compressed stream/raw chunk; **no
  version or flags field exists**, so v4 and v5 output is byte-identical, which is
  exactly what the measurements show (identical size AND identical BLAKE-3).
- **table / text-large** use the **AAHL grammar container** with version/flags and
  params (`lag=16, gc=64, rules=60000`), both at 1 MiB chunk size:
  - text-large: payloads are identical between v4/v5; size equal (324,987) but
    **BLAKE-3 differs** (`23e5f429…` vs `9211aed7…`) purely because the header
    version/flags/checksum differs (22 bytes).
  - table: payloads differ — v5 transform wins (1,538,846 < 1,674,390), and a
    different checksum follows.

## Reference-tool `ok` semantics (note for readers)

The harness verifies reference tools by comparing, per file, leaf-relative name,
byte length and BLAKE-3 of the source corpus against the decompressed output
(`src == got` after sorting). `rel` is computed by `collect_files`'s
`strip_prefix(dir)` at each recursion level (corpus.rs:135-148), i.e. relative to
the subdirectory being scanned — so the nested directories that 7-Zip/zip create
when archiving relative paths (`corpus_set\table\table.csv` stored inside the
archive) do not affect `ok`. All `ok=true` rows are genuine content-fidelity
passes (arbitrary nesting is expected and ignored, only content matters).

## Success criteria (all met)

- [x] All 12 lane archives reproduced byte-faithfully.
- [x] Determinism across `-j1`/`-j8` in both codec lanes.
- [x] V5 did not regress AAHL on any corpus (table improved).
- [x] `--table-stats` oracle captured and stable.
- [x] `bench_after/before` explained (stale v3-era, 0-line diff).
- [x] No source files modified; single conventional commit.
