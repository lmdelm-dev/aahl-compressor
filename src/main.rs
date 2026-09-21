mod aahl;
mod arith;
mod bench;
mod binning;
mod corpus;
mod grammar;
mod spectral;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Take, Write};
use std::path::{Path, PathBuf};

const MAGIC_HDR: &[u8; 4] = b"AAHL";
const MAGIC_FTR: &[u8; 4] = b"AAHE";
const MAGIC_STORE: &[u8; 2] = b"AS"; // STORE container magic (offset 0)
const VERSION: u16 = 3;
const FLAG_FOLD: u16 = 0x0001;
const FLAG_GLOBAL: u16 = 0x0002; // persistent cross-block grammar (v2 blocks)
const FLAG_SNAPSHOT: u16 = 0x0004; // reserved (Phase 3 snapshot histories)
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
const RECORD_GC: u8 = 0x02; // reserved (Phase 2 GC records; never emitted in P1)
const STORE_VERSION: u16 = 1;
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
        archive: PathBuf,
        inputs: Vec<PathBuf>,
        #[arg(long, default_value_t = 65_536)]
        chunk_size: u32,
    },
    /// List contents
    List { archive: PathBuf },
    /// Extract archive
    Extract { archive: PathBuf, out_dir: PathBuf },
    /// Report per-mode block sizes for a single file (dev/diagnostic)
    #[command(name = "blocksize")]
    BlockSize { input: PathBuf, #[arg(long)] sweep: bool },
    /// Build the standard benchmark corpus set (deterministic)
    #[command(name = "corpus")]
    BuildCorpus {
        set_dir: PathBuf,
        #[arg(long)]
        source: Option<PathBuf>,
    },
    /// Run the benchmark suite over a corpus set, incl. reference tools
    #[command(name = "bench")]
    RunBench {
        set_dir: PathBuf,
        #[arg(long, default_value = "bench_results.tsv")]
        tsv: PathBuf,
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

fn cmd_create(archive: &Path, inputs: &[PathBuf], chunk_size: u32) -> Result<()> {
    if !(4096..=1_048_576).contains(&chunk_size) {
        bail!("chunk-size must be 4KiB..1MiB (fold is O(n*m), keep small)");
    }
    let files = collect_files(inputs)?;
    let mut out = File::create(archive).with_context(|| format!("create {}", archive.display()))?;

    let params = pack_params(PARAM_LAG_DEFAULT, PARAM_GC_DEFAULT, PARAM_RULES_DEFAULT, 0);
    // 20-byte v3 prefix: magic(4) | version u16 | flags u16 | chunk_size u32 | params u64
    let mut prefix = [0u8; 20];
    prefix[..4].copy_from_slice(MAGIC_HDR);
    prefix[4..6].copy_from_slice(&VERSION.to_le_bytes());
    prefix[6..8].copy_from_slice(&(FLAG_FOLD | FLAG_GLOBAL).to_le_bytes());
    prefix[8..12].copy_from_slice(&chunk_size.to_le_bytes());
    prefix[12..20].copy_from_slice(&params.to_le_bytes());
    out.write_all(&prefix)?;
    write_u16(&mut out, header_checksum(&prefix))?; // HEADER_LEN_V3 == 22

    let mut chunk_index: HashMap<[u8; 32], u32> = HashMap::new();
    let mut chunks: Vec<ChunkMeta> = Vec::new();
    let mut entries: Vec<FileEntry> = Vec::new();
    // One persistent grammar owns the whole archive; it advances only for
    // chunks that actually get emitted (DATA records). Dedup-skipped pieces
    // are never encoded, so they never touch it — the decoder never re-reads them.
    let mut grammar = grammar::PersistentGrammar::default();

    for (arc_name, disk_path) in &files {
        let data =
            std::fs::read(disk_path).with_context(|| format!("read {}", disk_path.display()))?;
        let mut refs: Vec<u32> = Vec::new();
        if !data.is_empty() {
            for piece in data.chunks(chunk_size as usize) {
                let hash = *blake3::hash(piece).as_bytes();
                if let Some(&idx) = chunk_index.get(&hash) {
                    refs.push(idx);
                    continue;
                }
                // AAHL core: custom fold codec + persistent grammar, NOT zstd/LZ.
                let packed = grammar.compress(piece);
                let idx = chunks.len() as u32;
                // v3 DATA record: kind u8 | body_len u32 | hash(32) | unpacked_len u32 | packed
                out.write_all(&[RECORD_DATA])?;
                let body_len = 32u32 + 4 + packed.len() as u32;
                write_u32(&mut out, body_len)?;
                out.write_all(&hash)?;
                write_u32(&mut out, piece.len() as u32)?;
                let data_off = out.stream_position()?;
                out.write_all(&packed)?;
                chunk_index.insert(hash, idx);
                chunks.push(ChunkMeta {
                    hash,
                    unpacked_len: piece.len() as u32,
                    packed_len: packed.len() as u32,
                    offset: data_off,
                });
                refs.push(idx);
            }
        }
        entries.push(FileEntry {
            path: arc_name.clone(),
            file_len: data.len() as u64,
            refs,
        });
    }

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

/// STORE container (6B magic+version+mode, raw payload, standard footer).
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
        let file_len = std::fs::metadata(disk_path)
            .with_context(|| format!("stat {}", disk_path.display()))?
            .len();
        write_u16(out, pb.len() as u16)?;
        out.write_all(pb)?;
        write_u64(out, file_len)?;
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
    if !(1..=3).contains(&ver) {
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

    // Chunk region. v1/v2 use a fixed [hash|unpacked|packed|packed] table;
    // v3 uses record framing: kind u8 | body_len u32 | body. DATA records
    // carry compressed chunks; GC records are validated and skipped.
    let mut chunks = Vec::with_capacity(num_chunks);
    f.seek(SeekFrom::Start(header_len))?;
    if ver >= 3 {
        while f.stream_position()? < table_offset {
            let kind = read_u8(&mut f)?;
            let body_len = read_u32(&mut f)? as u64;
            match kind {
                RECORD_DATA => {
                    let mut hash = [0u8; 32];
                    f.read_exact(&mut hash)?;
                    let unpacked = read_u32(&mut f)?;
                    let packed = body_len - 32 - 4; // hash + unpacked_len
                    if body_len < 36 || table_offset - f.stream_position()? < packed {
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
                    f.seek(SeekFrom::Current(body_len as i64))?;
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
        },
    ))
}

/// STORE container reader. Layout (offset 0): magic b"AS" (2) | version u16 |
/// mode u16, then the file table (nfiles u64, then per file path_len u16 +
/// path + file_len u64, no chunk refs), then the raw concatenated payload.
/// Suffix is the same footer as compressed containers.
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
    let mut files = Vec::with_capacity(nfiles);
    for _ in 0..nfiles {
        let pl = read_u16(&mut f)? as usize;
        let mut pb = vec![0u8; pl];
        f.read_exact(&mut pb)?;
        let path = String::from_utf8(pb).context("non-utf8 path")?;
        let file_len = read_u64(&mut f)?;
        files.push(FileEntry {
            path,
            file_len,
            refs: Vec::new(),
        });
    }
    let store_payload = f.stream_position()?;
    let total: u64 = files.iter().map(|e| e.file_len).sum();
    if store_payload + total != flen - FOOTER_LEN {
        bail!("store payload length mismatch (truncated?)");
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
        },
    ))
}

fn read_chunk(f: &mut File, meta: &ChunkMeta) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(meta.offset))?;
    let mut packed = vec![0u8; meta.packed_len as usize];
    f.read_exact(&mut packed)?;
    let raw = aahl::decompress_block(&packed, meta.unpacked_len as usize)?;
    if raw.len() as u32 != meta.unpacked_len {
        bail!("chunk size mismatch after decode");
    }
    let h = *blake3::hash(&raw).as_bytes();
    if h != meta.hash {
        bail!("chunk hash mismatch (corrupt)");
    }
    Ok(raw)
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

fn cmd_extract(archive: &Path, out_dir: &Path) -> Result<()> {
    let (mut f, idx) = open_index(archive)?;
    std::fs::create_dir_all(out_dir)?;

    if idx.store_mode {
        f.seek(SeekFrom::Start(idx.store_payload))?;
        for e in &idx.files {
            let dest = out_dir.join(&e.path);
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

    // v2/v3 persistent grammar: chunk decode order is the honest contract, so
    // decode every chunk once in index order through a single grammar and
    // serve the file references from the full cache.
    let ordered_cache: HashMap<u32, Vec<u8>> = if idx.flags & FLAG_GLOBAL != 0 {
        let mut g = grammar::PersistentGrammar::default();
        let mut cache: HashMap<u32, Vec<u8>> = HashMap::with_capacity(idx.chunks.len());
        for (i, meta) in idx.chunks.iter().enumerate() {
            let raw = read_chunk_grammar(&mut f, meta, &mut g)?;
            cache.insert(i as u32, raw);
        }
        cache
    } else {
        HashMap::new()
    };
    let mut cache: HashMap<u32, Vec<u8>> = HashMap::new();

    for e in &idx.files {
        let dest = out_dir.join(&e.path);
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
                let raw = read_chunk(&mut f, meta)?;
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
    g: &mut grammar::PersistentGrammar,
) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(meta.offset))?;
    let mut packed = vec![0u8; meta.packed_len as usize];
    f.read_exact(&mut packed)?;
    let raw = g.decompress(&packed, meta.unpacked_len as usize)?;
    if raw.len() as u32 != meta.unpacked_len {
        bail!("chunk size mismatch after decode");
    }
    let h = *blake3::hash(&raw).as_bytes();
    if h != meta.hash {
        bail!("chunk hash mismatch (corrupt)");
    }
    Ok(raw)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Create {
            archive,
            inputs,
            chunk_size,
        } => cmd_create(&archive, &inputs, chunk_size),
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
        Cmd::RunBench { set_dir, tsv } => {
            bench::run_bench(&set_dir, &tsv)?;
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

        cmd_create(&arc, &[src.clone()], 4096).unwrap();
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
        cmd_create(&arc, &[src.clone()], 4096).unwrap();

let (_f, idx) = open_index(&arc).unwrap();
        assert!(idx.store_mode);
        let path_len = idx.files[0].path.len() as u64;
        let arc_len = std::fs::metadata(&arc).unwrap().len();
        // STORE: 6B container + 8B nfiles + (2+path+8) path entry + raw + 36B footer
        let overhead = 6u64 + 8 + (2 + path_len + 8) + FOOTER_LEN;
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
        cmd_create(&arc, &[src.clone()], 4096).unwrap();
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
        cmd_create(&arc, &[src.clone()], 4096).unwrap();
        let (_f, idx) = open_index(&arc).unwrap();
        assert!(idx.store_mode, "empty file should take store path");
        assert_eq!(idx.files.len(), 1);
        assert_eq!(idx.files[0].file_len, 0);
        let outd = dir.join("ex");
        cmd_extract(&arc, &outd).unwrap();
        assert_eq!(std::fs::read(outd.join("e.txt")).unwrap(), b"");
    }

    #[test]
    fn deterministic_byte_identical_archive() {
        let dir = scratch("deterministic");
        let src = dir.join("d.txt");
        let blob = b"determinism matters for a codec ".repeat(1500);
        write_file(&src, &blob);
        let a = dir.join("a.aahl");
        let b = dir.join("b.aahl");
        cmd_create(&a, &[src.clone()], 4096).unwrap();
        cmd_create(&b, &[src.clone()], 4096).unwrap();
        let ba = std::fs::read(&a).unwrap();
        let bb = std::fs::read(&b).unwrap();
        assert_eq!(ba, bb, "archive bytes must be deterministic per chunk-size");
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
}
