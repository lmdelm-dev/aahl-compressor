# PARALLELISM-V6: current-HEAD jobs scaling, determinism, and the honest answer

Step 2 of the V6 mission. Reuses the Phase A `bench-jobs`/`bench-parstats` infra
unchanged; no source modified.

## Harness

- `aahl bench-jobs corpus_set --tsv <out> --raw-tsv <raw> --chunk-size 1048576 --runs N --jobs 1,2,4,8 --corpora <...>`
- Environment: same machine/toolchain as `BASELINE-V6.md`. Default create path
  (v5 container; table transform enabled on `table`). `-j 1` baseline.

## Results (current HEAD = 62f1498, BLAKE3 of full archive each run)

| corpus     | jobs | create ms (med) | speedup vs j1 | efficiency | archive bytes | archive BLAKE3 (first 16) | identical |
|------------|------|-----------------|---------------|------------|---------------|---------------------------|-----------|
| table      | 1    | 150134          | 1.00          | 1.00       | 1,538,846     | f368280196eef73b          | yes       |
| table      | 2    | 151383          | 0.99          | 0.50       | 1,538,846     | f368280196eef73b          | yes       |
| table      | 4    | 151083          | 0.99          | 0.25       | 1,538,846     | f368280196eef73b          | yes       |
| table      | 8    | 150761          | 1.00          | 0.12       | 1,538,846     | f368280196eef73b          | yes       |
| text-large | 1    | 15918           | 1.00          | 1.00       | 324,987       | 9211aed78170840f          | yes       |
| text-large | 2    | 15757           | 1.01          | 0.51       | 324,987       | 9211aed78170840f          | yes       |
| text-large | 4    | 16047           | 0.99          | 0.25       | 324,987       | 9211aed78170840f          | yes       |
| text-large | 8    | 16129           | 0.99          | 0.12       | 324,987       | 9211aed78170840f          | yes       |
| random     | 1..8 | 22-25           | ~1.00         | ~0.12-0.5  | 1,048,678     | b271772de23c3eed          | yes       |
| precompressed | 1..8 | 25-28        | ~1.00         | ~0.12-0.5  | 2,097,334     | f12a3ffea73a3061          | yes       |
| tiny/empty | 1..8 | 20-22           | ~1.00         | ~0.12-0.5  | 100 / 101     | f8e89a9c… / f096d90c…     | yes       |

Every `det` column is `same`: the full-archive BLAKE3 is identical across every
run and every jobs value. **Archive bytes + BLAKE3 are exactly the V5 lane values
from the Step 1 baseline.**

## Why (measured, not guessed — Phase A parstats, unchanged)

`bench-parstats.tsv` (Phase A) shows serial commit dominates:

- table j8: chunks=9, tasks=9, used=8, `discover_us=3`, `commit_us=49,557,003`
  (99.99% of the 49,590 ms create is serial commit), `peak_workers=1`
- text-large j8: chunks=2, tasks=2, used=0, `commit_us=15,810,173` (~100%),
  `peak_workers=1`

The `commit` stage emits discovered grammar serially for determinism; workers
only pre-discover rules. With tiny per-chunk discovery and a serial commit gate,
`-j N` cannot help wall time.

## ANSWER (mission STEP 2 key question)

> Is AAHL actually doing enough parallel work to benefit from multiple workers?

**No.** Speedup is 1.00 ± 0.01 and parallel efficiency is ~1/N for both the v4
lane (Phase A baseline + `jobs-scaling.tsv`) and the v5 table path (this doc).
AAHL's parallel architecture is *deterministic by construction* (threads never
affect emitted bytes) — that is its real property — but it does **not** scale
wall time. Treating "-j N exists" as "parallel support" would be a false claim.
The serial commit stage is the bottleneck by 4 orders of magnitude.

## Files

- `bench/v6-jobs-table.tsv`, `bench/v6-jobs-table.raw.tsv` (table, j1/2/4/8, 1 run)
- `bench/v6-jobs-fast.tsv`, `bench/v6-jobs-fast.raw.tsv` (text-large/random/precompressed/tiny/empty, j1/2/4/8, 5 runs)

Step 2 **KEPT** conclusion: determinism across threads holds; no parallelism
speedup exists to reclaim.