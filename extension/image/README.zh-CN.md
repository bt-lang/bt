# Image 图片扩展

## 功能

**1.0.0** 是独立构建、按需安装的 `kind=wasm` 扩展，在有界 shared WASI worker 内完成静态图片解码、变换和编码，不要求安装 video 扩展或外部图形程序。

注册表中的扩展包使用 1.1.4 之后的当前扩展 manifest 格式。公开发布的 BT 1.1.4 二进制会拒绝该格式；请使用更新后的 `main` 构建或下一个正式版本安装。

## 格式与颜色规则

| 格式 | 读取 | 写入 | 透明度 / 压缩 |
|---|---|---|---|
| PNG | 支持，16 位输入转换为 8 位 | RGBA8 | 无损；`compression` 为 `fast`、`default` 或 `best`。 |
| JPEG（`jpeg` / `jpg`） | 支持 | RGB8 | 有损质量 1..100；透明度与 `background` 合成。 |
| WebP | 支持有损与无损输入 | 仅无损 | 保留 RGBA8，拒绝有损质量选项。 |
| BMP | 支持 | RGBA8 | 无压缩输出并保留 alpha；下游查看器需支持 BMP alpha。 |

按文件内容检测输入格式，不依赖扩展名。输出格式必须显式传入小写字符串。不支持的编解码器、选项、损坏文件或非法操作返回英文错误。动画输入由底层解码器按静态图片处理，不公开帧时间或动画输出。视频抽帧交给 `video`，再把图片文件路径传给 `image`。编码后的 BT `Bytes` 也是明确的协作边界，不假设跨扩展图片对象或零拷贝互通。

保留像素统一为 **8 位非预乘 alpha RGBA**，RGB 按 sRGB 解释。不保留或应用 ICC、EXIF 方向、元数据和高位深精度；需要修正方向时显式旋转。带滤镜的缩放临时预乘 alpha，避免透明边缘色晕；插值和合成使用编码 sRGB，而非线性光。颜色调整不改变 alpha，水印采用 Porter-Duff source-over 合成。

## 语法、参数与返回值

唯一全局入口是 `image(path)`。它创建绑定项目路径的轻量对象，不读取图片内容，也不创建文件。绑定时宿主仍会规范化 `path_write` 参数，并检查进程级策略、项目边界和已存在的父目录。像素操作（`info`、`resize`、`crop`、`rotate`、水印、`text`、`adjust`、`save`、`encode` 和 `pixel`）第一次使用时加载绑定路径，之后复用已加载的像素。缺失或损坏的图片在需要加载的像素操作处报错；加载失败可以重试。`create` 和 `decode` 则直接建立或替换像素，即使绑定文件不存在或已损坏也可使用。两者都保留同一对象和绑定路径；输入非法或超出像素预算时，原有像素保持不变。两者都不写文件，只有显式 `save(path, format, options)` 写入；保存不改变绑定路径。`close()` 也可以释放从未加载过像素的对象。

下面列出的参数均必传；默认选项传 `{}`。坐标为相对左上角的整数像素。修改方法与 `save` 返回**同一个 Image 句柄**，可链式调用；`encode`、`info`、`pixel` 和 `close` 返回各自文档规定的结果。

| 调用 | 参数和行为 | 返回值 |
|---|---|---|
| `image(path)` | 绑定项目路径，宿主通过 `path_write` 规范化；1..4096 UTF-8 字节，父目录必须存在，图片文件可以不存在。不读取或写入图片内容，仍执行宿主路径和进程级策略检查。 | Image |
| `img.decode(data)` | 从编码后的 BT Bytes 替换像素，最多 16,777,152 字节。不读取绑定文件。 | 同一 Image |
| `img.create(width, height, color)` | 用纯色画布替换像素；正整数尺寸；`color` 为 `[r,g,b,a]`，四个 0..255 整数。不读取绑定文件。 | 同一 Image |
| `img.info()` | 查看尺寸和像素表示，字段见下表。 | object |
| `img.resize(width, height, filter)` | 精确正整数输出尺寸，不自动保持比例。滤镜为 `nearest`、`triangle`、`catmull_rom`、`gaussian`、`lanczos3`。 | 同一 Image |
| `img.crop(x, y, width, height)` | 非负原点和正整数尺寸，矩形必须完全在图片内。 | 同一 Image |
| `img.rotate(degrees)` | 顺时针整数角度 `90`、`180`、`270`，不插值，不支持任意角度。 | 同一 Image |
| `img.watermark(path, x, y, opacity)` | 通过 `path_read` 读取水印；坐标 -16384..16384，数值不透明度 0..1，超出目标边界处裁切。 | 同一 Image |
| `img.watermark_bytes(data, x, y, opacity)` | 相同合成规则，水印来源为编码后的 BT Bytes，不保留水印句柄。 | 同一 Image |
| `img.text(text, x, y, scale, color)` | 内置 8×8 字体，支持可打印 ASCII U+0020..U+007E 和换行，最多 1024 字节。坐标 -16384..16384，整数缩放 1..16，RGBA 颜色。不支持的字符在绘制前报错；换行下移 8×scale 像素。 | 同一 Image |
| `img.adjust(options)` | 在一次像素遍历中完成下表颜色变换。 | 同一 Image |
| `img.save(path, format, options)` | 宿主 `path_write`；流式写同目录临时文件，关闭后原子替换目标。父目录必须存在。 | 同一 Image |
| `img.encode(format, options)` | 编码 BT Bytes，最多 16,777,152 字节；更大输出请用 `save`。 | Bytes |
| `img.pixel(x, y)` | 读取一个非预乘 RGBA 像素，越界返回 `empty`。 | array 或 empty |
| `img.close()` | 立即释放保留像素并使句柄失效，后续调用（包括第二次 close）报错。 | true |

### Image.info 返回字段

| 字段 | 类型 | 必有 | 默认值 | 范围 / 取值 | 含义 |
|---|---|---|---|---|---|
| `width` | int | 是 | 无 | 1..16384 | 当前像素宽度。 |
| `height` | int | 是 | 无 | 1..16384 | 当前像素高度。 |
| `channels` | int | 是 | 4 | 4 | RGBA 通道数。 |
| `pixel_bytes` | int | 是 | 无 | 4..67108864 | 保留 RGBA 分配大小，即宽×高×4。 |
| `color_space` | string | 是 | `srgb` | `srgb` | 编码 RGB 通道值的解释方式。 |
| `alpha` | string | 是 | `straight` | `straight` | 保留像素中的 alpha 关联方式。 |

### Image.adjust 选项

所有字段可选，未知和重复字段报错。调整顺序为：对比度→亮度→限幅→gamma→饱和度/灰度→反色→最终四舍五入/限幅。Gamma 使用 `255 * (channel / 255) ** (1 / gamma)`；饱和度从亮度插值，系数为 0.2126、0.7152、0.0722。灰度优先于饱和度。Alpha 保持不变，完全透明像素的 RGB 也参与颜色处理。

| 字段 | 类型 | 必填 | 默认值 | 范围 / 取值 | 含义 |
|---|---|---|---|---|---|
| `brightness` | number | 否 | 0 | -255..255 | 对比度之后添加的通道偏移。 |
| `contrast` | number | 否 | 1 | 0..4 | 围绕通道值 127.5 的对比度倍率。 |
| `saturation` | number | 否 | 1 | 0..4 | 0 为灰度，1 保持饱和度。 |
| `gamma` | number | 否 | 1 | 0.1..10 | 幂调整，大于 1 提亮中间调。 |
| `grayscale` | bool | 否 | false | true / false | 将 RGB 替换为亮度。 |
| `invert` | bool | 否 | false | true / false | 将调整后的 RGB 替换为 255 减通道值。 |

### Image.save / Image.encode 选项

拒绝未知字段以及不适用于所选格式的字段。压缩不保证输出比原文件更小；JPEG 质量 100 仍是有损。保存不会改变保留的 RGBA 像素。

| 字段 | 类型 | 必填 | 默认值 | 范围 / 取值 | 含义 |
|---|---|---|---|---|---|
| `quality` | int | 否 | 85 | 1..100，仅 JPEG | JPEG 编码质量。 |
| `compression` | string | 否 | `default` | `fast`、`default`、`best`，仅 PNG | 无损 PNG 编码投入。 |
| `background` | array | 否 | `[255,255,255,255]` | 四个 0..255 整数，alpha 必须为 255，仅 JPEG | 去除透明度时使用的不透明底色。 |

## 示例

```bt
img = image('preview.png').create(640, 360, [20, 40, 80, 255])
img.text('BT image', 20, 20, 3, [255, 255, 255, 220])
   .adjust({brightness: 8, saturation: 1.1})
   .resize(320, 180, 'triangle')
   .save('preview.png', 'png', {compression: 'best'})

encoded = img.encode('jpeg', {quality: 85})
copy = image('copy.jpg').decode(encoded)
// 输出：320
print copy.info().width
copy.close()
img.close()

source = image('preview.png')
source.crop(0, 0, 100, 80).rotate(90)
      .watermark('preview.png', -20, -20, 0.25)
      .save('converted.webp', 'webp', {})
source.close()
```

文件访问受宿主项目预打开目录和 BT 进程级策略约束。bindings 显式声明 `path_read` 和 `path_write`，没有隐藏在选项对象内的文件路径。`pixel(-1,0)` 这类未找到结果返回 `empty`；处理失败为错误，不使用含义模糊的 `null`。

## 资源与执行注意事项

- 单图任一轴最多 16384，最多 16,777,216 像素。
- 单 worker 最多 32 句柄（包含未加载对象）、128 KiB 路径内容和 33,554,432 保留像素（128 MiB RGBA8）。
- 两个隔离 worker，总计最多 64 句柄，队列 16，最多 4 个在途调用，单调用超时 30 秒，空闲保留 300 秒；句柄固定到创建它的 worker。
- 编码输入必须为最多 64 MiB 的普通文件，通过带字节上限的可定位流读取。解码分配预算 128 MiB，解码前检查头部像素限额。编码文件输出由 writer 限至 128 MiB。
- 变换临时缓冲、编解码器和 ABI 缓冲不计入保留像素预算。缩放可能同时保留原图、预乘原图及目标图；JPEG 另有 RGB 底色缓冲。create/decode 的原子替换可能暂时同时保留旧像素和新像素。不加载完整视频，不保留编码缓存，不使用 Base64 传输。
- 显式 `close` 释放分配供复用。WASM 线性内存可能保留峰值容量直到 worker 销毁；`pixel_bytes` 是活动像素而非进程 RSS。常驻应用应及时 close。
- 图片调用对 BT 调用方同步，在宿主有界扩展 worker 中执行。不希望等待图片处理的请求流程应使用宿主后台任务机制；扩展本身不创建私有线程。宿主超时中断并使该 worker 失效，释放其对象。
- 参数/选项校验先于像素修改。普通操作失败保留原图；原子保存失败清理暂存文件。外部中断或崩溃可能在输出旁留下有界大小的 `.<destination>.bt-image-<slot>.tmp` 临时文件。每目标固定四槽（0..3），限制遗留暂存文件数；槽满时报错，只能在确认没有写入者使用后清理旧文件。

## 构建、打包与验证

要求 Rust 1.88 或更新版、`wasm32-wasip1` target 和启用 WASM 扩展的 BT 二进制。本开发包要求当前源码的 shared 句柄身份与数组空结果修复；`bt_min_version` 为 1.1.4 不代表旧已发布 1.1.4 二进制兼容。不需要 WASI C 编译器。独立 Cargo.lock 固定依赖，不向 BT 可执行文件加入图像编解码器。

从 BT 源码仓库根目录执行：

```powershell
rustup target add wasm32-wasip1
extension/image/build.ps1 -BtPath target/debug/bt.exe
extension/image/verify.ps1 -BtPath target/debug/bt.exe
target/debug/bt.exe ext install extension/image/target/image-1.0.0.bts path/to/project
cargo test --locked --manifest-path extension/image/Cargo.toml
cargo test --locked --manifest-path extension/image/Cargo.toml --release -- --ignored --nocapture
```

构建脚本检查格式、运行原生测试、构建 release WASI，仅将运行元数据、声明、README 和 module.wasm 放到 `target/package`，再构建并检查 `target/image-1.0.0.bts`。生成的二进制均已忽略。单测使用合成像素，对四种格式做往返验证，检查透明度、几何、文字和颜色变化，覆盖损坏输入、不支持选项、限额、重复释放、越界 `empty` 和原子保存清理。惰性对象测试覆盖缺失/损坏路径、加载失败后重试、源文件删除后复用像素、create/decode 原子替换、像素计数和未加载对象的数量限制。安装后的 BT 烟雾还验证同一对象上的 256 次替换、1,000 次链式调用和 14 个错误场景。

显式 release 基准进行十次 1920×1080 PNG 解码、缩放至 960×540（三角滤镜）、颜色调整/文字与 JPEG 质量 85 编码。输出耗时和活动资源计数，不承诺通用速度或 RSS。实际测量及已安装包的 BT 烟雾证据见工作区验收记录。WASI 产物可在兼容宿主间移植，只有该记录列出的实际平台才算已验证。

## 源码与许可证

| 路径 | 用途 |
|---|---|
| `src/lib.rs` | 公开分发、像素所有权、编解码和图片操作。 |
| `src/tests.rs` | 确定性功能、资源和性能验证。 |
| `bindings.json` / `manifest.json` | API 契约、包身份与有界运行配置。 |
| `build.ps1` | 可重复的独立 WASI 构建和包校验。 |
| `verify.ps1` | 独立安装、完整烟雾、BT 错误场景和惰性对象生命周期检查。 |
| `smoke.bt` | 合成像素的全部公开 API 验收。 |
| `Cargo.toml` / `Cargo.lock` | 固定直接版本和锁定的传递依赖图。 |

源码 Copyright 2026 Lifeng Yan，使用 **MIT OR Apache-2.0**。包包含 `LICENSE-MIT`、`LICENSE-APACHE`、`COPYRIGHT`、`THIRD_PARTY_LICENSES.txt`。所选 image-rs 编解码器均为纯 Rust；`image` 0.25.10 使用 MIT OR Apache-2.0，要求 Rust 1.88；`font8x8` 0.3.1 为 MIT，内嵌源于 IBM 的公有领域位图字形，不捆绑外部字体或图片素材。

选择前核对的依赖来源：[image manifest](https://docs.rs/crate/image/0.25.10/source/Cargo.toml)、[font8x8 源码/许可证](https://docs.rs/crate/font8x8/0.3.1/source/LICENSE)、[纯 Rust WebP 编解码器](https://github.com/image-rs/image-webp)。仓库合规工具生成完整 WASI 依赖声明。
