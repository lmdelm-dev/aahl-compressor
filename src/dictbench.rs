//! V6 STEP 4 measurement: grammar-seeded dictionaries (`aahl train` +
//! `create/extract/test --dict`) vs zstd trained dictionaries, on one
//! held-out stream per domain. Uniform lane: training = first ~60% of the
//! concatenated domain bytes (cut at a chunk boundary so the held-out never
//! reuses a split chunk), held-out = the remainder. Domains: prose
//! (corpus_set/text-large/large.txt), table (corpus_set/table/table.csv),
//! code (repo src/*.rs). Reports archive sizes, timing, and dict-file sizes;
//! the docs translate those into fair break-evens (dict transmitted once).

use anyhow::{bail, Context, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::{corpus, dict};

/// zstd wants an explicit level; -19 mirrors the repo's reference lane.
const ZSTD_LEVEL: &str = "-19";

fn run(prog: &Path, args: &[&str]) -> Result<()> {
    let st = Command::new(prog)
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

fn median(v: &[u64]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut w = v.to_vec();
    w.sort_unstable();
    w[w.len() / 2]
}

fn find_zstd() -> Option<PathBuf> {
    if let Some(p) = corpus::which("zstd") {
        return Some(p);
    }
    for cand in [r"C:\Users\Pc\AppData\Local\Microsoft\WinGet\Links\zstd.exe"] {
        let pb = PathBuf::from(cand);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

/// Deterministic domain sources: (domain name, arc-sorted file list).
fn domain_files(set_dir: &Path) -> Result<Vec<(String, Vec<PathBuf>)>> {
    let mut out = Vec::new();
    let prose = set_dir.join("text-large/large.txt");
    if prose.is_file() {
        out.push(("prose".to_string(), vec![prose]));
    }
    let table = set_dir.join("table/table.csv");
    if table.is_file() {
        out.push(("table".to_string(), vec![table]));
    }
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut code: Vec<PathBuf> = Vec::new();
    for f in fs::read_dir(&src)? {
        let f = f?;
        if f.path().extension().map(|e| e == "rs").unwrap_or(false) {
            code.push(f.path());
        }
    }
    code.sort();
    if !code.is_empty() {
        out.push(("code".to_string(), code));
    }
    Ok(out)
}

/// Deterministic 60/40 stream split. `full` is concatenated outside this
/// function; the training prefix is rounded down to a whole number of
/// `chunk`-sized pieces, so held-out starts cleanly on a piece boundary and
/// never reuses a split chunk.
fn split_stream(full: &Path, train: &Path, held: &Path, chunk: usize) -> Result<(u64, u64)> {
    let bytes = fs::read(full)?;
    let n = bytes.len() as u64;
    let target = n * 3 / 5;
    let k = (target / chunk as u64) * chunk as u64;
    fs::write(train, &bytes[..k as usize])?;
    fs::write(held, &bytes[k as usize..])?;
    Ok((k, n - k))
}

/// aahl lane on one input stream: `runs` creates (median), one extract,
/// one byte-identical roundtrip check. Returns (archive_bytes, create_ms,
/// extract_ms).
fn aahl_lane(
    exe: &Path,
    root: &Path,
    name: &str,
    input: &Path,
    dictp: Option<&Path>,
    chunk_size: usize,
    runs: usize,
) -> Result<(u64, u64, u64)> {
    let dir = root.join(format!("aahl-{name}"));
    fs::create_dir_all(&dir)?;
    let arc = dir.join("archive.aahl");
    let out = dir.join("out");
    let _ = fs::remove_dir_all(&out);

    let mut create_ms = Vec::new();
    for _ in 0..runs.max(1) {
        let _ = fs::remove_file(&arc);
        let (ms, _) = timed(|| {
            let mut args: Vec<String> = vec!["create".into(), arc.to_string_lossy().into_owned()];
            if chunk_size != 1_048_576 {
                args.push("--chunk-size".into());
                args.push(chunk_size.to_string());
            }
            if let Some(d) = dictp {
                args.push("--dict".into());
                args.push(d.to_string_lossy().into_owned());
            }
            args.push(input.to_string_lossy().into_owned());
            run(&exe, &args.iter().map(String::as_str).collect::<Vec<_>>())
        })?;
        create_ms.push(ms);
    }

    fs::create_dir_all(&out)?;
    let (extract_ms, _) = timed(|| {
        let mut args: Vec<String> = vec!["extract".into(), arc.to_string_lossy().into_owned(), out.to_string_lossy().into_owned()];
        if let Some(d) = dictp {
            args.push("--dict".into());
            args.push(d.to_string_lossy().into_owned());
        }
        run(&exe, &args.iter().map(String::as_str).collect::<Vec<_>>())
    })?;

    let iname = input.file_name().context("input file name")?;
    let got = fs::read(out.join(iname)).context("read extracted stream")?;
    if got != fs::read(input)? {
        bail!("aahl roundtrip mismatch for {name}");
    }
    let size = fs::metadata(&arc).map(|m| m.len()).unwrap_or(0);
    Ok((size, median(&create_ms), extract_ms))
}

/// zstd lane: `-19 -c` (+ `-D dict`) to stdout, decoded back and compared.
/// Returns (compressed_bytes, compress_ms, decompress_ms).
fn zstd_lane(
    zstd: &Path,
    root: &Path,
    name: &str,
    input: &Path,
    dictp: Option<&Path>,
) -> Result<(u64, u64, u64)> {
    let dir = root.join(format!("zstd-{name}"));
    fs::create_dir_all(&dir)?;
    let out = dir.join("stream.zst");
    let dec = dir.join("dec.zst");

    let (compress_ms, _) = timed(|| {
        let mut cmd = Command::new(zstd);
        cmd.arg(ZSTD_LEVEL).arg("-c");
        if let Some(d) = dictp {
            cmd.arg("-D").arg(d);
        }
        let p = cmd
            .arg(input)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn zstd compress")?;
        let outp = p.wait_with_output().context("wait zstd compress")?;
        if !outp.status.success() {
            bail!("zstd compress exit {:?}", outp.status.code());
        }
        fs::write(&out, outp.stdout)?;
        Ok(())
    })?;
    let (decompress_ms, _) = timed(|| {
        let mut cmd = Command::new(zstd);
        cmd.arg("-d").arg("-c");
        if let Some(d) = dictp {
            cmd.arg("-D").arg(d);
        }
        let p = cmd
            .arg(&out)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn zstd decompress")?;
        let outp = p.wait_with_output().context("wait zstd decompress")?;
        if !outp.status.success() {
            bail!("zstd decompress exit {:?}", outp.status.code());
        }
        fs::write(&dec, outp.stdout)?;
        Ok(())
    })?;
    if fs::read(&dec)? != fs::read(input)? {
        bail!("zstd roundtrip mismatch for {name}");
    }
    let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    Ok((size, compress_ms, decompress_ms))
}

/// V6 STEP 4 harness: `aahl bench-dict SET_DIR [--tsv ..] [--small-tsv ..]
/// [--chunk-size ..] [--runs ..]`.
pub fn run_dict_bench(
    set_dir: &Path,
    out_tsv: &Path,
    small_tsv: &Path,
    chunk_size: usize,
    runs: usize,
) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let zstd = find_zstd().context("zstd not found (needed for the zstd dict lanes)")?;
    let root = set_dir.join("work/dict");
    fs::create_dir_all(&root)?;
    let train_dir = root.join("train");
    let held_dir = root.join("held");
    let dicts = root.join("dicts");
    fs::create_dir_all(&train_dir)?;
    fs::create_dir_all(&held_dir)?;
    fs::create_dir_all(&dicts)?;

    let mut summary = String::from(
        "domain\ttrain_bytes\theldout_bytes\tlane\tsize\tratio\tcreate_ms\textract_ms\tdict_bytes\n",
    );
    let mut small =
        String::from("domain\tprefix_bytes\tlane\tsize\tratio\n");

    for (name, files) in domain_files(set_dir)? {
        eprintln!("== dict bench domain: {name}");
        let full = root.join(format!("{name}.full"));
        let train = train_dir.join(format!("{name}.train"));
        let held = held_dir.join(format!("{name}.held"));
        let mut w = fs::File::create(&full)?;
        for p in &files {
            let mut r = fs::File::open(p).with_context(|| format!("open {}", p.display()))?;
            io::copy(&mut r, &mut w)?;
        }
        let (train_bytes, held_bytes) = split_stream(&full, &train, &held, chunk_size)?;
        let _ = fs::remove_file(&full);
        if held_bytes == 0 || train_bytes == 0 {
            eprintln!("  {name}: stream too small, skipped");
            continue;
        }

        // aahl dictionary trained on the training stream only.
        let aahld = dicts.join(format!("{name}.aahld"));
        let rep = dict::train(
            &[train.clone()],
            &dict::TrainOptions { chunk_size, max_rules: dict::MAX_RULES, min_benefit: 1 },
        )?;
        dict::save(&aahld, &rep.rules, rep.sample_hash)?;
        let aahld_bytes = fs::metadata(&aahld).map(|m| m.len()).unwrap_or(0);

        // zstd dictionary trained on the same stream. zstd --train expects one
        // sample per file (or -B to split a single file into block samples);
        // our train stream is one concatenated file, so split it into blocks.
        // zstd bails on too few samples, so target >= 24 blocks while never
        // exceeding the aahl chunk size (a larger block would give zstd an
        // unfair dictionary, tuned to more data than the aahl lane trains on).
        let z_block = ((train_bytes / 24) as usize).clamp(4096, chunk_size);
        let zdict = dicts.join(format!("{name}.zdict"));
        run(
            &zstd,
            &[
                "--train",
                &format!("-B{z_block}"),
                "--maxdict",
                "1400000",
                "-o",
                zdict.to_string_lossy().as_ref(),
                train.to_string_lossy().as_ref(),
            ],
        )?;
        let zdict_bytes = fs::metadata(&zdict).map(|m| m.len()).unwrap_or(0);

        // four lanes over the HELD-OUT stream.
        let (a_n, a_nc, a_nx) = aahl_lane(&exe, &root, &name, &held, None, chunk_size, runs)?;
        let (a_d, a_dc, a_dx) = aahl_lane(&exe, &root, &name, &held, Some(&aahld), chunk_size, runs)?;
        let (z_n, z_nc, z_nx) = zstd_lane(&zstd, &root, &name, &held, None)?;
        let (z_d, z_dc, z_dx) = zstd_lane(&zstd, &root, &name, &held, Some(&zdict))?;

        for (lane, size, create, extract, dbytes) in [
            ("aahl-nodict", a_n, a_nc, a_nx, 0u64),
            ("aahl-dict", a_d, a_dc, a_dx, aahld_bytes),
            ("zstd-nodict", z_n, z_nc, z_nx, 0u64),
            ("zstd-dict", z_d, z_dc, z_dx, zdict_bytes),
        ] {
            let ratio = if held_bytes > 0 { size as f64 / held_bytes as f64 } else { 1.0 };
            println!(
                "  {name:<6} {lane:<12} {size:>10}B  ratio {ratio:.4}  (dict {dbytes}B)"
            );
            summary.push_str(&format!(
                "{name}\t{train_bytes}\t{held_bytes}\t{lane}\t{size}\t{ratio:.4}\t{create}\t{extract}\t{dbytes}\n"
            ));
        }

        // Small-file prefixes from the held-out stream.
        let held_all = fs::read(&held)?;
        for kb in [1u64, 4, 16, 64, 100] {
            let n = (kb * 1024).min(held_all.len() as u64) as usize;
            let prefix = root.join(format!("small-{name}-{kb}k"));
            fs::write(&prefix, &held_all[..n])?;
            let pname = format!("{name}-{kb}k");
            let (a_n, _, _) = aahl_lane(&exe, &root, &pname, &prefix, None, chunk_size, 1)?;
            let (a_d, _, _) = aahl_lane(&exe, &root, &pname, &prefix, Some(&aahld), chunk_size, 1)?;
            let (z_n, _, _) = zstd_lane(&zstd, &root, &pname, &prefix, None)?;
            let (z_d, _, _) = zstd_lane(&zstd, &root, &pname, &prefix, Some(&zdict))?;
            for (lane, size) in [
                ("aahl-nodict", a_n),
                ("aahl-dict", a_d),
                ("zstd-nodict", z_n),
                ("zstd-dict", z_d),
            ] {
                let ratio = if n > 0 { size as f64 / n as f64 } else { 1.0 };
                small.push_str(&format!("{name}\t{n}\t{lane}\t{size}\t{ratio:.4}\n"));
            }
            let _ = fs::remove_file(&prefix);
        }
    }

    if let Some(p) = out_tsv.parent() {
        fs::create_dir_all(p)?;
    }
    if let Some(p) = small_tsv.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(out_tsv, summary)?;
    fs::write(small_tsv, small)?;
    Ok(())
}
