# Pending release notes

This file is the source of truth for user-visible changes that have not been
published in a numbered BT release. Keep the English and Simplified Chinese
sections equivalent. During a release, move every dated entry into the matching
official website update page, then restore this file to this empty template.

## English

- 2026-09-21: Add `window.bt.surface` for xcap screen freezing, binary PNG reads, Canvas PNG imports, image clipboard output, PNG/JPEG saving, and explicit image release. Application-local images use bounded storage shared by editors and pins. Apps can create local resource child windows with physical or logical content geometry, native caption offset correction, optional transparency and always-on-top, image release on destruction, and bounded structured messages and close notifications. Image operations follow desktop/screen permissions, saving also requires `fs` permission, and child-window operations require desktop permission. Screen acquisition no longer depends on the built-in selector overlay: Wayland can attempt capture while child-window placement is left to the compositor; exact overlay placement and always-on-top behavior remain platform-dependent. File and directory dialogs invoked from a WebView no longer block the UI event loop and belong to the calling window, including child editors. On Windows, undecorated surface windows are created hidden and disable system window transitions to avoid startup flashes and unwanted zoom/fade effects.

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
