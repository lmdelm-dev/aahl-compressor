# AAHL V6 STEP 5: context-conditioned folded-token codec

## Status

This is an isolated experiment. The codec and `bench-ctx` harness are present
for reproducibility, but the codec is not selected by `compress_block`, is not
assigned an archive block tag, and does not change the archive container.

## Hypothesis

A folded-token stream may benefit from a context-conditioned adaptive model that
uses the previous token and a sparsely learned order-2 context. The experiment
measures complete block cost, including folding, rule metadata, frame fields,
checksum, and arithmetic payload, rather than payload bytes alone.

## Codec

The implementation is in `src/context.rs`.

- Raw bytes are folded with the existing deterministic `aahl::fold` pass.
- Literals occupy symbols `0..256`; folded rules follow them.
- Order-1 uses `arith::v4_ctx` and an adaptive Fenwick-backed frequency model.
- Order-2 uses 1,024 deterministic hashed `(previous2, previous)` buckets.
  Buckets are allocated lazily and are eligible only after two observations;
  accumulated likelihood wins select the order-2 model once it is ahead of
  order-1.
- The range-coder primitive is reused from `arith::Encoder`/`Decoder`; the
  context model, block format, selection logic, and validation are implemented
  separately for this experiment. Only `encode_step` visibility was widened to
  `pub(crate)`.
- The custom frame is:
  `A0 X | rule_count u32 | rule pairs u16/u16 | token_count u32 |
  blob_len u32 | arithmetic blob | checksum u32`.
- Empty input uses the fixed six-byte empty block. Non-empty framing is
  `18 + 4 * rule_count` bytes before the arithmetic blob.
- Structural bounds, checksum mismatch, truncation, invalid symbols, invalid
  rule references, and invalid expected lengths return errors. Decoder paths
  are checked before indexing model state.

## Harness and accounting

`src/contextbench.rs` exposes:

```text
aahl bench-ctx <files-or-directories...> [options]
```

The reproducible run used:

```text
cargo run --release -q -- bench-ctx "corpus_set\\table\\table.csv" "corpus_set\\text-large\\large.txt" "corpus_set\\random\\random.bin" "corpus_set\\precompressed" "corpus_set\\tiny\\one.bin" "corpus_set\\empty\\empty.bin" "src" --runs 3 --chunk-size 65536 --runs-tsv "bench\\ctx-runs.tsv" --summary-tsv "bench\\ctx-summary.tsv"
```

The inputs contain 23 files and 229 chunks. `bench/ctx-runs.tsv` has 1,603
rows: four size lanes per chunk and three context repetitions per chunk. Every
row has `ok=1`. `bench/ctx-summary.tsv` has 115 rows, five modes for each of
the 23 inputs.

The comparison modes are:

- `fold`: the existing folded/Huffman baseline.
- `order0`: existing adaptive order-0 arithmetic over folded tokens.
- `order1_tok`: existing order-1 arithmetic over folded tokens.
- `order1_byte`: existing order-1 arithmetic over raw bytes.
- `context`: the new context-conditioned folded-token codec.

`model_bytes=0` means that no model or frequency table is serialized into the
block. The adaptive models are reconstructed during decoding; their in-memory
RAM footprint is not included in this wire-size experiment. Baseline timing
columns are zero because those lanes use the existing size-only measurement
API; only the context lane performs timed encode/decode calls. Baseline and
context timing values must not be compared as like-for-like throughput.

## Complete-size results

All values below are sums over the 65,536-byte chunks in the stated input
group. Deltas are context minus the corresponding baseline.

| group | raw | fold | order0 | order1_tok | order1_byte | context |
|---|---:|---:|---:|---:|---:|---:|
| all 23 files | 14,149,354 | 6,060,949 | 5,896,239 | 6,527,149 | 7,445,710 | 6,604,904 |
| non-`src` | 13,650,536 | 5,867,423 | 5,714,880 | 6,342,585 | 7,189,171 | 6,417,906 |
| `src` | 498,818 | 193,526 | 181,359 | 184,564 | 256,539 | 186,998 |

| group | vs fold | vs order0 | vs order1_tok |
|---|---:|---:|---:|
| all 23 files | +543,955 (+8.9747%) | +708,665 (+12.0189%) | +77,755 (+1.1913%) |
| non-`src` | +550,483 (+9.3820%) | +703,026 (+12.3017%) | +75,321 (+1.1878%) |
| `src` | -6,528 (-3.3732%) | +5,639 (+3.1093%) | +2,434 (+1.3188%) |

Representative per-input results:

| input | fold | order0 | order1_tok | context | context vs fold |
|---|---:|---:|---:|---:|---:|
| `table.csv` | 2,081,142 | 1,978,863 | 2,045,779 | 2,086,628 | +5,486 (+0.2636%) |
| `large.txt` | 411,582 | 397,483 | 413,051 | 432,237 | +20,655 (+5.0184%) |
| `random.bin` | 1,124,719 | 1,112,827 | 1,294,583 | 1,299,674 | +174,955 (+15.5554%) |
| `precompressed/in.bin` | 1,124,719 | 1,112,827 | 1,294,583 | 1,299,674 | +174,955 (+15.5554%) |
| `precompressed/z.zst` | 1,124,980 | 1,112,854 | 1,294,563 | 1,299,663 | +174,683 (+15.5277%) |
| `tiny/one.bin` | 275 | 20 | 20 | 24 | -251 (-91.2727%) |

The tiny result is a framing floor, not evidence of useful compression: the
six-byte empty frame is reserved separately, while non-empty custom framing
already exceeds the one-byte payload.

## Findings and decision

1. The context model is not a broad win. It is 8.97% larger than `fold` and
   1.19% larger than `order1_tok` over the full corpus. The order-2 model does
   not pay for its additional state transitions on these inputs.
2. The source-code subgroup is the one consistent exception: context is 3.37%
   smaller than fold, but still 3.11% larger than order-0 and 1.32% larger than
   order-1 token. This is a useful signal, not a container-level win.
3. Random and precompressed data lose by about 15.5%; the adaptive model has
   little exploitable structure while still paying the same custom framing.
4. **REJECTED: adopting this context codec in the archive format.** Keep the
   codec and benchmark as isolated tooling so the experiment can be remeasured,
   but do not add a tag, reader/writer branch, or format-version obligation for
   a negative aggregate result.

## Verification

- Context unit tests cover empty and structured round trips, deterministic
  encoding, order-2 improvement over order-1 on a synthetic stream, truncation,
  and corruption rejection.
- Release validation passed: 141 library/unit tests and 8 dictionary CLI tests.
- Default archive compatibility passed: 9 unique chunks,
  `9,128,841B` raw to `1,538,846B` archive, `aahl test` verified the archive,
  and the archive BLAKE3 remained
  `f368280196eef73b15cc9d763c6f029fc2583eeb5fccd975ee10bf8e14bfe2d0`.
