# Video 扩展 1.0.0

## 功能

用于异步处理本地视频文件的独立官方 `kind=wasm` 扩展，按需安装，不依赖 image 或 SQLite 扩展。宿主的 PATH 中必须已经安装 FFmpeg 和 ffprobe；本扩展包不包含也不下载 FFmpeg 可执行文件或编解码库。已验证的后端为 Windows x64 上的 FFmpeg 8.1.1。WASI 模块可独立构建。Linux 和 macOS 是计划支持的宿主平台，但尚未验证其端到端行为。注册表中的扩展包使用 1.1.4 之后的当前扩展 manifest 格式，并要求 BT 包含可选的 `bts_host.process_request` 导入。公开发布的 BT 1.1.4 二进制既不能安装也不能运行该包；请使用更新后的 `main` 构建或下一个正式版本。

## 语法、参数和返回值

所有名称均使用 snake_case。每个处理方法都会立即返回一个 `VideoJob`；解码和编码在有界的原生后台进程中执行。`info()` 同样是异步操作。选项对象传入 `{}` 时使用默认值。`video` 是唯一的全局入口；返回对象应使用 `clip` 或 `source` 等变量名，避免局部变量遮蔽入口函数。

| 调用 | 参数与行为 | 返回值 |
|---|---|---|
| `video(path, options)` | 已存在的媒体文件名；打开选项见下表。仅校验文件系统元数据。 | `Video` |
| `clip.job(id)` | 来自 `job.id()` 的正整数；必须使用仍有效的源对象，且规范化后的项目相对路径、项目及仍在运行的 worker 均与任务一致。 | 已有 `VideoJob` 的别名 |
| `clip.info()` | 探测时长、容器和流元数据。 | `VideoJob` |
| `clip.transcode(output, options)` | 编码为 MP4 或 WebM；编码选项见下表。 | `VideoJob` |
| `clip.trim(output, start, duration, options)` | 起点以秒为单位，范围 0..86400；时长以秒为单位，范围 0.001..86400；截取范围必须位于源媒体内。 | `VideoJob` |
| `clip.concat(paths, output, options)` | 包含 1..7 个额外项目相对文件名的数组；当前源媒体作为第一段。 | `VideoJob` |
| `clip.frame(output, time)` | 提取一张 PNG/JPEG 图片；时间以秒为单位，包含 0，不包含源媒体时长的终点。 | `VideoJob` |
| `clip.resize(output, width, height, options)` | 宽高均为 2..8192 的偶数整数，总像素数最多 16,777,216。 | `VideoJob` |
| `clip.extract_audio(output)` | 将第一条音频流输出为 WAV、FLAC 或 M4A。 | `VideoJob` |
| `clip.replace_audio(audio, output, options)` | 使用已存在媒体文件中的音频替换音轨；保留视频时长。 | `VideoJob` |
| `clip.close()` | 释放源描述符；已有任务保留各自的源信息快照。 | `true` |
| `job.status()` | 轮询一次并推进预检/编码阶段，不等待媒体处理结束。 | 状态对象 |
| `job.cancel()` | 取消未完成的工作并返回状态快照；已完成的输出予以保留。 | 状态对象 |
| `job.id()` | 返回用于在后续请求中恢复任务的标识符。 | 正整数 |
| `job.close()` | 取消未完成的工作并释放句柄；全部别名随之失效，恢复已关闭的 ID 会失败。 | `true` |

顶层路径参数使用 BT 的 `path_read`/`path_write` 角色，因此 `@/` 指向项目根目录，相对于源码的路径遵循普通 BT 路径解析规则。`concat(paths, ...)` 数组中的项目必须是**项目相对路径**，不带 `@/`。所有路径都必须位于项目内。URL 语法、父目录跳转、百分号、符号链接和控制字符均会被拒绝。输出文件的父目录必须已经存在。输出文件不能预先存在：任务通过 `create_new` 预留文件，绝不替换调用者已有的文件。与 `image` 通过文件路径交换数据，不传递媒体字节、Base64 字符串或跨扩展对象句柄。

### 打开选项

| 字段 | 类型 | 必填 | 默认值 | 有效范围 | 含义 |
|---|---|---|---|---|---|
| `timeout_ms` | int | 否 | 60000 | 1..300000 | 整个任务共用一个截止时间，包含所有探测、编码及轮询调用之间的等待时间。 |

### 编码选项

| 字段 | 类型 | 必填 | 默认值 | 有效范围 | 含义 |
|---|---|---|---|---|---|
| `quality` | int | 否 | 5 | 2..31 | MPEG-4 的 `q:v`；VP9 CRF 为该数值的两倍，即 4..62。数值越低质量越高；它不是百分比，也不保证输出大小。 |
| `fps` | number | 否 | 无 | 1..120，有限数值 | 强制使用恒定输出帧率。省略时保留源时间信息；拼接除外，拼接使用第一段源媒体的平均帧率。 |
| `audio` | bool | 否 | true | true/false | 保留第一条音频流；为 true 时，拼接的每个输入都必须包含音频。为 false 时生成纯视频。`replace_audio` 始终包含替换音频。 |

未知选项字段和不支持的组合会返回英文错误。

### 状态对象

| 字段 | 类型 | 必填 | 默认值 | 可选值/范围 | 含义 |
|---|---|---|---|---|---|
| `state` | string | 是 | `probing` | `probing`、`running`、`succeeded`、`failed`、`cancelled`、`timed_out` | 任务生命周期状态。 |
| `elapsed_ms` | int | 是 | 0 | 非负数 | 自任务创建起经过的实际时间。 |
| `processed_seconds` | number | 是 | 0 | 非负数 | FFmpeg 进度中最新的已编码输出时间轴位置；探测期间保持为 0。不是百分比。 |
| `error` | string | 是 | 空字符串 | 受宿主 stderr 的 1 MiB 尾部保留上限约束 | 英文失败详情；成功时为空。 |
| `result` | object 或 empty | 是 | `empty` | 元数据或输出对象 | 仅在成功完成后提供结果。缺失的流字段仍然不存在；外部 JSON null 保留为 BT `null`。 |

输出结果包含：

| 字段 | 类型 | 必填 | 默认值 | 范围 | 含义 |
|---|---|---|---|---|---|
| `path` | string | 是 | 无 | 项目相对文件名 | 已完成的输出文件。 |
| `size_bytes` | int | 是 | 无 | 至少 1，且小于 2 GiB | 编码文件大小。 |

`info()` 结果包含：

| 字段 | 类型 | 必填 | 默认值 | 范围 | 含义 |
|---|---|---|---|---|---|
| `duration` | number | 是 | 无 | 0.001..86400 秒 | 容器时长。 |
| `format` | string | 是 | 无 | FFprobe 格式标识符 | 容器名称；可能包含以逗号分隔的别名。 |
| `size_bytes` | int | 是 | 未报告时为 0 | 0..32 GiB | ffprobe 报告的输入大小。 |
| `streams` | 对象数组 | 是 | 无 | 1..16 条流 | 流描述符，见下表。 |

流对象仅公开以下 ffprobe 字段；无法获取的字段不存在。有理数和时间字符串保留后端精度，避免有损归一化。

| 字段 | 类型 | 必填 | 默认值 | 范围 | 含义 |
|---|---|---|---|---|---|
| `index` | int | 取决于后端 | 无 | 非负数 | 容器中的流索引。 |
| `codec_type` | string | 取决于后端 | 无 | `video`、`audio` 或其他 FFprobe 类型 | 媒体种类。 |
| `codec_name` | string | 取决于后端 | 无 | 后端编解码器标识符 | 解码编解码器。 |
| `width`, `height` | int | 仅视频 | 无 | 1..8192；乘积 <=16,777,216 | 应用显示旋转前的编码帧尺寸。 |
| `avg_frame_rate` | string | 仅视频 | 无 | 有理数；>0 且 <=240 fps | 源媒体平均帧率。 |
| `sample_rate` | string | 仅音频 | 无 | 8000..192000 的整数文本 | 音频采样率，单位 Hz。 |
| `channels` | int | 仅音频 | 无 | 1..8 | 源声道数量。 |
| `duration` | string | 取决于后端 | 无 | 秒数文本 | 单条流的时长。 |
| `start_time` | string | 取决于后端 | 无 | 秒数文本 | 单条流的起始时间戳。 |

## 格式与时间轴规则

输入为本地 MP4/MOV/MKV/WebM/AVI 视频或 WAV/MP3/M4A/FLAC/OGG 音频。根据文件名显式选择解复用器；不支持伪装的播放列表、网络协议和设备输入。实际可用的解码器取决于宿主安装的 FFmpeg。不支持的编解码器通过 `job.status().error` 报告失败。

| 输出后缀 | 编码 |
|---|---|
| `.mp4` | 原生 MPEG-4 Part 2 视频 + AAC 音频，启用 faststart；不是 H.264。 |
| `.webm` | `libvpx-vp9` 视频 + `libopus` 音频；必须安装这些编码器。 |
| `.png` | 一张无损 RGB 图片。 |
| `.jpg`, `.jpeg` | 一张有损 JPEG 图片，使用 FFmpeg 编码器的默认设置。 |
| `.wav` | PCM 有符号 16 位音频。 |
| `.flac` | FLAC 音频。 |
| `.m4a` | AAC 音频。 |

视频输出为不带 alpha 的 8 位 YUV 4:2:0；音频统一为双声道 48 kHz。元数据、章节、字幕、附件及额外的音视频轨道会被丢弃。本版本不执行 HDR 色调映射，也不保留颜色元数据。源尺寸为奇数时向下取整为偶数；缩放使用明确指定的尺寸，可能改变宽高比。不需要 GPU。

按时间裁剪以秒定位并重新编码，因此可精确到帧，不受关键帧位置限制；端点会量化到解码后的帧或音频采样边界。抽帧选择请求时间点或其后的第一张解码帧。拼接会重置每段的时间戳，缩放至第一段输入的偶数尺寸，统一像素宽高比和帧率，并对音频重采样。启用音频时，拼接滤镜可能填充每段中较短的流；受帧和音频取整影响，总时长可能与各段时长之和略有不同。任意片段缺少音频时请使用 `audio:false`。输入时长之和不得超过 86400 秒。替换音频从时间 0 开始，较短时补静音，较长时裁切至视频时长。已有视频会重新编码，以确保容器与编解码器组合确定。不保证毫秒级的精确时长。

## 任务、Web 请求与资源边界

定期调用 `status()`，例如每隔 50–250 ms 调用一次。每次轮询推进一个阶段：逐个探测并校验输入，然后编码。停止轮询时，当前原生进程仍会完成或超时；下一阶段等待再次调用状态查询。没有阻塞式 `wait()` API。Web 处理器应创建任务、返回 `job.id()`，再在后续短请求中通过 `source.job(id)` 恢复任务。首先重新打开相同的源路径；使用不同的源或已关闭的源会被拒绝。路径身份比较忽略冗余的 `.` 和分隔符，但保留大小写。恢复操作不会复制任务，也不会启动另一个进程。关闭源对象不会影响已有任务；关闭任何一个任务别名都会使所有别名失效。重新打开源时设置的超时不会改变原任务的截止时间。例如，在后续请求中：

```bt
source = video('@/source.mp4', {})
job = source.job(id)
source.close()
snapshot = job.status()
// 输出：当前任务状态；任务到达终态后应关闭任务。
print snapshot.state
```

不要在 Web 请求中执行睡眠/轮询循环。ID **不是身份验证令牌**；应用接受客户端传入的 ID 前，必须检查任务归属权限。ID 在关闭、服务关闭或共享 worker 被淘汰后失效。

每个 worker 最多保留 64 个源描述符和 32 个任务。应关闭已完成任务以回收名额。进程宿主允许每个 worker 最多 4 个活动进程、32 个句柄，整个宿主最多 32 个活动进程；超载会直接拒绝，不进入无界队列。共享调用队列上限为 32，单次调用超时为 2 秒，空闲 TTL 为 5 分钟。所有操作的最长截止时间均为 5 分钟。宿主仅保留每个进程 stdout 和 stderr 各自最新的 1 MiB 内容；被截断的探测 JSON 会被拒绝。媒体载荷不会进入 WASM。

输入上限为 32 GiB、24 小时、16 条流、每个轴 8192 像素、每帧 16,777,216 像素、240 fps，以及 8 声道/192 kHz 音频。探测采用 5 MB / 5 秒的分析限制。FFmpeg 对每个输入使用一条解码线程，使用两条编码线程、一条普通滤镜线程、一条复杂滤镜线程，单次内存分配最多 64 MiB，复用队列最多 128 个数据包。这些设置约束处理规模，但**不是 FFmpeg 总 RSS 的硬配额**；解码器内部及多个输入流可能保留多帧。输出上限为 2 GiB；达到上限时任务失败并删除文件。

取消/关闭请求会迅速返回；原生 worker 随后终止并等待精确的子进程退出，关闭管道并删除未完成的自有文件。因此，清理结果可能在取消后稍晚才能观察到。已完成文件在关闭后保留。扩展或服务关闭时同样会取消其拥有的原生任务。宿主执行进程级文件策略和声明路径检查。原生执行仍发生在 WASI 沙箱之外；安装扩展即表示像信任其他本地程序依赖一样信任其代码。

## 示例（CLI）

```bt
clip = video('@/source.mp4', {timeout_ms: 60000})
job = clip.resize('@/small.mp4', 640, 360, {quality: 5})
snapshot = job.status()
while snapshot.state == 'probing' || snapshot.state == 'running' {
    sleep(50)
    snapshot = job.status()
}
assert(snapshot.state == 'succeeded', snapshot.error)
// 输出：已完成文件的元数据。
print json(snapshot.result)
job.close()
clip.close()
```

调用 `clip.frame('@/frame.png', 0.5)`，轮询至成功后，再通过独立安装的 image 扩展打开 `@/frame.png` 进行后续编辑。

## 构建与验证

在扩展目录中运行 `build.ps1 -BtExe <path-to-current-bt.exe>`，使用锁定的 WASI 依赖构建，仅将运行文件和许可证暂存到 `target/`，创建 `video-1.0.0.bts` 后执行检查。该 `.bts` 独立版本化。`verify.ps1 -BtExe <path> -Package <package>` 在 `target/` 下创建独立项目，安装扩展包，使用生成的测试媒体运行 `smoke.bt`，再独立解码和探测其输出。脚本要求 PATH 中存在 ffmpeg 和 ffprobe，不会提交、发布、下载编解码器或上传文件。

```powershell
cargo test --locked --manifest-path extension/video/Cargo.toml -- --test-threads=1
cargo fmt --manifest-path extension/video/Cargo.toml -- --check
cargo build --locked --manifest-path extension/video/Cargo.toml --target wasm32-wasip1 --release
```

原生测试覆盖全部处理方法、容器/编码输出、尺寸、裁剪/拼接/替换时长、PNG/JPEG 文件签名、完整输出解码、非法路径/选项/元数据、已有输出保护、编码中取消、超时、同源 ID 恢复、不匹配的源、无效或不存在的 ID、已关闭的源、共享任务别名，以及重复释放和复用。测试实际执行原生宿主协议，不使用模拟实现。安装到真实 BT 的扩展包检查记录位于工作区 `docs/` 验收文档中。真实 BT 安装验收要求出现明确的 `VIDEO_SMOKE_PASS` 标记，并检查五个错误场景；仅凭 CLI 退出码为零不能认定脚本执行成功。

## 许可证与后端来源

源码：Copyright 2026 Lifeng Yan，采用 MIT OR Apache-2.0。扩展包随附 `LICENSE-MIT`、`LICENSE-APACHE`、`COPYRIGHT` 和 `THIRD_PARTY_LICENSES.txt`。Rust JSON 依赖固定为 serde_json 1.0.151（MIT OR Apache-2.0，最低 Rust 1.71），所有传递依赖的版本和校验和均锁定在 Cargo.lock 中。参见 [serde_json 官方 manifest](https://github.com/serde-rs/json/blob/master/Cargo.toml)。

FFmpeg 是单独安装的程序，通过参数数组调用，不使用 shell。[FFmpeg 官方法律说明](https://ffmpeg.org/legal.html) 说明了不同构建配置对应的 LGPL/GPL 区别。本次测试的 Gyan 8.1.1 Windows 构建启用了 GPL 和 version3；本项目不重新分发该构建。任何分发后端的人员都必须独立满足所分发具体构建的声明和源码提供义务。支持的参数遵循官方 [FFmpeg CLI](https://ffmpeg.org/ffmpeg.html) 和 [FFprobe CLI](https://ffmpeg.org/ffprobe.html) 文档。
