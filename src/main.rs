mod aahl;
mod arith;
mod binning;
mod grammar;
mod spectral;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC_HDR: &[u8; 4] = b"AAHL";
const MAGIC_FTR: &[u8; 4] = b"AAHE";
const VERSION: u16 = 2;
const FLAG_FOLD: u16 = 0x0001;
const FLAG_GLOBAL: u16 = 0x0002; // persistent cross-block grammar (v2 blocks)
const HEADER_LEN: u64 = 20;
const FOOTER_LEN: u64 = 36;

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
    Bench { input: PathBuf, #[arg(long)] sweep: bool },
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

    out.write_all(MAGIC_HDR)?;
    write_u16(&mut out, VERSION)?;
    write_u16(&mut out, FLAG_FOLD | FLAG_GLOBAL)?;
    write_u32(&mut out, chunk_size)?;
    write_u64(&mut out, 0)?; // reserved

    let mut chunk_index: HashMap<[u8; 32], u32> = HashMap::new();
    let mut chunks: Vec<ChunkMeta> = Vec::new();
    let mut entries: Vec<FileEntry> = Vec::new();
    // One persistent grammar owns the whole archive; it advances only for
    // chunks that actually get emitted (G blocks). Dedup-skipped pieces are
    // never encoded, so they never touch it — the decoder never re-reads them.
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
                let offset = out.stream_position()?;
                out.write_all(&hash)?;
                write_u32(&mut out, piece.len() as u32)?;
                write_u32(&mut out, packed.len() as u32)?;
                out.write_all(&packed)?;
                chunk_index.insert(hash, idx);
                chunks.push(ChunkMeta {
                    hash,
                    unpacked_len: piece.len() as u32,
                    packed_len: packed.len() as u32,
                    offset,
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
    let total = out.stream_position()?;

    let raw_total: u64 = entries.iter().map(|e| e.file_len).sum();
    println!(
        "aahl: {} files, {} unique chunks ({}B raw -> {}B archive)",
        entries.len(),
        chunks.len(),
        raw_total,
        total
    );
    Ok(())
}

struct ArchiveIndex {
    version: u16,
    flags: u16,
    chunk_size: u32,
    chunks: Vec<ChunkMeta>,
    files: Vec<FileEntry>,
}

fn open_index(archive: &Path) -> Result<(File, ArchiveIndex)> {
    let mut f = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let flen = f.metadata()?.len();
    if flen < HEADER_LEN + FOOTER_LEN {
        bail!("file too small to be aahl");
    }
    let mut hdr = [0u8; 4];
    f.seek(SeekFrom::Start(0))?;
    f.read_exact(&mut hdr)?;
    if &hdr != MAGIC_HDR {
        bail!("bad magic (not aahl)");
    }
    let ver = read_u16(&mut f)?;
    if ver != VERSION && ver != 1 {
        bail!("unsupported version {ver}");
    }
    let flags = read_u16(&mut f)?;
    let chunk_size = read_u32(&mut f)?;
    let _reserved = read_u64(&mut f)?;

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

    let mut chunks = Vec::with_capacity(num_chunks);
    f.seek(SeekFrom::Start(HEADER_LEN))?;
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
    println!("{:>12}  {:>6}  {}", "SIZE", "CHUNKS", "PATH");
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

    // v2 persistent grammar: chunk decode order is the honest contract, so
    // decode every chunk once in index order through a single grammar and
    // serve the file references from the full cache.
    let ordered_cache: HashMap<u32, Vec<u8>> = if idx.version == 2 && (idx.flags & FLAG_GLOBAL) != 0 {
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
        Cmd::Bench { input, sweep } => {
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
    }
}
