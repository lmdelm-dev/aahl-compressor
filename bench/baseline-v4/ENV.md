# AAHL v5 Milestone 1 - Phase 0 baseline environment record

Generated: 2026-09-22 by the v5 measurement gate (no compression-logic changes).

## Repository / build

- Git commit (HEAD): `27a00718b467ca273b81f1c4db504550e29437c8` (v4, `feat: v4 hot-rule context model + deeper folding to outpass xz/7z9/zstd`)
- Branch: main (tracking origin/main), clean tree at measurement time
- Build profile: `cargo build --release` with the **default Cargo release profile** (no
  `[profile.release]` overrides in Cargo.toml: opt-level 3, no LTO, debug=false, 16 codegen units)
- Cargo.toml: name = "aahl", version = "0.1.0", edition = "2021"
- AAHL version string (clap `--version`): `aahl 0.1.0`
- Dependencies: clap 4 (derive), blake3 1, anyhow 1, rayon 1

## Toolchain

- rustc 1.98.1 (48a229cea 2026-09-01)
- cargo 1.98.1 (797e8a9bc 2026-08-05)

## Machine

- OS: Microsoft Windows 10 Pro, version 10.0.19045, 64-bit
- CPU: Intel(R) Core(TM) i7-8750H CPU @ 2.20GHz (6 physical cores, 12 logical, MaxClockSpeed 2208 MHz)
- RAM: 15.9 GB total (16,654,452 KB visible)

## Reference benchmark tools (bench shell-outs; compression engines are yardsticks only)

| tool | lane in bench | version | path |
|------|---------------|---------|------|
| 7-Zip | 7z9 (LZMA2 -mx=9), zip6 (deflate -mx=6) | 7-Zip 26.02 (x64), 2026-06-25 | C:\Program Files\7-Zip\7z.exe |
| xz (XZ Utils) | xz | xz 5.8.3 / liblzma 5.8.3 | C:\Users\Pc\AppData\Local\Microsoft\WinGet\Links\xz.exe |
| zstd | zstd | Zstandard CLI v1.5.7 | C:\Users\Pc\AppData\Local\Microsoft\WinGet\Links\zstd.exe |
| WinRAR | rar | RAR 7.13 x64 (2025-07-28) - **evaluation build** ("Version de démonstration") | C:\Program Files\WinRAR\Rar.exe |

Note: The WinRAR copy installed here is the evaluation (demo) build, which can watermark
RAR containers; rar lanes are recorded as-is and marked as evaluation-build output.
