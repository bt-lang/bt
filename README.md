<div align="center">
  <img src=".github/assets/cat_animated.svg" width="190" alt="BT cat mascot">
  <h1>BT Programming Language</h1>
  <p><strong>One compact language runtime for scripts, web services, desktop apps, FFI, and extensions.</strong></p>
  <p>
    <a href="README.md">English</a> ·
    <a href="README.zh-CN.md">Simplified Chinese</a> ·
    <a href="https://btlang.org/en">Website</a> ·
    <a href="https://btlang.org/en/docs/index">Documentation</a>
  </p>
  <p>
    <img src="https://img.shields.io/badge/implemented%20in-Rust-CE422B?style=flat-square" alt="Implemented in Rust">
    <img src="https://img.shields.io/badge/runtime-CLI%20%7C%20Web%20%7C%20Desktop-2563EB?style=flat-square" alt="CLI, Web, and Desktop runtime">
    <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-374151?style=flat-square" alt="Windows, Linux, and macOS">
    <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-0F766E?style=flat-square" alt="MIT OR Apache-2.0 license">
  </p>
</div>

BT is a compact interpreted programming language implemented in Rust. Its JavaScript-like syntax compiles to register-based bytecode, while one shared runtime powers command-line programs, web services, desktop applications, native FFI, and installable extensions.

## Highlights

- **Familiar syntax** — JavaScript-like expressions, functions, classes, closures, destructuring, and chainable APIs.
- **Register-based VM** — source code is compiled to bytecode before execution.
- **One runtime, multiple targets** — build CLI tools, concurrent web services, and packaged desktop applications.
- **Explicit empty-value model** — `empty` means that no value exists; `null` remains an explicit value.
- **Extensible by design** — use native FFI, WASM/WASI extensions, pure BT extensions, and object bindings.
- **Long-running workloads** — bounded runtime resources and observable statistics support resident services.

## A first look

```bt
fn greet(name) {
    'Hello, ' + name
}

// Output: Hello, BT
print greet('BT')
```

Every BT expression has a runtime result. Multiline blocks return their last statement by default, and `return` can still exit a function early.

BT also distinguishes two empty states:

- `empty` means that a value does not exist, such as a missing field, an out-of-range index, or a function without a result.
- `null` is an explicit value used for JSON nulls, database NULLs, or failed conversions.

## Quick start

Install the stable Rust toolchain, then run a checked-in BT example directly from the repository:

```text
cargo run --release --bin bt -- -c examples/compat/empty-null.bt
```

When the current directory contains `main.bt`, running `bt` without a source argument executes that file. Otherwise, it starts the interactive prompt.

Run the basic desktop example:

```text
cargo run --release --features desktop --bin bt_app -- run examples/desktop
```

## Build and test

| Target | Command |
|---|---|
| CLI runtime | `cargo build --release --bin bt` |
| Desktop runtime | `cargo build --release --features desktop --bin bt_app` |
| Default test suite | `cargo test` |
| Desktop test suite | `cargo test --features desktop` |

Windows desktop builds require the MSVC toolchain, the Windows SDK, and the WebView2 Runtime. The Linux and macOS packages used by CI are documented in [the release workflow](.github/workflows/build.yml).

Inside a desktop project containing `app.json`, use the built runtime to create a bundled executable:

```text
path/to/bt_app build
```

## Examples

The catalog below covers every top-level entry in [`examples/`](examples/). Some entries are user-facing tutorials, while others are focused regression, stress, or acceptance fixtures.

### Language and runtime

| Example | Description |
|---|---|
| [`compat/`](examples/compat/) | Regression scripts for block results, classes and closures, destructuring, `empty`/`null`, and snake_case standard-library APIs. |
| [`bytes-modbus.bt`](examples/bytes-modbus.bt) | Builds a Modbus TCP request and parses a binary register response. |
| [`permission-stats.bt`](examples/permission-stats.bt) | Validates permission allow/deny configuration and runtime denial counters. |
| [`process-pipe.bt`](examples/process-pipe.bt) | Starts a child process and reads its standard output, standard error, and exit information. |
| [`reqwest-pool-bench.bt`](examples/reqwest-pool-bench.bt) | Sends repeated requests to a local HTTP endpoint and reports connection-pool reuse statistics. |
| [`runtime-pools-stats.bt`](examples/runtime-pools-stats.bt) | Prints configured HTTP and MySQL pool limits and current transaction state. |
| [`runtime-stats.bt`](examples/runtime-stats.bt) | Reads a minimal snapshot of the shared runtime and bounded I/O configuration. |

### Networking and web

| Example | Description |
|---|---|
| [`net-phase2-tcp-server.bt`](examples/net-phase2-tcp-server.bt) | Starts an event-driven TCP echo server with connect, message, close, and error callbacks. |
| [`net-phase2-tcp-client.bt`](examples/net-phase2-tcp-client.bt) | Connects to the TCP example, exchanges one message, and closes the connection. |
| [`net-phase2-udp-server.bt`](examples/net-phase2-udp-server.bt) | Starts a UDP echo socket and replies to each sender address. |
| [`net-phase2-udp-client.bt`](examples/net-phase2-udp-client.bt) | Sends a datagram to the local UDP example. |
| [`net-phase2-ws-server.bt`](examples/net-phase2-ws-server.bt) | Hosts a WebSocket route with lifecycle callbacks and echo messages. |
| [`net-phase2-ws-client.bt`](examples/net-phase2-ws-client.bt) | Connects to the WebSocket example and handles asynchronous messages. |
| [`net-phase3-stats.bt`](examples/net-phase3-stats.bt) | Inspects bounded network queues, message limits, and idle timeout settings. |
| [`net-phase3-tcp-burst-client.bt`](examples/net-phase3-tcp-burst-client.bt) | Sends a short burst of TCP requests to validate repeated request/response handling. |
| [`net-stress-tcp-server.bt`](examples/net-stress-tcp-server.bt) | Counts and echoes a sustained TCP message workload for stress validation. |
| [`net-stress-tcp-client.bt`](examples/net-stress-tcp-client.bt) | Drives the TCP stress server across repeated connections and payload batches. |
| [`net-stress-udp-server.bt`](examples/net-stress-udp-server.bt) | Counts high-volume UDP datagrams until an explicit stop message arrives. |
| [`net-stress-udp-client.bt`](examples/net-stress-udp-client.bt) | Sends a high-volume UDP workload with numbered payloads. |
| [`net-stress-ws-server.bt`](examples/net-stress-ws-server.bt) | Echoes and counts WebSocket messages for sustained-connection testing. |
| [`net-stress-ws-client.bt`](examples/net-stress-ws-client.bt) | Drives the WebSocket stress server and verifies echoed responses. |
| [`net-web/`](examples/net-web/) | Runs the BT web engine through `net.listen({type: 'web'})` with a local site entry. |
| [`web-blocking-policy/`](examples/web-blocking-policy/) | Demonstrates which blocking operations are accepted or rejected inside web request handling. |
| [`longrun-audit/`](examples/longrun-audit/) | Combines a local web service and probe workload for long-running resource audits. |

### Desktop applications

| Example | Description |
|---|---|
| [`desktop/`](examples/desktop/) | A small diary application showing a static frontend and `window.bt.call()` backend bridge. |
| [`desktop-api/`](examples/desktop-api/) | Exercises the public desktop APIs exposed through `window.bt`. |
| [`desktop-dev-reload/`](examples/desktop-dev-reload/) | Demonstrates resource watching, exclusion rules, and development-time reload. |
| [`desktop-html/`](examples/desktop-html/) | Packages a frontend-only HTML, CSS, and JavaScript desktop application without a BT backend. |
| [`desktop-icon-appjson/`](examples/desktop-icon-appjson/) | Validates an application icon configured through `app.json`. |
| [`desktop-icon-html/`](examples/desktop-icon-html/) | Validates a packaged static application with an HTML entry and ICO asset. |
| [`desktop-remote/`](examples/desktop-remote/) | Loads a remote web page while retaining the local BT bridge. |
| [`desktop-server/`](examples/desktop-server/) | Starts a local BT server and loads it inside a desktop window. |
| [`desktop-starter-cdp/`](examples/desktop-starter-cdp/) | Provides a generated starter project used for WebView2 CDP bridge acceptance. |
| [`desktop-starter-auto-cdp/`](examples/desktop-starter-auto-cdp/) | Exercises automated starter creation and the first CDP launch flow. |
| [`desktop-starter-auto-cdp2/`](examples/desktop-starter-auto-cdp2/) | Repeats the automated starter/CDP flow to cover subsequent-launch behavior. |

### Extensions, FFI, and devices

| Example | Description |
|---|---|
| [`device-serial.bt`](examples/device-serial.bt) | Scans available serial ports through the device API. |
| [`ext-install-demo/`](examples/ext-install-demo/) | Uses an installed SQLite extension to create, write, and query a local database. |
| [`extension-development/`](examples/extension-development/) | Contains extension-development projects for BT, shared runtimes, and SQLite/WASM packaging. |
| [`ffi-testlib/`](examples/ffi-testlib/) | Calls the cross-platform native test library with explicit FFI signatures and long-running checks. |
| [`ffi-user32/`](examples/ffi-user32/) | Demonstrates Windows `user32.dll` calls with inferred and explicit FFI signatures. |

## Repository layout

| Path | Purpose |
|---|---|
| `.github/` | Public CI workflows, contribution guidance, and README assets. |
| `src/` | Lexer, parser, compiler, bytecode VM, standard library, web runtime, desktop runtime, and bundle support. |
| `src-tauri/` | Tauri configuration, permissions, capabilities, and the minimal compile-time frontend placeholder. |
| `crates/` | The extension SDK and the native FFI test library used by the Cargo workspace. |
| `src-tauri/icons/` | Compile-time icons embedded in `bt` and `bt_app`. |
| `examples/` | Runnable language, extension, web, and desktop examples used by contributors and CI. |
| `benches/` | Repeatable quality and performance workloads. |
| `tools/quality/` | Public regression, benchmark, and long-running validation scripts. |

## Documentation and contributing

- Read the [English documentation](https://btlang.org/en/docs/index) or [Simplified Chinese documentation](https://btlang.org/zh-hans/docs/index).
- Review the [contributing guide](.github/CONTRIBUTING.md) before opening a pull request.
- Follow the [Code of Conduct](CODE_OF_CONDUCT.md) in all project spaces, and report vulnerabilities through the private channel described in the [Security Policy](SECURITY.md).
- Use the [release quality gate](tools/quality/release-gate.md) before publishing a release.

## License

BT is licensed under either of

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option. See [COPYRIGHT](COPYRIGHT) for copyright ownership. Binary
release archives include a target- and feature-specific
`THIRD-PARTY-NOTICES.txt`; bundled extension packages carry their own notices.
