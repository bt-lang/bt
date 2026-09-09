# Pending release notes

This file is the source of truth for user-visible changes that have not been
published in a numbered BT release. Keep the English and Simplified Chinese
sections equivalent. During a release, move every dated entry into the matching
official website update page, then restore this file to this empty template.

## English

<!-- Add dated release-note entries here. -->

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

<!-- 在此添加带日期的待发布说明。 -->

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
