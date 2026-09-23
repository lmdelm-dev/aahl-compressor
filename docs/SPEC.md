# AAHL container format spec

This document is normative for the v4 format and the v2 STORE container.
Offsets are little-endian. All sizes are in bytes.

## 1. Top level

There are exactly two container kinds, selected by the magic at offset 0:

| Magic   | Kind        | Used when |
|---------|-------------|-----------|
| `AAHL`  | Compressed  | the grammar path wins |
| `AS`    | STORE       | the input would shrink no further (incompressible) |

Both share the same 36-byte footer (`AAHE` magic + 4 x u64), so extraction
starts from the footer and walks back.

### 1.1 Relative layout decisions

A create run first writes the compressed v3 layout fully, then computes the
STORE alternative cost and compares:

```
store_total = 6 (store magic+version+mode)
            + 8 (table_offset u64, replaces the v3 file-count u64)
            + raw_total
            + 36 (footer)
            + per entry (2 + len(path) + 8)
```

Entry widths use `path_len u16 | path | file_len u64`, matching the v2 STORE
table (blake3 excluded from this estimate, so the estimate is conservative in
the compressed container's favor). If `store_total < compressed_total` the
archive is trivially rewritten as a STORE container, so **an AAHL archive is
never larger than its inputs** (modulo the honest tables above).

## 2. Compressed container (`AAHL`)

```
offset  field                size  value
0       magic                4     "AAHL"
4       version              2     = 4
6       flags                2     FLAG_FOLD (0x0001) | FLAG_GLOBAL (0x0002)
8       chunk_size           4     u32 bytes per chunk (CLI default 1048576)
12      params               8     u64 packed params (see Â§2.1)
20      header_checksum      2     u16 = first 2 bytes of blake3(prefix[0..20])
22      ...DATA records...   var   tagged stream (see Â§3), each optionally
                                  followed by grammar-GC records
table_offset
        num_files            8     u64
        file table           var   (Â§4)
EOF-36  "AAHE"               4
EOF-32  table_offset         8     u64
EOF-24  table_len            8     u64
EOF-16  num_chunks           8     u64  (unique chunks encoded)
EOF-8   num_files            8     u64
EOF      (end)
```

`HEADER_LEN_V3 == 22`. The reader must reject any archive whose 20-byte
prefix does not reproduce `header_checksum` (catches header corruption before
any allocation is driven by `chunk_size`/`params`).

### 2.1 params (u64)

| bits    | field        | mask        | default |
|---------|--------------|-------------|---------|
| 0..8    | lag          | 0xFF        | 16      |
| 8..24   | gc_interval  | 0xFFFF      | 64      |
| 32..56  | max_rules    | 0xFF_FFFF   | 60000   |
| 56..64  | flags2       | 0xFF        | 0       |

`lag` is encoder-only (Â§DESIGN); the decoder never needs it. `max_rules` is a
decoder safety cap (defensive bound on model growth). Both `lag` and
`gc_interval` are sanity-checked against the negative sizes they would allow.sanity-checked against the negative sizes they would allow.

### 2.2 Persistent token model contexts (normative for v4)

'G' blocks entropy-code grammar tokens against the archive-wide persistent
model. In v4 the model allocates exactly `256 + HOT_RULES + RULE_CTX + 1 == 457`
contexts (HOT_RULES = 192, RULE_CTX = 8, both format constants):

| context range                        | symbols         | purpose                              |
|--------------------------------------|-----------------|--------------------------------------|
| 0 .. 256                             | literal bytes   | order-1 over raw/literal symbols     |
| 256 .. 256+HOT_RULES                 | rule ids        | exact per-rule context (hot window)  |
| 256+HOT_RULES .. 256+HOT_RULES+RULE_CTX | rule ids     | cold rules hashed into RULE_CTX buckets |
| 256+HOT_RULES+RULE_CTX (sentinel)    | chunk boundary  | reseeding at chunk starts            |

The context of a token is a pure function of (`symbol`, `mode`); it never
depends on the alphabet size `n`, so the encoder and decoder derive identical
contexts from the same rule table even after grow/shrink and GC renumbering.
For a rule id `s >= 256`: hot rows cover `s - 256 < HOT_RULES` exactly; cold
rows use `cold_ctx(s) = 256 + HOT_RULES + (((s - 256 - HOT_RULES) * 0x9E37_79B9) as usize) % RULE_CTX`.
The model grows/shrinks by repeating *last* contexts (t_mix-style) so rolling
alphabet changes are deterministic. Changing HOT_RULES or RULE_CTX is a format
break; archives must be created and extracted with the same constants.

### 2.3 Version history

| version | meaning                                        |
|---------|------------------------------------------------|
| 1..=3   | legacy compressed containers (RULE_CTX=8 only) |
| 4       | current: adds the exact hot-rule contexts above |
| >= 5    | rejected by this reader                        |

v4 readers open v1..=v4 and reject anything newer (the guard is exercised by
`container_tests::future_container_versions_are_rejected`). v3 archives remain
byte-compatible: a v4 reader extracts them with the legacy 8-bucket model.

## 3. Record stream

Between header and table, the stream is a sequence of tagged records. Every
record is `kind u8 | body_len u32 | body`:

| kind | name | body                                 |
|------|------|--------------------------------------|
| 0x01 | DATA | `blake3[32]` of the raw chunk, `unpacked_len u32`, then the packed block |
| 0x02 | GC   | `flags u8 (0)`, `num_survivors u32`, `survivor u32*` (grammar GC) |

Reader structural bounds:

- `max_chunks <= (table_offset - 22) / 5` (each DATA needs >= 5 bytes of record
  header at minimum).
- `max_files  <= (file_len - table_offset) / 18` (each table entry needs >= 18
  bytes at minimum: 2 + path + 8 + 8 header words).
- Before slicing a body, `body_len` is bounded by
  `table_offset.saturating_sub(stream_position)` so a corrupt length cannot
  drive unbounded allocations.
- DATA records check `body_len >= 36` **before** computing the packed length as
  `body_len - 32 - 4`, so underflow is impossible.

### 3.1 The packed block

Within a DATA body, the packed block is produced by `grammar::PersistentGrammar`
or the stateless `aahl::compress_block` fallback. Two families exist:

**Persistent grammar ('G' blocks)** â€” layout:

```
[0xA0, b'G'] num_new_rules u32 (l u16, r u16)* num_tokens u32 blob_len u32 blob
```

Each G block appends rules to the archive's single persistent grammar and
entropy-codes tokens against the persistent model (v4 context layout in 2.2). Rules referenced are always
defined in the same block (forward references are impossible), so the decoder
reconstructs exactly the encoder's table.

**Stateless (`aahl::compress_block`, tag byte at index 0):**

- `[0xA0, b'F']` recursive pair-folding + canonical Huffman (fold lexicographic).
- `[0xA0, b'S']` spectral blocks â€” FFT period detection; stores one exact period
  (verified in time domain) instead of a grammar.
- `[0xA0, b'B']` binning â€” structurally similar pieces grouped into bins so
  each bin's grammar sees clean repetition.
- `[0xA0, b'A']` adaptive order-0 arithmetic after fold.
- `[0xA0, b'D']` order-1 over folded grammar tokens.
- `[0xA0, b'C']` order-1 over raw bytes.
- raw-with-6-byte-header for near-max-entropy bytes.

`decompress_block` dispatches on the tag; `unpacked_len` from the DATA record
plus the block's own token count gives the exact expected length, and the blake3
hash is checked after decode.

## 4. File table (compressed)

```
per entry:
  path_len u16 | path bytes | file_len u64 | num_refs u64 | ref u32*
```

`refs` index into the unique-chunk stream (0-based, first-seen order).
Duplicate chunks and duplicate files are deduplicated at create time; identical
files share refs. `file_len` may exceed the sum of chunk lengths is not a thing
â€” extraction writes `min(ref.len, remaining)` per ref in order, so the total
reassembles the original byte sequence.

## 5. STORE container (`AS`, version 2)

```
offset  field          size  value
0       magic          2     "AS"
2       version        2     = 2
4       mode           2     = 0 (single raw container)
6       num_files      8     u64
14      table          var   per entry: path_len u16 | path | file_len u64 | blake3[32]
...     payload        var   raw bytes, files in table order (no interleaving)
EOF-36  "AAHE" footer  ...
EOF-32  table_offset   8     u64
EOF-24  table_len      8     u64
EOF-16  num_chunks     8     u64 (always 0 for STORE)
EOF-8   num_files      8     u64
```

STORE mode is fully alternative â€” there are no chunk records, so extraction
reads a file's `file_len` bytes straight from the payload and verifies the
blake3 hash. Reader structural bound: `num_files <= (file_len - table_offset)/42`
(2 + path + 8 + 32 per entry minimum) protects the table walk.

### 5.1 Why version 2 (STORE v1 -> v2 break)

STORE v1 stored no payload checksums. A corrupted `path_len` would shift the
payload read for every later file **silently** â€” sizes and names looked right,
bytes were wrong. v2 writes a per-file blake3 in the table and verifies each
payload on extract; any mismatch is an error, never a wrong file.

## 6. Footer

Same for both containers: `AAHE` + `table_offset u64` + `table_len u64` +
`num_chunks u64` + `num_files u64`. Extraction opens the archive, seeks to
`EOF-36`, reads the footer, and `table_offset` must be `< EOF-36` with a
`table_len` that lands exactly at EOF-36. The store-vs-compressed mode is
re-detected from the first two bytes (which brutally splits on `AA`/`AS`).

## 5.2 Version 5: measured table transform (Phase B)

v5 is a *container* change, not a codec change: the grammar/fold/token
streams are byte-identical to v4. The only delta is that a chunk MAY be
emitted as a table transform stream (T) wrapped in a table header before
the normal grammar block, when an oracle measurement says the wrapped form
is strictly smaller than the plain one.

Decision rule (deterministic, stateless, measured - never heuristic):

```
a = compress_block(raw).len()
b = wrapper_len(meta) + compress_block(t_stream).len()
emit transform iff b < a
```

`compress_block` (aahl.rs) and `wrap_block` (table.rs) are both pure
functions of their inputs, so the gate is a pure function of chunk bytes:
determinism (j1/j4/j8 byte-identical) is preserved by construction and
enforced by the jobs-scaling harness.

### 5.2.1 Header

```
offset  field                size  value
0       magic                4     "AAHL"
4       version              2     = 5
6       flags                2     FLAG_FOLD (0x0001) | FLAG_GLOBAL (0x0002)
                                  | FLAG_TABLE (0x0008) when any chunk was
                                  transformed
8       chunk_size           4     u32 bytes per chunk (CLI default 1048576)
12      params               8     u64 packed params (see 2.1)
20      header_checksum      2     u16 = first 2 bytes of blake3(prefix[0..20])
```

`create --no-table` forces `version = 4` and writes a v4 archive
byte-for-byte (the v4 lane in every bench row). Normal create writes v5.

### 5.2.2 Table chunk records (FLAG_TABLE)

When `version >= 5` and the oracle selected the transform for a chunk:

```
DATA record ([RECORD_DATA] kind | body_len u32 | hash(32) | unpacked_len u32)
  body  = table_wrapper | t_stream_grammar_block

table_wrapper:
  byte 0-1   tag       0xA0 0x54  ("T", never produced by any codec tag)
  meta      core + per-column descriptors (see 5.2.3)
  t_stream  compressed inner block (grammar block of the column-major
            transform stream; decompresses to exactly t_len bytes)
```

The reader does NOT need FLAG_TABLE to decide the decode path: the first two
bytes of every unpacked body are checked. Codec tags are all `[0xA0, letter]`
(A/B/C/D/G/R/S); `T` (0x54) is never a tag, so the 2-byte check is
unambiguous: `A0 T` => un-wrap, verify + invert; anything else => direct
decode. The flag only short-circuits the check on chunk records where no
transform was used.

### 5.2.3 Table wrapper metadata

```
byte 0      version      u8 = 1
byte 1      kind         u8 = 0 (grid)
byte 2      row_delim    u8
byte 3      col_delim    u8
byte 4-5    n_cols       u16
byte 6-9    n_rows       u32 (grid rows in the transform)
byte 10-13  t_len        u32 (exact decompressed length of the inner block)
byte 14-17  head_len     u32 (bytes of partial head, stored verbatim)
byte 18-21  tail_len     u32 (bytes of partial tail, stored verbatim)
then per-column descriptors in order:
  flags    u8   bit0 = fixed-width; bits1..2 = dmode
  width    u32  only when bit0 set (fixed-width column byte width)

core = 22 bytes (1+1+1+1+2+4+4+4+4); wrapper_len = 2 + core + descriptors.
verification on decode: blake3(entry hash) + exact len, same as v4; the
inner block decompresses to t_len and `table::inverse(raw, meta, len)`
recovers the original chunk bytes exactly.
```

### 5.2.4 Transform stream T (column-major)

T is a valid AAHL *codec payload* - a byte stream understood by the same
grammar path - so the wrapper is transparent to the grammar/decode stages:
`compressed(T)` is a normal G block. Layout per column:

- variable columns: `[uvarint row-count][uvarint bytes-per-row ...]` then
  the raw cell bytes, row-major. Fixed-width columns store no length stream.
- `dmode` = 0 raw cells; 1 = delta (zigzag-diff of consecutive numeric rows);
  2 = day-number date; 3 reserved. Delta is used ONLY when the measured
  delta profile is smaller than the raw profile for that column (per-column
  oracle, so the gate is honest).

### 5.2.5 Why version 5 (v4 -> v5 break)

v4 left table data on the table: the row-major CSV/JSON grid defeats the
grammar (repeated numeric cell boundaries, no run reuse across columns).
v5 keeps the codec intact and adds a measured column-major re-layout before
it - the same class of win a BWT/MTF gives LZ, but deterministic and
opt-in per chunk. The `--no-table` v4 lane stays byte-identical for
comparability and for archives that must not change.

## 7. Compatibility matrix

| Format | Produced by | Read by |
|--------|-------------|---------|
| v5 compressed (version=5, FLAG_TABLE 0x0008) | current `create` (default, table transform) | current `extract` |
| v4 compressed (version=4) | current `create --no-table`; all pre-Phase-B archives | current `extract` |
| v3 compressed (version=3) | never emitted now | current `extract` (read test v3) |
| STORE v2 (`AS` version=2) | current `create` (incompressible) | current `extract` |
| v2/older feature flags | never emitted now | rejected (checksum/version guard) |