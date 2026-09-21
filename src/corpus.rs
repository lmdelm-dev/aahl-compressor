use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const WORDS: &[&str] = &[
    "the", "of", "and", "to", "in", "is", "you", "that", "it", "he", "was", "for", "on",
    "are", "as", "with", "his", "they", "at", "be", "this", "have", "from", "or", "one",
    "had", "by", "word", "but", "not", "what", "all", "were", "we", "when", "your", "can",
    "said", "there", "use", "an", "each", "which", "she", "do", "how", "their", "if", "will",
    "up", "other", "about", "out", "many", "then", "them", "these", "so", "some", "her",
    "would", "make", "like", "him", "into", "time", "has", "look", "two", "more", "write",
    "go", "see", "number", "no", "way", "could", "people", "my", "than", "first", "water",
    "been", "call", "who", "oil", "its", "now", "find", "long", "down", "day", "did", "get",
    "come", "made", "may", "part", "alpha", "beta", "gamma", "delta", "epsilon", "zeta",
    "eta", "theta", "iota", "kappa", "lambda", "mu", "nu", "xi", "omicron", "pi", "rho",
    "sigma", "tau", "upsilon", "phi", "chi", "psi", "omega", "packet", "buffer", "sector",
    "queue", "store", "fold", "token", "burst", "slab", "wedge", "orbit", "vertex", "anchor",
];

pub struct Prng(u64);

impl Prng {
    pub fn new(seed: u64) -> Self {
        Prng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
    pub fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            for b in self.next_u64().to_le_bytes() {
                if i < buf.len() {
                    buf[i] = b;
                    i += 1;
                }
            }
        }
    }
    pub fn pick<'a>(&mut self, list: &'a [&'a str]) -> &'a str {
        list[(self.next_u64() % list.len() as u64) as usize]
    }
}

fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(path, data).with_context(|| format!("write {}", path.display()))
}

fn gen_prose(rng: &mut Prng, lines: usize, ml: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * ml * 5 / 3);
    for _ in 0..lines {
        let nw = 4 + (rng.next_u64() % 9) as usize;
        for w in 0..nw {
            let word = rng.pick(WORDS);
            if w == 0 {
                let mut c = word.chars();
                let first = c.next().unwrap();
                out.push(first.to_ascii_uppercase() as u8);
                out.extend(c.as_str().bytes());
            } else {
                out.extend(word.bytes());
            }
            if w != nw - 1 {
                out.push(b' ');
            }
        }
        if rng.next_u64() % 5 == 0 {
            out.push(b'.');
        }
        out.push(b'\n');
        if out.len() >= lines * ml * 4 {
            break;
        }
    }
    out
}

fn gen_text_large(rng: &mut Prng) -> Vec<u8> {
    let mut out = Vec::new();
    for seed in 0..40u32 {
        let head = format!(
            "Section {seed} of the great archive, chapter the {seed}th\n{}\n",
            "=".repeat(60)
        );
        out.extend(head.bytes());
        out.extend(gen_prose(rng, 900, 60));
        out.extend(b"\n\n");
        for i in 0..6u32 {
            let tag = format!("[record={seed}-{i}] key=value pair list\n");
            out.extend(tag.bytes());
        }
    }
    out
}

fn gen_table(rng: &mut Prng, rows: usize) -> Vec<u8> {
    let cats = ["widget", "gadget", "sprocket", "bearing", "capsule", "relay", "diode", "bolt"];
    let mut csv = String::from("id,sku,category,quantity,price_usd,warehouse,created\n");
    let mut js = String::new();
    for i in 0..rows {
        let cat = cats[(rng.next_u64() % cats.len() as u64) as usize];
        let wh = format!("W{}{}", (rng.next_u64() % 12) + 1, (rng.next_u64() % 8) + 1);
        let qty = (rng.next_u64() % 500) + 1;
        let price = rng.next_u64() % 10_000;
        let day = (rng.next_u64() % 28) + 1;
        csv.push_str(&format!(
            "{i},{cat}-{i:05},{cat},{qty},{price}.{:02},{wh},2026-{:02}-{day:02}\n",
            price % 100,
            (i % 12) + 1
        ));
        js.push_str(&format!(
            "{{\"id\":{i},\"sku\":\"{cat}-{i:05}\",\"category\":\"{cat}\",\"qty\":{qty},\"price\":{price}.{:02},\"wh\":\"{wh}\",\"day\":{day}}}\n",
            price % 100
        ));
    }
    let mut out = Vec::with_capacity(csv.len() + js.len());
    out.extend(b"# CSV table\n");
    out.extend(csv.bytes());
    out.extend(b"\n# JSON lines\n");
    out.extend(js.bytes());
    out
}

fn collect_files(dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let p = ent.path();
        if p.is_dir() {
            collect_files(&p, out)?;
        } else if p.is_file() {
            let rel = p
                .strip_prefix(dir)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, p));
        }
    }
    Ok(())
}

fn copy_src_tree(src: &Path, dst: &Path, exts: &[&str]) -> Result<usize> {
    let mut files: Vec<PathBuf> = Vec::new();
    visit(src, &mut files)?;
    let mut n = 0;
    for p in files {
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !exts.is_empty() && !exts.contains(&ext.as_str()) {
            continue;
        }
        // flatten: every corpus dir is a flat set of files (rel == basename)
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dst_path = dst.join(&name);
        if dst_path.exists() {
            continue; // keep first occurrence, skip same-named duplicates
        }
        write_file(&dst_path, &fs::read(&p)?)?;
        n += 1;
    }
    Ok(n)
}

fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let p = ent.path();
        if p.is_dir() {
            visit(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

fn concat_rs(src: &Path, dst: &Path) -> Result<usize> {
    let mut files: Vec<PathBuf> = Vec::new();
    visit(src, &mut files)?;
    files.sort();
    let mut out = Vec::new();
    let mut n = 0;
    for p in files {
        if p.extension().map(|e| e.to_string_lossy()) == Some("rs".into()) {
            let data = fs::read(&p)?;
            out.extend(format!("// ==== {} ====\n", p.display()).bytes());
            out.extend(data);
            out.push(b'\n');
            n += 1;
        }
    }
    if n > 0 {
        write_file(dst, &out)?;
    }
    Ok(n)
}

pub fn build_corpus(set_dir: &Path, source_dir: Option<&Path>) -> Result<()> {
    fs::create_dir_all(set_dir)?;
    let mut built = 0usize;

    // --- synthetic, deterministic ---
    let mut rng = Prng::new(0xC0FFEE_2026);
    write_file(
        &set_dir.join("text-large/large.txt"),
        &gen_text_large(&mut rng),
    )?;
    write_file(&set_dir.join("table/table.csv"), &gen_table(&mut rng, 60_000))?;
    let mut rng = Prng::new(0xDEADBEEF);
    let mut rand1m = vec![0u8; 1_048_576];
    rng.fill(&mut rand1m);
    write_file(&set_dir.join("random/random.bin"), &rand1m)?;
    write_file(&set_dir.join("empty/empty.bin"), b"")?;
    write_file(&set_dir.join("tiny/one.bin"), b"A")?;
    built += 6;

    // --- precompressed artifact: zstd -19 of the seeded random blob ---
    let pre_dir = set_dir.join("precompressed");
    fs::create_dir_all(&pre_dir)?;
    write_file(&pre_dir.join("in.bin"), &rand1m)?;
    let zstd = which("zstd");
    if let Some(z) = zstd {
        let ok = std::process::Command::new(z)
            .args(["-19", "-f", "-q", "-o"])
            .arg(pre_dir.join("z.zst"))
            .arg(pre_dir.join("in.bin"))
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            built += 1;
        }
    }

    // --- real corpora (only if a source dir is provided) ---
    if let Some(src) = source_dir {
        if src.is_dir() {
            let opt: Result<()> = (|| {
                let n = concat_rs(src, &set_dir.join("code-concat/code.rs"))?;
                if n == 0 {
                    bail!("no .rs files in source");
                }
                code_copy_ok(set_dir, src, n)
            })();
            if opt.is_ok() {
                built += 1;
            }
            let bin = set_dir.join("binary-mixed");
            let n = copy_src_tree(src, &bin, &["exe", "dll"])?;
            if n > 0 {
                built += 1;
            } else if bin.read_dir().map(|mut d| d.next().is_none()).unwrap_or(true) {
                fs::remove_dir_all(&bin).ok();
            }
            let inst = set_dir.join("installer");
            let mut copied = 0usize;
            let mut files: Vec<PathBuf> = Vec::new();
            visit(src, &mut files)?;
            for p in files {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if name.contains("setup") || name.contains("installer") || name.contains("copilot")
                {
                    write_file(&inst.join("installer-setup.exe"), &fs::read(&p)?)?;
                    copied += 1;
                    break;
                }
            }
            if copied == 0 {
                fs::remove_dir_all(&inst).ok();
            } else {
                built += 1;
            }
        } else {
            bail!("source dir does not exist: {}", src.display());
        }
    } else {
        for name in ["code-concat", "code-multi", "binary-mixed", "installer"] {
            fs::remove_dir_all(set_dir.join(name)).ok();
        }
    }

    println!(
        "corpus set built at {} ({} entries; real corpora only included when --source is given)",
        set_dir.display(),
        built
    );
    Ok(())
}

fn code_copy_ok(set_dir: &Path, src: &Path, _n: usize) -> Result<()> {
    copy_src_tree(src, &set_dir.join("code-multi"), &["rs"])?;
    Ok(())
}

pub fn which(cmd: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    let mut best: Option<PathBuf> = None;
    for dir in std::env::split_paths(&paths) {
        for exe in [cmd.to_string(), format!("{cmd}.exe")] {
            let cand = dir.join(&exe);
            if cand.is_file() {
                if best.is_none() {
                    best = Some(cand);
                }
            }
        }
    }
    best
}

pub fn corpus_dirs(set_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for ent in fs::read_dir(set_dir).with_context(|| format!("read {}", set_dir.display()))? {
        let ent = ent?;
        let p = ent.path();
        if p.is_dir() {
            dirs.push(p);
        }
    }
    dirs.sort();
    Ok(dirs)
}

pub fn list_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut v = Vec::new();
    collect_files(dir, &mut v)?;
    v.sort();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for (rel, p) in list_files(dir).unwrap() {
            out.push((rel, fs::read(&p).unwrap()));
        }
        out.sort();
        out
    }

    #[test]
    fn corpus_build_is_deterministic() {
        let a = std::env::temp_dir().join("aahl_det_a");
        let b = std::env::temp_dir().join("aahl_det_b");
        for d in [&a, &b] {
            let _ = fs::remove_dir_all(d);
        }
        build_corpus(&a, None).unwrap();
        build_corpus(&b, None).unwrap();
        let sa = snapshot(&a);
        let sb = snapshot(&b);
        assert_eq!(sa.len(), sb.len(), "different file count");
        for ((an, av), (bn, bv)) in sa.iter().zip(sb.iter()) {
            assert_eq!(an, bn, "mismatched file name");
            assert_eq!(av, bv, "mismatched content in {an}");
        }
    }

    #[test]
    fn prng_is_deterministic_and_dense() {
        let mut r1 = Prng::new(42);
        let mut r2 = Prng::new(42);
        let mut a = [0u8; 4096];
        let mut b = [0u8; 4096];
        r1.fill(&mut a);
        r2.fill(&mut b);
        assert_eq!(a, b);
        assert!(a.iter().any(|&x| x != 0));
        assert!(a.chunks(4).any(|c| c != [0x2C, 0xB0, 0xC0, 0x76]));
    }

    #[test]
    fn generated_sets_have_expected_shape() {
        let d = std::env::temp_dir().join("aahl_det_shape");
        let _ = fs::remove_dir_all(&d);
        build_corpus(&d, None).unwrap();
        for name in ["text-large", "table", "random", "empty", "tiny", "precompressed"] {
            assert!(
                d.join(name).exists(),
                "missing synthetic corpus {name}"
            );
        }
    }
}