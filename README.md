# AAHL - grammar-folding archive

AAHL is a from-scratch lossless compressor. It compresses by building a
persistent recursive pair-folding grammar across an archive's chunks, then
entropy-coding the resulting symbol stream with hand-rolled adaptive arithmetic
and order-1 models. No LZ77/Huffman-per-block like zip, no PPM like rar, no
zstd/lzma crates — every codec here is written from scratch against the format
spec (no third-party compression dependency).

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
- **v5 table transform**: real CSV/DB payloads are measured, organized into
  grids, and packed first (oracle-gated, never guessed), so aahl beats dedicated
  stream codecs on table-like streams. `--no-table` writes a byte-identical v4
  archive for apples-to-apples comparison.
- **Grammar-seeded dictionaries**: `aahl train` builds a small `.aahld` from
  sample files; `create --dict` seeds the grammar with it. The dict is a pure
  seed (rule hashes are validated, so a wrong/garbage dict fails closed, never
  silently corrupts). Dictionaries are 2-3 orders of magnitude smaller than
  zstd's.
- **v4 token model**: exact order-1 contexts for the 192 hottest grammar rules
  (format constants in docs/SPEC.md) plus deterministic cold hashing; v3
  archives stay readable, and older readers reject v4 containers cleanly.
- **No worse than raw**: an archive is never larger than the input. Chunks that
  resist all codecs fall back to STORE; whole incompressible inputs use the
  STORE container.
- **Honest measurement**: every codec experiment is isolated behind a bench
  subcommand and judged by benchmark, never a vibe. See "Experimental codecs".

## Building

```
cargo build --release
```

This is a cargo workspace: `aahl` (CLI), `aahl-gui` (desktop front-end), and
`aahl-shellext` (Windows Explorer extension) share one version.

## Usage

```
aahl create [OPTIONS] <ARCHIVE> <INPUTS>...
aahl list   <ARCHIVE>
aahl test   <ARCHIVE> [--json]
aahl extract <ARCHIVE> <OUT_DIR>
aahl train  <OUT_DICT> <SAMPLES>...        # grammar dictionary (.aahld)
aahl corpus <SET_DIR> [--source DIR]
aahl bench  <SET_DIR> [--tsv bench_results.tsv]
```

- `create` defaults to 1 MiB chunks (`--chunk-size`), grammar snapshot lag 16
  (`--lag`), GC every 64 chunks (`--gc-interval`), serial discovery
  (`--jobs 1`). Duplicate chunks are deduplicated; identical files share refs.
  `--dict <file>` seeds the grammar from a trained dictionary; `--no-table`
  disables the v5 table transform (byte-identical v4 output).
- `extract` recreates the recorded relative paths under `OUT_DIR`. Paths are
  validated so no entry may escape the output directory (absolute paths, `..`,
  and drive prefixes are rejected).
- `train` builds `OUT_DICT` from sample files/directories (`--chunk-size`,
  `--max-rules`, `--min-benefit`). The dict is read-only at archive time: it
  only seeds the first chunk's grammar.
- `corpus` builds a deterministic synthetic corpus set (text, CSV/JSON, random
  bytes, empty/tiny, precompressed). With `--source DIR` it also copies real
  `.rs`/`.exe`/`.dll`/installer files from a source tree.
- `bench` compresses every corpus set with AAHL and (when installed) `zip6`
  (7-Zip deflate -mx=6), `7z9` (LZMA2 -mx=9), `xz -9`, `zstd -19`, and
  `rar` (WinRAR -m5), and writes a TSV of sizes and create/extract times.
- `test` decodes every chunk in index order (through the persistent
  grammar, applying interleaved GC records) and verifies hashes, lengths,
  and per-file ref/length consistency. STORE containers are already fully
  verified at open. Exits non-zero (and lists errors) on any corruption.
- `--json` on `create`, `list`, and `test` prints a machine-readable
  summary (files/chunks/bytes, index, or ok/errors).

See `docs/BENCHMARKS.md` for the harness methodology and `docs/SPEC.md` for
the full subcommand/format surface.

## Ranking vs reference tools

From `aahl bench corpus_set` (v0.2.1, ratio = archive bytes / raw bytes):

| corpus | aahl-v5 | xz -9 | zstd -19 | rar -m5 | notes |
|---|---|---|---|---|---|
| text | **0.236** (1st) | 0.282 | 0.285 | 0.302 | aahl beats all stream codecs |
| table CSV | 0.169 (3rd) | **0.153** (1st) | 0.165 | 0.184 | aahl beats rar |
| precompressed | 1.000 (tie) | 1.000 | 1.000 | 1.000 | no blow-up (zip/7z expand) |
| random | 1.000 (tie) | 1.000 | 1.000 | 1.000 | no blow-up |

On table-like streams aahl-v5 (with the measured table transform) lands between
`zstd -19` and `rar -m5` (0.169 vs 0.165 and 0.184), within ~10% of `xz -9`.
Dictionaries have their own measured comparison in `bench/dict-summary.tsv` —
aahl-dict wins relative gains on every corpus with a dict 2-3 orders smaller
than zstd's.

## GUI

`aahl-gui` is a native desktop front-end (eframe/egui + rfd) that drives
the `aahl` CLI as its engine:

```
cargo run -p aahl-gui          # development
cargo build --release -p aahl-gui
```

It locates the engine in order: `AAHL_BIN` env var, then `aahl` on PATH,
then a sibling `aahl` executable next to the GUI. It uses the wgpu
backend (DX12/Vulkan) and includes a `Demo` toggle backed by a fake
engine so the UI is usable without the CLI.

## Windows shell extension

`shellext` registers Explorer context-menu commands (add to archive, extract
here / to folder, test) for the current user, without elevation:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-context-menu.ps1
```

From a release build the zip contains `aahl.exe`, `aahl-gui.exe`,
`aahl_shellext.dll` and the two `.ps1` scripts. The menu drives the real
engine: `aahl-gui --create <files...>` for new archives (save dialog), the
CLI for extract/test. Uninstall with `.\uninstall-context-menu.ps1`; see
`docs/SHELLEXT.md` for the registry layout, discovery order, and verification.

## Experimental codecs

Everything below is isolated behind a bench subcommand — none of it changes the
archive format bytes. Each has a decision record in `docs/ABLATION.md` and a
full write-up in `docs/*-EXPERIMENT.md`:

| experiment | subcommand | verdict |
|---|---|---|
| tANS/FSE entropy backend | `aahl ans-bench` | REJECTED for the container (token-mode frequency tables cost ~20x the win); literal mode kept isolated |
| grammar-seeded dictionary | `aahl train` / `bench-dict` | KEPT as opt-in `--dict` ability |
| context-conditioned codec | `aahl bench-ctx` | REJECTED (loses to order-0 by ~1-13%); kept as tooling |
| x86 BCJ branch/call filter | `aahl bench-bcj` | REJECTED (best lane -0.0008%); kept as tooling |
| parallelism | `aahl bench-jobs` | KEPT determinism; no speedup (oracle-bound) |

The default path is byte-identical across all of them (the table-corpus archive
still reproduces blake3 `f368280196eef73b15cc9d763c6f029fc2583eeb5fccd975ee10bf8e14bfe2d0`).

## Format

Two containers share the footer:

- **Compressed** (`AAHL` v3): header + params + recorded stream of DATA chunks
  and interleaved grammar-GC records, then the file table. v4/v5 add the hot-rule
  model and the table transform; v3 archives stay readable.
- **STORE** (`AS` v2): raw concatenated payload with a per-file blake3 in the
  table — used when the input is incompressible.

See `docs/SPEC.md` for the byte-level layout and `docs/DESIGN.md` for the
grammar, GC, lag, and parallelism rationale.

## Testing

```
cargo test                # full suite (unit + container + fuzz)
cargo test --release      # suite in release mode (overflow checks off)
cargo test -p aahl-gui    # GUI backend tests
cargo test -p aahl-shellext   # shell-extension COM tests (exercises the real DLL)
```

The suite includes byte-identity determinism checks (serial vs parallel,
lag/gc sweeps), corruption/truncation rejection fuzz (payload and store mode),
round-trip tests across every block mode, and archive-vs-raw bounds checks
(store is honest about overhead).

## License

BSD-3-Clause. Reference codecs (7-Zip, xz, zstd) are only invoked by `bench` if
present on the machine; nothing is bundled.