# AAHL - grammar-folding archive

AAHL is a from-scratch lossless compressor (no LZ77/Huffman-per-block like zip,
no PPM like rar, no zstd/lzma crates). It compresses by building a persistent
recursive pair-folding grammar across an archive's chunks, then entropy-coding
the resulting symbol stream with hand-rolled adaptive arithmetic, order-1, and
order-1+2 blend models.

```

bytes -> chunk -> fold (recursive pair merging) -> symbols
       -> per-symbol persistent model (order-1 / order-1+2 blend)
       -> arithmetic bitstream
```

## Highlights

- **Deterministic**: the same input always produces byte-identical archives,
  across runs, chunk sizes, and `--jobs` (parallel discovery is pure; commits
  stay serial).
- **Lossless with integrity**: every chunk carries a blake3 hash; extraction
  verifies decode order, lengths, and hashes. STORE containers carry a
  per-file blake3. Corruption anywhere is detected at open or extract, never a
  silent wrong file.
- **Verbatim-faithful**: `-j1` and any `-jN` are byte-identical; the decoder is
  a pure function of archive bytes (snapshot lag is an encoder-only concept).
- **No worse than raw**: an archive is never larger than the input. Chunks that
  resist all codecs fall back to STORE; whole incompressible inputs use the
  STORE container.

## Building

```
cargo build --release
```

## Usage

```
aahl create [OPTIONS] <ARCHIVE> <INPUTS>...
aahl list   <ARCHIVE>
aahl extract <ARCHIVE> <OUT_DIR>
aahl corpus <SET_DIR> [--source DIR]
aahl bench  <SET_DIR> [--tsv bench_results.tsv]
aahl blocksize <INPUT> [--sweep]
```

- `create` defaults to 64 KiB chunks (`--chunk-size`), grammar snapshot lag 16
  (`--lag`), GC every 64 chunks (`--gc-interval`), serial discovery
  (`--jobs 1`). Duplicate chunks are deduplicated; identical files share refs.
- `extract` recreates the recorded relative paths under `OUT_DIR`. Paths are
  validated so no entry may escape the output directory (absolute paths, `..`,
  and drive prefixes are rejected).
- `corpus` builds a deterministic synthetic corpus set (text, CSV/JSON, random
  bytes, empty/tiny, precompressed). With `--source DIR` it also copies real
  `.rs`/`.exe`/`.dll`/installer files from a source tree.
- `bench` compresses every corpus set with AAHL and (when installed) `zip6`
  (7-Zip deflate -mx=6), `7z9` (LZMA2 -mx=9), `xz -9`, `zstd -19`, and
  `rar` (WinRAR -m5), and writes a TSV of sizes and create/extract times.

## Format

Two containers share the footer:

- **Compressed** (`AAHL` v3): header + params + recorded stream of DATA chunks
  and interleaved grammar-GC records, then the file table.
- **STORE** (`AS` v2): raw concatenated payload with a per-file blake3 in the
  table — used when the input is incompressible.

See `docs/SPEC.md` for the byte-level layout and `docs/DESIGN.md` for the
grammar, GC, lag, and parallelism rationale.

## Testing

```
cargo test                # full suite (unit + container + fuzz)
cargo test --release      # suite in release mode (overflow checks off)
```

The suite includes byte-identity determinism checks (serial vs parallel,
lag/gc sweeps), corruption/truncation rejection fuzz (payload and store mode),
round-trip tests across every block mode, and archive-vs-raw bounds checks
(store is honest about overhead).

## License

BSD-3-Clause. Reference codecs (7-Zip, xz, zstd) are only invoked by `bench` if
present on the machine; nothing is bundled.