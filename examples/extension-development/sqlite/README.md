# BT SQLite extension

This is BT's official SQLite extension example, version 1.0.0. It uses a
shared WASI extension runtime to retain a SQLite connection. Its query API
mirrors the MySQL standard library: call `query(sql)`, then `bind()` or
`binds()`, and finally `one()`, `all()`, or `exec()`.

## Behavior

- `sqlite_open(path, options)` retains the connection in a shared worker.
- `query().bind().one()` returns `empty` for no row, `null` for SQLite `NULL`,
  and BT `Bytes` for a BLOB.
- `query().all()` is constrained by `max_rows` and `max_result_bytes`.
- `query().bind().exec()` returns `total`, `rows_affected`,
  `last_insert_id`, `batch_count`, `batch_size`, and `workers`.
- `query().binds().batch().workers().exec()` performs sequential batch writes
  inside a single-connection transaction.
- `transaction()` runs multiple write statements in one transaction;
  `{ sql, binds }` is the preferred statement form.
- Tests cover busy timeouts, WAL, concurrent reads, close invalidation, and
  object-count reclamation.

## Build and package

Install the WASI target and provide a WASI-capable C compiler for bundled
SQLite, then run:

```text
rustup target add wasm32-wasip1
cargo build --locked --manifest-path examples/extension-development/sqlite/Cargo.toml --target wasm32-wasip1 --release
copy examples\extension-development\sqlite\target\wasm32-wasip1\release\sqlite.wasm examples\extension-development\sqlite\module.wasm
cargo run --locked -- ext build examples/extension-development/sqlite -o examples/extension-development/sqlite/sqlite-1.0.0.bts
copy examples\extension-development\sqlite\sqlite-1.0.0.bts examples\extension-development\extensions\sqlite.bts
```

The two tracked `.bts` files must have identical contents. Verify them with:

```text
powershell -ExecutionPolicy Bypass -File tools/compliance/verify-sqlite-packages.ps1
```

Run the Rust-side extension tests with:

```text
cargo test --locked --manifest-path examples/extension-development/sqlite/Cargo.toml
```

After installing the extension in a BT project, see `demo.bt` in the parent
example directory.

## License

The BT extension source is Copyright 2026 Lifeng Yan and is available under
MIT OR Apache-2.0. The WASM module statically contains public-domain SQLite.
The package includes `LICENSE-MIT`, `LICENSE-APACHE`, `COPYRIGHT`, and
`THIRD_PARTY_LICENSES.txt` with the complete locked Rust dependency notices.
