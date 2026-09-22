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

fn run_aahl(corpus_dir: &Path, root: &Path, exe: &Path) -> Result<Row> {
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
        for (_, p) in &files {
            args.push(p.to_string_lossy().into_owned());
        }
        run(exe, &args.iter().map(String::as_str).collect::<Vec<_>>())
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

pub fn run_bench(set_dir: &Path, out_tsv: &Path) -> Result<Vec<Row>> {
    let exe = std::env::current_exe().context("current_exe")?;
    atmosphere(&exe)?;
    let root = set_dir.parent().unwrap_or(set_dir).to_path_buf();
    let mut rows = Vec::new();
    let dirs = corpus::corpus_dirs(set_dir)?;
    for d in dirs {
        let name = d.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        eprintln!("== corpus: {name}");
        if let Ok(r) = run_aahl(&d, &root, &exe) {
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
