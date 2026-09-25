# DICT-EXPERIMENT.md

Grammar-seeded dictionary (V6 STEP 4) - design, ABI, and an honest against
measurement on held-out data.

## Why a dictionary at all

The V6 grammar (chunks_kept = 422 rules from the CYOA train lane) is shipped inside
every archive header, so each archive carries its own full rule table. That is
correct and round-trips, but the definitions are transmitted once per archive: a
hundred archives over the same corpus (logs, firmware, structured dumps) retransmit
the same chunk table every time. A dictionary makes the rules a named, reusable
artifact - train once against a corpus, pass --dict to create/extract/test - so
rules are defined exactly once and every archive thereafter stores only the
archived piece data against that shared table.

Standard zstd provides the same idea with --train/-o dictionary files (and 
--patch-from for delta archives). The honest question for STEP 4 is not "does a
rule table help", it is: does AAHL train benefit from being a grammar (a DAG of
piece-to-piece derivations with a benefit-trimmed rule set) rather than a flat
(chunk -> list of equal-weighted prefixes) table, when measured against a zstd
trained dictionary on the same corpus?

## What was built and the seam that holds it

Components (all in src): train writes d.aahld (dict::Dict), create --dict reads it
and emits a v6 dict-flagged archive, extract/test --dict re-seed the same rule
table and verify. The record format:

    kind  u8   = 0x03 (RECORD_DICT)
    body  u32  = 36  (rule_hash[32] + n_rules u32)
    then  32B  rule_hash (blake3 over the rule table)
    then  4B   n_rules

Load path (SRC main.rs, RECORD_DICT): requires body == 36, rule_hash[0..4] parses
as n_rules u32 with n <= MAX_RULES, and the dict rule table is past the FLAG. The
seam fails closed: extract/test without --dict on a dict-flagged archive bail, and
extract/test WITH --dict on a plain archive bail. The archive stores chunk data
(chunk -> PC n-gram piece rules) and its chunk record uses the streaming grammar;
the dictionary only re-uses the SAME grammar declared in-flight. That is why the
round-trip is byte-exact - data never re-derives grammar, it is a suffix list.

Benefit trim during train: pieces whose clamped benefit (avg occurrence, times
trimmed benefit rule) is below min-benefit are dropped (trimmed_by_benefit in the
train JSON). Chunk-size/count are bounded by grammar caps (MAX_RULES etc.) so a
pathological corpus cannot balloon the rule table.

## The measurement: grammar-seeded vs zstd trained

Benchmark (release build):

    cargo build --release
    ./target/release/aahl bench-dict <corpus_set> --chunk-size 65536 --runs 1

Lanes measure archive bytes on a held-out 40% tail of each domain's stream
given: (1) AAHL with no dict, (2) AAHL with the grammar dict, (3) zstd -19
baseline with no dict, (4) zstd with its trained dictionary. Training uses the
first ~60% of the stream, rounded down to whole chunks, so the two tools are
measured on the SAME bytes (aahl chunk 65536; zstd blocks derived from the
train length to keep >= 24 samples, see src/dictbench.rs). The two tools are
NOT directly comparable in isolation (different chunking, folding, dedup) -
what is compared is the DELTA each tool gets from its own dict, plus the
absolute held-out bytes and the dict file size.

Results (held-out archive bytes / ratio, dict file size):

    domain  train     held-out   aahl-nodict  aahl-dict            zstd-nodict  zstd-dict
    prose   786432    589497     172861      146817  (dict 3233B)  172820       165079  (dict 714KB)
    table   5439488   3689353    616051      598703  (dict 6537B)  468090       464243  (dict 1.4MB)
    code    262144    212146     75123       68560   (dict 6781B)  48169        44319   (dict 164KB)

Small-file prefixes from the held-out tail (ratio at 1KiB/4KiB/16KiB/64KiB/
100KiB) are in bench/dict-small.tsv; the dict margin grows as files shrink.

Verdict: the AAHL dictionary is KEPT as opt-in capability, but as a *tie to
inferior absolute ratio against zstd*, not a win. What it genuinely delivers:
(1) the biggest relative gain vs its own no-dict baseline on every domain
(prose -4.4pp: 0.2932 -> 0.2491, code -3.1pp: 0.3541 -> 0.3232, table -0.5pp:
0.1670 -> 0.1623; zstd's own dict gains are 1.3pp / 1.8pp / 0.1pp), and (2) a
dictionary 2-3 orders of magnitude smaller (3-7 KB vs 164 KB - 1.4 MB). On the
prose domain AAHL's dict even passes zstd's absolute ratio once the stream
grows (aahl-dict 0.2491 vs zstd-dict 0.2800). But zstd keeps a decisive
absolute edge on table and code, which is where its entropy coder wins. The
dict seam costs nothing on the default no-dict path (blake3 of a table archive
is unchanged, f3682801..) and is therefore safe to ship as opt-in tooling.

## Fail-closed contract (why `--dict` can be trusted)

1. extract/test on a dict-flagged archive WITHOUT --dict -> fail (bail, no
   output dir written).
2. extract/test on a plain archive WITH --dict -> fail (the archive does not
   declare the dict; passing a dict must not downgrade integrity).
3. test --dict verifies the archive in full, chunk by chunk, and fails if any
   record body or dict record is corrupt.
4. train on a corpus derives a rule table; create --dict writes the rule_hash
   record so extract can re-seed deterministically (rule_hash must match).

Every one of these branches is exercised by tests/dict_cli.rs (8 integration
tests: roundtrip, no-dict, wrong-dict, plain+dict, corrupt-dict, no-dict
determinism, dict determinism, dict determinism across -j) and the dict unit
tests under src/dict.rs; each fail-closed path has a dedicated testhole and
exits non-zero.

## Reproduce

    cargo build --release
    ./target/release/aahl train work/d.d.aahld work/src --json
    ./target/release/aahl create work/dc.aahl work/src --dict work/d.d.aahld
    ./target/release/aahl extract work/dc.aahl work/out --dict work/d.d.aahld
    ./target/release/aahl test   work/dc.aahl --dict work/d.d.aahld
    cargo test --release --test dict_cli     # the fail-closed seam
    ./target/release/aahl bench-dict corpus_set --chunk-size 65536 --runs 1

Round-trip byte hashes are equal. Fail-closed branches all exit non-zero and
write nothing. Measured numbers: bench/dict-summary.tsv, bench/dict-small.tsv.

