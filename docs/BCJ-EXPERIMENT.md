# AAHL V6 STEP 6: x86 BCJ branch/call filter

## Status

This is an isolated experiment. The filter and `bench-bcj` harness are present
for reproducibility, but the filter is not selected by `aahl::compress_block`,
is not assigned an archive block tag, and does not change the archive
container.

## Hypothesis

A branch/call converter (BCJ) may make relative x86 branch operands more
compressible before the existing AAHL block codec. The experiment measures
complete block cost, including AAHL compressed payload and a fixed five-byte
replay marker per input file.

## Filter

The implementation is in `src/bcj.rs` and follows Michael F. Schinder, "Branch,
Call, and Jump: A New Compression Technique" (1994), as described in the LZMA
SDK `x86_Convert` documentation.

The scanner recognizes:

- `E8 rel32` (`CALL` near)
- `E9 rel32` (`JMP` near)
- `0F 80..0F 8F rel32` (near conditional jumps)

For each complete recognized operand, forward conversion adds the running
instruction pointer to the little-endian displacement; inverse conversion
subtracts it. Arithmetic is modulo 2^32. The tested bases are:

| lane | base | low 32 address bits |
|---|---:|---:|
| `bcj-base0` | `0x0000000000000000` | `0x00000000` |
| `bcj-x86` | `0x0000000000400000` | `0x00400000` |
| `bcj-x64` | `0x0000000140000000` | `0x40000000` |

A displacement of zero is a valid relative displacement, not an empty
instruction: at base zero, `E8 00 00 00 00` becomes `E8 05 00 00 00` because the
running instruction pointer is five bytes past the opcode. Empty inputs and
opcode-free inputs are unchanged.

The strict `forward` and `inverse` APIs return an error for a truncated branch
operand. The benchmark uses `forward_terminal` and `inverse_terminal`, which
leave an incomplete operand at a chunk boundary unchanged while transforming
all complete candidates before it. The scanner does not perform target-range
validation: a fixed-size reversible filter has no per-branch metadata channel
for recording skipped targets, and an unrecorded skip would make the inverse
ambiguous. This is a documented scope limitation, not an archive format rule.

## Harness and accounting

`src/bcjbench.rs` exposes:

```text
aahl bench-bcj [corpus_root] [options]
```

The reproducible run used the checked-in binary corpus at `corpus_bin` with
1 MiB chunks, three repetitions, and a deterministic maximum of eight files per
class. The sample is selected by sorting each class by manifest path and
choosing evenly spaced entries. The output is written to
`bench/bcj-runs.tsv` and `bench/bcj-summary.tsv`.

The lanes are:

- `raw-aahl`: existing `aahl::compress_block` over the original file chunks.
- `bcj-base0`, `bcj-x86`, `bcj-x64`: filtered chunks, inverse-checked, then
  compressed with the same AAHL block function.
- `xz-x86-9e`: optional `xz --x86 --lzma2=preset=9e -c` reference.
- `zstd-x86-19`: optional `zstd -19 --zstd=wlog=23 -c` reference.

The reference lanes are whole-stream compressor comparisons, not isolated BCJ
measurements, and are not used to claim a filter win. `7z` was not available in
the measured environment. Every BCJ lane adds exactly five replay bytes per
input file. Transform and compression output is checked for byte determinism
across repetitions, and every transformed chunk is inverse-checked before
compression.

The run selected 32 files totaling 13,659,607 bytes across 40 chunks. The
per-run TSV contains 448 rows (32 files times three raw runs, nine BCJ runs,
and two reference runs), and all rows have `ok=1`.

## Complete-size results

Values below sum compressed payloads and replay markers for the selected
sample. Deltas are versus the corresponding `raw-aahl` total and include the
five-byte-per-file accounting.

| class | files | raw | bcj-base0 | delta | bcj-x86 | delta | bcj-x64 | delta |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| archive | 8 | 6,621,321 | 6,622,122 | +801 (+0.0121%) | 6,622,117 | +796 (+0.0120%) | 6,622,260 | +939 (+0.0142%) |
| cold-image | 8 | 1,585,922 | 1,585,965 | +43 (+0.0027%) | 1,585,964 | +42 (+0.0026%) | 1,585,967 | +45 (+0.0028%) |
| executable | 8 | 1,326,527 | 1,322,997 | -3,530 (-0.2661%) | 1,332,485 | +5,958 (+0.4491%) | 1,328,304 | +1,777 (+0.1340%) |
| installer | 8 | 594,922 | 597,526 | +2,604 (+0.4377%) | 599,107 | +4,185 (+0.7035%) | 596,718 | +1,796 (+0.3019%) |
| **all selected** | **32** | **10,128,692** | **10,128,610** | **-82 (-0.0008%)** | **10,139,673** | **+10,981 (+0.1084%)** | **10,133,249** | **+4,557 (+0.0450%)** |

The base-zero lane's 82-byte aggregate improvement is not a useful adoption
margin: it is the result of a 3,530-byte executable win offset by regressions
in the other three classes, and the fixed replay markers consume 160 bytes of
the selected sample. The x86 and x64 bases are worse than base zero overall.

The architecture split is mixed. The installer sample contains two amd64, one
i386, and five unknown files; the scanner does not parse PE structure and can
therefore transform byte patterns in non-code portions. The executable sample
is the strongest positive signal for base zero, while archive and cold-image
results are effectively framing-level noise.

Reference totals, shown only for context, were:

| class | xz | zstd |
|---|---:|---:|
| archive | 3,384,792 (-48.8804%) | 3,377,493 (-48.9906%) |
| cold-image | 1,560,584 (-1.5977%) | 1,561,870 (-1.5166%) |
| executable | 824,752 (-37.8262%) | 922,254 (-30.4760%) |
| installer | 356,252 (-40.1179%) | 396,900 (-33.2854%) |

## Decision

- **REJECTED: archive adoption.** The best isolated lane wins only 0.0008% on
  the complete selected sample, regresses on three of four classes, and adds a
  mandatory replay marker. The result does not justify a new block tag,
  reader/writer branch, or format-version obligation.
- **KEPT: isolated capability and benchmark.** `src/bcj.rs` and `aahl bench-bcj`
  remain available for future experiments, but they remain outside
  `compress_block` and the shipping archive path.
- A future attempt should use a PE-aware or section-aware candidate policy and
  include a real target/skip representation before reconsidering format
  adoption.

## Verification

- BCJ unit tests cover hardcoded call/jump/Jcc vectors, all near-Jcc opcodes,
  empty and opcode-free inputs, strict truncation errors, terminal chunk
  adapters, zero displacement, scanner advancement, deterministic bases, and
  the x64 low-address-bit case.
- Benchmark tests cover deterministic sampling, PE architecture classification,
  and wildcard aggregation across architectures.
- The full 32-file benchmark completed with 448 rows and all `ok=1`.
- Default archive compatibility remains unchanged: the table corpus creates
  9,128,841 raw bytes into a 1,538,846-byte archive with the established BLAKE3
  `f368280196eef73b15cc9d763c6f029fc2583eeb5fccd975ee10bf8e14bfe2d0`.
- Final release validation and dictionary CLI validation are recorded in
  `docs/ABLATION.md`.
