use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, Context, Result};

use crate::{aahl, bcj, corpus};

#[derive(Clone, Debug)]
pub struct Options {
    pub runs: usize,
    pub chunk_size: usize,
    pub max_files_per_class: usize,
    pub runs_tsv: PathBuf,
    pub summary_tsv: PathBuf,
    pub with_references: bool,
}

#[derive(Clone)]
struct Sample {
    class: String,
    rel: String,
    path: PathBuf,
    bytes: usize,
    arch: String,
}

impl Sample {
    fn chunks(&self, chunk_size: usize) -> usize {
        if self.bytes == 0 {
            0
        } else {
            self.bytes.div_ceil(chunk_size)
        }
    }
}

struct Row {
    class: String,
    arch: String,
    file: String,
    bytes: usize,
    chunks: usize,
    run: usize,
    lane: String,
    payload_bytes: usize,
    replay_bytes: usize,
    total_bytes: usize,
    raw_bytes: usize,
    changed_bytes: usize,
    transform_ms: u64,
    compress_ms: u64,
    ok: bool,
}

impl Row {
    fn delta_bytes(&self) -> i64 {
        self.total_bytes as i64 - self.raw_bytes as i64
    }

    fn delta_percent(&self) -> f64 {
        if self.raw_bytes == 0 {
            0.0
        } else {
            self.delta_bytes() as f64 * 100.0 / self.raw_bytes as f64
        }
    }

    fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{}\t{}\t{}\t{}",
            self.class,
            self.arch,
            self.file,
            self.bytes,
            self.chunks,
            self.run,
            self.lane,
            self.payload_bytes,
            self.replay_bytes,
            self.total_bytes,
            self.raw_bytes,
            self.delta_percent(),
            self.changed_bytes,
            self.transform_ms,
            self.compress_ms,
            if self.ok { 1 } else { 0 }
        )
    }
}

struct SummaryRow {
    class: String,
    arch: String,
    lane: String,
    run: usize,
    files: usize,
    bytes: usize,
    chunks: usize,
    raw_bytes: usize,
    payload_bytes: usize,
    replay_bytes: usize,
    total_bytes: usize,
    delta_bytes: i64,
    delta_percent: f64,
    median_compress_ms: u64,
    ok: bool,
}

impl SummaryRow {
    fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{}\t{}",
            self.class,
            self.arch,
            self.lane,
            self.run,
            self.files,
            self.bytes,
            self.chunks,
            self.raw_bytes,
            self.payload_bytes,
            self.replay_bytes,
            self.total_bytes,
            self.delta_bytes,
            self.delta_percent,
            self.median_compress_ms,
            if self.ok { 1 } else { 0 }
        )
    }
}

fn select_evenly<T: Clone>(items: &[T], max_items: usize) -> Vec<T> {
    if max_items == 0 || items.is_empty() {
        return Vec::new();
    }
    if items.len() <= max_items {
        return items.to_vec();
    }
    if max_items == 1 {
        return vec![items[0].clone()];
    }
    (0..max_items)
        .map(|i| items[i * (items.len() - 1) / (max_items - 1)].clone())
        .collect()
}

fn pe_architecture(data: &[u8]) -> String {
    if data.len() < 0x88 || &data[0..2] != b"MZ" {
        return "unknown".to_string();
    }
    let pe = u32::from_le_bytes(data[0x3c..0x40].try_into().unwrap()) as usize;
    if pe + 6 > data.len() || &data[pe..pe + 4] != b"PE\0\0" {
        return "unknown".to_string();
    }
    match u16::from_le_bytes(data[pe + 4..pe + 6].try_into().unwrap()) {
        0x014c => "i386".to_string(),
        0x8664 => "amd64".to_string(),
        0xaa64 => "arm64".to_string(),
        _ => "unknown".to_string(),
    }
}

fn manifest_samples(root: &Path, max_files_per_class: usize) -> Result<Vec<Sample>> {
    let manifest = root.join("manifest.tsv");
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("read {}", manifest.display()))?;
    let mut grouped: BTreeMap<String, Vec<(String, PathBuf, usize)>> = BTreeMap::new();
    for (line_no, line) in text.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 4 {
            bail!(
                "{}:{}: expected four TSV fields",
                manifest.display(),
                line_no + 1
            );
        }
        let bytes = fields[2].parse::<usize>().with_context(|| {
            format!("{}:{}: invalid byte count", manifest.display(), line_no + 1)
        })?;
        let stored_name = Path::new(fields[1]).file_name().with_context(|| {
            format!(
                "{}:{}: manifest path has no file name",
                manifest.display(),
                line_no + 1
            )
        })?;
        grouped.entry(fields[0].to_string()).or_default().push((
            fields[1].to_string(),
            root.join(&fields[0]).join(stored_name),
            bytes,
        ));
    }
    if grouped.is_empty() {
        bail!("{} has no samples", manifest.display());
    }

    let mut out = Vec::new();
    for (class, mut files) in grouped {
        files.sort_by(|a, b| a.0.cmp(&b.0));
        for (rel, path, manifest_bytes) in select_evenly(&files, max_files_per_class) {
            let data = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            if data.len() != manifest_bytes {
                bail!(
                    "{}: manifest says {manifest_bytes} bytes, found {}",
                    path.display(),
                    data.len()
                );
            }
            out.push(Sample {
                class: class.clone(),
                rel,
                arch: pe_architecture(&data),
                path,
                bytes: data.len(),
            });
        }
    }
    if out.is_empty() {
        bail!("no corpus files selected");
    }
    Ok(out)
}

fn compress_once(input: &[u8], chunk_size: usize) -> Result<(Vec<u8>, u64)> {
    let start = Instant::now();
    let mut encoded = Vec::new();
    for chunk in input.chunks(chunk_size) {
        encoded.extend_from_slice(&aahl::compress_block(chunk));
    }
    Ok((encoded, start.elapsed().as_millis() as u64))
}

fn transform_once(input: &[u8], base: u64, chunk_size: usize) -> Result<(Vec<u8>, usize, u64)> {
    let start = Instant::now();
    let mut transformed = Vec::with_capacity(input.len());
    let mut changed = 0usize;
    for chunk in input.chunks(chunk_size) {
        let block =
            bcj::forward_terminal(chunk, base).map_err(|e| anyhow::anyhow!("BCJ forward: {e}"))?;
        let restored =
            bcj::inverse_terminal(&block, base).map_err(|e| anyhow::anyhow!("BCJ inverse: {e}"))?;
        if restored != chunk {
            bail!("BCJ roundtrip mismatch");
        }
        changed += block
            .iter()
            .zip(chunk)
            .filter(|(left, right)| left != right)
            .count();
        transformed.extend_from_slice(&block);
    }
    Ok((transformed, changed, start.elapsed().as_millis() as u64))
}

fn find_tool(name: &str) -> Option<PathBuf> {
    corpus::which(name).or_else(|| {
        let candidate = PathBuf::from("C:\\Users\\Pc\\AppData\\Local\\Microsoft\\WinGet\\Links")
            .join(format!("{name}.exe"));
        candidate.is_file().then_some(candidate)
    })
}

fn reference_lane(prog: &Path, lane: &str, input: &Path) -> Result<(usize, u64)> {
    let start = Instant::now();
    let output = if lane == "xz-x86-9e" {
        Command::new(prog)
            .args(["--x86", "--lzma2=preset=9e", "-c"])
            .arg(input)
            .output()
    } else {
        Command::new(prog)
            .args(["-19", "--zstd=wlog=23", "-c"])
            .arg(input)
            .output()
    }
    .with_context(|| format!("run {}", prog.display()))?;
    if !output.status.success() {
        bail!(
            "{} lane failed for {}: {:?}",
            lane,
            input.display(),
            output.status.code()
        );
    }
    Ok((output.stdout.len(), start.elapsed().as_millis() as u64))
}

fn median(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn summarize(rows: &[Row], samples: &[Sample], runs: usize, chunk_size: usize) -> Vec<SummaryRow> {
    let mut out = Vec::new();
    let classes: BTreeSet<&str> = samples.iter().map(|s| s.class.as_str()).collect();
    let mut architectures = vec!["*"];
    architectures.extend(
        samples
            .iter()
            .map(|s| s.arch.as_str())
            .collect::<BTreeSet<_>>(),
    );

    for class in classes {
        for arch in &architectures {
            let selected: Vec<&Sample> = samples
                .iter()
                .filter(|s| s.class == class && (*arch == "*" || s.arch == *arch))
                .collect();
            if selected.is_empty() {
                continue;
            }
            let arch_matches = |row: &Row| *arch == "*" || row.arch == *arch;
            let lanes: BTreeSet<&str> = rows
                .iter()
                .filter(|r| r.class == class && arch_matches(r))
                .map(|r| r.lane.as_str())
                .collect();
            for lane in lanes {
                let max_run = rows
                    .iter()
                    .filter(|r| r.class == class && arch_matches(r) && r.lane == lane)
                    .map(|r| r.run)
                    .max()
                    .unwrap_or(0);
                for run in 0..=max_run.min(runs.saturating_sub(1)) {
                    let lane_rows: Vec<&Row> = rows
                        .iter()
                        .filter(|r| {
                            r.class == class && arch_matches(r) && r.lane == lane && r.run == run
                        })
                        .collect();
                    if lane_rows.is_empty() {
                        continue;
                    }
                    let raw_bytes: usize = rows
                        .iter()
                        .filter(|r| {
                            r.class == class
                                && arch_matches(r)
                                && r.lane == "raw-aahl"
                                && r.run == run
                        })
                        .map(|r| r.total_bytes)
                        .sum();
                    let payload_bytes = lane_rows.iter().map(|r| r.payload_bytes).sum();
                    let replay_bytes = lane_rows.iter().map(|r| r.replay_bytes).sum();
                    let total_bytes = lane_rows.iter().map(|r| r.total_bytes).sum();
                    let delta_bytes = total_bytes as i64 - raw_bytes as i64;
                    let delta_percent = if raw_bytes == 0 {
                        0.0
                    } else {
                        delta_bytes as f64 * 100.0 / raw_bytes as f64
                    };
                    out.push(SummaryRow {
                        class: class.to_string(),
                        arch: (*arch).to_string(),
                        lane: lane.to_string(),
                        run,
                        files: lane_rows
                            .iter()
                            .map(|r| r.file.clone())
                            .collect::<BTreeSet<_>>()
                            .len(),
                        bytes: selected.iter().map(|s| s.bytes).sum(),
                        chunks: selected.iter().map(|s| s.chunks(chunk_size)).sum(),
                        raw_bytes,
                        payload_bytes,
                        replay_bytes,
                        total_bytes,
                        delta_bytes,
                        delta_percent,
                        median_compress_ms: median(
                            &lane_rows.iter().map(|r| r.compress_ms).collect::<Vec<_>>(),
                        ),
                        ok: lane_rows.iter().all(|r| r.ok),
                    });
                }
            }
        }
    }
    out
}

fn write_tsv(path: &Path, header: &str, rows: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
    }
    let mut text = String::with_capacity(
        header.len() + rows.iter().map(String::len).sum::<usize>() + rows.len(),
    );
    text.push_str(header);
    text.push('\n');
    for row in rows {
        text.push_str(row);
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn run_bcj_bench(root: &Path, opts: &Options) -> Result<()> {
    if opts.runs == 0 {
        bail!("bench-bcj: --runs must be at least 1");
    }
    if opts.chunk_size == 0 {
        bail!("bench-bcj: --chunk-size must be greater than 0");
    }
    if opts.max_files_per_class == 0 {
        bail!("bench-bcj: --max-files-per-class must be at least 1");
    }
    if !root.is_dir() {
        bail!(
            "bench-bcj: corpus root is not a directory: {}",
            root.display()
        );
    }

    let samples = manifest_samples(root, opts.max_files_per_class)?;
    let xz = if opts.with_references {
        find_tool("xz")
    } else {
        None
    };
    let zstd = if opts.with_references {
        find_tool("zstd")
    } else {
        None
    };
    if opts.with_references {
        if xz.is_none() {
            eprintln!("bench-bcj: xz unavailable; reference lane omitted");
        }
        if zstd.is_none() {
            eprintln!("bench-bcj: zstd unavailable; reference lane omitted");
        }
    }

    let lanes = [
        ("bcj-base0", bcj::BASE_IP_ZERO),
        ("bcj-x86", bcj::BASE_X86),
        ("bcj-x64", bcj::BASE_X64),
    ];
    let mut rows = Vec::new();

    for sample in &samples {
        eprintln!(
            "bench-bcj: {}/{} {} ({} bytes, {})",
            sample.class,
            sample.rel,
            sample.arch,
            sample.bytes,
            sample.path.display()
        );
        let raw =
            fs::read(&sample.path).with_context(|| format!("read {}", sample.path.display()))?;
        let chunks = sample.chunks(opts.chunk_size);
        let mut first_raw: Option<Vec<u8>> = None;

        for run in 0..opts.runs {
            let (encoded, compress_ms) = compress_once(&raw, opts.chunk_size)?;
            if let Some(first) = &first_raw {
                if first != &encoded {
                    bail!("raw AAHL output changed on run {}", run + 1);
                }
            } else {
                first_raw = Some(encoded.clone());
            }
            let raw_size = encoded.len();
            rows.push(Row {
                class: sample.class.clone(),
                arch: sample.arch.clone(),
                file: sample.rel.clone(),
                bytes: sample.bytes,
                chunks,
                run,
                lane: "raw-aahl".to_string(),
                payload_bytes: raw_size,
                replay_bytes: 0,
                total_bytes: raw_size,
                raw_bytes: raw_size,
                changed_bytes: 0,
                transform_ms: 0,
                compress_ms,
                ok: true,
            });
        }

        for (lane, base) in lanes {
            let mut first_transformed: Option<Vec<u8>> = None;
            let mut first_encoded: Option<Vec<u8>> = None;
            for run in 0..opts.runs {
                let (transformed, changed_bytes, transform_ms) =
                    transform_once(&raw, base, opts.chunk_size)?;
                if let Some(first) = &first_transformed {
                    if first != &transformed {
                        bail!("{lane} output changed on run {}", run + 1);
                    }
                } else {
                    first_transformed = Some(transformed.clone());
                }
                let (encoded, compress_ms) = compress_once(&transformed, opts.chunk_size)?;
                if let Some(first) = &first_encoded {
                    if first != &encoded {
                        bail!("{lane} compressed output changed on run {}", run + 1);
                    }
                } else {
                    first_encoded = Some(encoded.clone());
                }
                let raw_size = first_raw.as_ref().map(Vec::len).unwrap_or(0);
                let total = encoded.len() + 5;
                rows.push(Row {
                    class: sample.class.clone(),
                    arch: sample.arch.clone(),
                    file: sample.rel.clone(),
                    bytes: sample.bytes,
                    chunks,
                    run,
                    lane: lane.to_string(),
                    payload_bytes: encoded.len(),
                    replay_bytes: 5,
                    total_bytes: total,
                    raw_bytes: raw_size,
                    changed_bytes,
                    transform_ms,
                    compress_ms,
                    ok: true,
                });
            }
        }

        let raw_size = first_raw.as_ref().map(Vec::len).unwrap_or(0);
        if let Some(prog) = &xz {
            let (bytes, compress_ms) = reference_lane(prog, "xz-x86-9e", &sample.path)?;
            rows.push(Row {
                class: sample.class.clone(),
                arch: sample.arch.clone(),
                file: sample.rel.clone(),
                bytes: sample.bytes,
                chunks,
                run: 0,
                lane: "xz-x86-9e".to_string(),
                payload_bytes: bytes,
                replay_bytes: 0,
                total_bytes: bytes,
                raw_bytes: raw_size,
                changed_bytes: 0,
                transform_ms: 0,
                compress_ms,
                ok: true,
            });
        }
        if let Some(prog) = &zstd {
            let (bytes, compress_ms) = reference_lane(prog, "zstd-x86-19", &sample.path)?;
            rows.push(Row {
                class: sample.class.clone(),
                arch: sample.arch.clone(),
                file: sample.rel.clone(),
                bytes: sample.bytes,
                chunks,
                run: 0,
                lane: "zstd-x86-19".to_string(),
                payload_bytes: bytes,
                replay_bytes: 0,
                total_bytes: bytes,
                raw_bytes: raw_size,
                changed_bytes: 0,
                transform_ms: 0,
                compress_ms,
                ok: true,
            });
        }
    }

    let runs_header = "class\tarch\tfile\tbytes\tchunks\trun\tlane\tpayload_bytes\treplay_bytes\ttotal_bytes\traw_bytes\tdelta_percent\tchanged_bytes\ttransform_ms\tcompress_ms\tok";
    let run_rows = rows.iter().map(Row::to_tsv).collect::<Vec<_>>();
    write_tsv(&opts.runs_tsv, runs_header, &run_rows)?;

    let summary_header = "class\tarch\tlane\trun\tfiles\tbytes\tchunks\traw_bytes\tpayload_bytes\treplay_bytes\ttotal_bytes\tdelta_bytes\tdelta_percent\tmedian_compress_ms\tok";
    let summaries = summarize(&rows, &samples, opts.runs, opts.chunk_size);
    let summary_rows = summaries.iter().map(SummaryRow::to_tsv).collect::<Vec<_>>();
    write_tsv(&opts.summary_tsv, summary_header, &summary_rows)?;

    println!("CLASS\tlane\tfiles\tbytes\traw_bytes\ttotal_bytes\tdelta_bytes\tdelta_percent");
    for row in summaries.iter().filter(|r| r.arch == "*" && r.run == 0) {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}",
            row.class,
            row.lane,
            row.files,
            row.bytes,
            row.raw_bytes,
            row.total_bytes,
            row.delta_bytes,
            row.delta_percent
        );
    }
    println!(
        "bench-bcj: {} selected files, {} run rows; summary {}",
        samples.len(),
        rows.len(),
        opts.summary_tsv.display()
    );
    if rows.iter().any(|r| !r.ok) {
        bail!("bench-bcj: roundtrip or determinism check failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{pe_architecture, select_evenly, summarize, Row, Sample};

    #[test]
    fn even_sampling_is_deterministic_and_spans_the_list() {
        let items: Vec<usize> = (0..10).collect();
        assert_eq!(select_evenly(&items, 3), vec![0, 4, 9]);
        assert_eq!(select_evenly(&items, 20), items);
        assert_eq!(select_evenly(&items, 1), vec![0]);
    }

    #[test]
    fn pe_machine_headers_are_classified_without_panics() {
        assert_eq!(pe_architecture(&[]), "unknown");
        assert_eq!(pe_architecture(b"MZnot-a-pe"), "unknown");
        let mut pe = vec![0u8; 0x100];
        pe[0..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        assert_eq!(pe_architecture(&pe), "amd64");
        pe[0x84..0x86].copy_from_slice(&0x014cu16.to_le_bytes());
        assert_eq!(pe_architecture(&pe), "i386");
    }

    #[test]
    fn wildcard_summary_aggregates_all_architectures() {
        let samples = vec![
            Sample {
                class: "executable".to_string(),
                rel: "a.dll".to_string(),
                path: "a.dll".into(),
                bytes: 10,
                arch: "amd64".to_string(),
            },
            Sample {
                class: "executable".to_string(),
                rel: "b.dll".to_string(),
                path: "b.dll".into(),
                bytes: 10,
                arch: "i386".to_string(),
            },
        ];
        let row = |file: &str, arch: &str, lane: &str, total: usize| Row {
            class: "executable".to_string(),
            arch: arch.to_string(),
            file: file.to_string(),
            bytes: 10,
            chunks: 1,
            run: 0,
            lane: lane.to_string(),
            payload_bytes: total,
            replay_bytes: 0,
            total_bytes: total,
            raw_bytes: 10,
            changed_bytes: 0,
            transform_ms: 0,
            compress_ms: 1,
            ok: true,
        };
        let rows = vec![
            row("a.dll", "amd64", "raw-aahl", 10),
            row("b.dll", "i386", "raw-aahl", 10),
            row("a.dll", "amd64", "bcj-base0", 12),
            row("b.dll", "i386", "bcj-base0", 12),
        ];
        let summaries = summarize(&rows, &samples, 1, 1024);
        let wildcard = summaries
            .iter()
            .find(|row| row.arch == "*" && row.lane == "bcj-base0")
            .unwrap();
        assert_eq!(wildcard.files, 2);
        assert_eq!(wildcard.bytes, 20);
        assert_eq!(wildcard.raw_bytes, 20);
        assert_eq!(wildcard.total_bytes, 24);
        assert_eq!(wildcard.delta_bytes, 4);
    }
}
