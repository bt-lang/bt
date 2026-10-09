# 发布质量与合规门禁

创建任何发布标签前，必须在已提交且干净的工作树中运行
`tools/quality/release-check.ps1`。完整门禁才能用于正式发布；
跳过参数只用于本地迭代，不能作为发布通过证明。

## 必需工具

- Stable Rust 及当前发布平台需要的 target。
- `cargo-about` 0.9.2：

  ```text
  cargo install cargo-about --version 0.9.2 --locked --features cli
  ```

- `cargo-audit` 0.22.2：

  ```text
  cargo install cargo-audit --version 0.22.2 --locked
  ```

- Gitleaks 8.30.1。CI 会使用官方 Windows x64 归档并校验 SHA-256：
  `d29144deff3a68aa93ced33dddf84b7fdc26070add4aa0f4513094c8332afc4e`。

## 自动门禁

完整门禁包含：

- `cargo fmt --all -- --check`。
- 根工作区与 SQLite 扩展的 locked metadata。
- 按平台和实际 feature 生成的 `THIRD-PARTY-NOTICES.txt`、非 Rust
  分发内容跟踪清单，以及 SQLite 扩展包内声明的可重复生成校验；源码文件中的
  许可证注释会保留，但不会把无关的实现代码复制进清单。
- 两个 SQLite `.bts` 内容完全一致且与当前源码、锁文件、manifest、
  bindings、WASM、BT 许可证与 SQLite/rusqlite 声明一致。
- `tools/compliance/verify-rustsec-policy.ps1`：必须保持零漏洞，且实际
  informational advisory 集合必须与已审查的依赖路径、target、feature、
  复核日期和移除条件完全一致。
- `cargo test --locked` 和 `cargo test --locked --features desktop`。
- `cargo check --locked --workspace --all-targets --all-features`；工作区 lint 将
  warning 视为错误。
- Debug/Release CLI、当前平台桌面 Release、关键示例回归和性能基线。
- 通过 `git archive` 产生不含 Git 历史的干净公开导出树。
- Gitleaks 扫描导出文件树，并将 `.bts`/`.btr`/`.zip` 递归解包两层后
  再扫描。
- 归档必须包含程序、`README.md`、`README.zh-CN.md`、`LICENSE-APACHE`、
  `LICENSE-MIT`、`COPYRIGHT` 和该归档专属的 `THIRD-PARTY-NOTICES.txt`。

运行完整本地门禁：

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/release-check.ps1
```

仅本地迭代时可显式跳过高开销阶段：

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/release-check.ps1 -SkipDesktop -SkipBenchmarks
```

## 发布产物

GitHub 流水线会先生成许可证清单，再产生四个经过校验的 ZIP：

- `bt-windows-x64.zip`：`bt.exe` 和 `bt-app.exe`。
- `bt-linux-x64.zip`：静态链接 musl 的 `bt` 和 GNU `bt-app`。
- `bt-macos-arm64.zip`：Apple Silicon `bt` 和 `bt-app`。
- `bt-macos-x64.zip`：Intel `bt` 和 `bt-app`。

实际文件名会在 `.zip` 前包含 Cargo 包版本。每个归档中的清单只覆盖该归档
两个程序实际采用的 target 和 feature 依赖图。手动通过 `workflow_dispatch`
运行时，归档会作为流水线产物保留 30 天。推送与 Cargo 包版本完全一致、以
`v` 开头的标签（例如 `v1.1.3`）时，也会构建这些归档并发布到 GitHub
Release。普通分支推送不会触发该流水线。

## 长稳验证

普通发布窗口至少运行 10 分钟：

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/run-longrun.ps1 -Build -DurationMinutes 10
```

高风险 I/O、Web、网络、进程、Bytes 或桌面改动建议运行：

```powershell
powershell -ExecutionPolicy Bypass -File tools/quality/run-longrun.ps1 -Build -DurationMinutes 60 -IncludeNet
```

`target/quality/` 下的 JSON 只用作本地证据或 CI artifact，不得提交。

[English](release-gate.md)

## 本地性能迭代

普通性能开发使用增量开发构建，只有明确要求发布新版本时才使用 release 构建。对照必须使用同一构建配置、机器、负载和并发数，并保留两个可执行文件的哈希及 JSON 报告。开发构建耗时不能推算生产吞吐量。基准脚本默认使用 `target/debug/bt`，`-Build` 执行 `cargo build --locked --bin bt`；正式发布门禁仍显式传入 release 可执行文件。

```powershell
tools/quality/run-benchmarks.ps1 -SelfTest
tools/quality/run-benchmarks.ps1 -Build -Iterations 7 -WebConcurrency 1 -Output target/quality/performance-serial.json
tools/quality/run-benchmarks.ps1 -Iterations 7 -WebConcurrency 16 -WebRequests 1000 -Output target/quality/performance-concurrent.json
python tools/quality/verify-web-deadlines.py --bt target/debug/bt.exe
```

Unix 的验收可执行文件路径为 `target/debug/bt`。`WebConcurrency` 默认为 `1`，范围为 `1..256`；`WebRequests` 是全部并发请求的总数。Web 延迟样本记录单次请求，吞吐量使用完成请求数除以包含客户端测试开销的整个测量窗口耗时；CLI 耗时包含启动、编译和执行。`total_operations`、`total_elapsed_ms`、`web_concurrency` 和 `bt_sha256` 用于复核结果。任何 HTTP 错误都会让基准失败，不计入成功吞吐量。旧版 Web 吞吐计算会把结果乘以请求数，因此不能直接对比旧吞吐数值。
