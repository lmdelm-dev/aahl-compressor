# AAHL ablation study (2026-09)

Harness: `aahl ablate <corpus_set> --tsv out.tsv` (see `src/ablation.rs`).
Every number below is produced by the real codecs, not re-implementations.
Rows are byte-deterministic across runs; `create_ms` is wall-clock and excluded
from the determinism test. `grammar` = persistent cross-chunk pipeline (what
archives actually emit as 'G' blocks); `stateless` = best stateless block;
`order0`/`order1_tok`/`order1_byte`/`blend` = per-block codec sizes.

## Corpus set
Synthetic deterministic set: `precompressed`, `random`, `table` (CSV-ish),
`text-large`, `tiny`.

## Results (sum of the shipping `grammar` column across corpora x chunk sizes)

| RULE_CTX | grammar   | order1_tok | blend     | stateless  |
|----------|-----------|-----------|----------|------------|
| 8        | 22,288,346| 25,466,995| 25,646,975| 23,989,021 |
| 16       | 22,269,897| 25,579,286| 25,762,576| 23,993,456 |
| 32       | 22,313,521| 25,667,564| 25,849,704| 23,996,743 |
| 64       | 22,361,703| 25,727,777| 25,923,860| 23,998,345 |

## Findings

1. **RULE_CTX=16 gains are chunk-size dependent and not worth the format
   break.** Per-chunk deltas (grammar column, rtx16 minus rtx8, negative =
   smaller):

   | chunk  | table      | text-large |
   |--------|-----------|-----------|
   | 4096   | -30,519   | -388      |
   | 16384  | -256      | +1,185    |
   | 65536  | +24       | -255      |
   | 262144 | +9,813    | +1,947    |

   The 4096-chunk win is real (mostly the `table` corpus), but at the default
   pipeline chunk size (65536) it is noise, and at the best-compressing large
   chunk (262144) RULE_CTX=16 **loses**. The aggregate "win" in the table above
   is driven entirely by the small-chunk rows.

   RULE_CTX is a *format-level constant*: writer and reader must agree on the
   token-context layout or G/order-1-token blocks fail their per-chunk blake3
   check (loud failure, never silent corruption â€” but existing v3 archives
   would stop reading). A 0.08% aggregate gain concentrated at the
   non-default chunk size does not justify a v3->v4 format bump. **Decision:
   keep RULE_CTX=8.** Clone-friendly to test? The archive's own default and
   the SPEC compat matrix stay honest.

2. **The order-1+2 "blend" codec never wins** on any corpus at any chunk size.
   It is dead code in `arith.rs`; wiring it into the persistent model (the
   original hypothesis) would *not* have helped â€” every blend row >= order1_tok
   row. Rejected.

3. **The persistent grammar's advantage is at small chunk sizes.** For
   `text-large`, grammar @4096 = 411,859 vs stateless @4096 = 805,987 (~49%);
   @65536 the advantage collapses (412,002 vs 411,747). For `table` the
   dominant term is cross-chunk rule reuse + recursion at small chunks.

4. **Recursion (gm-norec vs grammar) is worth 14-30%** on compressible corpora
   at small chunk sizes; at large chunk sizes chunks are self-contained enough
   that norec ~= grammar.

## Action taken (this commit)
- **Decoder stall fix (kept, the substantive bug find).** The fuzz suite
  (`payload_mutation_fuzz`, mutation of a valid stream through `create` +
  `extract`) exposed a latent decoder hang at higher RULE_CTX values:
  `Decoder::threshold` left `range = 0` after `range /= total` on a corrupt
  overshoot, then the next `decode_step` kept `range *= size` at 0 and the
  renorm loop `while range < TOP { range <<= 8 }` spun forever. Fix in
  `src/arith.rs`: clamp `self.range = 1` when division yields 0; `decode_step`
  uses `saturating_mul(size)`, clamps 0->1, and bounds renorm to
  `max_shifts = 8`. Valid-stream code paths unchanged (determinism preserved);
  corrupt input now returns a checksum failure instead of hanging.
  Regression test:
  `arith::tests::corrupt_overshoot_threshold_never_stalls_renorm`.
- **New invariant tests** pin the rule-context mapping (bucket range,
  permutation density, independence from alphabet size).
- **`aahl ablate` subcommand + `src/ablation.rs`** (deterministic, TSV output,
  model-footprint + run-timing columns). `aahl ablate <set_dir> --tsv <file>`.
- Full suite: 85 passed (was 81; +3 rule_ctx invariants + 1 overshoot
  regression). Round-trip, byte-determinism, corruption rejection, legacy v2
  read all still green.

## Raw rows
`ablation.tsv` (RULE_CTX=8) kept in repo; `ablation_rtx{16,32,64}.tsv` from
the sweep are reproducible via `aahl ablate` after flipping `RULE_CTX`.
## v4 sweep (shipping grammar: with_lag_v4, HOT_RULES=192, MAX_MERGES=1024)

Harness: `aahl ablate corpus_set --tsv out.tsv --chunk-sizes 65536,262144,1048576`.
`grammar` is the payload archives actually emit as G blocks.

| corpus      | chunk    | raw      | stateless | grammar   | model_bytes |
|-------------|----------|----------|-----------|-----------|-------------|
| table       | 65536    | 9128841  | 1978863   | 1979079   | 17286152    |
| table       | 262144   | 9128841  | 1738931   | 1738931   | 15918256    |
| table       | 1048576  | 9128841  | 1682478   | 1673892   | 19782640    |
| text-large  | 65536    | 1375929  | 397483    | 397483    | 15918256    |
| text-large  | 262144   | 1375929  | 342830    | 342830    | 15918256    |
| text-large  | 1048576  | 1375929  | 324804    | 324804    | 15918256    |

Findings (this drove the shipped defaults):

1. **Larger chunks are monotone-better for both corpora**, so the default was
   moved to 1 MiB (the allowed max). text-large's grammar equals its
   stateless best at every chunk (the model cost is only paid when G wins);
   table's grammar beats stateless only at 1 MiB (1673892 vs 1682478).
2. **The v4 model's marginal contribution is real but small on table**
   (~0.5% at 1 MiB vs the v3 8-bucket model). Followers of table rules are
   near-uniform; no HOT_RULES window (64..256) concentrates them, so the
   exact-context grid position is flat. The v4 exact rows pay off on
   rule-dominant streams (docstring/DIY corpora, repeated phrase blocks),
   covered by unit-level tests rather than this sweep.
3. **Contrast with the RULE_CTX ablation above**: widening buckets (8->16)
   also gained only at small chunks and lost at 262144. Together the two
   sweeps show the grammar's cross-chunk rule reuse - not context width - is
   the lever that scales with chunk size.
4. **model_bytes** stays well under the 64 MiB footprint cap (footprint test:
   `model_footprint_is_bounded_and_reported`); 19782640 bytes at 1 MiB/table
   is the high-water mark and corresponds to the largest alphabet (457
   contexts x 4096 counts x 8 bytes x 2 for Fenwick).

# Phase B: measured table transform - ablation decision record

Harness: `aahl ablate corpus_set --tsv bench/phase-b-ablation.tsv`.
`grammar` = the v4 payload; `grammar_tx` = the same grammar run where each
chunk whose oracle-selected transform is emitted as the wrapped T stream
(real code paths: `table::prepare` + `grammar.compress` + `table::wrap_block`).
`tx_selected` = unique chunks transformed. Everything deterministic
(asserted by `ablation_is_deterministic`).

| corpus        | chunk   | raw      | stateless | grammar   | grammar_tx | tx_selected |
|---------------|---------|----------|-----------|-----------|------------|-------------|
| table         | 4096    | 9,128,841 | 3,180,611 | 2,305,605 | 2,097,057  | 771         |
| table         | 16384   | 9,128,841 | 2,426,008 | 2,112,611 | 1,941,542  | 192         |
| table         | 65536   | 9,128,841 | 1,978,863 | 1,979,079 | 1,925,233  | 47          |
| table         | 262144  | 9,128,841 | 1,738,931 | 1,738,931 | 1,677,252  | 11          |
| table         | 1048576*| 9,128,841 | 1,682,478 | 1,673,892 | 1,538,846  | 3 (of 9)    |
| text-large    | 4096    | 1,375,929 |   805,987 |   415,421 |   415,421  | 0           |
| text-large    | 65536   | 1,375,929 |   397,483 |   397,483 |   397,483  | 0           |
| precompressed | 4096    | 2,097,189 | 2,100,267 | 2,100,267 | 2,100,267  | 0           |
| random        | 4096    | 1,048,576 | 1,050,112 | 1,050,112 | 1,050,112  | 0           |
| tiny          | 4096    |         1 |         7 |         7 |         7  | 0           |

*1048576 rows come from the real create path (`bench/phase-b-table.tsv`,
`--table-stats`: 9 chunks, 8 grids, 3 transforms), not the ablation harness.

## Decisions

- **KEPT**: measured table transform (v5). grammar_tx < grammar at every
  chunk size on table; 0 on every non-table corpus; margins justify the
  oracle cost for default 1 MiB packing (1,673,892 -> 1,538,846 at the real
  create path).
- **KEPT**: strict `b < a` oracle with real codec sizes. Prose/JSON comas
  survive the detector (grids_found 1) but the oracle declines
  (transforms_chosen 0) - honest by construction.
- **KEPT**: transforms scale *down* with chunk size (771/2256 chunks at
  4096 -> 3/9 at 1 MiB) because small chunks are more often pure grid or
  pure prose; the oracle never miscounts, and small-chunk gains are large
  per-byte (2,305,605 -> 2,097,057 = -9.0% at 4096). Tables win most at the
  size the default does not use, and still win at the default.
- **REJECTED (not measured, cost-prohibitive)**: per-column adaptive
  context / column-aware grammar. The T-stream already reorganizes data so
  the *existing* grammar benefits; adding a second model doubles the model
  footprint (currently 16-20 MiB high-water at 1 MiB chunks) for a delta the
  oracle shows is already captured.
- **INCONCLUSIVE**: `DMODE_DATE` (day-number encoding). Fired on zero
  real-world rows (no date columns in the synthetic set); unit-tested, kept
  off by default (`DMODE_NONE` for non-numeric, non-monotonic columns).

# V6 STEP 3: tANS/FSE entropy backend - decision record

Harness: `aahl ans-bench corpus_set\table\table.csv corpus_set\text-large\large.txt corpus_set\random\random.bin corpus_set\precompressed\in.bin corpus_set\precompressed\z.zst --recut --runs 3 --runs-tsv bench\ans-runs.tsv --summary-tsv bench\ans-summary.tsv` (225 rows, all ok=1, deterministic).
Codec + methodology + full numbers: see `docs/ANS-EXPERIMENT.md`.

| corpus       | mode  | arith bytes | tANS bytes | delta bytes | delta % | decision |
|--------------|-------|-------------|------------|-------------|---------|----------|
| table.csv    | lit   | 15,937,308  | 15,919,251 | -18,057     | -0.1133 | win      |
| table.csv    | recut | 4,614,765   | 4,603,218  | -11,547     | -0.2502 | win      |
| table.csv    | tok   | 5,055,351   | 5,152,851  | +97,500     | +1.9286 | lose     |
| large.txt    | lit   | 2,264,814   | 2,264,118  | -696        | -0.0307 | marginal |
| large.txt    | recut | 974,412     | 979,431    | +5,019      | +0.5151 | lose     |
| large.txt    | tok   | 974,412     | 994,137    | +19,725     | +2.0243 | lose     |
| random.bin   | lit   | 3,146,739   | 3,148,842  | +2,103      | +0.0668 | lose     |
| random.bin   | recut | 3,145,746   | 3,148,842  | +3,096      | +0.0984 | lose     |
| random.bin   | tok   | 3,169,068   | 3,282,819  | +113,751    | +3.5894 | lose     |
| precompressed (in.bin + z.zst) | tok | 6,338,301 | 6,566,124 | +227,823  | +3.5944 | lose     |

## Decisions

- **REJECTED: tANS token mode** as an entropy backend. On table.csv the tANS
  payload is actually 0.02% smaller than arith's, but the per-block frequency
  table (4 bytes per used symbol; folded alphabets run 919-1280 symbols) is
  ~20x larger than that gain, so complete cost is +1.9%..+3.6% on every
  corpus. A payload win that disappears entirely when the table is counted is
  rejected by the STEP-3 decision rule as written.
- **KEPT (out-of-container, measured-capability only): tANS literal mode** -
  deterministic complete-cost wins on compressible corpora at the default
  1 MiB chunk (table.csv -18,057 B / -0.11%, large.txt -0.03%), losses
  limited to +0.07%..+0.11% on random/precompressed. Codec is fully isolated
  in `src/tans.rs` (10 unit tests + normalization regression, self-contained
  block format, decoder never panics) and is NOT wired into any block tag, so
  the archive format is byte-for-byte unchanged (create blake3 reproduced:
  f368280196eef73b15cc9d763c6f029fc2583eeb5fccd975ee10bf8e14bfe2d0; test
  suite 118 -> 129). Format adoption would require a new block tag +
  writer/reader agreement for at most -0.25%; same reasoning that rejected
  RULE_CTX=16 (ABLATION.md) applies: margin too small to justify a format
  break, so tANS stays out of the container.
- **KEPT (tooling): `aahl ans-bench`** stays in the tree so format work can
  re-measure before committing to a tag; both TSVs are committed.
- **Decode speed note (non-decision)**: tANS decodes literals 3-5x faster
  (13-15 ms vs 50-64 ms per 1 MiB) but encodes ~1.3-1.6x slower; not a
  format driver.

## Test suite delta (this commit)
118 -> 129 tests (10 tans codec tests, 1 ansbench harness test, folded
normalization regression inside `normalization_properties`). All byte-
determinism, corruption-rejection, round-trip, legacy-read tests still green.
Pre-existing unrelated warnings unchanged.
