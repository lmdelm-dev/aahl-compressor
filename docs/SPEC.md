# AAHL container format spec

This document is normative for the v3 format and the v2 STORE container.
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
4       version              2     = 3
6       flags                2     FLAG_FOLD (0x0001) | FLAG_GLOBAL (0x0002)
8       chunk_size           4     u32 bytes per chunk (CLI default 65536)
12      params               8     u64 packed params (see §2.1)
20      header_checksum      2     u16 = first 2 bytes of blake3(prefix[0..20])
22      ...DATA records...   var   tagged stream (see §3), each optionally
                                  followed by grammar-GC records
table_offset
        num_files            8     u64
        file table           var   (§4)
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

`lag` is encoder-only (§DESIGN); the decoder never needs it. `max_rules` is a
decoder safety cap (defensive bound on model growth). Both `lag` and
`gc_interval` are sanity-checked against the negative sizes they would allow.

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

**Persistent grammar ('G' blocks)** — layout:

```
[0xA0, b'G'] num_new_rules u32 (l u16, r u16)* num_tokens u32 blob_len u32 blob
```

Each G block appends rules to the archive's single persistent grammar and
entropy-codes tokens against the persistent model. Rules referenced are always
defined in the same block (forward references are impossible), so the decoder
reconstructs exactly the encoder's table.

**Stateless (`aahl::compress_block`, tag byte at index 0):**

- `[0xA0, b'F']` recursive pair-folding + canonical Huffman (fold lexicographic).
- `[0xA0, b'S']` spectral blocks — FFT period detection; stores one exact period
  (verified in time domain) instead of a grammar.
- `[0xA0, b'B']` binning — structurally similar pieces grouped into bins so
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
— extraction writes `min(ref.len, remaining)` per ref in order, so the total
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

STORE mode is fully alternative — there are no chunk records, so extraction
reads a file's `file_len` bytes straight from the payload and verifies the
blake3 hash. Reader structural bound: `num_files <= (file_len - table_offset)/42`
(2 + path + 8 + 32 per entry minimum) protects the table walk.

### 5.1 Why version 2 (STORE v1 -> v2 break)

STORE v1 stored no payload checksums. A corrupted `path_len` would shift the
payload read for every later file **silently** — sizes and names looked right,
bytes were wrong. v2 writes a per-file blake3 in the table and verifies each
payload on extract; any mismatch is an error, never a wrong file.

## 6. Footer

Same for both containers: `AAHE` + `table_offset u64` + `table_len u64` +
`num_chunks u64` + `num_files u64`. Extraction opens the archive, seeks to
`EOF-36`, reads the footer, and `table_offset` must be `< EOF-36` with a
`table_len` that lands exactly at EOF-36. The store-vs-compressed mode is
re-detected from the first two bytes (which brutally splits on `AA`/`AS`).

## 7. Compatibility matrix

| Format | Produced by | Read by |
|--------|-------------|---------|
| v3 compressed (version=3) | current `create` | current `extract` |
| STORE v2 (`AS` version=2) | current `create` (incompressible) | current `extract` |
| v2/older feature flags | never emitted now | rejected (checksum/version guard) |