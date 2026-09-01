# Release quality and compliance gate

Run `tools/quality/release-check.ps1` from a clean, committed checkout before
creating any release tag. The complete gate is required for a formal release;
the skip switches are only for local iteration and do not qualify a build for
publication.

## Required tools

- Stable Rust with the targets needed by the selected release platform.
- `cargo-about` 0.9.2, installed with:

  ```text
  cargo install cargo-about --version 0.9.2 --locked --features cli
  ```

- `cargo-audit` 0.22.2, installed with:

  ```text
  cargo install cargo-audit --version 0.22.2 --locked
  ```

- Gitleaks 8.30.1. CI verifies the official Windows x64 archive against SHA-256
  `d29144deff3a68aa93ced33dddf84b7fdc26070add4aa0f4513094c8332afc4e`.

## Automated release checks

The complete gate enforces:

- `cargo fmt --all -- --check`.
- Locked root and SQLite extension metadata.
- Reproducible platform- and feature-specific `THIRD-PARTY-NOTICES.txt`
  generation, the tracked non-Rust distribution inventory, and the tracked
  SQLite package notice. Source-file license comments are retained without
  copying unrelated implementation bodies into the notice.
- Matching, current SQLite `.bts` packages containing BT licenses, copyright,
  SQLite/rusqlite notices, source, lockfile, manifest, bindings, and WASM.
- `tools/compliance/verify-rustsec-policy.ps1`, which requires zero
  vulnerabilities and an exact match to the reviewed informational advisory
  set, dependency paths, targets, features, review dates, and removal criteria.
- `cargo test --locked` and `cargo test --locked --features desktop`.
- `cargo check --locked --workspace --all-targets --all-features`; warnings
  are denied by workspace lint configuration.
- Debug and release CLI builds, the platform desktop release build, critical
  example regressions, and the performance baseline.
- A clean `git archive` public export with no Git history.
- Gitleaks scans of the exported tracked tree and of `.bts`, `.btr`, and `.zip`
  contents expanded through two archive levels.
- Release archive contents: programs plus `README.md`, `README.zh-CN.md`,
  `LICENSE-APACHE`, `LICENSE-MIT`, `COPYRIGHT`, and the generated
  `THIRD-PARTY-NOTICES.txt` for that archive.

Run the complete local gate from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/release-check.ps1
```

For a local iteration only, expensive stages can be skipped explicitly:

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/release-check.ps1 -SkipDesktop -SkipBenchmarks
```

## Release artifacts

The manually dispatched GitHub workflow generates notices before building and
publishes four verified ZIP archives:

- `bt-windows-x64.zip`: `bt.exe` and `bt_app.exe`.
- `bt-linux-x64.zip`: statically linked musl `bt` and GNU `bt_app`.
- `bt-macos-arm64.zip`: Apple Silicon `bt` and `bt_app`.
- `bt-macos-x64.zip`: Intel `bt` and `bt_app`.

Archive names include the Cargo package version before `.zip`. Each archive
contains the notice generated from exactly the target and feature graphs used
by its two programs. The workflow has only a `workflow_dispatch` trigger so
repository pushes do not consume build minutes.

## Long-running validation

Run at least ten minutes for a normal release window:

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/run-longrun.ps1 -Build -DurationMinutes 10
```

For high-risk I/O, web, networking, process, bytes, or desktop changes:

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/run-longrun.ps1 -Build -DurationMinutes 60 -IncludeNet
```

Generated quality JSON under `target/quality/` is local evidence or a CI
artifact and must not be committed.

[Simplified Chinese](release-gate.zh-CN.md)
