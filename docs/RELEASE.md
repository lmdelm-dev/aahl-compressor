# Release Playbook

How to publish an AAHL release.

## Artifacts

| Artifact | Platform | How it's built |
|---|---|---|
| `aahl-x.y.z-x86_64-pc-windows-msvc.zip` (CLI + GUI) | Windows x64 | `cargo build --release --workspace` |
| `aahl-x.y.z-x86_64-unknown-linux-musl.tar.gz` (CLI) | Linux x64, static | `scripts/build-linux.sh` |

The Linux binary is statically linked musl, so one build runs on every Linux
distribution (glibc or musl) without dependency installation.

## Steps

1. **Update the version.** AAHL is a cargo workspace; the single source of
   truth is `version` under `[workspace.package]` in `Cargo.toml` (both the
   CLI and GUI packages inherit it). Bump it here, e.g. `0.2.0` for this
   release.

2. **Build locally (void these in CI).**

   Windows (this also builds the GUI — that is the point of the workspace):
   ```
   cargo build --release --workspace
   ```

   Linux CLI (on a Linux host, or macOS/CI; not cross-buildable from this
   Windows box because a musl linker is required):
   ```
   scripts/build-linux.sh
   ```

3. **Smoke-test the Windows GUI** — must launch against the sibling release
   CLI with no startup crash (it pins the wgpu renderer to DX12/Vulkan to
   avoid the Intel GL-driver crash, so any OpenGL-only environment is
   silently skipped).

4. **Verify the suite** (both runtimes):
   ```
   cargo test --release            # 118 container/unit/fuzz tests
   cargo test --release -p aahl-gui # 4 backend tests
   ```

5. **Commit** version bump + any doc changes (`docs/RELEASE.md`, README
   updates, `LICENSE`, `scripts/`).

6. **Tag and release** (requires `gh` authenticated):
   ```
   git tag -a "v$VERSION" -m "aahl $VERSION"
   git push origin main
   git push origin "v$VERSION"
   ```
   Create the GitHub release with assets (adjust paths to your tag):
   ```
   gh release create "v$VERSION" \
     "target/release/aahl.exe" \
     "dist/aahl-$VERSION-x86_64-pc-windows-msvc.zip" \
     "dist/aahl-$VERSION-x86_64-unknown-linux-musl.tar.gz" \
     --title "aahl $VERSION" \
     --notes "See docs/RELEASE.md and the changelog."
   ```

   The GitHub Actions workflow (`.github/workflows/release.yml`) also fires on
   the tag: it rebuilds the Windows CLI+GUI zip and the Linux musl tarball on
   clean runners and attaches them to the release, so hand-building is
   optional. If the workflow is used, let it finish and verify both assets
   landed, then re-run any `gh release create` attempt with `--` for the
   already-attached assets only if missing.

## Smoke commands (post-release)

```
aahl create out.aahl <INPUTS>...
aahl list --json out.aahl
aahl test --json out.aahl
```

Integrity check on a fresh clone (no build):
```
curl -LO <asset-url>/aahl-$VERSION-x86_64-unknown-linux-musl.tar.gz
tar xzf aahl-*.tar.gz
./aahl test --json out.aahl
```