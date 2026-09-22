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
  (`MIN_PAIR_COUNT >= 4` minimum pair agreement, capped by `MAX_MERGES=512`,
  symbol ceiling `MAX_SYMS=4096`).
- `reuse_pass` runs a byte-level trie over rule expansions (`MAX_PHRASE=256`),
  greedily replacing raw bytes with existing rule ids; `invent` creates new
  rules for recurring pairs and appends them to the table.
- The model is an adaptive arithmetic coder whose alphabet grows as rules are
  invented and shrinks when GC renumbers survivors. `PersistentTokenModel`
  (arith.rs) conditions each token on its predecessor — order-1 over grammar
  tokens with bucketed rule contexts — and a blending variant adds order-2
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
size is rewritten as the STORE container (§SPEC 5). This is the honesty
mechanism: **compression never makes things bigger**.

## 3. Snapshot lag (encoder-only)

`lag` (default 16; `--lag`) means chunk `r` is *discovered* against the grammar
state as of `r - lag` chunks earlier. The decoder is completely lag-agnostic:
every G block carries the rule definitions it references, so lag only changes
how far the encoder looks back.

Why lag exists at all: it bounds the grammar snapshot a parallel worker has to
clone (§4). With `lag=1` the encoder uses the live grammar directly (the legacy
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
3. The decoder never sees a fork prefix — it only ever reconstructs the live
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

The body is bounded defensively on read (§SPEC 3) and `apply_gc` rebuilds the
pair index/trie so no stale rule id can survive a remap.

## 6. Integrity model

- Every unique chunk is stored once with its blake3 `hash` in the DATA record
  header; extraction decodes the block and verifies length + hash before use.
  A corrupted block errors out — it cannot produce silently wrong file bytes.
- The v3 header carries a blake3-derived u16 checksum, so `chunk_size`/`params`
  corruption is caught before it drives allocation.
- STORE v2 carries per-file blake3 in the table (§SPEC 5.1) precisely because
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

## 9. Bench methodology

`bench` compresses each corpus set five ways when reference tools exist —
`aahl`, `zip6` (7z deflate -mx=6), `7z9` (7z LZMA2 -mx=9), `xz -9` (concatenated
stream), `zstd -19` (concatenated stream) — and verifies every round trip by
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
  on record boundaries, but the store-fallback rewrite (§SPEC 1.1) happens
  after full compression, so volume sizing must negotiate after create.
