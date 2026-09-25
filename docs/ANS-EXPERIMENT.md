# AAHL V6 STEP 3: tANS/FSE entropy backend — experiment record

Harness: `aahl ans-bench <files...> --recut --runs 3 --runs-tsv bench\ans-runs.tsv --summary-tsv bench\ans-summary.tsv`
Code: `src/tans.rs` (codec) + `src/ansbench.rs` (bench). Raw rows:
`bench/ans-runs.tsv` (225 rows) and `bench/ans-summary.tsv` (45 rows).

## Hypothesis

A from-scratch tANS/FSE-style entropy coder (Duda 2009, "Asymmetric numeral
systems"; spread rules follow Collet's FSE) can beat the shipping adaptive
order-0 range coder (arith) on per-chunk payload — but only if the frequency
table + framing overhead is counted honestly. The bench therefore reports
COMPLETE cost per block: table + payload + framing, never payload alone.

## Codec (src/tans.rs)

- State domain [L, 2L), L = 1 << K, K in 7..=12 (auto-bumped to fit the
  alphabet). Decode: slot = x & (L-1) → (sym, j), renorm while v < L.
- Normalization: floor(c·L/T) clamped ≥1, then **cyclic** largest-remainder
  correction (largest remainder first when growing, smallest first when
  shrinking, ties by symbol id). A single-pass correction proved WRONG on
  skewed folded-token histograms (sum left at 4625 for L=4096, exposed by
  release-mode OOB instead of the debug_assert) — the cyclic pass fixes it
  and is pinned by `normalization_properties`.
- Spread: Duda center-ruler, ((2j+1)·L / 2f) scaled 2^30, total order
  (fraction, symbol, j). Deterministic.
- Block format (self-contained, NOT an archive format): 1 log2 u8 | 2 num_used
  u16 | 4U pairs (sym u16, freq u16) | 4 num_tokens | 4 blob_len | payload.
  Byte framing = 11 + 4U; table share = 3 + 4U; payload = blob.
- Safety: every structural violation and truncation returns Err; decoder never
  panics (fuzz: bitflips_never_panic, truncated_blocks_always_err). Final
  state checked (`x == L`), payload padding validated to be zero.
- 10 unit tests + folded normalization regression: 118 → 129 tests total.

## Cost accounting (applies to BOTH codecs equally)

Shared 'A' frame before payload = 2 + 4 + 4·R (tag, num_rules, rule list).
- arith-lit  = 14 + blob                       arith-tok = 14 + 4R + blob
- tans-lit   = 6 + block                       tans-tok  = 6 + 4R + block
- recut adds no frame; arith_bytes = the exact inner block bytes and is the
  reference column for tans-recut rows.

arith_sizes() (real shipping cost) cross-checked against every arith row
(ok on all files), and each tANS block is verified to equal
table_bytes + framing_bytes + payload_bytes at runtime.

## Complete-cost results (bytes; negative = tANS smaller; sum over chunks)

| corpus      | mode       | arith      | tans       | delta     | delta %  |
|-------------|------------|------------|------------|-----------|----------|
| table.csv   | lit        | 15,937,308 | 15,919,251 | -18,057   | -0.1133  |
| table.csv   | recut      | 4,614,765  | 4,603,218  | -11,547   | -0.2502  |
| table.csv   | tok        | 5,055,351  | 5,152,851  | +97,500   | +1.9286  |
| large.txt   | lit        | 2,264,814  | 2,264,118  | -696      | -0.0307  |
| large.txt   | recut      | 974,412    | 979,431    | +5,019    | +0.5151  |
| large.txt   | tok        | 974,412    | 994,137    | +19,725   | +2.0243  |
| random.bin  | lit        | 3,146,739  | 3,148,842  | +2,103    | +0.0668  |
| random.bin  | recut      | 3,145,746  | 3,148,842  | +3,096    | +0.0984  |
| random.bin  | tok        | 3,169,068  | 3,282,819  | +113,751  | +3.5894  |
| precomp/in.bin| lit       | 3,146,739  | 3,148,842  | +2,103    | +0.0668  |
| precomp/in.bin| recut     | 3,145,746  | 3,148,842  | +3,096    | +0.0984  |
| precomp/in.bin| tok       | 3,169,068  | 3,282,819  | +113,751  | +3.5894  |
| precomp/z.zst| lit       | 3,146,901  | 3,149,337  | +2,436    | +0.0774  |
| precomp/z.zst| recut     | 3,145,875  | 3,149,370  | +3,495    | +0.1111  |
| precomp/z.zst| tok       | 3,169,233  | 3,283,305  | +114,072  | +3.5994  |

## Findings

1. **Token mode is REJECTED (payload-only win vanishes with table cost).**
   On table.csv the tANS payload is 9,564 bits *smaller* than arith's
   (39,545,484 vs 39,555,048) — a 0.02% win — but the per-chunk frequency
   table costs 789,480 bits, so complete cost is +97,500 bytes (+1.9%).
   Folded token alphabets (919–1280 symbols) force 4·U table bytes per
   chunk; no chunk is large enough to amortize them. Precompressed +
   random corpora lose +3.6% for the same reason (payload parity, table
   entirely overhead).

2. **Literal mode is a real but small complete-cost win: −0.03% to −0.11%**
   on compressible corpora (table.csv −18,057 B; large.txt −696 B), and a
   −0.25% win on table.csv's recut inner stream. Deterministic (all 225
   rows ok=1; every number size-stable across runs). It loses +0.067% to
   +0.11% on random/precompressed — where the payload is self-entropy and
   tANS's per-chunk table cannot pay back.

3. **Decode is 3–5× FASTER (tANS 13–15 ms vs arith 50–64 ms per 1 MiB
   literal chunk); encode is ~1.3–1.6× slower.** The decoder is a branchy
   table walk with no adaptive model, the arith decoder maintains a Fenwick/
   context model per step. Irrelevant to archive size; noted for the record.

4. **Measurements are complete and honest by construction**: both codecs pay
   identical framing, arith rows are stamped from arith_sizes() (real
   complete cost) before tANS rows are computed, and the recut mode re-runs
   the actual shipping pipeline (table::prepare + grammar::compress +
   wrap_block) so tANS-recut cannot hide behind a friendlier code path.

## Verdict

- **REJECTED: tANS token mode** as an entropy backend (payload advantage
  ~20x smaller than its table overhead; complete cost +1.9%..+3.6% on every
  corpus). This matches the decision rule's rejection clause exactly.
- **KEPT (capability only): tANS literal mode** — deterministic complete-cost
  improvement on the compressible corpora at the default 1 MiB chunk, codec
  isolated in src/tans.rs and NOT wired into any block tag. The archive
  format is byte-for-byte identical (create blake3 unchanged,
  f3682801…bfe2d0). Adoption inside the format would require a new block tag
  (writer/reader agreement, v-format bump) for at most −0.25%; per the
  RULE_CTX=16 precedent in ABLATION.md, a win of this size does not justify a
  format break, so tANS stays out of the container.
- **Observability kept**: `aahl ans-bench` remains in the tree so any future
  format work can re-measure before committing to a tag.

## Bugs found (worth record)
- normalize_symbols single-pass grow/shrink left Σf = 4625 ≠ L=4096 on
  skewed folded-token histograms (silent in release; survived unit tests
  because they used balanced counts). Failing case captured from real folded
  corpora, fixed with the cyclic correction, regression-tested.
- decode_tokens originally indexed `freq[sym_value]` instead of the position
  of sym within `used` → index-out-of-bounds on sparse alphabets; fixed via a
  sym→pos map, covered by roundtrip_many_random_alphabets.

