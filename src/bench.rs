use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::corpus;

pub struct Row {
    pub name: String,
    pub raw: u64,
    pub size: u64,
    pub create_ms: u64,
    pub extract_ms: u64,
    pub ok: bool,
}

fn run(prog: &Path, args: &[&str]) -> Result<()> {
    let st = std::process::Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("run {} {args:?}", prog.display()))?;
    if !st.success() {
        bail!("exit {:?}", st.code());
    }
    Ok(())
}

fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(u64, T)> {
    let t = Instant::now();
    let v = f()?;
    Ok((t.elapsed().as_millis() as u64, v))
}

fn file_hash_map(dir: &Path) -> Result<Vec<(String, u64, [u8; 32])>> {
    let files = corpus::list_files(dir)?;
    let mut out = Vec::new();
    for (rel, p) in files {
        let data = fs::read(&p)?;
        out.push((rel, data.len() as u64, *blake3::hash(&data).as_bytes()));
    }
    out.sort();
    Ok(out)
}

fn run_aahl(corpus_dir: &Path, root: &Path, exe: &Path, chunk_size: usize) -> Result<Row> {
    let name = corpus_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = corpus_files(corpus_dir)?;
    let raw: u64 = files.iter().map(|(_, p)| fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    let work = root.join("work").join(&name);
    fs::create_dir_all(&work)?;
    let arc = work.join("aahl.aahl");
    let (create_ms, _) = timed(|| {
        let mut args = vec!["create".to_string(), arc.to_string_lossy().into_owned()];
        if chunk_size != 1_048_576 {
            args.push("--chunk-size".to_string());
            args.push(chunk_size.to_string());
        }
        for (_, p) in &files {
            args.push(p.to_string_lossy().into_owned());
        }
        run(&exe, &args.iter().map(String::as_str).collect::<Vec<_>>())
    })?;
    let out_dir = work.join("out");
    fs::create_dir_all(&out_dir)?;
    let (extract_ms, _) = timed(|| {
        run(
            exe,
            &[
                "extract",
                arc.to_string_lossy().as_ref(),
                out_dir.to_string_lossy().as_ref(),
            ],
        )
    })?;
    let src = file_hash_map(corpus_dir)?;
    let got = file_hash_map(&out_dir)?;
    let ok = src == got;
    if !ok {
        bail!("aahl roundtrip mismatch for {name}");
    }
    let size = fs::metadata(&arc).map(|m| m.len()).unwrap_or(0);
    Ok(Row {
        name: format!("{name}/aahl"),
        raw,
        size,
        create_ms,
        extract_ms,
        ok,
    })
}

fn atmosphere(exe: &Path) -> Result<()> {
    let v = std::process::Command::new(exe).arg("--version").output();
    if !v.map(|o| o.status.success()).unwrap_or(false) {
        bail!("aahl exe not runnable: {}", exe.display());
    }
    Ok(())
}

fn corpus_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    corpus::list_files(dir)
}

fn run_external(
    corpus_dir: &Path,
    root: &Path,
    tool: &str,
    bin: &Path,
    args: Vec<String>,
) -> Result<Row> {
    let name = corpus_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = corpus_files(corpus_dir)?;
    let raw: u64 = files.iter().map(|(_, p)| fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    let work = root.join("work").join(&name);
    fs::create_dir_all(&work)?;
    let out = work.join(format!("ref.{tool}"));
    let file_args: Vec<String> = files.iter().map(|(_, p)| p.to_string_lossy().into_owned()).collect();

    let stream_codec = tool == "xz" || tool == "zstd";

    let (create_ms, _) = timed(|| {
        let mut cmd = std::process::Command::new(bin);
        cmd.args(&args);
        if stream_codec {
            // xxz/zstd -c ... : output to stdout, capture it
            let p = cmd
                .args(&file_args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawn create")?;
            let outp = p.wait_with_output().context("wait create")?;
            if !outp.status.success() {
                bail!("{tool} create exit {:?}", outp.status.code());
            }
            fs::write(&out, outp.stdout)?;
        } else {
            // 7z a -tzip/-t7z <archive> files...
            cmd.arg(&out).args(&file_args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
            let st = cmd.status().context("run create")?;
            if !st.success() {
                bail!("{tool} create exit {:?}", st.code());
            }
        }
        Ok(())
    })?;

    let (extract_ms, ok) = if stream_codec {
        // decompress back to a single stream and compare against the
        // concatenation of the corpus files in archive order.
        let dec = work.join(format!("dec.{tool}"));
        let (extract_ms, _) = timed(|| {
            let p = std::process::Command::new(bin)
                .arg("-d")
                .arg("-c")
                .arg(&out)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawn decode")?;
            let outp = p.wait_with_output().context("wait decode")?;
            if !outp.status.success() {
                bail!("{tool} decode exit {:?}", outp.status.code());
            }
            fs::write(&dec, outp.stdout)?;
            Ok(())
        })?;
        let mut concat = Vec::new();
        for (_, p) in &files {
            concat.extend(fs::read(p)?);
        }
        let got = fs::read(&dec)?;
        let ok = got == concat;
        (extract_ms, ok)
    } else {
        let t = work.join(format!("ref.test.{tool}"));
        fs::create_dir_all(&t)?;
        let (extract_ms, _) = if tool == "rar" {
            // Rar x -o+ -y <archive> <dest>
            timed(|| {
                run(
                    bin,
                    &[
                        "x",
                        "-y",
                        "-o+",
                        out.to_string_lossy().as_ref(),
                        t.to_string_lossy().as_ref(),
                    ],
                )
            })?
        } else {
            timed(|| {
                run(
                    bin,
                    &["x", "-y", &format!("-o{}", t.to_string_lossy()), out.to_string_lossy().as_ref()],
                )
            })?
        };
        let src = file_hash_map(corpus_dir)?;
        let got = file_hash_map(&t)?;
        (extract_ms, src == got)
    };
    let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok(Row {
        name: format!("{name}/{tool}"),
        raw,
        size,
        create_ms,
        extract_ms,
        ok,
    })
}

pub fn find_tool(name: &str) -> Option<PathBuf> {
    if name == "7z" {
        for p in [
            r"C:\Program Files\7-Zip\7z.exe",
            r"C:\Program Files (x86)\7-Zip\7z.exe",
        ] {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                return Some(pb);
            }
        }
    }
    if name == "rar" {
        for p in [
            r"C:\Program Files\WinRAR\Rar.exe",
            r"C:\Program Files (x86)\WinRAR\Rar.exe",
        ] {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                return Some(pb);
            }
        }
    }
    corpus::which(name)
}

pub fn run_bench(set_dir: &Path, out_tsv: &Path, chunk_size: usize) -> Result<Vec<Row>> {
    let exe = std::env::current_exe().context("current_exe")?;
    atmosphere(&exe)?;
    let root = set_dir.parent().unwrap_or(set_dir).to_path_buf();
    let mut rows = Vec::new();
    let dirs = corpus::corpus_dirs(set_dir)?;
    for d in dirs {
        let name = d.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        eprintln!("== corpus: {name}");
        if let Ok(r) = run_aahl(&d, &root, &exe, chunk_size) {
            rows.push(r);
        } else {
            eprintln!("  aahl: skipped (error)");
        }
        let batch = external_tools();
        for t in batch {
            let bin = find_tool(t.bin);
            if let Some(b) = bin {
                match run_external(&d, &root, t.tool, &b, t.args.clone()) {
                    Ok(r) => rows.push(r),
                    Err(e) => eprintln!("  {}: {}", t.tool, e),
                }
            } else {
                eprintln!("  {}: tool not found, skipped", t.tool);
            }
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    print_table(&rows, out_tsv)?;
    Ok(rows)
}

fn print_table(rows: &[Row], out_tsv: &Path) -> Result<()> {
    println!();
    println!(
        "{:<28} {:>12} {:>12} {:>10} {:>10}  {}",
        "corpus", "raw", "size", "create_ms", "extract_ms", "ratio"
    );
    let mut tsv = String::from("corpus\traw\tsize\tcreate_ms\textract_ms\tratio\tok\n");
    for r in rows {
        let ratio = if r.raw > 0 { r.size as f64 / r.raw as f64 } else { 1.0 };
        println!(
            "{:<28} {:>12} {:>12} {:>10} {:>10}  {:.3} {}",
            r.name,
            r.raw,
            r.size,
            r.create_ms,
            r.extract_ms,
            ratio,
            if r.ok { "" } else { "FAIL" }
        );
        tsv.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{:.4}\t{}\n",
            r.name, r.raw, r.size, r.create_ms, r.extract_ms, ratio, r.ok
        ));
    }
    if let Some(p) = out_tsv.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(out_tsv, tsv)?;
    Ok(())
}

struct ExternalTool {
    tool: &'static str,
    bin: &'static str,
    args: Vec<String>,
}

fn external_tools() -> Vec<ExternalTool> {
        vec![
            // 7z zip deflate level 6 (approximates "zip -6")
            ExternalTool {
                tool: "zip6",
                bin: "7z",
                args: vec!["a".into(), "-tzip".into(), "-mx=6".into()],
            },
            // 7z LZMA2 extreme (the real "7z -9")
            ExternalTool {
                tool: "7z9",
                bin: "7z",
                args: vec!["a".into(), "-m0=LZMA2".into(), "-mx=9".into()],
            },
            // stream codecs: compress <files...> to stdout, captured
            ExternalTool {
                tool: "xz",
                bin: "xz",
                args: vec!["-9".into(), "-c".into()],
            },
            ExternalTool {
                tool: "zstd",
                bin: "zstd",
                args: vec!["-19".into(), "-c".into()],
            },
            // WinRAR: best compression. Found via find_tool (WinRAR install dir).
            ExternalTool {
                tool: "rar",
                bin: "rar",
                args: vec!["a".into(), "-m5".into(), "-ep".into()],
            },
        ]
}

// =====================================================================
// Phase A1: AAHL-only jobs-scaling benchmark (harness extension).
// Drives the *existing* `create -j N` CLI; the codec itself is not
// touched. Per (corpus, jobs, run) we time a full create and extract,
// record archive bytes + BLAKE3, and verify the round trip.
// =====================================================================

#[derive(Clone)]
pub struct JobsRow {
    pub corpus: String,
    pub jobs: usize,
    pub run: usize,
    pub raw: u64,
    pub arc_bytes: u64,
    pub arc_blake3: String,
    pub create_ms: u64,
    pub extract_ms: u64,
    pub ok: bool,
}

fn median_u64(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2
    }
}

fn run_aahl_jobs_round(
    corpus_dir: &Path,
    root: &Path,
    exe: &Path,
    chunk_size: usize,
    jobs: usize,
    run_id: usize,
) -> Result<JobsRow> {
    let name = corpus_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let files = corpus_files(corpus_dir)?;
    let raw: u64 = files
        .iter()
        .map(|(_, p)| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    let work = root.join("jobs-work").join(&name);
    fs::create_dir_all(&work)?;
    let arc = work.join(format!("aahl.j{jobs}.r{run_id}.aahl"));
    let out_dir = work.join(format!("out.j{jobs}.r{run_id}"));
    fs::create_dir_all(&out_dir)?;

    let (create_ms, _) = timed(|| {
        let mut args = vec!["create".to_string(), arc.to_string_lossy().into_owned()];
        if chunk_size != 1_048_576 {
            args.push("--chunk-size".to_string());
            args.push(chunk_size.to_string());
        }
        args.push("-j".to_string());
        args.push(jobs.to_string());
        for (_, p) in &files {
            args.push(p.to_string_lossy().into_owned());
        }
        run(&exe, &args.iter().map(String::as_str).collect::<Vec<_>>())
    })?;
    let (extract_ms, _) = timed(|| {
        run(
            exe,
            &[
                "extract",
                arc.to_string_lossy().as_ref(),
                out_dir.to_string_lossy().as_ref(),
            ],
        )
    })?;
    let data = fs::read(&arc)?;
    let src = file_hash_map(corpus_dir)?;
    let got = file_hash_map(&out_dir)?;
    let ok = src == got;
    if !ok {
        bail!("aahl jobs roundtrip mismatch for {name} j{jobs} r{run_id}");
    }
    Ok(JobsRow {
        corpus: name,
        jobs,
        run: run_id,
        raw,
        arc_bytes: data.len() as u64,
        arc_blake3: blake3::hash(&data).to_hex().to_string(),
        create_ms,
        extract_ms,
        ok,
    })
}

/// Run the jobs-scaling matrix. `summary_tsv` gets one row per (corpus x jobs)
/// with median/min/max over `runs` timing repetitions, speedup vs jobs=1 and
/// parallel efficiency; `raw_tsv` gets every per-run row. The determinism
/// column is true only if archive bytes + full-archive BLAKE3 are identical
/// across all runs AND all jobs values for that corpus.
pub fn run_jobs_bench(
    set_dir: &Path,
    summary_tsv: &Path,
    raw_tsv: &Path,
    chunk_size: usize,
    runs: usize,
    jobs_list: &[usize],
    want: &[String],
) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    atmosphere(&exe)?;
    let root = set_dir.parent().unwrap_or(set_dir).to_path_buf();
    let dirs = corpus::corpus_dirs(set_dir)?;
    let runs = runs.max(1);
    let mut rows: Vec<JobsRow> = Vec::new();

    for d in &dirs {
        let name = d
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !want.is_empty() && !want.iter().any(|w| w == &name) {
            continue;
        }
        eprintln!("== jobs corpus: {name}");
        // one untimed serial round to warm the OS page cache
        let _ = run_aahl_jobs_round(d, &root, &exe, chunk_size, 1, usize::MAX);
        for &jobs in jobs_list {
            for r in 0..runs {
                match run_aahl_jobs_round(d, &root, &exe, chunk_size, jobs, r) {
                    Ok(row) => rows.push(row),
                    Err(e) => eprintln!("  j{jobs} r{r}: {e:#}"),
                }
            }
        }
    }

    rows.sort_by(|a, b| {
        (a.corpus.as_str(), a.jobs, a.run).cmp(&(b.corpus.as_str(), b.jobs, b.run))
    });

    let mut raw_ts =
        String::from("corpus\tjobs\trun\traw\tarc_bytes\tarc_blake3\tcreate_ms\textract_ms\tok\n");
    for r in &rows {
        raw_ts.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.corpus, r.jobs, r.run, r.raw, r.arc_bytes, r.arc_blake3, r.create_ms, r.extract_ms,
            r.ok
        ));
    }
    if let Some(p) = raw_tsv.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(raw_tsv, raw_ts)?;

    let mut by_corpus: Vec<String> = rows.iter().map(|r| r.corpus.clone()).collect();
    by_corpus.sort();
    by_corpus.dedup();

    println!();
    println!(
        "{:<12} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7} {:>7}  {}",
        "corpus", "jobs", "med_cr", "min_cr", "max_cr", "med_ex", "min_ex", "speedup", "eff", "det"
    );
    let mut summary =
        String::from("corpus\tjobs\truns\traw\tratio\tarc_bytes\tarc_blake3\tcreate_median_ms\tcreate_min_ms\tcreate_max_ms\textract_median_ms\textract_min_ms\textract_max_ms\tspeedup_vs_j1\tefficiency\tdeterministic\n");
    for c in &by_corpus {
        let crows: Vec<&JobsRow> = rows.iter().filter(|r| &r.corpus == c).collect();
        let j1: Vec<u64> = crows
            .iter()
            .filter(|r| r.jobs == 1)
            .map(|r| r.create_ms)
            .collect();
        let j1_med = if !j1.is_empty() {
            let mut v = j1.clone();
            median_u64(&mut v)
        } else {
            0
        };
        let mut distinct_archives: Vec<(u64, &str)> =
            crows.iter().map(|r| (r.arc_bytes, r.arc_blake3.as_str())).collect();
        distinct_archives.sort();
        distinct_archives.dedup();
        let det = distinct_archives.len() == 1;
        for &jobs in jobs_list {
            let set: Vec<&JobsRow> = crows.iter().copied().filter(|r| r.jobs == jobs).collect();
            if set.is_empty() {
                continue;
            }
            let mut cm: Vec<u64> = set.iter().map(|r| r.create_ms).collect();
            let mut ex: Vec<u64> = set.iter().map(|r| r.extract_ms).collect();
            let cmed = median_u64(&mut cm);
            let cmin = *cm.iter().min().unwrap();
            let cmax = *cm.iter().max().unwrap();
            let emed = median_u64(&mut ex);
            let emin = *ex.iter().min().unwrap();
            let emax = *ex.iter().max().unwrap();
            let row = set[0];
            let speedup = if j1_med > 0 {
                j1_med as f64 / cmed.max(1) as f64
            } else {
                1.0
            };
            let efficiency = speedup / jobs.max(1) as f64;
            summary.push_str(&format!(
                "{}\t{}\t{}\t{}\t{:.4}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{}\n",
                c,
                jobs,
                runs,
                row.raw,
                row.arc_bytes as f64 / row.raw.max(1) as f64,
                row.arc_bytes,
                row.arc_blake3,
                cmed,
                cmin,
                cmax,
                emed,
                emin,
                emax,
                speedup,
                efficiency,
                det
            ));
            println!(
                "{:<12} {:>5} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7.2} {:>7.2}  {}",
                c,
                jobs,
                cmed,
                cmin,
                cmax,
                emed,
                emin,
                speedup,
                efficiency,
                if det { "same" } else { "DIFF!" }
            );
        }
    }
    if let Some(p) = summary_tsv.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(summary_tsv, summary)?;
    eprintln!("jobs-scaling summary -> {}", summary_tsv.display());
    Ok(())
}


// =====================================================================
// Phase A2: parallel-encoder internals per (corpus x jobs). Thin wrapper
// around `create -j N --par-stats FILE`; reads the counters back into a
// TSV. Measurement-only - the archive bytes produced are identical to a
// plain `create -j N`.
// =====================================================================

fn count_unique_chunks(files: &[(String, PathBuf)], chunk_size: usize) -> Result<u64> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for (_, p) in files {
        let data = fs::read(p)?;
        if !data.is_empty() {
            for piece in data.chunks(chunk_size.max(1)) {
                seen.insert(*blake3::hash(piece).as_bytes());
            }
        }
    }
    Ok(seen.len() as u64)
}

fn parse_parstats(txt: &str, out: &mut [i64; 8]) {
    for line in txt.lines() {
        let mut it = line.split('\t');
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            let idx = match k {
                "par_tasks" => 0,
                "par_used" => 1,
                "par_stale" => 2,
                "par_fallback" => 3,
                "par_discover_us" => 4,
                "par_commit_us" => 5,
                "par_wait_us" => 6,
                "par_peak_workers" => 7,
                _ => continue,
            };
            out[idx] = v.trim().parse().unwrap_or(-1);
        }
    }
}

pub fn run_parstats(
    set_dir: &Path,
    out_tsv: &Path,
    chunk_size: usize,
    jobs_list: &[usize],
    want: &[String],
) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    atmosphere(&exe)?;
    let root = set_dir.parent().unwrap_or(set_dir).to_path_buf();
    let dirs = corpus::corpus_dirs(set_dir)?;
    let mut tsv = String::from(
        "corpus\tjobs\tchunks\ttasks\tused\tstale\tfallback\tdiscover_us\tcommit_us\twait_us\tpeak_workers\tcreate_ms\tarc_bytes\tarc_blake3\n",
    );
    for d in &dirs {
        let name = d
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !want.is_empty() && !want.iter().any(|w| w == &name) {
            continue;
        }
        let files = corpus_files(&d)?;
        let chunks = count_unique_chunks(&files, chunk_size)?;
        for &jobs in jobs_list {
            if jobs <= 1 {
                continue;
            }
            eprintln!("== parstats: {name} j{jobs}");
            let work = root.join("parstats-work").join(&name);
            fs::create_dir_all(&work)?;
            let arc = work.join(format!("aahl.j{jobs}.aahl"));
            let st = work.join(format!("stat.j{jobs}.txt"));
            let (create_ms, _) = timed(|| {
                let mut args = vec![
                    "create".to_string(),
                    arc.to_string_lossy().into_owned(),
                    "--par-stats".to_string(),
                    st.to_string_lossy().into_owned(),
                ];
                if chunk_size != 1_048_576 {
                    args.push("--chunk-size".to_string());
                    args.push(chunk_size.to_string());
                }
                args.push("-j".to_string());
                args.push(jobs.to_string());
                for (_, p) in &files {
                    args.push(p.to_string_lossy().into_owned());
                }
                run(&exe, &args.iter().map(String::as_str).collect::<Vec<_>>())
            })?;
            let txt = fs::read_to_string(&st)
                .with_context(|| format!("read parstats {}", st.display()))?;
            let mut v = [-1i64; 8];
            parse_parstats(&txt, &mut v);
            let data = fs::read(&arc)?;
            tsv.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                name,
                jobs,
                chunks,
                v[0],
                v[1],
                v[2],
                v[3],
                v[4],
                v[5],
                v[6],
                v[7],
                create_ms,
                data.len(),
                blake3::hash(&data).to_hex()
            ));
        }
    }
    if let Some(p) = out_tsv.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(out_tsv, tsv)?;
    eprintln!("parstats -> {}", out_tsv.display());
    Ok(())
}
