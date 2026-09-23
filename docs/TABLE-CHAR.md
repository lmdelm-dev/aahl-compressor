# TABLE-CHAR: Phase B0 characterization of the table corpus

Source: `aahl char corpus_set/table/table.csv --max-rows 2000` (deterministic
strided sample; release build). The report is produced by the live
`table::characterize` path, not a static file -- regenerate with the same
command.

## Corpus summary

| metric        | value      |
|---------------|-----------:|
| total_bytes   | 9,128,841  |
| total_lines   | 120,004    |
| jsonish_lines | 60,000     |
| grid          | yes        |
| col_delim     | `,`        |
| n_cols        | 7          |
| grid_lines    | 120,001    |
| grid_pct      | 100.00%    |
| sample_rows   | 2,001      |

The corpus is a clean 7-column CSV grid (100% of lines match the dominant
delimiter/uniformity profile), plus exactly half the lines (- 60k) are
JSON-ish (`{` + `"`). This is the "table" class Phase B exists for: strict
row-major structure with per-row text cells.

## Per-column profile (strided sample of 2001 rows)

| col | fixed | width | cardinality | int_rows | numeric_pct | monotonic | entropy | delta_smaller | raw_profile | delta_profile |
|-----|-------|------:|------------:|---------:|------------:|:---------:|--------:|:-------------:|------------:|--------------:|
| 0   | no    | 2     | 2001        | 1000     | 50.0%       | no        | 3.6934  | no            | 17,637      | 0             |
| 1   | no    | 3     | 2001        | 0        | 0.0%        | no        | 4.6495  | no            | 34,042      | 0             |
| 2   | no    | 8     | 17          | 0        | 0.0%        | no        | 3.9294  | no            | 27,047      | 0             |
| 3   | no    | 8     | 865         | 1000     | 50.0%       | no        | 3.7250  | no            | 13,617      | 0             |
| 4   | no    | 9     | 1907        | 0        | 0.0%        | no        | 4.1332  | no            | 23,806      | 0             |
| 5   | no    | 9     | 193         | 0        | 0.0%        | no        | 3.3342  | no            | 15,534      | 0             |
| 6   | no    | 7     | 57          | 0        | 0.0%        | no        | 3.5958  | no            | 20,707      | 0             |

## Reading

- **Structure**: no fixed-width columns (variable-width renderings), entropy
  3.3-4.6 bits/byte across columns (near-uniform within each column), and a
  50/50 split of numeric rows in cols 0/3 (the JSON/CSV halves) -- the
  column-major T-stream *de-correlates* the numeric/alpha alternation that
  defeats the row-major grammar.
- **Delta is offered, measured, and declined at the profile level**: no
  column is monotonic, so `DMODE_INT` zigzag-diff would not shrink either
  50%-numeric column (delta_profile recorded as 0 = not smaller). The real
  create-time oracle (per chunk, real codec) confirms: `transforms_chosen`
  3 of 8 grids at 1 MiB, i.e. the *layout* change (column-major) wins even
  where the *delta* encoding does not.
- **Interpretation**: on this synthetic corpus the Phase B win comes from
  the column-major re-layout plus the grammar seeing homogeneous columns,
  not from delta/date modes. The per-chunk oracle still picked 3/9 chunks
  at the shipping default (table 1,674,390 -> 1,538,846 total), and the
  ablation shows grammar_tx <= grammar at every chunk size (Phase B
  decision record in ABLATION.md).