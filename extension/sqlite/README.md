# BT SQLite extension

This directory contains the source of BT's official SQLite extension, version
1.0.0. It uses a shared WASI extension runtime to retain SQLite connections. Its query API
mirrors the MySQL standard library: call `query(sql)`, then `bind()` or
`binds()`, and finally `one()`, `all()`, or `exec()`.

## Source layout

| Path | Purpose |
|---|---|
| `src/lib.rs` | Extension entry points, connection and query state, SQL execution, and value conversion. |
| `src/tests.rs` | Regression tests for the public chained query interface and resource lifecycle. |
| `bindings.json` | Public functions, object methods, stable dispatch IDs, and disposal metadata. |
| `manifest.json` | Package identity, permissions, and shared runtime limits. |
| `Cargo.toml` / `Cargo.lock` | Standalone Rust project and locked dependencies. |
| `build.ps1` | Format check, native tests, WASI build, and isolated package staging. |
| `verify.ps1` / `smoke.bt` | Fresh-project package installation and canonical/legacy BT API acceptance. |
| `THIRD_PARTY_LICENSES.txt` | Generated dependency notices for the WASI build. |

## Behavior

- `sqlite(path, options)` retains the connection in a shared worker. `options`
  remains required; pass `{}` for defaults.
- `query().bind().one()` returns `empty` for no row, `null` for SQLite `NULL`,
  and BT `Bytes` for a BLOB.
- `query().all()` is constrained by `max_rows` and `max_result_bytes`.
- `query().bind().exec()` returns `total`, `rows_affected`,
  `last_insert_id`, `batch_count`, `batch_size`, and `workers`.
- `query().binds().batch().workers().exec()` performs sequential batch writes
  inside a single-connection transaction. `batch()` and `workers()` are retained
  for MySQL-compatible query configuration and statistics; they do not introduce
  parallel writes on the SQLite connection.
- `transaction()` runs multiple write statements in one transaction;
  `{ sql, binds }` is the preferred statement form.
- Close query objects with `query.close()` and connections with `db.close()`
  after use.

```bt
db = sqlite('@/app.db', {})
query = db.query('SELECT 42 AS answer')
row = query.one()
// Output: 42
print row.answer
query.close()
db.close()
```

`sqlite_open(path, options)` remains available as a deprecated compatibility
alias for existing scripts. Both names use the same connection implementation,
argument rules, permissions, and resource limits. New scripts should use `sqlite`.
The legacy dispatch ID is unchanged; the canonical entry has its own stable ID.

## Build and package

Run these commands from the `bt-lang` repository root. Install the WASI target
and provide a WASI-capable C compiler and sysroot for bundled SQLite first.
Configure the compiler according to the local WASI toolchain.

```powershell
rustup target add wasm32-wasip1
extension/sqlite/build.ps1
extension/sqlite/verify.ps1
```

The scripts use `target/debug/bt.exe` by default; pass `-BtPath` to select another
BT executable. Set `CC_wasm32_wasip1` and `AR_wasm32_wasip1` in the current
process to the WASI SDK's `clang` and `llvm-ar` when required by your toolchain.
Generated staging files and `sqlite-1.0.0.bts` stay under `extension/sqlite/target/`.
The archive contains exactly the manifest, bindings, WASM module, README, and
four license/notice files. Rebuild before packaging source changes. Install the
package into a BT project with `bt ext install <package.bts> <project>`.

## Validation

Run the native regression suite and format check from the repository root:

```text
cargo test --locked --manifest-path extension/sqlite/Cargo.toml
cargo fmt --manifest-path extension/sqlite/Cargo.toml -- --check
```

Tests cover value boundaries, query reuse, transactions, batch rollback, result
limits, busy timeouts, WAL, concurrent readers, and object disposal. Native tests
do not require a WASI C toolchain. The repository release gate includes these
tests and verifies the locked dependency and license inventories.

`verify.ps1` checks the eight-file archive and calls both registered entry points
from a fresh BT project. It verifies cross-entry writes and reads, `empty`/`null`
and BLOB boundaries, repeated close, required options, invalid option types, and
disposed handles. Each run stores scripts, the installed package, and
`results.json` under a new `target/entry-acceptance-*` directory.

Generate or check third-party notices from the repository root:

```powershell
tools/compliance/generate-third-party-licenses.ps1
tools/compliance/generate-third-party-licenses.ps1 -Check
```

## License

The BT extension source is Copyright 2026 Lifeng Yan and is available under
MIT OR Apache-2.0. The WASM module statically contains public-domain SQLite.
The package includes `LICENSE-MIT`, `LICENSE-APACHE`, `COPYRIGHT`, and
`THIRD_PARTY_LICENSES.txt` with the complete locked Rust dependency notices.
