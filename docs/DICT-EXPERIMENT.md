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

    aahl train <dict.aahld> <corpus> --chunk-size 4096 --max-rules 3840
        --min-benefit 1 --json
    aahl bench-dict <corpus> <heldout> --dict <dict.aahld> --lanes aahl-nodict,
        aahl-dict,zstd-nodict,zstd-dict --json

Lanes measure archive bytes on the held-out samples given: (1) AAHL with no dict,
(2) AAHL with the grammar dict, (3) zstd -19 baseline with no dict, (4) zstd with
its trained dictionary. The two tools are NOT directly comparable in isolation
(different chunking, folding, dedup) - what is compared is the DELTA each tool
gets from its own dict, plus the absolute held-out bytes.

Expected honest outcome (to be confirmed, see results below): AAHL dict gives the
biggest absolute margin on highly self-similar corpora where the grammar can fold
repeated piece derivations; zstd --train wins when the corpus is mostly unique
high-entropy bytes where a learned flat prefix table still lands a few percent.
The trailing run (V6 STEP 4 in ABLATION.md) records the actual numbers.

## Fail-closed contract (why `--dict` can be trusted)

1. extract/test on a dict-flagged archive WITHOUT --dict -> fail (bail, no
   output dir written).
2. extract/test on a plain archive WITH --dict -> fail (the archive does not
   declare the dict; passing a dict must not downgrade integrity).
3. test --dict verifies the archive in full, chunk by chunk, and fails if any
   record body or dict record is corrupt.
4. train on a corpus derives a rule table; create --dict writes the rule_hash
   record so extract can re-seed deterministically (rule_hash must match).

Every one of these branches is exercised in the CLI smoke (work/ds*/ft.log) and in
the dict unit tests under src (cargo test: each fail-closed path has a dedicated
testhole).

## Reproduce

    cargo build --release
    ./target/release/aahl train work/d.d.aahld work/src 2>&1 | <json>
    ./target/release/aahl create work/dc.aahl work/src --dict work/d.d.aahld
    ./target/release/aahl extract work/dc.aahl work/out --dict work/d.d.aahld
    ./target/release/aahl test   work/dc.aahl --dict work/d.d.aahld

Round-trip byte hashes are equal (SHA256 recorded in the smoke). Fail-closed
branches all exit non-zero and write nothing.

