# AAHL v5 Milestone 1 - Phase 0 synthetic corpus set hashes

Corpus set path: `corpus_set` (deterministically rebuilt by `aahl corpus <dir>`; see
corpus.rs::build_corpus for the generators and seeds). Set was NOT regenerated in this
session - the pre-existing files match the builder exactly (7 files, sizes below).

Per-file BLAKE3 and SHA-256 (2026-09-22):

| file | bytes | BLAKE3 | SHA-256 |
|------|-------|--------|---------|
| corpus_set/empty/empty.bin | 0 | af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262 | E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855 |
| corpus_set/precompressed/in.bin | 1048576 | 2a744867886bc810b5e5da7dbbafbce1550c676f28cbf1c289064ec1383c9817 | 83BCF1BFF04ED7C45F53D410FC4C854F72A6EE5CE772E143E364E6F7C46C73C0 |
| corpus_set/precompressed/z.zst | 1048613 | 4a23e21af8d6b1b678ccd731812b1c9fd456e9dd4e45ddcd44b2bf1ca86c38f6 | 8EA92F40FA80FAE8A84446B30DC3C0DDD5C181167DBD7F7C23F4083086FEA3A5 |
| corpus_set/random/random.bin | 1048576 | 2a744867886bc810b5e5da7dbbafbce1550c676f28cbf1c289064ec1383c9817 | 83BCF1BFF04ED7C45F53D410FC4C854F72A6EE5CE772E143E364E6F7C46C73C0 |
| corpus_set/table/table.csv | 9128841 | 57b039558c7e8201ba1ed6caf4f2f1bd1dd6de2d92556be33cd3caacd3b2c672 | 360CA54FA086CD5B1955E4B7EECAEC5AF305FD1BB10F21C498DCA200380AC712 |
| corpus_set/text-large/large.txt | 1375929 | d9405db6ce60d86d49fe7ae6617ba190251b8ffcbc93d5f57a094259bdf2691e | 58691CB03543EB35DC70A9523B94765BDFB9FB1B7D454C898E280FC8492CAA01 |
| corpus_set/tiny/one.bin | 1 | 32684bfa28c0c84d6f210511aace0efc5171c7889148ba89208d5aa29705fa98 | 559AEAD08264D5795D3909718CDD05ABD49572E84FE55590EEF31A88A08FDFFD |

Set-level hash (byte concatenation of the 7 files in sorted path order):

- BLAKE3: 759fcc3d13aea0e1e086ce5070bbeafbfa1bd453fa3f357b35162365c7f82906
- SHA-256: C15CD844E663145465D112968757D4DE33B3A846E3EA4D7300107B431FE652E1

File count: 7. Total bytes: 13,650,536.

Note: random.bin and precompressed/in.bin share the same 1 MiB seeded PRNG blob
(PRNG seed 0xDEADBEEF for the synthetic portion), so their hashes are identical by
construction.
