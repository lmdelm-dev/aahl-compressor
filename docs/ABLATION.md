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
   check (loud failure, never silent corruption — but existing v3 archives
   would stop reading). A 0.08% aggregate gain concentrated at the
   non-default chunk size does not justify a v3->v4 format bump. **Decision:
   keep RULE_CTX=8.** Clone-friendly to test? The archive's own default and
   the SPEC compat matrix stay honest.

2. **The order-1+2 "blend" codec never wins** on any corpus at any chunk size.
   It is dead code in `arith.rs`; wiring it into the persistent model (the
   original hypothesis) would *not* have helped — every blend row >= order1_tok
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
