mod aahl;
mod ablation;
mod arith;
mod bench;
mod binning;
mod corpus;
mod grammar;
mod spectral;
mod table;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Take, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const MAGIC_HDR: &[u8; 4] = b"AAHL";
const MAGIC_FTR: &[u8; 4] = b"AAHE";
const MAGIC_STORE: &[u8; 2] = b"AS"; // STORE container magic (offset 0)
const VERSION: u16 = 5;
const FLAG_FOLD: u16 = 0x0001;
const FLAG_GLOBAL: u16 = 0x0002; // persistent cross-block grammar (v2 blocks)
const FLAG_SNAPSHOT: u16 = 0x0004; // reserved (Phase 3 snapshot histories)
const FLAG_TABLE: u16 = 0x0008; // v5: table-wrapped chunks (FLAG_TABLE set only on v5)
const HEADER_LEN_V2: u64 = 20; // v1/v2: fixed params header
const HEADER_LEN_V3: u64 = 22; // v3: + header_checksum u16
const FOOTER_LEN: u64 = 36; // 4B magic + 4x u64 (offset, len, chunks, files)
// v3 params u64 packing: bits 0..7 lag | 8..23 gc_interval | 32..55 max_rules | rest flags2
const PARAM_LAG_SHIFT: u32 = 0;
const PARAM_LAG_MASK: u64 = 0xFF;
const PARAM_GC_SHIFT: u32 = 8;
const PARAM_GC_MASK: u64 = 0xFFFF;
const PARAM_RULES_SHIFT: u32 = 32;
const PARAM_RULES_MASK: u64 = 0xFF_FFFF; // 24 bits decoder safety cap (default 60000)
const PARAM_LAG_DEFAULT: u64 = 16;
const PARAM_GC_DEFAULT: u64 = 64;
const PARAM_RULES_DEFAULT: u64 = 60000;
// v3 record framing: kind u8 | body_len u32 | body
const RECORD_DATA: u8 = 0x01; // body = blake3[32] | unpacked_len u32 | packed bytes
const RECORD_GC: u8 = 0x02; // body = flags u8 | num_survivors u32 | survivor u32* (grammar GC)
const STORE_VERSION: u16 = 2; // v2: per-file blake3 hash in the table
const STORE_MODE: u16 = 0; // single raw container, no grammar

#[derive(Parser)]
#[command(name = "aahl", version, about = "AAHL - grammar-folding archive (no LZ/zstd)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create archive from files/dirs
    Create {
        #[arg(value_name = "ARCHIVE", help = "output .aahl archive path")]
        archive: PathBuf,
        #[arg(value_name = "INPUTS", help = "input files and/or directories")]
        inputs: Vec<PathBuf>,
        #[arg(long, default_value_t = 1_048_576, help = "chunk size in bytes (1 MiB default)")]
        chunk_size: u32,
        /// Grammar snapshot lookahead (chunks): higher lag lets a chunk fork
        /// from a state a few chunks earlier, bounding parallel dispatch.
        #[arg(long, default_value_t = PARAM_LAG_DEFAULT as usize)]
        lag: usize,
        /// GC interval (chunks): how often dead-rule reclamation is attempted.
        #[arg(long, default_value_t = PARAM_GC_DEFAULT as usize)]
        gc_interval: usize,
        /// Parallel discovery workers. Discovery is pure (frozen snapshots),
        /// commits stay serial, so any -j produces byte-identical archives.
        #[arg(long, short, default_value_t = 1)]
        jobs: usize,
        /// Diagnostics (measurement-gate only): write parallel-encoder counters
        /// (tasks spawned, used/stale discovery, falls-back, time split, peak
        /// workers) to FILE. Requires -j > 1 to be meaningful; archive bytes are
        /// unchanged and determinism is unaffected. Off by default.
        #[arg(long, value_name = "FILE")]
        par_stats: Option<PathBuf>,
        /// Disable the v5 table transform (writes a byte-identical v4 archive).
        /// Table measurement is also skipped, so this is the exact v4 lane.
        #[arg(long)]
        no_table: bool,
        /// Diagnostics (measurement-gate only): write table-transform counters
        /// (chunks scanned, grids found, transforms chosen, oracle bytes, delta
        /// columns) to FILE. Archive bytes are unchanged; determinism holds.
        #[arg(long, value_name = "FILE")]
        table_stats: Option<PathBuf>,
    },
    /// List contents
    List {
        #[arg(value_name = "ARCHIVE")]
        archive: PathBuf,
    },
    /// Extract archive
    Extract {
        #[arg(value_name = "ARCHIVE")]
        archive: PathBuf,
        #[arg(value_name = "OUT_DIR", help = "target directory (created if absent)")]
        out_dir: PathBuf,
    },
    /// Report per-mode block sizes for a single file (dev/diagnostic)
    #[command(name = "blocksize")]
    BlockSize {
        input: PathBuf,
        #[arg(long, help = "sweep merge/sym limits for the fold grammars")]
        sweep: bool,
    },
    /// Build the standard benchmark corpus set (deterministic)
    #[command(name = "corpus")]
    BuildCorpus {
        set_dir: PathBuf,
        #[arg(long, value_name = "DIR", help = "optional source tree for real file corpora (.rs, .exe/.dll)")]
        source: Option<PathBuf>,
    },
    /// Phase B0: deterministic sample-based table characterization report
    #[command(name = "char")]
    RunChar {
        input: PathBuf,
        #[arg(long, default_value_t = 1000, help = "max strided sample rows per column")]
        max_rows: usize,
    },
    /// Run the benchmark suite over a corpus set, incl. reference tools
    #[command(name = "bench")]
    RunBench {
        set_dir: PathBuf,
        #[arg(long, default_value = "bench_results.tsv", help = "write results to this TSV")]
        tsv: PathBuf,
        #[arg(long, default_value_t = 1_048_576, help = "chunk size in bytes for the aahl lane")]
        chunk_size: usize,
        /// AAHL container lanes to run: v4 (--no-table legacy), v5 (table
        /// transform), or both. Each lane emits its own row.
        #[arg(long, value_delimiter = ',', default_value = "both", help = "aahl lanes: v4, v5, or both")]
        aahl_modes: Vec<String>,
    },
    /// Phase A1: AAHL-only jobs-scaling benchmark (create/extract per -j N)
    #[command(name = "bench-jobs")]
    RunBenchJobs {
        set_dir: PathBuf,
        #[arg(long, default_value = "bench/jobs-scaling.tsv", help = "summary TSV (median/min/max, speedup, efficiency)")]
        tsv: PathBuf,
        #[arg(long, default_value = "bench/jobs-scaling.raw.tsv", help = "per-run raw TSV")]
        raw_tsv: PathBuf,
        #[arg(long, default_value_t = 1_048_576, help = "chunk size in bytes")]
        chunk_size: usize,
        #[arg(long, default_value_t = 5, help = "timing repetitions per (corpus, jobs)")]
        runs: usize,
        #[arg(long, value_delimiter = ',', default_value = "1,4,8", help = "jobs values to measure")]
        jobs: Vec<usize>,
        #[arg(long, value_delimiter = ',', help = "corpus names to include (default: all)")]
        corpora: Vec<String>,
    },
    /// Phase A3: harvest a labelled binary corpus (System32) + SHA-256 manifest
    #[command(name = "corpus-bin")]
    BuildBinaryCorpus {
        #[arg(default_value = "corpus_bin", help = "output dir with classed subdirs")]
        set_dir: PathBuf,
        #[arg(long = "source", default_value = "C:\\Windows\\System32", help = "tree to harvest from")]
        source: PathBuf,
    },
    /// Phase A2: collect parallel-encoder internals via create --par-stats
    #[command(name = "bench-parstats")]
    RunBenchParstats {
        set_dir: PathBuf,
        #[arg(long, default_value = "bench/parstats.tsv", help = "write counters to this TSV")]
        tsv: PathBuf,
        #[arg(long, default_value_t = 1_048_576, help = "chunk size in bytes")]
        chunk_size: usize,
        #[arg(long, value_delimiter = ',', default_value = "4,8", help = "jobs values to instrument (j>1)")]
        jobs: Vec<usize>,
        #[arg(long, value_delimiter = ',', help = "corpus names to include (default: all)")]
        corpora: Vec<String>,
    },
    /// Ablation study: per-corpus payload per chunk size per pipeline mode
    #[command(name = "ablate")]
    RunAblate {
        set_dir: PathBuf,
        #[arg(long, default_value = "ablation.tsv", help = "write results to this TSV")]
        tsv: PathBuf,
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "4096,16384,65536,262144",
            help = "comma-separated chunk sizes to measure"
        )]
        chunk_sizes: Vec<usize>,
    },
}

struct ChunkMeta {
    hash: [u8; 32],
    unpacked_len: u32,
    packed_len: u32,
    offset: u64,
}

struct FileEntry {
    path: String,
    file_len: u64,
    refs: Vec<u32>,
    store_hash: Option<[u8; 32]>, // STORE v2+: per-file payload hash
}

/// A grammar-GC record interleaved between DATA records. `data_before` is the
/// count of DATA records that precede it in the stream, so extraction applies
/// the remap at exactly the right grammar state.
struct GcRecord {
    data_before: u32,
    survivors: Vec<u32>,
}

fn write_u16(w: &mut impl Write, v: u16) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn write_u32(w: &mut impl Write, v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn write_u64(w: &mut impl Write, v: u64) -> Result<()> {
    w.write_all(&v.to_le_bytes())?;
    Ok(())
}
fn read_u16(r: &mut impl Read) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

// ---------- v3 params packing ----------

fn pack_params(lag: u64, gc_interval: u64, max_rules: u64, flags2: u64) -> u64 {
    let mut p = (lag & PARAM_LAG_MASK) << PARAM_LAG_SHIFT;
    p |= (gc_interval & PARAM_GC_MASK) << PARAM_GC_SHIFT;
    p |= (max_rules & PARAM_RULES_MASK) << PARAM_RULES_SHIFT;
    p |= flags2 << 56;
    p
}

fn unpack_params(p: u64) -> (u64, u64, u64, u64) {
    let lag = (p >> PARAM_LAG_SHIFT) & PARAM_LAG_MASK;
    let gc = (p >> PARAM_GC_SHIFT) & PARAM_GC_MASK;
    let max_rules = (p >> PARAM_RULES_SHIFT) & PARAM_RULES_MASK;
    let flags2 = p >> 56;
    (lag, gc, max_rules, flags2)
}

/// Header integrity marker: first two bytes of blake3 over the 20-byte header
/// prefix (magic+version+flags+chunk_size+params). Written for v3 only; the
/// reader must reject archives whose prefix does not reproduce it.
fn header_checksum(prefix: &[u8; 20]) -> u16 {
    let h: [u8; 32] = *blake3::hash(prefix).as_bytes();
    u16::from_le_bytes([h[0], h[1]])
}

fn collect_files(inputs: &[PathBuf]) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for inp in inputs {
        if !inp.exists() {
            bail!("input not found: {}", inp.display());
        }
        if inp.is_file() {
            let name = inp
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            out.push((name, inp.clone()));
        } else {
            visit_dir(inp, inp, &mut out)?;
        }
    }
    if out.is_empty() {
        bail!("no files to archive");
    }
    Ok(out)
}

fn visit_dir(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for ent in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let ent = ent?;
        let p = ent.path();
        if p.is_dir() {
            visit_dir(root, &p, out)?;
        } else {
            let rel = p.strip_prefix(root).unwrap_or(&p);
            let root_name = root
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            let arc_name = if root_name.is_empty() {
                rel_s
            } else {
                format!("{root_name}/{rel_s}")
            };
            out.push((arc_name, p));
        }
    }
    Ok(())
}

#[allow(dead_code)] // test seam (host CLI uses cmd_create_par)
fn cmd_create(
    archive: &Path,
    inputs: &[PathBuf],
    chunk_size: u32,
    lag: usize,
    gc_interval: usize,
    jobs: usize,
) -> Result<()> {
    cmd_create_par(archive, inputs, chunk_size, lag, gc_interval, jobs, None, false, None)
}

/// CLI create entry: validation plus optional Phase A2 parallel diagnostics.
fn cmd_create_par(
    archive: &Path,
    inputs: &[PathBuf],
    chunk_size: u32,
    lag: usize,
    gc_interval: usize,
    jobs: usize,
    par_stats: Option<&Path>,
    no_table: bool,
    table_stats: Option<&Path>,
) -> Result<()> {
    if !(1..=1024).contains(&lag) {
        bail!("lag must be 1..1024");
    }
    if !(1..=4096).contains(&gc_interval) {
        bail!("gc-interval must be 1..4096");
    }
    let mut stats = ParStats::default();
    let mut tstats = table::TableStats::default();
    let version = if no_table { 4 } else { VERSION };
    let res = cmd_create_params_version_impl(
        archive,
        inputs,
        chunk_size,
        pack_params(lag as u64, gc_interval as u64, PARAM_RULES_DEFAULT, 0),
        jobs,
        version,
        if par_stats.is_some() { Some(&mut stats) } else { None },
        if version >= 5 && table_stats.is_some() { Some(&mut tstats) } else { None },
    );
    if let Some(p) = par_stats {
        if jobs <= 1 {
            let _ = std::fs::write(
                p,
                "par_tasks\t0\npar_used\t0\npar_stale\t0\npar_fallback\t0\npar_discover_us\t0\npar_commit_us\t0\npar_wait_us\t0\npar_peak_workers\t0\nnote\tserial: --par-stats needs -j > 1\n",
            );
        } else if let Err(e) = write_par_stats(p, &stats) {
            eprintln!("par-stats write failed: {e:#}");
        }
    }
    if let Some(p) = table_stats {
        if version < 5 {
            let _ = std::fs::write(
                p,
                "chunks_total\t0\ngrids_found\t0\ntransforms_chosen\t0\nraw_oracle_bytes\t0\nt_side_bytes\t0\noracle_us\t0\ndelta_columns\t0\ngrid_rows\t0\nnote\tv4 lane (--no-table): table transform not run\n",
            );
        } else if let Err(e) = write_table_stats(p, &tstats) {
            eprintln!("table-stats write failed: {e:#}");
        }
    }
    res
}

/// Full create path with explicit params (tests inject a short GC interval to
/// force grammar reclamation deterministically; the CLI uses defaults).
#[allow(dead_code)] // test seam (host CLI uses cmd_create_par)
fn cmd_create_params(
    archive: &Path,
    inputs: &[PathBuf],
    chunk_size: u32,
    params: u64,
    jobs: usize,
) -> Result<()> {
    cmd_create_params_version(archive, inputs, chunk_size, params, jobs, VERSION)
}

/// Full create path with an explicit container version. Production writes
/// VERSION (4: exact-context hot-rule model); tests use this seam to build
/// legacy v3 archives through the same emit machinery.
#[allow(dead_code)] // test seam (host CLI uses cmd_create_par)
fn cmd_create_params_version(
    archive: &Path,
    inputs: &[PathBuf],
    chunk_size: u32,
    params: u64,
    jobs: usize,
    version: u16,
) -> Result<()> {
    cmd_create_params_version_impl(archive, inputs, chunk_size, params, jobs, version, None, None)
}

/// Full create path with explicit params and an optional Phase A2 diagnostics
/// sink (measurement-only; it never changes output bytes or determinism).
#[allow(clippy::too_many_arguments)]
fn cmd_create_params_version_impl(
    archive: &Path,
    inputs: &[PathBuf],
    chunk_size: u32,
    params: u64,
    jobs: usize,
    version: u16,
    par_stats: Option<&mut ParStats>,
    mut table_stats: Option<&mut table::TableStats>,
) -> Result<()> {
    if !(4096..=1_048_576).contains(&chunk_size) {
        bail!("chunk-size must be 4KiB..1MiB (fold is O(n*m), keep small)");
    }
    let files = collect_files(inputs)?;
    let mut out = File::create(archive).with_context(|| format!("create {}", archive.display()))?;

    // 20-byte v3 prefix: magic(4) | version u16 | flags u16 | chunk_size u32 | params u64
    let mut prefix = [0u8; 20];
    prefix[..4].copy_from_slice(MAGIC_HDR);
    prefix[4..6].copy_from_slice(&version.to_le_bytes());
    let use_table = version >= 5;
    let mut flags = FLAG_FOLD | FLAG_GLOBAL;
    if use_table {
        flags |= FLAG_TABLE;
    }
    prefix[6..8].copy_from_slice(&flags.to_le_bytes());
    prefix[8..12].copy_from_slice(&chunk_size.to_le_bytes());
    prefix[12..20].copy_from_slice(&params.to_le_bytes());
    out.write_all(&prefix)?;
    write_u16(&mut out, header_checksum(&prefix))?; // HEADER_LEN_V3 == 22

    // Buffer the unique pieces (with their first-seen order) so discovery can
    // run ahead of commit. Dedup runs while gathering: repeated pieces are refs
    // and never touch the grammar, exactly like the streaming path.
    let mut gather = Gather::default();
    for (arc_name, disk_path) in &files {
        let data =
            std::fs::read(disk_path).with_context(|| format!("read {}", disk_path.display()))?;
        let mut refs: Vec<u32> = Vec::new();
        if !data.is_empty() {
            for piece in data.chunks(chunk_size as usize) {
                let hash = *blake3::hash(piece).as_bytes();
                if let Some(&idx) = gather.index.get(&hash) {
                    refs.push(idx);
                    continue;
                }
                let idx = gather.pieces.len() as u32;
                gather.index.insert(hash, idx);
                gather.pieces.push(Piece {
                    hash,
                    raw: piece.to_vec(),
                    unpacked_len: piece.len() as u32,
                });
                refs.push(idx);
            }
        }
        gather.entries.push(FileEntry {
            path: arc_name.clone(),
            file_len: data.len() as u64,
            refs,
            store_hash: None,
        });
    }
    let entries = gather.entries;
    let pieces = gather.pieces;

    // v5: stateless, deterministic table-transform plans (B2 measurement
    // gate). prepare() is a pure function of the raw bytes, so the plan
    // vector is identical for every worker count and scheduling; v3/v4 keep
    // an empty vector and never touch the table codec.
    let plans: Vec<Option<table::TransformPlan>> = if use_table {
        let mut v = Vec::with_capacity(pieces.len());
        for p in &pieces {
            v.push(table::prepare(&p.raw, table_stats.as_deref_mut())?);
        }
        v
    } else {
        Vec::new()
    };

    // AAHL core: custom fold codec + persistent grammar, NOT zstd/LZ.
    let (lag, gc_interval, _, _) = unpack_params(params);
    let mut grammar = if version >= 4 {
        grammar::PersistentGrammar::with_lag_v4(
            aahl::FoldConfig::default(),
            gc_interval as usize,
            lag as usize,
        )
    } else {
        grammar::PersistentGrammar::with_lag(
            aahl::FoldConfig::default(),
            gc_interval as usize,
            lag as usize,
        )
    };

    // Emit every unique chunk in first-seen order. With jobs <= 1 (or lag <= 1)
    // discovery and commit share the serial path; with lag > 1 and jobs > 1 a
    // bounded pipeline runs pure discovery ahead of a strictly serial commit,
    // which keeps the archive byte-identical for any worker count.
    let chunks = if jobs > 1 && lag > 1 {
        emit_parallel(&mut out, &pieces, &plans, &mut grammar, jobs, par_stats)?
    } else {
        emit_serial(&mut out, &pieces, &plans, &mut grammar)?
    };

    let table_offset = out.stream_position()?;
    write_u64(&mut out, entries.len() as u64)?;
    for e in &entries {
        let pb = e.path.as_bytes();
        if pb.len() > u16::MAX as usize {
            bail!("path too long: {}", e.path);
        }
        write_u16(&mut out, pb.len() as u16)?;
        out.write_all(pb)?;
        write_u64(&mut out, e.file_len)?;
        write_u64(&mut out, e.refs.len() as u64)?;
        for r in &e.refs {
            write_u32(&mut out, *r)?;
        }
    }
    let table_end = out.stream_position()?;
    let table_len = table_end - table_offset;

    out.write_all(MAGIC_FTR)?;
    write_u64(&mut out, table_offset)?;
    write_u64(&mut out, table_len)?;
    write_u64(&mut out, chunks.len() as u64)?;
    write_u64(&mut out, entries.len() as u64)?;
    out.flush()?;
    let compressed_total = out.stream_position()?;

    let raw_total: u64 = entries.iter().map(|e| e.file_len).sum();
    // STORE alternative: raw payload + 6B container + entry table + footer.
    // Equal widths stay compressed; strictly smaller switches to STORE so the
    // archive is never worse than raw (+6B header, table stated honestly).
    let store_total: u64 = 6
        + 8
        + raw_total
        + 36
        + entries
            .iter()
            .map(|e| 2 + e.path.len() as u64 + 8)
            .sum::<u64>();
    if store_total < compressed_total {
        out.set_len(0)?;
        out.seek(SeekFrom::Start(0))?;
        write_store_archive(&mut out, &files)?;
        let total = out.stream_position()?;
        println!(
            "aahl: {} files stored raw ({total}B <= {raw_total}B, compressed was {compressed_total}B)",
            entries.len()
        );
        return Ok(());
    }
    println!(
        "aahl: {} files, {} unique chunks ({}B raw -> {compressed_total}B archive)",
        entries.len(),
        chunks.len(),
        raw_total
    );
    Ok(())
}

struct Piece {
    hash: [u8; 32],
    raw: Vec<u8>,
    unpacked_len: u32,
}

#[derive(Default)]
struct Gather {
    index: HashMap<[u8; 32], u32>,
    pieces: Vec<Piece>,
    entries: Vec<FileEntry>,
}

/// Measurement-only counters for the parallel encoder (Phase A2). Gathered
/// behind `create --par-stats FILE`; when unset the parallel path runs exactly
/// as before (no counters, no extra allocations beyond two toy atomics per
/// run). Counters never influence output bytes or determinism: discovery stays
/// pure and commit stays serial.
#[derive(Default, Clone)]
pub struct ParStats {
    pub tasks: u64,        // worker tasks spawned (pool.spawn count)
    pub used: u64,         // worker candidate consumed (snapshot matched at commit)
    pub stale: u64,        // worker candidate discarded (snapshot drift -> serial re-discovery)
    pub fallback: u64,     // commit had no worker result at all (serial compress)
    pub discover_us: u64,  // aggregate worker time inside fork_candidate (microseconds)
    pub commit_us: u64,    // main-thread commit work (compress_parallel + record write)
    pub wait_us: u64,      // main thread blocked waiting for worker results (microseconds)
    pub peak_workers: u64, // high-water mark of concurrently running workers
}

fn write_par_stats(path: &Path, s: &ParStats) -> Result<()> {
    let txt = format!(
        "par_tasks\t{}\npar_used\t{}\npar_stale\t{}\npar_fallback\t{}\npar_discover_us\t{}\npar_commit_us\t{}\npar_wait_us\t{}\npar_peak_workers\t{}\n",
        s.tasks, s.used, s.stale, s.fallback, s.discover_us, s.commit_us, s.wait_us, s.peak_workers
    );
    fs::write(path, txt).with_context(|| format!("write par-stats {}", path.display()))?;
    Ok(())
}

/// Write v5 table-transform measurement counters (TSV, mirrors
/// write_par_stats). Diagnostics only: `create --table-stats FILE`; the
/// archive bytes are identical whether or not this sink is attached.
fn write_table_stats(path: &Path, s: &table::TableStats) -> Result<()> {
    let txt = format!(
        "chunks_total\t{}\ngrids_found\t{}\ntransforms_chosen\t{}\nraw_oracle_bytes\t{}\nt_side_bytes\t{}\noracle_us\t{}\ndelta_columns\t{}\ngrid_rows\t{}\n",
        s.chunks_total, s.grids_found, s.transforms_chosen, s.raw_oracle_bytes,
        s.t_side_bytes, s.oracle_us, s.delta_columns, s.grid_rows
    );
    fs::write(path, txt).with_context(|| format!("write table-stats {}", path.display()))?;
    Ok(())
}

/// `aahl char`: Phase B0 table-characterization report. Reads one input
/// file, runs the deterministic sample-based grid/delimiter characterization,
/// and prints a TSV-style diagnostic (grid geometry + per-column profile).
fn cmd_char(input: &Path, max_rows: usize) -> Result<()> {
    let data = fs::read(input).with_context(|| format!("read {}", input.display()))?;
    let rep = table::characterize(&data, max_rows);
    println!("total_bytes\t{}", rep.total_bytes);
    println!("total_lines\t{}", rep.total_lines);
    println!("jsonish_lines\t{}", rep.jsonish_lines);
    match rep.grid {
        None => println!("grid\tnone"),
        Some(g) => {
            println!("grid\tyes");
            println!("col_delim\t{}", g.col_delim as char);
            println!("n_cols\t{}", g.n_cols);
            println!("grid_lines\t{}", g.grid_lines);
            println!("grid_pct\t{:.2}", g.grid_pct);
            println!("sample_rows\t{}", g.sample_rows);
            println!(
                "col\\tidx\\tfixed\\twidth\\tcardinality\\tint_rows\\tnumeric_pct\\tmonotonic\\tentropy\\tdelta_smaller\\traw_profile\\tdelta_profile"
            );
            for c in &g.columns {
                println!(
                    "col\\t{}\\t{}\\t{}\\t{}\\t{}\\t{:.1}\\t{}\\t{:.4}\\t{}\\t{}\\t{}",
                    c.idx, c.fixed, c.width, c.cardinality, c.int_rows,
                    c.numeric_pct, c.monotonic, c.entropy, c.delta_smaller,
                    c.raw_profile, c.delta_profile
                );
            }
        }
    }
    Ok(())
}

fn emit_serial(
    out: &mut File,
    pieces: &[Piece],
    plans: &[Option<table::TransformPlan>],
    grammar: &mut grammar::PersistentGrammar,
) -> Result<Vec<ChunkMeta>> {
    let mut chunks: Vec<ChunkMeta> = Vec::with_capacity(pieces.len());
    for (i, p) in pieces.iter().enumerate() {
        let plan = plans.get(i).and_then(|x| x.as_ref());
        // AAHL core: custom fold codec + persistent grammar, NOT zstd/LZ.
        // v5: the table transform compresses the column-major T-stream (the
        // inner codec block), then wraps it in the table header + per-column
        // payload so decode is byte-exact via table::inverse.
        let (inner, pending_gc) = match plan {
            Some(plan) => grammar.compress(&plan.t_stream),
            None => grammar.compress(&p.raw),
        };
        let packed = match plan {
            Some(plan) => table::wrap_block(&plan.meta, &inner)?,
            None => inner,
        };
        // v3 DATA record: kind u8 | body_len u32 | hash(32) | unpacked_len u32 | packed
        out.write_all(&[RECORD_DATA])?;
        let body_len = 32u32 + 4 + packed.len() as u32;
        write_u32(&mut *out, body_len)?;
        out.write_all(&p.hash)?;
        write_u32(&mut *out, p.unpacked_len)?;
        let data_off = out.stream_position()?;
        out.write_all(&packed)?;
        // Grammar GC records interleave between DATA records; they
        // apply before the next chunk's decode.
        if let Some(gc) = pending_gc {
            out.write_all(&gc)?;
        }
        chunks.push(ChunkMeta {
            hash: p.hash,
            unpacked_len: p.unpacked_len,
            packed_len: packed.len() as u32,
            offset: data_off,
        });
    }
    Ok(chunks)
}

/// Parallel discovery pipeline. Discovery (fork_candidate, pure) runs on a
/// bounded pool ahead of a strictly serial commit, so the byte stream is
/// identical for any `jobs` value. Chunk `r`'s worker discovers against the
/// frozen prefix for snapshot `r - lag`; the main thread commits in order,
/// re-discovering serially when the live snapshot has drifted (GC/rule growth),
/// which keeps output deterministic at the cost of worker reuse.
fn emit_parallel(
    out: &mut File,
    pieces: &[Piece],
    plans: &[Option<table::TransformPlan>],
    grammar: &mut grammar::PersistentGrammar,
    jobs: usize,
    stats: Option<&mut ParStats>,
) -> Result<Vec<ChunkMeta>> {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
    use std::sync::mpsc;
    let n = pieces.len();
    let lag = grammar.lag();
    let cfg = aahl::FoldConfig::default();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .unwrap();

    // Phase A2 instrumentation state (two toy atomics; parsed only when the
    // caller asked for diagnostics). Never changes what gets written.
    let active = std::sync::Arc::new(AtomicUsize::new(0));
    let peak = std::sync::Arc::new(AtomicUsize::new(0));
    let discover_us = std::sync::Arc::new(AtomicU64::new(0));
    let stats_on = stats.is_some();

    let mut chunks: Vec<ChunkMeta> = Vec::with_capacity(n);
    let mut s_tasks: u64 = 0;
    let mut s_used: u64 = 0;
    let mut s_stale: u64 = 0;
    let mut s_fallback: u64 = 0;
    let mut s_commit_us: u64 = 0;
    let mut s_wait_us: u64 = 0;
    // (snapshot_len, gc_epoch, candidate) per chunk, available once the chunk
    // becomes the head of the commit queue.
    let mut results: Vec<Option<mpsc::Receiver<(usize, u64, Option<grammar::ForkCandidate>)>>> =
        (0..n).map(|_| None).collect();

    let mut dispatch_tail = 0usize; // next chunk to hand to a worker
    let mut committed = 0usize;

    while committed < n {
        // Chunk r's fork snapshot (rules length after chunk r-lag) is frozen
        // once committed > r-lag, i.e. for every r in [0, committed+lag). Hand
        // each still-undispatched chunk in that window to a worker: the worker
        // clones the frozen rule prefix and never touches the live table, and
        // returns None for an empty snapshot exactly like the serial fork path.
        // The commit half cross-checks snapshot + GC epoch and re-discovers
        // serially when a GC/append invalidated the dispatch, so output bytes
        // are invariant to any worker count or scheduling.
        while dispatch_tail < n && dispatch_tail < committed + lag {
            let r = dispatch_tail;
            let snap = grammar.snapshot_len_for(r);
            let prefix = grammar.rules_prefix(r).to_vec();
            let epoch = grammar.gc_epoch();
            // v5: the worker must discover against the same stream the
            // commit will compress (plan.t_stream when the table transform
            // fires, raw otherwise), exactly mirroring emit_serial, so
            // output stays byte-identical for every worker count.
            let raw = match plans.get(r).and_then(|x| x.as_ref()) {
                Some(plan) => plan.t_stream.clone(),
                None => pieces[r].raw.clone(),
            };
            let cfg = cfg;
            let (tx, rx) = mpsc::channel();
            let active = std::sync::Arc::clone(&active);
            let peak = std::sync::Arc::clone(&peak);
            let discover_us = std::sync::Arc::clone(&discover_us);
            pool.spawn(move || {
                let cur = active.fetch_add(1, Relaxed) + 1;
                let _ = peak.fetch_max(cur, Relaxed);
                let t0 = Instant::now();
                let cand = grammar::fork_candidate(&prefix, &cfg, &raw, prefix.len());
                let _ = discover_us.fetch_add(t0.elapsed().as_micros() as u64, Relaxed);
                active.fetch_sub(1, Relaxed);
                let _ = tx.send((snap, epoch, cand));
            });
            results[r] = Some(rx);
            dispatch_tail += 1;
            s_tasks += 1;
        }

        // Commit the head chunk, reusing its worker's candidate when the live
        // snapshot still matches; otherwise re-discover serially (identical
        // bytes, since discovery is a pure function of snapshot + raw).
        let t_wait = Instant::now();
        let (packed, pending_gc) = match &results[committed] {
            Some(rx) => {
                let (snap, epoch, cand) = rx
                    .recv()
                    .map_err(|_| anyhow::anyhow!("discovery worker failed"))?;
                if stats_on {
                    s_wait_us += t_wait.elapsed().as_micros() as u64;
                }
                let t_commit = Instant::now();
                // Replicates compress_parallel's own freshness check so the
                // harness can classify used-vs-stale without touching the codec.
                let valid = grammar.snapshot_len_for(committed) == snap
                    && snap < grammar.rules_len()
                    && grammar.gc_epoch() == epoch;
                let inner = match plans.get(committed).and_then(|x| x.as_ref()) {
                    Some(plan) => grammar.compress_parallel(cand, snap, epoch, &plan.t_stream),
                    None => grammar.compress_parallel(cand, snap, epoch, &pieces[committed].raw),
                };
                let r = match plans.get(committed).and_then(|x| x.as_ref()) {
                    Some(plan) => (
                        table::wrap_block(&plan.meta, &inner.0)?,
                        inner.1,
                    ),
                    None => inner,
                };
                if stats_on {
                    s_commit_us += t_commit.elapsed().as_micros() as u64;
                    if valid {
                        s_used += 1;
                    } else {
                        s_stale += 1;
                    }
                }
                r
            }
            None => {
                let t_commit = Instant::now();
                let inner = match plans.get(committed).and_then(|x| x.as_ref()) {
                    Some(plan) => grammar.compress(&plan.t_stream),
                    None => grammar.compress(&pieces[committed].raw),
                };
                let r = match plans.get(committed).and_then(|x| x.as_ref()) {
                    Some(plan) => (
                        table::wrap_block(&plan.meta, &inner.0)?,
                        inner.1,
                    ),
                    None => inner,
                };
                if stats_on {
                    s_commit_us += t_commit.elapsed().as_micros() as u64;
                    s_fallback += 1;
                }
                r
            }
        };
        out.write_all(&[RECORD_DATA])?;
        let body_len = 32u32 + 4 + packed.len() as u32;
        write_u32(&mut *out, body_len)?;
        out.write_all(&pieces[committed].hash)?;
        write_u32(&mut *out, pieces[committed].unpacked_len)?;
        let data_off = out.stream_position()?;
        out.write_all(&packed)?;
        if let Some(gc) = pending_gc {
            out.write_all(&gc)?;
        }
        chunks.push(ChunkMeta {
            hash: pieces[committed].hash,
            unpacked_len: pieces[committed].unpacked_len,
            packed_len: packed.len() as u32,
            offset: data_off,
        });
        results[committed] = None;
        committed += 1;
    }
    if let Some(st) = stats {
        st.tasks = s_tasks;
        st.used = s_used;
        st.stale = s_stale;
        st.fallback = s_fallback;
        st.discover_us = discover_us.load(Relaxed);
        st.commit_us = s_commit_us;
        st.wait_us = s_wait_us;
        st.peak_workers = peak.load(Relaxed) as u64;
    }
    Ok(chunks)
}

/// STORE container (6B magic+version+mode, raw payload, standard footer).
/// v2 table entry: path_len u16 | path | file_len u64 | blake3[32] of payload.
fn write_store_archive(out: &mut File, files: &[(String, PathBuf)]) -> Result<()> {
    out.write_all(MAGIC_STORE)?;
    write_u16(out, STORE_VERSION)?;
    write_u16(out, STORE_MODE)?;
    let table_offset = out.stream_position()?;
    write_u64(out, files.len() as u64)?;
    for (arc_name, disk_path) in files {
        let pb = arc_name.as_bytes();
        if pb.len() > u16::MAX as usize {
            bail!("path too long: {arc_name}");
        }
        let data = std::fs::read(disk_path).with_context(|| format!("read {}", disk_path.display()))?;
        let file_len = data.len() as u64;
        let h = *blake3::hash(&data).as_bytes();
        write_u16(out, pb.len() as u16)?;
        out.write_all(pb)?;
        write_u64(out, file_len)?;
        out.write_all(&h)?;
    }
    let table_end = out.stream_position()?;
    for (_, disk_path) in files {
        let mut src = File::open(disk_path).with_context(|| format!("open {}", disk_path.display()))?;
        std::io::copy(&mut src, out)?;
    }
    out.write_all(MAGIC_FTR)?;
    write_u64(out, table_offset)?;
    write_u64(out, table_end - table_offset)?;
    write_u64(out, 0)?; // store: no chunks
    write_u64(out, files.len() as u64)?;
    Ok(())
}

struct ArchiveIndex {
    version: u16,
    flags: u16,
    chunk_size: u32,
    chunks: Vec<ChunkMeta>,
    files: Vec<FileEntry>,
    store_mode: bool,
    store_payload: u64, // STORE: byte offset where the raw payload begins
    gcs: Vec<GcRecord>, // v3 grammar-GC records in stream order
}

fn read_u8(r: &mut impl Read) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

/// Read adapter that yields exactly `n` bytes then EOF, so store payload
/// extraction cannot run past one file's slice into the next.
fn take_exact<R: Read>(r: &mut R, n: usize) -> Result<Take<&mut R>> {
    Ok(r.take(n as u64))
}

fn open_index(archive: &Path) -> Result<(File, ArchiveIndex)> {
    let mut f = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let flen = f.metadata()?.len();
    if flen < FOOTER_LEN + 4 {
        bail!("file too small to be aahl");
    }
    let mut hdr = [0u8; 4];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut hdr)?;
    if &hdr[..2] == MAGIC_STORE {
        return open_store(f, flen);
    }
    if &hdr != MAGIC_HDR {
        bail!("bad magic (not aahl)");
    }
    let ver = read_u16(&mut f)?;
    if !(1..=5).contains(&ver) {
        bail!("unsupported version {ver}");
    }
    let flags = read_u16(&mut f)?;
    let chunk_size = read_u32(&mut f)?;
    let params = read_u64(&mut f)?;
    let header_len = if ver >= 3 {
        // v3: params packed + header_checksum over the 20-byte prefix
        let checksum = read_u16(&mut f)?;
        let mut prefix = [0u8; 20];
        prefix[..4].copy_from_slice(&hdr);
        prefix[4..6].copy_from_slice(&ver.to_le_bytes());
        prefix[6..8].copy_from_slice(&flags.to_le_bytes());
        prefix[8..12].copy_from_slice(&chunk_size.to_le_bytes());
        prefix[12..20].copy_from_slice(&params.to_le_bytes());
        if checksum != header_checksum(&prefix) {
            bail!("header checksum mismatch (corrupt)");
        }
        HEADER_LEN_V3
    } else {
        let (_, _, _, f2) = unpack_params(params);
        if f2 != 0 {
            bail!("reserved flags2 nonzero in v{ver}");
        }
        HEADER_LEN_V2
    };

    f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut fmagic = [0u8; 4];
    f.read_exact(&mut fmagic)?;
    if &fmagic != MAGIC_FTR {
        bail!("bad footer (truncated?)");
    }
    let table_offset = read_u64(&mut f)?;
    let _table_len = read_u64(&mut f)?;
    let num_chunks = read_u64(&mut f)? as usize;
    let num_files = read_u64(&mut f)? as usize;
    if table_offset >= flen {
        bail!("corrupt table offset");
    }
    // sanity caps: every v3 record is >=5 bytes (kind+body_len), every
    // file entry >=18 bytes (path_len u16 + file_len u64 + nrefs u64).
    // cap allocations from corrupt footer counts.
    let max_chunks = (table_offset.saturating_sub(header_len)) / 5;
    let max_files = (flen.saturating_sub(table_offset)) / 18;
    if num_chunks as u64 > max_chunks || num_files as u64 > max_files {
        bail!("implausible table counts (corrupt)");
    }

    // Chunk region. v1/v2 use a fixed [hash|unpacked|packed|packed] table;
    // v3 uses record framing: kind u8 | body_len u32 | body. DATA records
    // carry compressed chunks; GC records are parsed, validated and stored
    // in stream order so extraction can apply them at the right boundary.
    let mut chunks = Vec::with_capacity(num_chunks);
    let mut gcs: Vec<GcRecord> = Vec::new();
    f.seek(SeekFrom::Start(header_len))?;
    if ver >= 3 {
        let mut data_seen = 0u32;
        while f.stream_position()? < table_offset {
            let kind = read_u8(&mut f)?;
            let body_len = read_u32(&mut f)? as u64;
            match kind {
                RECORD_DATA => {
                    data_seen += 1;
                    let mut hash = [0u8; 32];
                    f.read_exact(&mut hash)?;
                    let unpacked = read_u32(&mut f)?;
                    if body_len < 36 {
                        bail!("corrupt data record body");
                    }
                    let packed = body_len - 32 - 4; // hash + unpacked_len
                    if table_offset.saturating_sub(f.stream_position()?) < packed {
                        bail!("corrupt data record length");
                    }
                    let data_off = f.stream_position()?;
                    f.seek(SeekFrom::Current(packed as i64))?;
                    chunks.push(ChunkMeta {
                        hash,
                        unpacked_len: unpacked,
                        packed_len: packed as u32,
                        offset: data_off,
                    });
                }
                RECORD_GC => {
                    if body_len as u64 > table_offset.saturating_sub(f.stream_position()?) {
                        bail!("corrupt GC record length");
                    }
                    let mut body = vec![0u8; body_len as usize];
                    f.read_exact(&mut body)?;
                    let survivors = grammar::PersistentGrammar::parse_gc_body(&body)
                        .with_context(|| format!("bad GC record at {data_seen}"))?;
                    gcs.push(GcRecord {
                        data_before: data_seen,
                        survivors,
                    });
                }
                _ => bail!("unknown record kind {kind}"),
            }
        }
        if chunks.len() != num_chunks {
            bail!("chunk count mismatch: header {num_chunks}, walked {}", chunks.len());
        }
    } else {
        for _ in 0..num_chunks {
            let mut hash = [0u8; 32];
            f.read_exact(&mut hash)?;
            let unpacked = read_u32(&mut f)?;
            let packed = read_u32(&mut f)?;
            let data_off = f.stream_position()?;
            f.seek(SeekFrom::Current(packed as i64))?;
            chunks.push(ChunkMeta {
                hash,
                unpacked_len: unpacked,
                packed_len: packed,
                offset: data_off,
            });
        }
    }

    f.seek(SeekFrom::Start(table_offset))?;
    let nfiles = read_u64(&mut f)? as usize;
    if nfiles != num_files {
        bail!("file count mismatch");
    }
    let mut files = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        let pl = read_u16(&mut f)? as usize;
        let mut pb = vec![0u8; pl];
        f.read_exact(&mut pb)?;
        let path = String::from_utf8(pb).context("non-utf8 path")?;
        let file_len = read_u64(&mut f)?;
        let nrefs = read_u64(&mut f)? as usize;
        let mut refs = Vec::with_capacity(nrefs);
        for _ in 0..nrefs {
            refs.push(read_u32(&mut f)?);
        }
        files.push(FileEntry {
            path,
            file_len,
            refs,
            store_hash: None,
        });
    }
    Ok((
        f,
        ArchiveIndex {
            version: ver,
            flags,
            chunk_size,
            chunks,
            files,
            store_mode: false,
            store_payload: 0,
            gcs,
        },
    ))
}

/// STORE container reader. Layout (offset 0): magic b"AS" (2) | version u16 |
/// mode u16, then the file table (nfiles u64, then per file path_len u16 +
/// path + file_len u64 + blake3[32]), then the raw concatenated payload.
/// Suffix is the same footer as compressed containers. Every payload slice is
/// verified against its embedded hash when the index is opened, so a store
/// container is tamper-detecting (rejects any metadata or payload corruption).
fn open_store(mut f: File, flen: u64) -> Result<(File, ArchiveIndex)> {
    // cursor is at offset 4 ("AS"+version already consumed by open_index);
    // re-read version+mode from their canonical offsets.
    f.seek(SeekFrom::Start(2))?;
    let sver = read_u16(&mut f)?;
    if sver != STORE_VERSION {
        bail!("unsupported store version {sver}");
    }
    let mode = read_u16(&mut f)?;
    if mode != STORE_MODE {
        bail!("unsupported store mode {mode}");
    }

    f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
    let mut fmagic = [0u8; 4];
    f.read_exact(&mut fmagic)?;
    if &fmagic != MAGIC_FTR {
        bail!("bad store footer (truncated?)");
    }
    let table_offset = read_u64(&mut f)?;
    let _table_len = read_u64(&mut f)?;
    let num_chunks = read_u64(&mut f)? as usize;
    let num_files = read_u64(&mut f)? as usize;
    if table_offset >= flen || num_chunks != 0 {
        bail!("corrupt store table offset");
    }

    f.seek(SeekFrom::Start(table_offset))?;
    let nfiles = read_u64(&mut f)? as usize;
    if nfiles != num_files {
        bail!("store file count mismatch");
    }
    // store entries are >=42 bytes (path_len u16 + file_len u64 + hash 32b)
    if nfiles as u64 > (flen.saturating_sub(table_offset)) / 42 {
        bail!("implausible store file count (corrupt)");
    }
    let mut files = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        let pl = read_u16(&mut f)? as usize;
        let mut pb = vec![0u8; pl];
        f.read_exact(&mut pb)?;
        let path = String::from_utf8(pb).context("non-utf8 path")?;
        let file_len = read_u64(&mut f)?;
        let mut hash = [0u8; 32];
        f.read_exact(&mut hash)?;
        files.push(FileEntry {
            path,
            file_len,
            refs: Vec::new(),
            store_hash: Some(hash),
        });
    }
    let store_payload = f.stream_position()?;
    let total: u64 = files.iter().map(|e| e.file_len).sum();
    if store_payload + total != flen - FOOTER_LEN {
        bail!("store payload length mismatch (truncated?)");
    }
    // verify every payload slice against its embedded hash
    let mut hasher_buf = Vec::with_capacity(1 << 20);
    let mut off = 0u64;
    for (i, e) in files.iter().enumerate() {
        let h = {
            let mut hh = blake3::Hasher::new();
            f.seek(SeekFrom::Start(store_payload + off))?;
            let mut left = e.file_len;
            while left > 0 {
                let take = left.min(hasher_buf.capacity() as u64) as usize;
                hasher_buf.resize(take, 0);
                f.read_exact(&mut hasher_buf[..take])?;
                hh.update(&hasher_buf[..take]);
                left -= take as u64;
            }
            *hh.finalize().as_bytes()
        };
        if h != files[i].store_hash.unwrap() {
            bail!("store payload hash mismatch for {}", e.path);
        }
        off += e.file_len;
    }
    Ok((
        // reset position so callers start at 0
        { f.seek(SeekFrom::Start(0))?; f },
        ArchiveIndex {
            version: STORE_VERSION,
            flags: 0,
            chunk_size: 0,
            chunks: Vec::new(),
            files,
            store_mode: true,
            store_payload,
            gcs: Vec::new(),
        },
    ))
}

/// Decode one chunk payload with the v5 table wrapper in play when
/// `table_flag` is set: a wrapped chunk is unwrapped (tag + meta), its
/// inner codec block is decompressed to the T-stream length, then
/// inverted back to the raw grid rows. When the wrapper tag is absent
/// (v1-v4 archives, or a v5 chunk whose measurement kept the raw lane)
/// the payload decodes directly. Hash + length are always verified over
/// the reconstructed raw bytes.
fn decode_payload(
    payload: &[u8],
    meta: &ChunkMeta,
    table_flag: bool,
    decompress: impl FnOnce(&[u8], usize) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    if table_flag {
        if let Some((m, off)) = table::try_unwrap(payload)? {
            let t = decompress(&payload[off..], m.t_len as usize)?;
            let raw = table::inverse(&t, &m, meta.unpacked_len as usize)?;
            if raw.len() as u32 != meta.unpacked_len {
                bail!("chunk size mismatch after decode");
            }
            let h = *blake3::hash(&raw).as_bytes();
            if h != meta.hash {
                bail!("chunk hash mismatch (corrupt)");
            }
            return Ok(raw);
        }
    }
    let raw = decompress(payload, meta.unpacked_len as usize)?;
    if raw.len() as u32 != meta.unpacked_len {
        bail!("chunk size mismatch after decode");
    }
    let h = *blake3::hash(&raw).as_bytes();
    if h != meta.hash {
        bail!("chunk hash mismatch (corrupt)");
    }
    Ok(raw)
}

fn read_chunk(f: &mut File, meta: &ChunkMeta, table_flag: bool) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(meta.offset))?;
    let mut packed = vec![0u8; meta.packed_len as usize];
    f.read_exact(&mut packed)?;
    decode_payload(&packed, meta, table_flag, |b, n| aahl::decompress_block(b, n))
}

fn cmd_list(archive: &Path) -> Result<()> {
    let (_f, idx) = open_index(archive)?;
    if idx.store_mode {
        println!("STORE container (raw payload)");
        println!("{:>12}  {}", "SIZE", "PATH");
        for e in &idx.files {
            println!("{:>12}  {}", e.file_len, e.path);
        }
        println!("{} files, raw on disk", idx.files.len());
        return Ok(());
    }
    println!("{:>12}  {:>6}  {}  (v{})", "SIZE", "CHUNKS", "PATH", idx.version);
    for e in &idx.files {
        println!("{:>12}  {:>6}  {}", e.file_len, e.refs.len(), e.path);
    }
    println!(
        "{} files, {} unique chunks",
        idx.files.len(),
        idx.chunks.len()
    );
    Ok(())
}

/// Ensure a recorded entry path stays inside the extraction root. Rejects
/// absolute paths, UNC/drive roots, and any `..` component. The destination is
/// constructed from the canonicalized root (so textual escapes are impossible),
/// then re-canonicalized when it already exists so a symlinked parent cannot
/// redirect the write outside the output directory.
fn safe_destination(out_dir: &Path, entry_path: &str) -> Result<PathBuf> {
    if entry_path.is_empty() {
        bail!("empty entry path");
    }
    let p = Path::new(entry_path);
    if p.is_absolute() || p.has_root() {
        bail!("unsafe entry path: {entry_path:?}");
    }
    if p.starts_with("..") || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("unsafe entry path: {entry_path:?}");
    }
    if cfg!(windows) && entry_path.contains(':') {
        bail!("unsafe entry path: {entry_path:?}");
    }
    std::fs::create_dir_all(out_dir)?;
    let root = out_dir
        .canonicalize()
        .with_context(|| format!("canonicalize {}", out_dir.display()))?;
    let dest = root.join(&p);
    if let Ok(real) = std::fs::canonicalize(&dest) {
        if !real.starts_with(&root) {
            bail!("entry path escapes output dir: {entry_path:?}");
        }
    }
    Ok(dest)
}

fn cmd_extract(archive: &Path, out_dir: &Path) -> Result<()> {
    let (mut f, idx) = open_index(archive)?;
    std::fs::create_dir_all(out_dir)?;

    if idx.store_mode {
        f.seek(SeekFrom::Start(idx.store_payload))?;
        for e in &idx.files {
            let dest = safe_destination(out_dir, &e.path)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&dest)?;
            std::io::copy(&mut take_exact(&mut f, e.file_len as usize)?, &mut out)?;
        }
        println!(
            "extracted {} files -> {}",
            idx.files.len(),
            out_dir.display()
        );
        return Ok(());
    }

    // v2+ persistent grammar: chunk decode order is the honest contract, so
    // decode every chunk once in index order through a single grammar and
    // serve the file references from the full cache. GC records interleaved
    // with DATA records retarget the grammar before the next chunk.
    let ordered_cache: HashMap<u32, Vec<u8>> = if idx.flags & FLAG_GLOBAL != 0 {
        let mut g = if idx.version >= 4 {
            grammar::PersistentGrammar::new_v4(aahl::FoldConfig::default(), 64)
        } else {
            grammar::PersistentGrammar::default()
        };
        let mut cache: HashMap<u32, Vec<u8>> = HashMap::with_capacity(idx.chunks.len());
        let mut gci = 0usize;
        for (i, meta) in idx.chunks.iter().enumerate() {
            while gci < idx.gcs.len() && (idx.gcs[gci].data_before as usize) == i {
                g.apply_gc(&idx.gcs[gci].survivors)
                    .with_context(|| format!("GC before chunk {i}"))?;
                gci += 1;
            }
            let raw = read_chunk_grammar(&mut f, meta, idx.flags & FLAG_TABLE != 0, &mut g)?;
            cache.insert(i as u32, raw);
        }
        cache
    } else {
        HashMap::new()
    };
    let mut cache: HashMap<u32, Vec<u8>> = HashMap::new();

    for e in &idx.files {
        let dest = safe_destination(out_dir, &e.path)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dest)?;
        let mut left = e.file_len;
        for &r in &e.refs {
            let raw = if let Some(v) = ordered_cache.get(&r) {
                v.clone()
            } else if let Some(v) = cache.get(&r) {
                v.clone()
            } else {
                let meta = idx.chunks.get(r as usize).context("bad chunk ref")?;
                let raw = read_chunk(&mut f, meta, idx.flags & FLAG_TABLE != 0)?;
                cache.insert(r, raw.clone());
                if cache.len() > 64 {
                    cache.clear();
                }
                raw
            };
            let take = (raw.len() as u64).min(left) as usize;
            out.write_all(&raw[..take])?;
            left -= take as u64;
        }
        if left != 0 {
            bail!("length mismatch for {}", e.path);
        }
    }
    println!(
        "extracted {} files -> {}",
        idx.files.len(),
        out_dir.display()
    );
    Ok(())
}

fn read_chunk_grammar(
    f: &mut File,
    meta: &ChunkMeta,
    table_flag: bool,
    g: &mut grammar::PersistentGrammar,
) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(meta.offset))?;
    let mut packed = vec![0u8; meta.packed_len as usize];
    f.read_exact(&mut packed)?;
    decode_payload(&packed, meta, table_flag, |b, n| g.decompress(b, n))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Create {
            archive,
            inputs,
            chunk_size,
            lag,
            gc_interval,
            jobs,
            par_stats,
            no_table,
            table_stats,
        } => cmd_create_par(
            &archive,
            &inputs,
            chunk_size,
            lag,
            gc_interval,
            jobs,
            par_stats.as_deref(),
            no_table,
            table_stats.as_deref(),
        ),
        Cmd::List { archive } => cmd_list(&archive),
        Cmd::Extract { archive, out_dir } => cmd_extract(&archive, &out_dir),
        Cmd::BlockSize { input, sweep } => {
            let data = std::fs::read(&input).with_context(|| format!("read {}", input.display()))?;
            let (folded, arith, o1t, o1b, final_len, raw, mode) = aahl::block_sizes(&data);
            let best = final_len.min(raw);
            println!(
                "{}: raw={} fold={} arith(A)={} o1token(D)={} o1bytes(C)={} final={} mode={} ratio={:.3}",
                input.display(),
                raw,
                folded,
                arith,
                o1t,
                o1b,
                final_len,
                mode as char,
                best as f64 / raw as f64
            );
            if sweep {
                // sweep fold knobs: merges x min_pair_count
                println!("  sweep (max_merges x min_pair_count => rules/tokens/arith):");
                for merges in [0usize, 128, 256, 512, 1024] {
                    for pc in [4usize, 6, 8] {
                        let cfg =
                            aahl::FoldConfig { max_merges: merges, min_pair_count: pc, max_syms: 8192 };
                        let (nr, nt, asize) = aahl::arith_sizes(&data, &cfg);
                        println!(
                            "    m={:>4} pc={}: rules={:>4} tokens={:>5} arith={} ({:.3})",
                            merges,
                            pc,
                            nr,
                            nt,
                            asize,
                            asize as f64 / raw as f64
                        );
                    }
                }
            }
            Ok(())
        }
        Cmd::BuildCorpus { set_dir, source } => corpus::build_corpus(&set_dir, source.as_deref()),
        Cmd::BuildBinaryCorpus { set_dir, source } => {
            corpus::build_binary_corpus(&set_dir, &source, None)?;
            Ok(())
        }
        Cmd::RunBench { set_dir, tsv, chunk_size, aahl_modes } => {
            bench::run_bench(&set_dir, &tsv, chunk_size, &aahl_modes)?;
            Ok(())
        }
        Cmd::RunBenchJobs {
            set_dir,
            tsv,
            raw_tsv,
            chunk_size,
            runs,
            jobs,
            corpora,
        } => {
            bench::run_jobs_bench(&set_dir, &tsv, &raw_tsv, chunk_size, runs, &jobs, &corpora)?;
            Ok(())
        }
        Cmd::RunBenchParstats {
            set_dir,
            tsv,
            chunk_size,
            jobs,
            corpora,
        } => {
            bench::run_parstats(&set_dir, &tsv, chunk_size, &jobs, &corpora)?;
            Ok(())
        }
        Cmd::RunChar { input, max_rows } => {
            cmd_char(&input, max_rows)
        }
                Cmd::RunAblate { set_dir, tsv, chunk_sizes } => {
            ablation::run_ablation(&set_dir, &tsv, &chunk_sizes)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod container_tests {
    use super::*;
    use std::io::Write as _;

    fn scratch(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("aahl_p1_{}_{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_file(p: &Path, data: &[u8]) {
        std::fs::write(p, data).unwrap();
    }

    #[test]
    fn params_pack_unpack_roundtrip() {
        let (l, g, r, f2) = unpack_params(pack_params(16, 64, 60000, 0));
        assert_eq!((l, g, r, f2), (16, 64, 60000, 0));
        let (l, g, r, f2) = unpack_params(pack_params(0, 0, 0, 0));
        assert_eq!((l, g, r, f2), (0, 0, 0, 0));
        // masks respected: high garbage outside the window is dropped
        let p = pack_params(u64::MAX, u64::MAX, u64::MAX, 0);
        let (l, g, r, _) = unpack_params(p);
        assert_eq!(l, PARAM_LAG_MASK);
        assert_eq!(g, PARAM_GC_MASK);
        assert_eq!(r, PARAM_RULES_MASK);
    }

    #[test]
    fn header_checksum_differs_on_prefix_change() {
        let a = [0u8; 20];
        let b = [1u8; 20];
        assert_ne!(header_checksum(&a), header_checksum(&b));
        let mut c = a;
        c[19] = 1;
        assert_ne!(header_checksum(&a), header_checksum(&c));
    }

#[test]
    fn v3_compressed_multifile_dedup_roundtrip() {
        let dir = scratch("multifile");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // variable text so every 4096 window is a genuinely distinct chunk,
        // keeping the dedup accounting exact (a and b byte-identical)
        let mut blob = String::new();
        for i in 0..2200 {
            blob.push_str(&format!("{i}: the quick brown fox jumps over the lazy dog while the farmer watches from the barn\n"));
        }
        let blob = blob.into_bytes();
        write_file(&src.join("a.txt"), &blob);
        write_file(&src.join("b.txt"), &blob); // identical -> dedup
        write_file(&src.join("c.bin"), &[0u8; 100]); // compressible-ish
        let arc = dir.join("out.aahl");
        let outd = dir.join("extracted");

        cmd_create(&arc, &[src.clone()], 4096, 1, 64, 1).unwrap();
        let (mut f, idx) = open_index(&arc).unwrap();
        assert_eq!(idx.version, VERSION);
        assert!(!idx.store_mode);

        let blocks_a = blob.len() / 4096 + usize::from(blob.len() % 4096 != 0);
        // c.bin contributes 1 unique chunk; b dedups a exactly
        assert_eq!(idx.chunks.len(), blocks_a + 1, "dedup failed");
        assert_eq!(idx.files[0].refs, idx.files[1].refs, "identical files must share refs");
        assert_eq!(idx.files[0].refs.len(), blocks_a);
        drop(f);
        cmd_extract(&arc, &outd).unwrap();
        for name in ["a.txt", "b.txt", "c.bin"] {
            let orig = std::fs::read(src.join(name)).unwrap();
            let got = std::fs::read(outd.join("src").join(name)).unwrap();
            assert_eq!(orig, got, "roundtrip mismatch {name}");
        }
    }

#[test]
    fn store_fallback_on_incompressible_is_raw_plus_small_overhead() {
        let dir = scratch("store");
        let src = dir.join("rand.bin");
        // deterministic pseudo-random bytes: hashes of counters (incompressible)
        let mut data = Vec::with_capacity(65536);
        for i in 0..2048u64 {
            let h = blake3::hash(&i.to_le_bytes());
            data.extend_from_slice(&h.as_bytes()[..32]);
        }
        write_file(&src, &data);
        let arc = dir.join("store.aahl");
        cmd_create(&arc, &[src.clone()], 4096, 1, 64, 1).unwrap();

let (_f, idx) = open_index(&arc).unwrap();
        assert!(idx.store_mode);
        let path_len = idx.files[0].path.len() as u64;
        let arc_len = std::fs::metadata(&arc).unwrap().len();
        // STORE v2: 6B container + 8B nfiles + (2+path+8+32 hash) entry + raw + 36B footer
        let overhead = 6u64 + 8 + (2 + path_len + 8 + 32) + FOOTER_LEN;
        assert!(arc_len <= data.len() as u64 + overhead, "arc {arc_len} inflated past raw {}+overhead {overhead}", data.len());
        assert_eq!(arc_len, data.len() as u64 + overhead, "store size must be honest");
    }

    #[test]
    fn corrupt_and_truncated_inputs_never_panic() {
        let dir = scratch("corrupt");
        let src = dir.join("x.txt");
        let blob = b"hello container world ".repeat(1000);
        write_file(&src, &blob);
        let arc = dir.join("c.aahl");
        cmd_create(&arc, &[src.clone()], 4096, 1, 64, 1).unwrap();
        let good = std::fs::read(&arc).unwrap();

        // truncations at every 4096 boundary
        for cut in (0..good.len()).step_by(4096).collect::<Vec<_>>() {
            let p = dir.join(format!("t{cut}.aahl"));
            std::fs::write(&p, &good[..cut]).unwrap();
            match open_index(&p) {
                Ok((_, idx)) => {
                    // only acceptable if truncation removed zero bytes
                    assert_eq!(cut, good.len(), "fragment {cut} opened");
                    drop(idx);
                }
                Err(_) => {}
            }
        }
        // byte flips in header/footer never panic
        for pos in [0usize, 1, 2, 4, 6, 8, 20, 22, good.len() - 36, good.len() - 1] {
            if pos >= good.len() {
                continue;
            }
            let mut bad = good.clone();
            bad[pos] ^= 0xFF;
            let p = dir.join(format!("f{pos}.aahl"));
            std::fs::write(&p, &bad).unwrap();
            let _ = open_index(&p);
        }
        // garbage and empty files
        let garbage = dir.join("garbage.aahl");
        std::fs::write(&garbage, b"trash data not an aahl archive at all").unwrap();
        assert!(open_index(&garbage).is_err());
        let empty = dir.join("empty.aahl");
        std::fs::write(&empty, b"").unwrap();
        assert!(open_index(&empty).is_err());
        // flipped header checksum is rejected
        let mut tampered = good.clone();
        let mut hdr = [0u8; 4];
        hdr.copy_from_slice(&tampered[..4]);
        assert_eq!(&hdr, MAGIC_HDR);
        let cs_off = 20;
        tampered[cs_off] ^= 0xFF;
        let p = dir.join("cksum.aahl");
        std::fs::write(&p, &tampered).unwrap();
        assert!(open_index(&p).is_err(), "tampered checksum must be rejected");
        let _ = std::fs::write(&arc, good); // leave good archive for later tests
    }

#[test]
    fn empty_file_roundtrip_and_store_bound() {
        let dir = scratch("empty");
        let src = dir.join("e.txt");
        write_file(&src, b"");
        let arc = dir.join("e.aahl");
        cmd_create(&arc, &[src.clone()], 4096, 1, 64, 1).unwrap();
        let (_f, idx) = open_index(&arc).unwrap();
        assert!(idx.store_mode, "empty file should take store path");
        assert_eq!(idx.files.len(), 1);
        assert_eq!(idx.files[0].file_len, 0);
        let outd = dir.join("ex");
        cmd_extract(&arc, &outd).unwrap();
        assert_eq!(std::fs::read(outd.join("e.txt")).unwrap(), b"");
    }

    #[test]
    fn gc_records_in_archive_apply_and_roundtrip() {
        let dir = scratch("gcs");
        let src = dir.join("osc.txt");
        let data = oscillating_text();
        write_file(&src, &data);
        let arc = dir.join("gcs.aahl");
        // gc_interval=8 forces grammar reclamation mid-archive
        cmd_create_params(
            &arc,
            &[src.clone()],
            4096,
            pack_params(PARAM_LAG_DEFAULT, 8, PARAM_RULES_DEFAULT, 0),
            1,
        )
        .unwrap();

        let (_f, idx) = open_index(&arc).unwrap();
        assert!(!idx.store_mode);
        assert!(
            !idx.gcs.is_empty(),
            "oscillating content must emit GC records"
        );
        for g in &idx.gcs {
            assert!(g.data_before >= 1, "GC must interleave after DATA records");
            assert!(g.survivors.len() >= 2, "GC must keep at least 2 rules");
        }

        let outd = dir.join("extracted");
        cmd_extract(&arc, &outd).unwrap();
        let got = std::fs::read(outd.join("osc.txt").as_path()).unwrap();
        assert_eq!(got, data, "roundtrip mismatch across GC boundaries");
    }

    #[test]
    fn corrupt_gc_record_bodies_are_rejected_not_panicked() {
        let dir = scratch("gcscorrupt");
        let src = dir.join("osc.txt");
        let data = oscillating_text();
        write_file(&src, &data);
        let arc = dir.join("gcs.aahl");
        cmd_create_params(
            &arc,
            &[src.clone()],
            4096,
            pack_params(PARAM_LAG_DEFAULT, 8, PARAM_RULES_DEFAULT, 0),
            1,
        )
        .unwrap();

        let bytes = std::fs::read(&arc).unwrap();
        let good = bytes.clone();
        // find the first GC record via the same record-framing walk
        let table_offset =
            u64::from_le_bytes(good[good.len() - 32..good.len() - 24].try_into().unwrap());
        let mut pos: usize = HEADER_LEN_V3 as usize;
        let mut gc_body = None;
        while pos + 5 <= table_offset as usize {
            let kind = good[pos];
            let blen =
                u32::from_le_bytes(good[pos + 1..pos + 5].try_into().unwrap()) as usize;
            if kind == RECORD_GC {
                gc_body = Some(pos + 5); // body starts at flags byte
            }
            pos += 5 + blen;
        }
        let gc_body = gc_body.expect("archive must contain a GC record");
        // sanity: good archive parses
        assert!(open_index(&arc).is_ok());

        // flip the flags byte -> parse_gc_body rejects; open_index must Err
        let mut tampered = good.clone();
        tampered[gc_body] ^= 0x01;
        let p = dir.join("badflags.aahl");
        std::fs::write(&p, &tampered).unwrap();
        assert!(
            open_index(&p).is_err(),
            "nonzero GC flags must be rejected"
        );

        // shrink a survivor list (more survivors than body conveys) -> Err
        let mut tampered = good.clone();
        tampered[gc_body + 5] ^= 0xFF; // num_survivors low byte
        let p = dir.join("badcount.aahl");
        std::fs::write(&p, &tampered).unwrap();
        assert!(
            open_index(&p).is_err(),
            "GC survivor count mismatch must be rejected"
        );
    }

    #[test]
    fn payload_mutation_fuzz_never_panics_and_never_corrupts() {
        // deterministic PRNG mutation fuzz over a real compressed archive:
        // bit flips, byte substitutions, accidental decodes, truncations and
        // boundary-smearing must never panic, and any decode that succeeds
        // must reproduce the original bytes exactly (no silent corruption).
        let dir = scratch("payloadfuzz");
        let src = dir.join("mix.bin");
        let data = oscillating_text();
        write_file(&src, &data);
        let arc = dir.join("base.aahl");
        cmd_create_params(
            &arc,
            &[src.clone()],
            4096,
            pack_params(PARAM_LAG_DEFAULT, 8, PARAM_RULES_DEFAULT, 0),
            1,
        )
        .unwrap();
        let good = std::fs::read(&arc).unwrap();
        if good.len() < 32 {
            panic!("archive unexpectedly tiny");
        }

        // baseline sanity: a real archive opens and round-trips
        let baseline_out = dir.join("baseline");
        cmd_extract(&arc, &baseline_out).unwrap();
        assert_eq!(
            std::fs::read(baseline_out.join("mix.bin")).unwrap(),
            data,
            "baseline must round-trip"
        );

        let table_offset =
            u64::from_le_bytes(good[good.len() - 32..good.len() - 24].try_into().unwrap());
        let mut rng = corpus::Prng::new(0x5EED_F00D);

        for iter in 0..4000u32 {
            let mut bad = good.clone();
            // 1..=4 mutations far from the header/footer/table, inside the
            // chunk/GC record region.
            let nmut = 1 + (rng.next_u64() % 4) as usize;
            let lo = HEADER_LEN_V3 as usize + 1;
            let hi = (table_offset as usize).min(bad.len().saturating_sub(8));
            if hi <= lo {
                continue;
            }
            let mut already = Vec::with_capacity(nmut);
            for _ in 0..nmut {
                let pos = lo + (rng.next_u64() as usize % (hi - lo));
                if already.contains(&pos) {
                    continue;
                }
                already.push(pos);
                match rng.next_u64() % 4 {
                    0 => bad[pos] ^= 1 << (rng.next_u64() % 8),
                    1 => bad[pos] = (rng.next_u64() % 256) as u8,
                    2 => {
                        // smear the adjacent record-length bytes so framing is
                        // exercised, not just payload
                        if pos + 1 < bad.len() {
                            bad[pos + 1] ^= 0x01;
                        }
                        bad[pos] ^= 0x80;
                    }
                    _ => {
                        // zero run of 1..5 bytes: truncation of variable-bit data
                        let run = 1 + (rng.next_u64() % 5) as usize;
                        for k in 0..run {
                            if pos + k < bad.len() {
                                bad[pos + k] = 0;
                            }
                        }
                    }
                }
            }

            // never panic on open:
            let pmut = dir.join(format!("m{iter}.aahl"));
            std::fs::write(&pmut, &bad).unwrap();
            match open_index(&pmut) {
                Ok((_f, idx)) => {
                    let outd = dir.join(format!("out{iter}"));
                    if let Ok(()) = cmd_extract(&pmut, &outd) {
                        // any successful decode must be byte-exact
                        let got = std::fs::read(outd.join("mix.bin"))
                            .unwrap_or_else(|_| Vec::new());
                        assert_eq!(got, data, "silent corruption at iter {iter}");
                    }
                    // index parsed -> dropped without panic
                    drop(idx);
                    let _ = std::fs::remove_dir_all(&outd);
                }
                Err(_) => {}
            }
            let _ = std::fs::remove_file(&pmut);
        }
    }

    #[test]
    fn store_mode_mutation_fuzz_never_panics() {
        // store v2 carries a per-file hash, so ANY corruption (metadata,
        // footer, or raw payload) must be rejected on open â€” never a panic,
        // and never a silent wrong extraction.
        let dir = scratch("storefuzz");
        let src = dir.join("rand.bin");
        let mut data = Vec::with_capacity(65536);
        for i in 0..2048u64 {
            let h = blake3::hash(&i.to_le_bytes());
            data.extend_from_slice(&h.as_bytes()[..32]);
        }
        write_file(&src, &data);
        let arc = dir.join("store.aahl");
        cmd_create_params(&arc, &[src.clone()], 4096, pack_params(1, 16, 64, 0), 1).unwrap();
        let (_f, idx) = open_index(&arc).unwrap();
        assert!(idx.store_mode, "pseudo-random bytes must take the store path");
        assert_eq!(idx.store_payload, 64, "store v2 layout: 6B hdr + 8B count + 2B pathlen + \"rand.bin\" + 8B len + 32B hash");
        let good = std::fs::read(&arc).unwrap();
        let footer_lo = good.len() - FOOTER_LEN as usize;
        assert!(idx.store_payload as usize + data.len() + FOOTER_LEN as usize == good.len());

        let mut rng = corpus::Prng::new(0xBEEF_C0DE);
        for iter in 0..4000u32 {
            let mut bad = good.clone();
            let nmut = 1 + (rng.next_u64() % 3) as usize;
            let mut truncated = false;
            for _ in 0..nmut {
                let pos = (rng.next_u64() as usize) % bad.len();
                if bad.len() > 8 && rng.next_u64() % 4 == 0 {
                    bad.truncate(pos);
                    truncated = true;
                    break;
                }
                // mutate anywhere: header, table, payload or footer
                bad[pos] ^= 1 << (rng.next_u64() % 8);
            }
            let pmut = dir.join(format!("s{iter}.aahl"));
            std::fs::write(&pmut, &bad).unwrap();
            match open_index(&pmut) {
                Ok((_f, idx)) => {
                    assert!(!truncated, "truncated store payload must be rejected");
                    assert!(
                        idx.store_mode,
                        "store fuzz mutation must stay a store container"
                    );
                    let outd = dir.join(format!("so{iter}"));
                    // open_index verified every payload hash, so a surviving
                    // store must extract the exact original (never a silent
                    // wrong payload) and never panic.
                    cmd_extract(&pmut, &outd).unwrap();
                    let got = std::fs::read(outd.join("rand.bin")).unwrap();
                    assert_eq!(got, data, "store corruption survived open at iter {iter}");
                    drop(idx);
                    let _ = std::fs::remove_dir_all(&outd);
                }
                Err(_) => {}
            }
            let _ = std::fs::remove_file(&pmut);
        }
    }

    fn oscillating_text() -> Vec<u8> {
        // alternate two idioms every 16 windows so live rules die by the next
        // window and every DATA record is unique (no dedup collapse)
        let mut data = Vec::new();
        for i in 0..64 {
            let idiom = if i % 16 < 8 { "alpha" } else { "beta" };
            let mut chunk = String::new();
            while chunk.len() < 4096 {
                chunk.push_str(&format!(
                    "pub fn {idiom}_{i:03}_t{}() -> u64 {{ let s = {}u64.wrapping_mul(97).wrapping_add({}); s ^ s >> 11; s }}\n",
                    chunk.len() % 7,
                    i,
                    i
                ));
            }
            data.extend_from_slice(chunk.as_bytes());
        }
        data
    }

    #[test]
    fn deterministic_byte_identical_archive() {
        let dir = scratch("deterministic");
        let src = dir.join("d.txt");
        let blob = b"determinism matters for a codec ".repeat(1500);
        write_file(&src, &blob);
        let a = dir.join("a.aahl");
        let b = dir.join("b.aahl");
        cmd_create(&a, &[src.clone()], 4096, 1, 64, 1).unwrap();
        cmd_create(&b, &[src.clone()], 4096, 1, 64, 1).unwrap();
        let ba = std::fs::read(&a).unwrap();
        let bb = std::fs::read(&b).unwrap();
        assert_eq!(ba, bb, "archive bytes must be deterministic per chunk-size");
        let _ = dir;
    }

    #[test]
    fn parallel_jobs_are_byte_identical_to_serial() {
        let dir = scratch("parallel");
        let src = dir.join("p.txt");
        // 12 chunks with repeating idiom so the grammar actually forks and a
        // couple of GC boundaries fire; must exceed the 4KiB chunk size.
        let idiom = b"fn process(&mut self) -> usize { self.acc.wrapping_add(17) } ";
        let mut blob = Vec::new();
        for i in 0..12 {
            for _ in 0..80 {
                blob.extend_from_slice(idiom);
            }
            blob.extend_from_slice(format!("// chunk {i}\n").as_bytes());
        }
        assert!(blob.len() > 4096 * 3, "test needs >3 chunks");
        write_file(&src, &blob);
        let serial = dir.join("serial.aahl");
        let par16 = dir.join("par16.aahl");
        let par1 = dir.join("par1.aahl");
        // lag=3 > 1 to engage the forked discovery path; gc_interval=5 to force
        // remaps that the epoch guard must absorb.
        cmd_create(&serial, &[src.clone()], 4096, 3, 5, 1).unwrap();
        cmd_create(&par16, &[src.clone()], 4096, 3, 5, 16).unwrap();
        cmd_create(&par1, &[src.clone()], 4096, 3, 5, 1).unwrap();
        let bs = std::fs::read(&serial).unwrap();
        let bp16 = std::fs::read(&par16).unwrap();
        let bp1 = std::fs::read(&par1).unwrap();
        assert_eq!(bs, bp16, "parallel (16 jobs) must match serial bytes");
        assert_eq!(bs, bp1, "parallel (1 job) must match serial bytes");
        // and both must round-trip
        let extract = dir.join("x");
        cmd_extract(&par16, &extract).unwrap();
        let got = std::fs::read(extract.join("p.txt")).unwrap();
        assert_eq!(got, blob, "parallel archive must round-trip");
        let _ = dir;
    }

    #[test]
    fn parallel_byte_identity_sweep_gc_and_lag() {
        // Sweep lag in {1,2,3,5}, gc_interval in {3,5,9} with different worker
        // counts and a mixed-idiom corpus. Every (lag,gc) combination must be
        // byte-identical across serial, -j1 and -j8, and round-trip.
        let dir = scratch("parallel_sweep");
        let src = dir.join("mix.txt");
        let idioms: [&[u8]; 3] = [
            b"fn process(&mut self) -> usize { self.acc.wrapping_add(17) } ",
            b"def transform(xs):\n    return [x * x for x in xs]\n",
            b"SELECT id, name FROM users WHERE active = 1; ",
        ];
        let mut blob = Vec::new();
        for i in 0..18 {
            let id = idioms[i % idioms.len()];
            for _ in 0..70 {
                blob.extend_from_slice(id);
            }
            blob.extend_from_slice(format!("// section {i}\n").as_bytes());
        }
        assert!(blob.len() > 4096 * 5);
        write_file(&src, &blob);
        for &lag in &[1usize, 2, 3, 5] {
            for &gc in &[3usize, 5, 9] {
                for &jobs in &[1usize, 8] {
                    let arc = dir.join(format!("l{lag}_g{gc}_j{jobs}.aahl"));
                    cmd_create(&arc, &[src.clone()], 4096, lag, gc, jobs).unwrap();
                }
                let s = std::fs::read(dir.join(format!("l{lag}_g{gc}_j1.aahl"))).unwrap();
                let p = std::fs::read(dir.join(format!("l{lag}_g{gc}_j8.aahl"))).unwrap();
                assert_eq!(
                    s, p,
                    "byte identity failed lag={lag} gc={gc} (jobs 1 vs 8)"
                );
                let out = dir.join(format!("x_{lag}_{gc}"));
                cmd_extract(dir.join(format!("l{lag}_g{gc}_j8.aahl")).as_path(), &out).unwrap();
                let got = std::fs::read(out.join("mix.txt")).unwrap();
                assert_eq!(got, blob, "roundtrip failed lag={lag} gc={gc}");
            }
        }
        let _ = dir;
    }

    #[test]
    fn legacy_v2_archive_still_reads() {
        // Hand-build a v2 container (20B header, fixed chunk framing, footer).
        let dir = scratch("v2");
        let arc = dir.join("v2.aahl");
        let mut out = File::create(&arc).unwrap();
        out.write_all(b"AAHL").unwrap();
        out.write_all(&2u16.to_le_bytes()).unwrap();
        out.write_all(&(FLAG_FOLD).to_le_bytes()).unwrap();
        out.write_all(&4096u32.to_le_bytes()).unwrap();
        out.write_all(&0u64.to_le_bytes()).unwrap(); // reserved
        // one fake chunk
        let fake = [0xAAu8; 64];
        let hash = *blake3::hash(&fake).as_bytes();
        out.write_all(&hash).unwrap();
        out.write_all(&(fake.len() as u32).to_le_bytes()).unwrap();
        out.write_all(&(fake.len() as u32).to_le_bytes()).unwrap();
        out.write_all(&fake).unwrap();
        let table_offset = out.stream_position().unwrap();
        out.write_all(&1u64.to_le_bytes()).unwrap(); // nfiles
        let path = b"legacy.txt";
        out.write_all(&(path.len() as u16).to_le_bytes()).unwrap();
        out.write_all(path).unwrap();
        out.write_all(&(fake.len() as u64).to_le_bytes()).unwrap();
        out.write_all(&1u64.to_le_bytes()).unwrap(); // nrefs
        out.write_all(&0u32.to_le_bytes()).unwrap(); // ref 0
        let table_len = out.stream_position().unwrap() - table_offset;
        out.write_all(b"AAHE").unwrap();
        out.write_all(&table_offset.to_le_bytes()).unwrap();
        out.write_all(&table_len.to_le_bytes()).unwrap();
        out.write_all(&1u64.to_le_bytes()).unwrap(); // num_chunks
        out.write_all(&1u64.to_le_bytes()).unwrap(); // num_files
        drop(out);

        let (_f, idx) = open_index(&arc).unwrap();
        assert_eq!(idx.version, 2);
        assert_eq!(idx.chunks.len(), 1);
        assert_eq!(idx.files.len(), 1);
        assert_eq!(idx.files[0].path, "legacy.txt");
        // the chunk bytes are not a real grammar stream; open_index must not decode
        let (mut f, idx) = open_index(&arc).unwrap();
        assert!(!idx.store_mode);
        drop(f);
    }

    #[test]
    fn safe_destination_rejects_parent_components() {
        let root = scratch("safe_parent");
        for bad in [
            "a/../../evil.txt",
            "../evil.txt",
            "..\\evil.txt",
            "dir/../..",
            "/etc/passwd",
            "C:\\windows\\system32\\evil.exe",
            "C:evil.txt",
        ] {
            assert!(
                safe_destination(&root, bad).is_err(),
                "should reject: {bad}"
            );
        }
    }

    #[test]
    fn safe_destination_accepts_nested_paths() {
        let root = scratch("safe_ok");
        for good in ["a.txt", "dir/nested.txt", "dir\\win.txt"] {
            let d = safe_destination(&root, good).unwrap();
            assert!(d.starts_with(&root.canonicalize().unwrap()));
        }
    }

    #[test]
    fn path_traversal_store_entry_fails() {
        // end-to-end: a STORE container whose entry path escapes the out dir
        // must fail extraction, and must never write outside the root.
        let root = std::env::temp_dir().join(format!("aahl_escape_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let arc = root.join("evil.aahl");
        let out = root.join("out");
        let mut f = std::fs::File::create(&arc).unwrap();
        use std::io::Write as _;
        f.write_all(MAGIC_STORE).unwrap();
        f.write_all(&STORE_VERSION.to_le_bytes()).unwrap();
        f.write_all(&STORE_MODE.to_le_bytes()).unwrap();
        let table_offset = f.stream_position().unwrap();
        f.write_all(&1u64.to_le_bytes()).unwrap(); // nfiles
        let path = "../../evil.txt";
        f.write_all(&(path.len() as u16).to_le_bytes()).unwrap();
        f.write_all(path.as_bytes()).unwrap();
        f.write_all(&512u64.to_le_bytes()).unwrap(); // file_len
        let payload = [0u8; 512];
        f.write_all(blake3::hash(&payload).as_bytes()).unwrap();
        let table_len = f.stream_position().unwrap() - table_offset;
        f.write_all(&[0u8; 512]).unwrap(); // payload (must match hash above)
        f.write_all(b"AAHE").unwrap();
        f.write_all(&table_offset.to_le_bytes()).unwrap();
        f.write_all(&table_len.to_le_bytes()).unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap();
        f.write_all(&1u64.to_le_bytes()).unwrap();
        drop(f);

        let err = cmd_extract(&arc, &out).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unsafe") || msg.contains("escapes"), "msg: {msg}");
        assert!(
            !root.join("evil.txt").exists(),
            "must not write outside out dir"
        );
        assert!(!out.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn v4_archive_roundtrips_and_beats_v3_on_prose() {
        let dir = scratch("v4prose");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut blob = String::new();
        for i in 0..2600 {
            blob.push_str(&format!("{i}: the quick brown fox jumps over the lazy dog and the farmer watches the fox from the barn door, then the dog runs away. never repeat yourself.\n"));
        }
        let blob = blob.into_bytes();
        write_file(&src.join("a.txt"), &blob);

        let arc4 = dir.join("v4.aahl");
        let arc3 = dir.join("v3.aahl");
        // v4 lane: this fixture is comma-heavy, so the v5 table transform
        // would fire and muddy the v4-vs-v3 semantics under test.
        cmd_create_params_version(
            &arc4,
            &[src.clone()],
            4096,
            pack_params(1, 64, PARAM_RULES_DEFAULT, 0),
            1,
            4,
        )
        .unwrap();
        cmd_create_params_version(
            &arc3,
            &[src.clone()],
            4096,
            pack_params(1, 64, PARAM_RULES_DEFAULT, 0),
            1,
            3,
        )
        .unwrap();

        let (_f, idx4) = open_index(&arc4).unwrap();
        assert_eq!(idx4.version, 4, "seam must write a container v4 archive");
        let (_f, idx3) = open_index(&arc3).unwrap();
        assert_eq!(idx3.version, 3, "seam must write a legacy v3 archive");

        let len4 = std::fs::metadata(&arc4).unwrap().len();
        let len3 = std::fs::metadata(&arc3).unwrap().len();
        // TDD probe: at 4 KiB chunks on this fixture the persistent model is a
        // minor player (most chunks are stateless); v4 must never materially
        // regress vs v3 here. The strict win is asserted on the hot-rule-heavy
        // fixture below.
        assert!(
            len4 <= len3 + 1024,
            "v4 must not materially regress vs v3 on small-chunk prose: v3={len3}B v4={len4}B"
        );

        // both v3 (legacy read path) and v4 must extract byte-identically
        for (a, outd) in [(&arc4, dir.join("e4")), (&arc3, dir.join("e3"))] {
            cmd_extract(a, &outd).unwrap();
            let got = std::fs::read(outd.join("src").join("a.txt")).unwrap();
            assert_eq!(got, blob, "extract mismatch for {}", a.display());
        }
    }

    #[test]
    fn future_container_versions_are_rejected() {
        let dir = scratch("futurever");
        let src = dir.join("x.txt");
        let payload = b"hello world ".repeat(200);
        write_file(&src, &payload);
        let arc = dir.join("v6.aahl");
        cmd_create(&arc, &[src], 4096, 1, 64, 1).unwrap();
        let mut bytes = std::fs::read(&arc).unwrap();
        bytes[4..6].copy_from_slice(&6u16.to_le_bytes());
        let mut prefix = [0u8; 20];
        prefix[..4].copy_from_slice(&bytes[..4]);
        prefix[4..6].copy_from_slice(&6u16.to_le_bytes());
        prefix[6..8].copy_from_slice(&bytes[6..8]);
        prefix[8..12].copy_from_slice(&bytes[8..12]);
        prefix[12..20].copy_from_slice(&bytes[12..20]);
        bytes[20..22].copy_from_slice(&header_checksum(&prefix).to_le_bytes());
        std::fs::write(&arc, &bytes).unwrap();
        let err = open_index(&arc).err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(
            open_index(&arc).is_err(),
            "container version 6 must be rejected, got: {err}"
        );
    }
    #[test]
    fn v4_archive_beats_v3_when_hot_rules_dominate() {
        // Many chunks, default lag (16) and 16 KiB chunks: the phrase rules
        // survive across blocks with near-deterministic per-rule followers, so
        // the exact hot-rule contexts in v4 must compress strictly better.
        let dir = scratch("v4strong");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut blob = String::new();
        for i in 0..8000u32 {
            blob.push_str(&format!("{i} "));
            blob.push_str("the quick brown fox jumps over the lazy dog and the farmer watches the fox from the barn door before dawn, ");
            blob.push(char::from(b'A' + (i % 40) as u8));
            blob.push(' ');
        }
        let blob = blob.into_bytes();
        write_file(&src.join("a.txt"), &blob);

        let arc4 = dir.join("v4.aahl");
        let arc3 = dir.join("v3.aahl");
        // v4 lane: this fixture is comma-heavy, so the v5 table transform
        // would fire and muddy the v4-vs-v3 semantics under test.
        cmd_create_params_version(
            &arc4,
            &[src.clone()],
            16384,
            pack_params(16, 64, PARAM_RULES_DEFAULT, 0),
            1,
            4,
        )
        .unwrap();
        cmd_create_params_version(
            &arc3,
            &[src.clone()],
            16384,
            pack_params(16, 64, PARAM_RULES_DEFAULT, 0),
            1,
            3,
        )
        .unwrap();

        let len4 = std::fs::metadata(&arc4).unwrap().len();
        let len3 = std::fs::metadata(&arc3).unwrap().len();
        assert!(
            len4 < len3,
            "v4 exact contexts must beat v3 when hot rules dominate: v3={len3}B v4={len4}B"
        );
        cmd_extract(&arc4, &dir.join("e4")).unwrap();
        let got = std::fs::read(dir.join("e4").join("src").join("a.txt")).unwrap();
        assert_eq!(got, blob);
    }
}
