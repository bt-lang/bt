# Contributing to BT

Thank you for helping improve BT. Keep changes focused, preserve the distinction between the lightweight `bt` CLI and the optional `bt-app` desktop runtime, and avoid adding work to unrelated VM hot paths.

## Development setup

1. Install the stable Rust toolchain.
2. Clone the repository, including the tracked root `Cargo.lock`.
3. On Windows, install the MSVC build tools and Windows SDK.
4. Install the platform dependencies required by Tauri 2 before building `bt-app`.

## Build and test

Run the checks that match the affected area:

```text
cargo fmt --all -- --check
cargo test --locked
cargo build --locked --bin bt
cargo build --locked --features desktop --bin bt-app
```

For language or standard-library changes, also run the relevant scripts under `tools/quality/`. For desktop behavior, test a runnable project under `examples/` and verify the actual interaction path, not only source-level unit tests.

Every Cargo build, check, and test must complete without compiler warnings. Workspace lints deny warnings so unused functions, imports, variables, and other warning-level defects fail the command instead of entering the public branch.

## Code expectations

- Preserve the runtime distinction between `empty` and `null`.
- Use `snake_case` for BT APIs, options, properties, and `on_xxx` callbacks.
- Keep caches, queues, and background workers explicitly bounded.
- Do not add Tauri, WebView, or desktop dependencies to `src/bt_cli_main.rs`.
- Add documentation comments for changed public structures and functions, following the surrounding module style.
- Update tests when behavior changes and document any validation that cannot be run locally.

## Pull requests

- Keep one logical change per pull request.
- Explain user-visible behavior, compatibility impact, and performance impact.
- Include the commands and results used for validation.
- Do not commit generated binaries, local credentials, website deployment state, or private operational tools.
- If a change affects a public API or configuration, describe the required documentation update. Maintainers synchronize the separate website documentation during release preparation.

## Community and security

Follow the repository [Code of Conduct](../CODE_OF_CONDUCT.md) in issues, pull requests, reviews, and other project spaces. Do not disclose suspected vulnerabilities in a public issue; use the private reporting process in the [Security Policy](../SECURITY.md).

## License

BT is dual-licensed under the [MIT License](../LICENSE-MIT) and the [Apache License 2.0](../LICENSE-APACHE), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in BT by you, as defined in the Apache License 2.0, is dual-licensed as above without any additional terms or conditions.
