use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::{aahl, context};

#[derive(Clone, Debug)]
pub struct Options {
    pub runs: usize,
    pub chunk_size: usize,
    pub runs_tsv: PathBuf,
    pub summary_tsv: PathBuf,
}

#[derive(Clone, Debug)]
struct Row {
    input: String,
    chunk: u64,
    run: usize,
    mode: String,
    raw_bytes: usize,
    total_bytes: usize,
    framing_bytes: usize,
    payload_bytes: usize,
    model_bytes: usize,
    fold_bytes: usize,
    encode_us: u64,
    decode_us: u64,
    ok: bool,
}

impl Row {
    fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.input,
            self.chunk,
            self.run,
            self.mode,
            self.raw_bytes,
            self.total_bytes,
            self.framing_bytes,
            self.payload_bytes,
            self.model_bytes,
            self.fold_bytes,
            self.encode_us,
            self.decode_us,
            if self.ok { 1 } else { 0 }
        )
    }
}

fn median(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn write_tsv(path: &Path, header: &str, rows: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut text = String::with_capacity(rows.len() * 100 + header.len() + 2);
    text.push_str(header);
    text.push('\n');
    for row in rows {
        text.push_str(row);
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn display_path(path: &Path) -> String {
    let relative = std::env::current_dir()
        .ok()
        .and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf());
    relative.to_string_lossy().replace('\\', "/")
}

fn visit_directory(directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(directory).with_context(|| format!("read_dir {}", directory.display()))?
    {
        entries.push(entry?.path());
    }
    entries.sort();
    for path in entries {
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name == "work" || name == ".git" || name == "target" {
                continue;
            }
            visit_directory(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn collect_files(inputs: &[PathBuf]) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for input in inputs {
        if !input.exists() {
            bail!("input not found: {}", input.display());
        }
        if input.is_file() {
            files.push(input.clone());
        } else if input.is_dir() {
            let mut nested = Vec::new();
            visit_directory(input, &mut nested)?;
            files.extend(nested);
        } else {
            bail!("input is neither file nor directory: {}", input.display());
        }
    }
    files.sort();
    files.dedup();
    let mut result = Vec::new();
    for path in files {
        let label = display_path(&path);
        result.push((label, path));
    }
    if result.is_empty() {
        bail!("no input files found");
    }
    Ok(result)
}

fn fold_frame_bytes(rules: usize) -> usize {
    274 + rules * 5
}

fn token_frame_bytes(rules: usize) -> usize {
    14 + rules * 4
}

fn byte_frame_bytes() -> usize {
    10
}

fn context_frame_bytes(rules: usize) -> usize {
    18 + rules * 4
}

fn baseline_row(
    input: &str,
    chunk: u64,
    mode: &str,
    raw_bytes: usize,
    total_bytes: usize,
    framing_bytes: usize,
    fold_bytes: usize,
) -> Row {
    let total_bytes = if raw_bytes == 0 { 6 } else { total_bytes };
    let framing_bytes = if raw_bytes == 0 { 6 } else { framing_bytes };
    Row {
        input: input.to_string(),
        chunk,
        run: 0,
        mode: mode.to_string(),
        raw_bytes,
        total_bytes,
        framing_bytes,
        payload_bytes: total_bytes.saturating_sub(framing_bytes),
        model_bytes: 0,
        fold_bytes: if raw_bytes == 0 { 6 } else { fold_bytes },
        encode_us: 0,
        decode_us: 0,
        ok: true,
    }
}

fn process_chunk(
    rows: &mut Vec<Row>,
    input: &str,
    chunk: u64,
    raw: &[u8],
    opts: &Options,
) -> Result<()> {
    let sizes = if raw.is_empty() {
        aahl::ModeSizes::default()
    } else {
        aahl::mode_sizes(raw)
    };
    let (rule_count, _, _) = if raw.is_empty() {
        (0, 0, 0)
    } else {
        aahl::arith_sizes(raw, &aahl::FoldConfig::default())
    };
    let fold_bytes = if raw.is_empty() { 6 } else { sizes.fold };
    let fold = baseline_row(
        input,
        chunk,
        "fold",
        raw.len(),
        fold_bytes,
        fold_frame_bytes(rule_count),
        fold_bytes,
    );
    let order0 = baseline_row(
        input,
        chunk,
        "order0",
        raw.len(),
        sizes.order0,
        token_frame_bytes(rule_count),
        fold_bytes,
    );
    let order1_tok = baseline_row(
        input,
        chunk,
        "order1_tok",
        raw.len(),
        sizes.order1_tok,
        token_frame_bytes(rule_count),
        fold_bytes,
    );
    let order1_byte = baseline_row(
        input,
        chunk,
        "order1_byte",
        raw.len(),
        sizes.order1_byte,
        byte_frame_bytes(),
        fold_bytes,
    );
    rows.push(fold);
    rows.push(order0);
    rows.push(order1_tok);
    rows.push(order1_byte);

    let mut previous: Option<Vec<u8>> = None;
    for run in 0..opts.runs {
        let start = Instant::now();
        let block =
            context::encode(raw).map_err(|error| anyhow::anyhow!("context encode: {error}"))?;
        let encoded_at = Instant::now();
        let decoded = context::decode(&block, raw.len())
            .map_err(|error| anyhow::anyhow!("context decode: {error}"))?;
        let decoded_at = Instant::now();
        let mut ok = decoded == raw;
        if let Some(first) = &previous {
            if *first != block {
                ok = false;
            }
        } else {
            previous = Some(block.clone());
        }
        let framing = if raw.is_empty() {
            6
        } else {
            let rule_count = if block.len() >= 6 {
                u32::from_le_bytes([block[2], block[3], block[4], block[5]]) as usize
            } else {
                0
            };
            context_frame_bytes(rule_count)
        };
        let total = block.len();
        let payload = total.saturating_sub(framing);
        if total != framing + payload {
            ok = false;
        }
        rows.push(Row {
            input: input.to_string(),
            chunk,
            run,
            mode: "context".to_string(),
            raw_bytes: raw.len(),
            total_bytes: total,
            framing_bytes: framing,
            payload_bytes: payload,
            model_bytes: 0,
            fold_bytes: if raw.is_empty() { 6 } else { fold_bytes },
            encode_us: (encoded_at - start).as_micros() as u64,
            decode_us: (decoded_at - encoded_at).as_micros() as u64,
            ok,
        });
    }
    Ok(())
}

#[derive(Default)]
struct Summary {
    raw: usize,
    total: usize,
    framing: usize,
    payload: usize,
    model: usize,
    encode: Vec<u64>,
    decode: Vec<u64>,
    ok: bool,
}

fn summarize(rows: &[Row]) -> Vec<String> {
    let mut grouped: BTreeMap<(String, String), Summary> = BTreeMap::new();
    for row in rows {
        let entry = grouped
            .entry((row.input.clone(), row.mode.clone()))
            .or_insert_with(|| Summary {
                ok: true,
                ..Summary::default()
            });
        entry.encode.push(row.encode_us);
        entry.decode.push(row.decode_us);
        entry.ok &= row.ok;
        if row.mode != "context" || row.run == 0 {
            entry.raw += row.raw_bytes;
            entry.total += row.total_bytes;
            entry.framing += row.framing_bytes;
            entry.payload += row.payload_bytes;
            entry.model += row.model_bytes;
        }
    }
    let mut fold_totals = BTreeMap::new();
    for row in rows {
        if row.mode == "fold" {
            *fold_totals.entry(row.input.clone()).or_insert(0usize) += row.total_bytes;
        }
    }
    let mut output = Vec::new();
    for ((input, mode), summary) in grouped {
        let reference = fold_totals.get(&input).copied().unwrap_or(0);
        let delta = summary.total as i64 - reference as i64;
        let pct = if reference == 0 {
            0.0
        } else {
            delta as f64 * 100.0 / reference as f64
        };
        output.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.4}\t{:.3}\t{:.3}\t{}",
            input,
            mode,
            summary.raw,
            summary.total,
            summary.framing,
            summary.payload,
            summary.model,
            delta,
            pct,
            median(&summary.encode) as f64 / 1000.0,
            median(&summary.decode) as f64 / 1000.0,
            if summary.ok { 1 } else { 0 }
        ));
    }
    output
}

pub fn run_context_bench(inputs: &[PathBuf], opts: &Options) -> Result<()> {
    if inputs.is_empty() {
        bail!("bench-ctx: no inputs");
    }
    if opts.runs == 0 {
        bail!("bench-ctx: --runs must be at least 1");
    }
    if opts.chunk_size == 0 {
        bail!("bench-ctx: --chunk-size must be at least 1");
    }
    let files = collect_files(inputs)?;
    let file_count = files.len();
    let mut rows = Vec::new();
    for (label, path) in &files {
        let data = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if data.is_empty() {
            process_chunk(&mut rows, label, 0, &[], opts)?;
        } else {
            for (chunk, piece) in data.chunks(opts.chunk_size).enumerate() {
                process_chunk(&mut rows, label, chunk as u64, piece, opts)
                    .with_context(|| format!("measure {}", path.display()))?;
            }
        }
    }
    let raw_header = "input\tchunk\trun\tmode\traw_bytes\ttotal_bytes\tframing_bytes\tpayload_bytes\tmodel_bytes\tfold_bytes\tencode_us\tdecode_us\tok";
    let raw_rows: Vec<String> = rows.iter().map(Row::to_tsv).collect();
    write_tsv(&opts.runs_tsv, raw_header, &raw_rows)?;
    let summary_header = "input\tmode\traw_bytes\ttotal_bytes\tframing_bytes\tpayload_bytes\tmodel_bytes\tdelta_vs_fold_bytes\tdelta_vs_fold_pct\tencode_ms\tdecode_ms\tok";
    let summary_rows = summarize(&rows);
    write_tsv(&opts.summary_tsv, summary_header, &summary_rows)?;
    let failed = rows.iter().any(|row| !row.ok);
    println!(
        "bench-ctx: {} files, {} rows, summary {}",
        file_count,
        rows.len(),
        opts.summary_tsv.display()
    );
    if failed {
        bail!("bench-ctx: round-trip or determinism check failed");
    }
    Ok(())
}
