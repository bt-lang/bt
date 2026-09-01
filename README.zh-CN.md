<div align="center">
  <img src=".github/assets/cat_animated.svg" width="190" alt="BT 猫吉祥物">
  <h1>BT 编程语言</h1>
  <p><strong>脚本、Web 服务、桌面应用、FFI 和扩展，共用一套轻量运行时。</strong></p>
  <p>
    <a href="README.md">English</a> ·
    <a href="README.zh-CN.md">简体中文</a> ·
    <a href="https://btlang.org/zh-hans">官网</a> ·
    <a href="https://btlang.org/zh-hans/docs/index">官方文档</a>
  </p>
  <p>
    <img src="https://img.shields.io/badge/implemented%20in-Rust-CE422B?style=flat-square" alt="使用 Rust 实现">
    <img src="https://img.shields.io/badge/runtime-CLI%20%7C%20Web%20%7C%20Desktop-2563EB?style=flat-square" alt="CLI、Web 和桌面运行时">
    <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-374151?style=flat-square" alt="Windows、Linux 和 macOS">
    <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-0F766E?style=flat-square" alt="MIT OR Apache-2.0 双许可证">
  </p>
</div>

BT 是一门使用 Rust 实现的轻量级解释型编程语言。源码以类 JavaScript 语法编写并编译为寄存器式字节码，同一套运行时可用于命令行程序、Web 服务、桌面应用、原生 FFI 和可安装扩展。

## 核心特点

- **语法直观**——支持类 JavaScript 表达式、函数、类、闭包、解构和链式 API。
- **寄存器式虚拟机**——源码先编译为字节码，再由 VM 执行。
- **一套运行时，多种场景**——可开发 CLI 工具、并发 Web 服务和可打包桌面应用。
- **明确的空值语义**——`empty` 表示值不存在，`null` 始终保留为显式值。
- **可扩展设计**——支持原生 FFI、WASM/WASI 扩展、纯 BT 扩展和对象绑定。
- **面向常驻服务**——通过有界运行时资源和可观测统计支持长期运行负载。

## 初识 BT

```bt
fn greet(name) {
    'Hello, ' + name
}

// 输出：Hello, BT
print greet('BT')
```

BT 中每个表达式都有运行时结果。花括号多行代码块默认返回最后一条语句的结果，函数仍可使用 `return` 提前返回。

BT 还明确区分两种空值：

- `empty` 表示“值不存在”，例如字段缺失、下标越界或函数没有结果。
- `null` 是明确存在的空值，用于 JSON null、数据库 NULL 或转换失败。

## 快速开始

先安装稳定版 Rust 工具链，然后在仓库根目录直接运行已经提交的 BT 示例：

```text
cargo run --release --bin bt -- -c examples/compat/empty-null.bt
```

当前目录存在 `main.bt` 时，直接运行 `bt` 会执行该文件；不存在时进入交互模式。

运行基础桌面示例：

```text
cargo run --release --features desktop --bin bt_app -- run examples/desktop
```

## 编译与测试

| 目标 | 命令 |
|---|---|
| CLI 运行时 | `cargo build --release --bin bt` |
| 桌面运行时 | `cargo build --release --features desktop --bin bt_app` |
| 默认测试 | `cargo test` |
| 桌面功能测试 | `cargo test --features desktop` |

Windows 桌面构建需要 MSVC 工具链、Windows SDK 和 WebView2 Runtime。CI 使用的 Linux、macOS 依赖可查看[发布工作流](.github/workflows/build.yml)。

在包含 `app.json` 的桌面项目目录中，可使用已编译的运行时生成独立可执行文件：

```text
path/to/bt_app build
```

## 示例

下表覆盖 [`examples/`](examples/) 中的全部顶层条目。其中既有面向使用者的完整示例，也有针对回归、压力和验收场景的测试夹具。

### 语言与运行时

| 示例 | 简介 |
|---|---|
| [`compat/`](examples/compat/) | 覆盖代码块返回值、类与闭包、解构、`empty`/`null` 和标准库 snake_case 命名的语义回归脚本。 |
| [`bytes-modbus.bt`](examples/bytes-modbus.bt) | 构造 Modbus TCP 请求帧并解析包含寄存器数据的二进制响应。 |
| [`permission-stats.bt`](examples/permission-stats.bt) | 验证权限允许/拒绝配置及运行时拒绝计数。 |
| [`process-pipe.bt`](examples/process-pipe.bt) | 启动子进程并读取标准输出、标准错误和退出信息。 |
| [`reqwest-pool-bench.bt`](examples/reqwest-pool-bench.bt) | 重复请求本地 HTTP 地址并输出连接池复用统计。 |
| [`runtime-pools-stats.bt`](examples/runtime-pools-stats.bt) | 输出 HTTP、MySQL 连接池上限和当前事务状态。 |
| [`runtime-stats.bt`](examples/runtime-stats.bt) | 读取共享运行时及有界 I/O 配置的最小状态快照。 |

### 网络与 Web

| 示例 | 简介 |
|---|---|
| [`net-phase2-tcp-server.bt`](examples/net-phase2-tcp-server.bt) | 启动带连接、消息、关闭和错误回调的事件驱动 TCP 回显服务。 |
| [`net-phase2-tcp-client.bt`](examples/net-phase2-tcp-client.bt) | 连接 TCP 示例服务，完成一次消息收发后关闭连接。 |
| [`net-phase2-udp-server.bt`](examples/net-phase2-udp-server.bt) | 启动 UDP 回显 socket，并按来源地址返回消息。 |
| [`net-phase2-udp-client.bt`](examples/net-phase2-udp-client.bt) | 向本地 UDP 示例发送一个数据报。 |
| [`net-phase2-ws-server.bt`](examples/net-phase2-ws-server.bt) | 创建包含生命周期回调和回显消息的 WebSocket 路由。 |
| [`net-phase2-ws-client.bt`](examples/net-phase2-ws-client.bt) | 连接 WebSocket 示例并异步处理服务端消息。 |
| [`net-phase3-stats.bt`](examples/net-phase3-stats.bt) | 查看有界网络队列、消息限制和空闲超时配置。 |
| [`net-phase3-tcp-burst-client.bt`](examples/net-phase3-tcp-burst-client.bt) | 连续发送一组 TCP 请求，验证重复收发处理。 |
| [`net-stress-tcp-server.bt`](examples/net-stress-tcp-server.bt) | 对持续 TCP 消息负载进行计数和回显，用于压力验证。 |
| [`net-stress-tcp-client.bt`](examples/net-stress-tcp-client.bt) | 通过重复连接和批量载荷驱动 TCP 压力服务。 |
| [`net-stress-udp-server.bt`](examples/net-stress-udp-server.bt) | 持续统计高频 UDP 数据报，直到收到明确的停止消息。 |
| [`net-stress-udp-client.bt`](examples/net-stress-udp-client.bt) | 发送带编号载荷的高频 UDP 压力数据。 |
| [`net-stress-ws-server.bt`](examples/net-stress-ws-server.bt) | 对 WebSocket 消息进行回显和计数，验证长连接负载。 |
| [`net-stress-ws-client.bt`](examples/net-stress-ws-client.bt) | 驱动 WebSocket 压力服务并校验回显响应。 |
| [`net-web/`](examples/net-web/) | 通过 `net.listen({type: 'web'})` 和本地站点入口启动 BT Web 引擎。 |
| [`web-blocking-policy/`](examples/web-blocking-policy/) | 演示 Web 请求处理中允许或拒绝的阻塞操作边界。 |
| [`longrun-audit/`](examples/longrun-audit/) | 组合本地 Web 服务与探针负载，用于长期运行资源审计。 |

### 桌面应用

| 示例 | 简介 |
|---|---|
| [`desktop/`](examples/desktop/) | 小型日记本应用，展示静态前端和 `window.bt.call()` 后端桥接。 |
| [`desktop-api/`](examples/desktop-api/) | 验证通过 `window.bt` 暴露的公开桌面 API。 |
| [`desktop-dev-reload/`](examples/desktop-dev-reload/) | 演示资源监听、排除规则和开发期热刷新。 |
| [`desktop-html/`](examples/desktop-html/) | 不使用 BT 后端，直接打包 HTML、CSS 和 JavaScript 前端应用。 |
| [`desktop-icon-appjson/`](examples/desktop-icon-appjson/) | 验证通过 `app.json` 配置应用图标。 |
| [`desktop-icon-html/`](examples/desktop-icon-html/) | 验证带 HTML 入口和 ICO 资源的静态应用打包。 |
| [`desktop-remote/`](examples/desktop-remote/) | 加载远程网页，同时保留本地 BT 桥接能力。 |
| [`desktop-server/`](examples/desktop-server/) | 启动本地 BT 服务并在桌面窗口中加载页面。 |
| [`desktop-starter-cdp/`](examples/desktop-starter-cdp/) | 用于 WebView2 CDP 桥接验收的初始化项目夹具。 |
| [`desktop-starter-auto-cdp/`](examples/desktop-starter-auto-cdp/) | 验证自动创建初始化项目及首次 CDP 启动流程。 |
| [`desktop-starter-auto-cdp2/`](examples/desktop-starter-auto-cdp2/) | 重复自动初始化与 CDP 流程，覆盖后续启动行为。 |

### 扩展、FFI 与设备

| 示例 | 简介 |
|---|---|
| [`device-serial.bt`](examples/device-serial.bt) | 通过设备 API 扫描当前可用的串口。 |
| [`ext-install-demo/`](examples/ext-install-demo/) | 使用已安装的 SQLite 扩展创建、写入并查询本地数据库。 |
| [`extension-development/`](examples/extension-development/) | 包含 BT 扩展、共享运行时及 SQLite/WASM 打包开发项目。 |
| [`ffi-testlib/`](examples/ffi-testlib/) | 使用完整 FFI 签名调用跨平台原生测试库，并包含长稳检查。 |
| [`ffi-user32/`](examples/ffi-user32/) | 演示在 Windows 上使用推断签名和完整签名调用 `user32.dll`。 |

## 仓库结构

| 路径 | 用途 |
|---|---|
| `.github/` | 对外 CI 工作流、贡献指南和 README 资源。 |
| `src/` | 词法、语法、编译器、字节码 VM、标准库、Web、桌面和 Bundle 核心源码。 |
| `src-tauri/` | Tauri 配置、权限、能力和编译所需的最小前端占位页。 |
| `crates/` | Cargo 工作区内的扩展 SDK 与原生 FFI 测试库。 |
| `src-tauri/icons/` | 编译期嵌入 `bt` 和 `bt_app` 的图标。 |
| `examples/` | 供使用者、贡献者和 CI 运行的语言、扩展、Web 与桌面示例。 |
| `benches/` | 可重复执行的质量与性能工作负载。 |
| `tools/quality/` | 对外公开的回归、基准和长稳验证脚本。 |

## 文档与贡献

- 阅读[简体中文文档](https://btlang.org/zh-hans/docs/index)或 [English documentation](https://btlang.org/en/docs/index)。
- 提交 Pull Request 前请先查看[贡献指南](.github/CONTRIBUTING.md)。
- 所有项目交流均应遵守[行为准则](CODE_OF_CONDUCT.md)；安全漏洞请按[安全策略](SECURITY.md)通过私密渠道报告。
- 发布版本前请执行[发布质量门禁](tools/quality/release-gate.zh-CN.md)。

## 许可证

BT 采用双许可证，使用者可任选以下一种：

- [Apache License 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

版权归属见 [COPYRIGHT](COPYRIGHT)。二进制发布归档会附带按目标平台和实际
feature 生成的 `THIRD-PARTY-NOTICES.txt`；打包的扩展包按需携带自身声明。
