# Pending release notes

This file is the source of truth for user-visible changes that have not been
published in a numbered BT release. Keep the English and Simplified Chinese
sections equivalent. During a release, move every dated entry into the matching
official website update page, then restore this file to this empty template.

## English

- 2026-10-09: Reuse cleared VM register buffers within a per-VM limit of 1 MiB and 32 idle frames, and remove an extra argument-value copy when calling BT functions. Reclaim stale class and instance bytecode-owner records during object creation and after outermost execution, reducing retained memory in resident applications while preserving live method owners, closures, return values, and error locations. No syntax or API migration is required.

- 2026-10-09: Reduce VM source-location and variable-lookup allocations while preserving diagnostics and local/closure precedence. Web requests now share a queue-inclusive execution deadline: expired queued scripts are skipped, BT loops and nested calls stop cooperatively, and sleep/HTTP/MySQL waits honor the remaining request budget. Buffered dynamic output is checked before appending; final returned response bodies are still checked before sending. Native blocking calls and external side effects cannot be forcibly interrupted or rolled back, and explicitly submitted background tasks keep their own lifecycle. Correct Web benchmark throughput accounting and add configurable concurrency and deadline/recovery acceptance checks.

- 2026-09-26: Linux desktop applications now require native Wayland, with no X11/XWayland backend or fallback. Screenshot acquisition uses the Screenshot portal with bounded authorization, image decoding, and temporary-file cleanup; clipboard operations use asynchronous native GTK selection handling. Global shortcuts use GlobalShortcuts independently of window positioning and no longer initialize an X11 hotkey manager. Install a desktop launcher matching `app.id` and authorize the desktop requests. Packaged Linux apps can enable `app.single_instance` to forward later launches to the existing window through per-user ownership and bounded acknowledged IPC (32 KiB per request, 64 queued requests). Development builds disable optimization, debug symbols, and LTO while retaining incremental caching; the formal release profile is unchanged.

- 2026-09-25: Native Wayland global shortcuts now use the desktop GlobalShortcuts portal with bounded asynchronous registration and session cleanup. An installed desktop launcher matching `app.id` and desktop authorization are required; concurrent registrations share a consent request. Unsupported keys, unavailable portals, rejection, and timeout produce errors instead of silently registering unusable shortcuts. The desktop controls the actual key bindings; Windows and macOS retain their native backend.

- 2026-09-25: Linux child WebView windows are created on the GTK main thread, preventing application crashes when opening settings, capture editors, or pinned images.

- 2026-09-25: Desktop builds now use native executable names: Windows retains `.exe`, while Linux and macOS omit it. Linux builds also export a PNG icon and `.desktop` launcher and attempt to attach a local GNOME file icon. Linux fixed-size windows honor programmatic content sizes and release their fixed-size constraints during fullscreen, restoring them on exit. Window placement reports `position_supported` from the active GTK backend; native Wayland returns zero position placeholders and rejects absolute positioning instead of reporting a successful move. Screen surface placement uses the same backend capability. Windows packaging and positioning remain unchanged.

- 2026-09-21: Add `window.bt.surface` for native screen freezing, binary PNG reads, Canvas PNG imports, image clipboard output, PNG/JPEG saving, and explicit image release. Application-local images use bounded storage shared by editors and pins. Apps can create local resource child windows with physical or logical content geometry, native caption offset correction, optional transparency and always-on-top, image release on destruction, and bounded structured messages and close notifications. Image operations follow desktop/screen permissions, saving also requires `fs` permission, and child-window operations require desktop permission. Screen acquisition no longer depends on the built-in selector overlay: Wayland uses the Screenshot portal while child-window placement is left to the compositor; exact overlay placement and always-on-top behavior remain platform-dependent. File and directory dialogs invoked from a WebView no longer block the UI event loop and belong to the calling window, including child editors. On Windows, undecorated surface windows are created hidden and disable system window transitions to avoid startup flashes and unwanted zoom/fade effects.

- 2026-09-17: Add explicit per-user interpreter installation with `bt install` and online updates with `bt update`, also available at the interactive prompt; remove the `--install` alias. Updates require an existing installation, download the latest stable platform ZIP directly from the official website, verify size, SHA-256 and executable version, and preserve equal/newer versions and the old binary on download or validation failure. Windows updates retain at most one previous running executable; existing sessions keep their original version. `bt update <name> [--project <dir>]` updates an installed official extension without downgrading or accepting a version argument, verifies compatibility and checksum, and restores the old package if publication or project validation fails. Installation configures a fixed user executable path, user `PATH`, and platform file associations on Windows, Linux, and macOS; only a newer semantic version replaces an installed binary. Associated `.bt` files run in a terminal and wait for Enter after completion, errors, or `exit()` without modifying the script. Ordinary launches perform no installation checks and add no exit pause; `bt install <name>` continues to install extensions. Linux desktop integration requires a desktop session, and systems may require the user to select the default application once.

- 2026-09-14: Windows packaged apps can opt into `app.single_instance` to forward later launches and file paths to the existing window through a bounded request queue. The frontend can drain queued arguments and listen for later requests. File associations are registered only on launches without file arguments, and Explorer is refreshed only when registry values change.

- 2026-09-11: Extension manifests can now carry catalog summaries, public developer identity, repository, SPDX license, and localized display metadata with canonical BCP 47 keys such as `zh-CN`. Official extension releases use the committed `manifest.json`, `bindings.json`, `README.md`, and `README.zh-CN.md` as the only maintained metadata and documentation sources for website publication and editor tooling.

<!-- Add dated release-note entries here. -->

### 2026-09-11

- Official extensions now maintain their English and Simplified Chinese README files only beside the source in `extension/<name>/`. Publishing an extension copies those files into an immutable website snapshot for the exact extension version, so later documentation changes on `main` do not alter the website until another version is published. Public source validation requires both language files.

### 2026-09-08

- Official SQLite extension source now lives in `extension/sqlite/`. Build and
  test commands use the new path; obsolete SQLite demos and checked-in `.bts`
  copies have been removed. Build packages locally from source or install the
  published extension through `bt install sqlite`. The preferred constructor is
  now `sqlite(path, options)`; `sqlite_open` remains a deprecated compatible alias.

- Add independently built `image` and `video` official extensions: bounded image
  editing and encoding, and asynchronous video probing, transcoding, trimming,
  concatenation, frames, resizing, and audio extraction/replacement. Video uses
  separately installed FFmpeg/ffprobe and requires this updated BT host. Their
  constructors are `image(path)` and `video(path, options)`. Image paths load on
  demand; `img.create(...)` and `img.decode(bytes)` replace pixels on the same
  object without reading the bound file or writing it automatically. Recover
  video jobs with `clip.job(id)` for the same source. The unreleased prefixed
  media entry names have been removed; saving still requires explicit arguments.
- Extension manifests no longer declare per-package permissions. All local `.bts`
  packages use the same host ABI without registry lookup or source-based runtime
  restrictions. The removed `permissions` field is rejected so extension authors
  must use the new manifest format. WASI project file access and the optional
  bounded native process service now follow only the BT process-wide policy.
  Native processes still run outside the WASI sandbox, so installing an extension
  means trusting its code.
- Chained shared-extension calls and recovered job handles reuse their existing
  host object identity; closing or rebuilding a timed-out worker retires its
  routes. Array-returning extension lookups can return `empty` for absence,
  preserving the distinction from explicit `null`.

## 简体中文

- 2026-10-09：复用已清空的 VM 寄存器缓冲，每个 VM 最多保留 1 MiB、32 个空闲帧，并消除调用 BT 函数时一次多余的参数值复制。在对象创建期间和最外层执行结束后清理失效的类与实例字节码归属记录，降低常驻应用的内存滞留，同时保留存活对象的方法归属、闭包、返回值和错误定位。无需迁移语法或 API。

- 2026-10-09：减少 VM 源码位置和变量读取的分配，同时保留错误定位、局部变量与闭包的优先级。Web 请求现在共用包含排队时间的执行期限：过期排队脚本会被跳过，BT 循环和嵌套调用协作式停止，休眠及 HTTP/MySQL 等待遵循请求剩余时间。动态输出缓冲在追加前检查上限，最终返回的响应体仍在发送前检查。原生阻塞调用和外部副作用无法被强制中断或回滚，显式提交的后台任务保留独立生命周期。修正 Web 基准吞吐量计算，并新增可配置并发数与超时恢复验收。

- 2026-09-26：Linux 桌面应用现要求原生 Wayland，不提供 X11/XWayland 后端或回退。屏幕图像通过 Screenshot portal 获取，授权等待、图像解码及临时文件清理均有边界；剪贴板使用异步原生 GTK selection。全局快捷键独立于窗口定位能力使用 GlobalShortcuts，不再初始化 X11 快捷键管理器。需要安装与 `app.id` 匹配的桌面启动器并允许桌面授权请求。Linux 打包应用可启用 `app.single_instance`，通过用户私有所有权和有界、带确认的 IPC 将重复启动转发到已有窗口（每条消息最多 32 KiB，最多排队 64 条）。开发构建关闭优化、调试符号和 LTO 并保留增量缓存；正式 release 配置不变。

- 2026-09-25：原生 Wayland 全局快捷键改用桌面 GlobalShortcuts portal，提供有界异步注册和会话清理。需要安装与 `app.id` 匹配的桌面启动器并获得桌面授权；并发注册合并为一次授权请求。不支持的按键、portal 不可用、拒绝授权和超时均返回错误，不再静默注册无法触发的快捷键。实际按键绑定由桌面管理；Windows 和 macOS 保留原生后端。

- 2026-09-25：Linux 子 WebView 窗口改为在 GTK 主线程创建，避免打开设置、截图编辑器或贴图窗口时整个应用崩溃。

- 2026-09-25：桌面构建改用平台原生可执行文件名：Windows 保留 `.exe`，Linux 与 macOS 不加后缀。Linux 构建同时导出 PNG 图标与 `.desktop` 启动器，并尝试设置本机 GNOME 文件图标。Linux 固定大小窗口支持通过程序调整内容尺寸，全屏时解除固定大小约束，退出全屏后恢复。窗口布局根据实际 GTK 后端返回 `position_supported`；原生 Wayland 的位置字段返回零占位值，并明确拒绝绝对定位，不再误报移动成功。截图 surface 使用同一后端能力判断定位。Windows 打包与定位行为保持不变。

- 2026-09-21：新增 `window.bt.surface`，提供 xcap 屏幕冻结、二进制 PNG 读取、Canvas PNG 导入、图片剪贴板输出、PNG/JPEG 保存和显式图片释放。应用内图片使用编辑器与贴图共享的有界存储。应用可创建加载本地资源的子窗口，支持物理或逻辑内容坐标、原生标题栏偏移修正、可选透明和始终置顶、销毁时释放图片，以及有界结构化消息和关闭通知。图片操作遵循 desktop/screen 权限，保存还要求 `fs` 权限，子窗口操作要求 desktop 权限。屏幕获取不再依赖内置选区覆盖层：Wayland 可以尝试捕获，子窗口定位交由合成器处理；精确覆盖定位和始终置顶行为仍取决于平台。从 WebView 调用的文件和目录对话框不再阻塞 UI 事件循环，并归属于发起调用的窗口，包括子编辑器。Windows 无标题栏 surface 窗口以隐藏状态创建并禁用系统窗口过渡，避免启动闪现和不必要的缩放／淡入淡出效果。

- 2026-09-17：新增显式用户级解释器安装 `bt install` 和在线更新 `bt update`，交互提示符也支持相同命令；移除 `--install` 别名。在线更新要求已有安装，直接从官网下载对应平台最新正式版 ZIP，校验大小、SHA-256 和可执行文件版本；同版或更高版本不替换，下载或校验失败保留原解释器。Windows 更新最多保留一个仍在运行的旧可执行文件，已有会话继续使用原版本。`bt update <name> [--project <dir>]` 更新已安装的官方扩展，不降级、不接受版本参数，校验兼容性和哈希，发布或项目校验失败时恢复旧包。安装在 Windows、Linux 和 macOS 上配置固定的用户可执行文件路径、用户 `PATH` 与平台文件关联；只有语义版本较新的解释器才替换安装版。通过关联打开的 `.bt` 文件在终端中运行，执行完成、报错或调用 `exit()` 后等待按 Enter，不修改脚本。普通启动不检查安装状态，也不增加退出暂停；`bt install <name>` 继续安装扩展。Linux 桌面集成要求有桌面会话，系统可能要求用户选择一次默认打开方式。

- 2026-09-14：Windows 打包应用可启用 `app.single_instance`，把后续启动和文件路径转交已有窗口，并通过有界请求队列供前端读取和监听。文件关联仅在不带文件参数的启动时注册，且只在注册表内容发生变化时通知资源管理器刷新。

- 2026-09-11：扩展 manifest 现在可以携带目录摘要、公开开发者身份、源码仓库、SPDX 许可证以及使用 `zh-CN` 等规范 BCP 47 键的本地化展示元数据。官方扩展发布使用已提交的 `manifest.json`、`bindings.json`、`README.md` 和 `README.zh-CN.md` 作为官网发布与编辑器工具唯一需要维护的元数据和文档源。

<!-- 在此添加带日期的待发布说明。 -->

### 2026-09-11

- 官方扩展现在只在 `extension/<name>/` 源码旁维护英文和简体中文 README。发布扩展时把这些文件复制为该扩展精确版本的官网不可变快照，因此 `main` 上后续发生的文档变化不会影响官网，直到开发者发布另一个版本。公开源码校验要求同时包含两种语言文件。

### 2026-09-08

- 官方 SQLite 扩展源码统一移至 `extension/sqlite/`，构建与测试命令使用新路径；
  删除过时的 SQLite demo 和仓库内预构建的 `.bts` 副本。可从源码在本地构建扩展包，
  或通过 `bt install sqlite` 安装已发布的扩展。推荐构造入口改为 `sqlite(path, options)`；
  `sqlite_open` 保留为已弃用的兼容别名。
- 新增独立构建的官方 `image` 和 `video` 扩展：提供有界图片编辑与编码，以及异步
  视频探测、转码、裁剪、拼接、抽帧、缩放和音轨提取/替换。视频扩展使用另行安装的
  FFmpeg/ffprobe，并要求使用本次更新后的 BT 宿主。构造入口为 `image(path)` 与
  `video(path, options)`。图片路径按需加载；`img.create(...)` 和 `img.decode(bytes)`
  在同一对象上替换像素，不读取绑定文件，也不自动写入文件。使用同来源的 `clip.job(id)`
  恢复视频任务。删除尚未发布的媒体前缀入口；保存仍需显式传参。
- 扩展 manifest 不再声明包级权限。所有本地 `.bts` 使用相同宿主 ABI，不查询官网，
  也不按来源限制运行能力。已删除的 `permissions` 字段会被拒绝，扩展作者必须使用新版
  manifest 格式。WASI 项目文件访问和可选的有界原生进程服务现在只服从 BT 进程级
  策略。原生进程仍运行在 WASI 沙箱之外，因此安装扩展即表示信任其代码。
- shared 扩展链式调用和恢复的任务句柄复用已有宿主对象身份；关闭或重建超时 worker
  时移除相应路由。声明数组返回的扩展查询可用 `empty` 表示不存在，并保留与显式
  `null` 的区别。
