# AAHL design notes

High-level rationale for the pieces that make AAHL different from "a shell
around zstd": the persistent folding grammar, snapshot lag, grammar GC, and
the parallelism that must not change a single output byte.

## 1. What AAHL is not

- No LZ77-derived dictionary coding (no zstd/lzma crates, no match-finder over
  a window).
- No per-input Huffman tables in the common path (stateless mode has canonical
  Huffman, but the persistent path entropy-codes with adaptive arithmetic).
- No external compression runtime; `bench` shells out to reference tools only
  as a yardstick.

The compressor's core idea: **grammar, not dictionary**. A recursive
pair-folding pass turns a byte stream into a small set of grammar rules, then
the resulting token stream is entropy-coded. Repetition is captured as *rule
reuse*, not as offset/length back-references.

## 2. The persistent grammar (`grammar.rs::PersistentGrammar`)

One grammar is owned by the whole archive. Chunks are fed in archive order on
both create and extract, so the encoder and decoder walk identical rule tables.

- `fold_stream` (`aahl.rs::fold*`) performs recursive pair merging
  (`MIN_PAIR_COUNT >= 4` minimum pair agreement, capped by `MAX_MERGES=1024` (deep folding; see section 8),
  symbol ceiling `MAX_SYMS=4096`).
- `reuse_pass` runs a byte-level trie over rule expansions (`MAX_PHRASE=256`),
  greedily replacing raw bytes with existing rule ids; `invent` creates new
  rules for recurring pairs and appends them to the table.
- The model is an adaptive arithmetic coder whose alphabet grows as rules are
  invented and shrinks when GC renumbers survivors. `PersistentTokenModel`
  (arith.rs) conditions each token on its predecessor â€” order-1 over grammar
  tokens with bucketed rule contexts â€” and a blending variant adds order-2
  evidence once it is statistically justified (`O2_MIN_TOTAL` evidence gate).

### Why a *persistent* grammar beats per-block stateless

Per-block folding restarts from an empty table every chunk, so long-range
repetition (the same identifier, same struct layout, same JSON key across
hundreds of blocks) costs full definition overhead each time. A persistent
grammar pays the definition cost once and then spends 1-for-1 tokens, which is
also why a grammar-GC exists at all: dead rules would otherwise accumulate.

### Failure to beat raw = STORE fallback

Every grammar block must beat `raw.len() + 6`; when it does not, the chunk is
not merged. `bytes with entropy >= 7.9 bits` short-circuit to STORE raw blocks
without paying fold cost. A whole archive whose compressed cost exceeds its raw
size is rewritten as the STORE container (Â§SPEC 5). This is the honesty
mechanism: **compression never makes things bigger**.

## 3. Snapshot lag (encoder-only)

`lag` (default 16; `--lag`) means chunk `r` is *discovered* against the grammar
state as of `r - lag` chunks earlier. The decoder is completely lag-agnostic:
every G block carries the rule definitions it references, so lag only changes
how far the encoder looks back.

Why lag exists at all: it bounds the grammar snapshot a parallel worker has to
clone (Â§4). With `lag=1` the encoder uses the live grammar directly (the legacy
serial path); the recompute-safety property still holds but the snapshot reuse
window is one chunk.

`snapshot_len_at(r)` reads a ring of rules lengths taken right after each
emitted chunk. When rules grow between the snapshot and the commit, the fork
candidate is discarded and the chunk is re-discovered serially so output bytes
are exactly what the serial path would emit.

## 4. Deterministic parallelism

Contracts:

1. **Discovery is a pure function of (frozen snapshot, raw chunk).**
2. **Commit is strictly serial**, on the main thread, in chunk order.
3. The decoder never sees a fork prefix â€” it only ever reconstructs the live
   table from transmitted definitions.

`emit_parallel` runs a bounded `rayon` pool where worker `r` clones the rules
prefix frozen at `r - lag` and returns a `ForkCandidate` plus the snapshot
length and GC epoch. The committer accepts a candidate only if the live
snapshot still matches; otherwise it re-discovers serially (identical bytes,
pure recompute). A commit that applied a GC between dispatch and commit causes
the epoch counter to reject the stale candidate, never to emit it.

Outcome, enforced by tests: `create -j1` and `create -jN` are byte-identical,
and chunk sizes are orthogonal to output.

## 5. Grammar collection (GC)

Rules carry a monotonic `seq` of their last use. On every `gc_interval` chunks
(default 64; GC record kind 0x02), the encoder computes the live rule set
(used at or after the window start) and, when it is strictly smaller than the
table, emits a GC record: `flags u8 | num_survivors u32 | survivor u32*` in
ascending pre-GC order.

The decoder applies exactly the same remap (survivors renumbered 0..k'-1), so:

- the model alphabet shrinks and token costs drop,
- `gc_epoch` increments, giving the parallel committer a second, unambiguous
  way to detect "this candidate's frozen prefix belongs to a pre-remap table",
  even when snapshot lengths coincidentally match.

The body is bounded defensively on read (Â§SPEC 3) and `apply_gc` rebuilds the
pair index/trie so no stale rule id can survive a remap.

## 6. Integrity model

- Every unique chunk is stored once with its blake3 `hash` in the DATA record
  header; extraction decodes the block and verifies length + hash before use.
  A corrupted block errors out â€” it cannot produce silently wrong file bytes.
- The v3 header carries a blake3-derived u16 checksum, so `chunk_size`/`params`
  corruption is caught before it drives allocation.
- STORE v2 carries per-file blake3 in the table (Â§SPEC 5.1) precisely because
  v1 proved that metadata shifts (`path_len`) corrupt payload slicing silently.
- The footer's `table_offset`/`table_len` must land exactly; a truncated or
  trailing-junk archive is rejected.

## 7. Block mode selection (stateless path)

`compress_block` tries, cheapest first, keeping the smallest:

1. fast raw STORE for near-max-entropy bytes (entropy >= 7.9);
2. fold + canonical Huffman (lexicographic) as the baseline;
3. spectral (FFT period detection, exact-period verified) for periodic data;
4. binning (cluster structurally similar pieces) for mixed-layering inputs;
5. adaptive order-0 arithmetic over the folded grammar;
6. order-1 over folded tokens; order-1 over raw bytes;
7. raw STORE safety net if nothing shrank the chunk.

Each candidate is charged against the shared fold result (`fold_stream` once,
all codecs consume the same tokens/rules) so the O(n*m) fold cost is paid once
per chunk. `decompress_block` dispatches on the leading tag byte.

## 8. Why these parameters?

- `--chunk-size` default 65536: fold is O(n*m) per chunk; 64KiB balances
  grammar reach against per-chunk cost. CLI range 4KiB..1MiB.
- `--lag` default 16: enough look-back for reuse to stabilize without making
  every parallel worker clone a huge prefix.
- `--gc-interval` default 64: rule liveness computed cheaply, GC record
  overhead amortized, and dead-rule decay fast enough on mixed corpora.
- `--jobs` default 1: serial by default (fastest for small inputs, simplest
  to reason about); any `-j` shrinks create time on large inputs with provably
  identical bytes.

- `RULE_CTX` (8 rule-context buckets in the token model): ablation sweep
  (docs/ABLATION.md) showed 16 buckets win only at the 4096 chunk size and
  lose at 262144; RULE_CTX is a format-level constant, so changing it would
  break v3 read-compat for existing archives. Kept at 8.

## 8.1 The v4 token model (exact hot-rule contexts)

The persistent model originally hashed every rule id into 8 order-1 buckets
(RULE_CTX=8, ~128 rules/bucket at 1k rules, near-flat priors). v4 splits rule
contexts into three regimes (format constants, see SPEC 2.2):

- **Literal symbols (ids < 256)**: order-1 as before.
- **Hot window (rule ids in the first HOT_RULES=192 ids)**: one *exact*
  context per rule. After GC renumbering the lowest ids are the longest-lived
  cross-chunk anchors, so their follower distributions win the most from
  exactness. 192 is < 1 model MiB of table space and covers the persistent
  core of typical text/code corpora.
- **Cold tail (ids >= 256 + 192)**: hashed into 8 buckets exactly as v3 (so
  the cold regime is provably no worse than the legacy model).

The context is a pure function of (symbol, mode) - never of alphabet size -
which is what keeps encoder/decoder contexts identical across grow/shrink and
GC renumbering. The v4 mapping is opt-in at grammar construction (`new_v4`);
`new()`/`with_gc()` remain v3, and a v3 archive extracts byte-identically on a
v4 reader.

Measured contribution (ablation, HOT_RULES=192, chunk 1 MiB): text-large is
stateless-equivalent (its grammar is rarely paid for in full), while table
gains ~0.5% (1,682,478 -> 1,673,892 payload). The grid position across
HOT_RULES in [64, 256] is flat because table's rule->rule followers are
near-uniform there is no "hot few" to exploit; the model's exactness matters
for rule-dominant streams where consecutive chunks reuse the same phrase rules
(covered by `tests_v4_model`, `v4_archive_beats_v3_when_hot_rules_dominate`).

### Default chunk size: 1 MiB

`create` shipped 64 KiB chunks. The ablation sweep (chunk 64 KiB -> 1 MiB at
MAX_MERGES=1024, v4) is monotonic for both flagship corpora and takes text-large
from 0.300 to 0.236 ratio at defaults - outpassing xz/7z9/zstd - while table
improves 1,979,079 -> 1,673,892. 1 MiB is the top of the allowed range (fold is
O(n*m)); the default is set there because the evidence is monotone up to it and
there is no smaller chunk that beats zstd on the text lane.
## 9. Bench methodology

`bench` compresses each corpus set five ways when reference tools exist â€”
`aahl`, `zip6` (7z deflate -mx=6), `7z9` (7z LZMA2 -mx=9), `xz -9` (concatenated
stream), `zstd -19` (concatenated stream) â€” and verifies every round trip by
blake3 hash comparison over the extracted/concat stream before recording a row.
Rows are fair on shape (file-by-file integrity for AAHL and 7z; for stream
codecs, valid compressed stream with the same bytes, hashes checked). A
`FAIL` marker is written for any tool that fails its own echo test; it is never
silently excluded from the TSV.

See `corpus.rs::build_corpus` for the deterministic synthetic set (text, CSV,
JSON-ish, random, empty/tiny, precompressed) plus optional `--source` copies of
real source/binaries/installer files.

## 10. Out of scope (evaluated, rejected)

- **Append to an existing archive**: the persistent grammar is append-only by
  design, but the footer's `table_len`/`num_chunks` and the file table live at
  EOF. Appending a new grammar epoch against a *different* snapshot then
  rewriting the footer is possible only if the chunk stream is re-emitted or
  versioned; neither is worth the format break on top of the GC-remap rule
  space. The fix for "add files later" today is re-create (dedup keeps it
  cheap).
- **True streaming compress (arbitrary backpressure)**: `create` already
  buffers unique pieces so discovery can run ahead of commit; emitting them
  incrementally would require the record stream to be fully reproducible from
  the grammar snapshot, which the parallel path already guarantees per chunk.
  Streaming *decompress* is a real win and cheap (decode chunk r, forget r-1
  except refs), but it is a plumbing task, not a format change.
- **Encryption**: intentionally separate. The archive is checksummed but not
  sealed; compress-then-encrypt any tool (age/gpg) preserves blake3 integrity.
- **Multivolume / split archives**: nothing in the format prevents splitting
  on record boundaries, but the store-fallback rewrite (Â§SPEC 1.1) happens
  after full compression, so volume sizing must negotiate after create.

## 11. Phase B: measured table transform (the why behind v5)

### 11.1 Problem

The Phase A matrix established exactly one categorical AAHL weakness on the
synthetic set: table (0.1834 vs 7z9's 0.1526, 5th/6 -- the only loss lane on
text-shaped data). The grammar is a token/fold model over a byte stream; a
row-major CSV/JSON grid gives it repeated numeric cell boundaries, no run
reuse across columns, and a per-row delimit-repeat tax. The data has
structure, but row-major layout makes the structure invisible to order-1
models.

### 11.2 Rejected alternatives (measured in B0)

- **No transform; accept the loss**: the target is a text-class packer; a
  known 18% gap on the flagship text-table corpus is a design miss, not a
  tuning miss.
- **External dictionary/preprocessor (BWT, MTF, PPM-style context)**: adds
  a codec to the pipeline; violates the "no LZ/LZMA/DEFLATE" invariant the
  project states; and would have to be paid on every chunk.
- **Heuristic grid detection (uniform delimiters only)**: fragile. JSON-ish
  lines and prose with commas both pass a naive uniform-count check; a
  heuristic that fires wrongly must store the raw chunk anyway, so the only
  honest version is to *measure* before deciding.

### 11.3 Chosen design

Keep the codec byte-identical and add a *measured* pre-pass in front of it:

1. **Detector** (`table::candidate`): delimiter grid over `DELIMS
   [',', ';', '\t', '|']`, `MIN_GRID_BYTES 64`, `MAX_COLS 4096`,
   `MAX_EDGE_BYTES 65536`; partial head/tail lines stored verbatim. It is a
   cheap, broad filter, NOT the gate.
2. **Oracle** (`table::prepare`): the actual gate. Compress the raw chunk
   and the T stream with the real codec (`aahl::compress_block`) and compare
   sizes; the wrapper is always charged. Stateless + deterministic, so the
   v5 archive is a pure function of chunk bytes.
3. **Transform** (`table::wrap_block` / `table::inverse`): column-major T
   stream - `[lengths][values]` per column, uvarint row-order lengths for
   variable columns, no length stream for fixed-width columns; `DMODE_INT`
   i64 zigzag-diff and `DMODE_DATE` day-number selected only when their
   measured profile is smaller.
4. **Sink**: `create --table-stats FILE` records `chunks_total, grids_found,
   transforms_chosen, raw_oracle_bytes, t_side_bytes, oracle_us,
   delta_columns, grid_rows` - measurement diagnostics only, never an input
   to the byte stream (same contract as ParStats).

### 11.4 Why the oracle cannot be beaten by the detector

Prose with commas scores `grids_found=1, transforms_chosen=0` on the real
codec (measured): the oracle declines because `wrapper_len + G(T) >= G(raw)`.
The delta columns and grid characterization (see TABLE-CHAR report) explain
*why* in absolute numbers; the oracle explains *whether* on the exact bytes.
The gate is `b < a`, strict, so a transform never makes a chunk larger.

### 11.5 Interaction with determinism

`prepare` is called per piece before the emit loop; `plans: Vec<Option<TransformPlan>>`
is computed once from pure functions and threaded through both `emit_serial`
and `emit_parallel`. The parallel path still discovers ahead on the *raw*
streams (fork_candidate unchanged); only the commit arm looks up the plan.
Consequence: v5 archives are byte-identical across `--jobs 1/4/8` (verified:
`phase-b-jobs-scaling.tsv`, blake3 `f3682801...` for table at all three job
counts).

### 11.6 v3/v4 read seams

`open_index` accepts versions 1..=5. Chunk records are decoded by their own
first two bytes, not by the header version: a v4 archive inside a v5 reader
or a v5 archive read by the same binary is handled uniformly. `--no-table`
locks the header to version 4, so the v4 lane is an *archival* guarantee,
not just a test hook.

### 11.7 Phase C boundary

Phase B ships the container change and its measurement harness. Phase C
(snapshot histories on the model, or any future format work) is explicitly
out of scope for this milestone; `FLAG_SNAPSHOT` (0x0004) remains reserved.
