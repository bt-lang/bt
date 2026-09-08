# BT image extension

Version **1.0.0** is an independently built and installed `kind=wasm` extension.
It decodes, transforms and encodes still images in a bounded shared WASI worker.
The extension does not require the video extension or an external graphics program.

## Formats and color rules

| Format | Read | Write | Alpha / compression |
|---|---|---|---|
| PNG | Yes, including 16-bit input converted to 8-bit | Yes, RGBA8 | Lossless; `compression` is `fast`, `default` or `best`. |
| JPEG (`jpeg` / `jpg`) | Yes | Yes, RGB8 | Lossy quality 1..100; alpha is composited on `background`. |
| WebP | Yes, lossy and lossless | Yes, lossless only | RGBA8 is preserved; lossy quality is rejected. |
| BMP | Yes | Yes, RGBA8 | Uncompressed output with alpha; downstream viewers must support BMP alpha. |

Decoding detects file contents, not the filename extension. Output format is an
explicit lowercase argument. Unsupported codecs, options, malformed files and
invalid operations return English errors. Animated inputs are treated as a still
image by the underlying decoder; frame timing and animation output are not exposed.
For video frame extraction use `video`, then pass the resulting image file path to
`image`. Encoded BT `Bytes` are another explicit collaboration boundary; no
cross-extension image-object or zero-copy interoperability is assumed.

All retained pixels use **8-bit straight-alpha RGBA**, interpreted as sRGB.
ICC profiles, EXIF orientation, metadata and high-bit-depth precision are not
preserved or applied. Rotate explicitly if orientation correction is required.
Filtered resizing temporarily premultiplies alpha to avoid transparent color
fringes; interpolation and compositing use encoded sRGB, not linear light. Alpha
is unchanged by color adjustment. Watermarks use Porter-Duff source-over.

## API

The only global entry is `image(path)`. It creates a lightweight object bound to a
project path without reading image contents or creating the file. The host still
normalizes the `path_write` argument and checks permissions, project boundaries
and the existing parent directory during binding. Pixel operations (`info`,
`resize`, `crop`, `rotate`, watermarks, `text`, `adjust`, `save`, `encode` and
`pixel`) load that path on their first use; subsequent calls reuse the pixels.
A missing or malformed input fails when a pixel operation needs to load it, and
a failed load can be retried. `create` and `decode` instead establish or replace
pixels directly, even if the bound file is missing or malformed. Both preserve
the same object and bound path; invalid input or an exceeded pixel budget leaves
the previous pixels unchanged. Neither operation writes a file. Only explicit
`save(path, format, options)` writes, and saving does not change the bound path.
`close()` also releases an object that has never loaded pixels.

All parameters shown below are required; pass `{}` for default options. All
coordinates are integer pixels relative to the top left. Mutating methods and
`save` return **the same Image handle** for chaining. `encode`, `info`, `pixel`
and `close` return their explicitly documented result instead.

| Call | Parameters and behavior | Result |
|---|---|---|
| `image(path)` | Bind a project path, normalized by the host with `path_write`; 1..4096 UTF-8 bytes, parent directory must exist. The image file may be absent. No image content is read or written; host path and permission checks still apply. | Image |
| `img.decode(data)` | Replace pixels from encoded BT Bytes, up to 16,777,152 bytes. Does not read the bound file. | same Image |
| `img.create(width, height, color)` | Replace pixels with a solid canvas; positive dimensions; `color` is `[r,g,b,a]`, four integers 0..255. Does not read the bound file. | same Image |
| `img.info()` | Inspect dimensions and representation; fields below. | object |
| `img.resize(width, height, filter)` | Exact positive output dimensions. Filter: `nearest`, `triangle`, `catmull_rom`, `gaussian`, `lanczos3`. Aspect ratio is not inferred. | same Image |
| `img.crop(x, y, width, height)` | Nonnegative origin and positive size; rectangle must be completely inside the image. | same Image |
| `img.rotate(degrees)` | Clockwise integer `90`, `180` or `270`; no interpolation or arbitrary-angle rotation. | same Image |
| `img.watermark(path, x, y, opacity)` | Read a second image using `path_read`; coordinates -16384..16384; opacity numeric 0..1. Edges clip to the target. | same Image |
| `img.watermark_bytes(data, x, y, opacity)` | Same composition with encoded BT Bytes; no retained watermark handle. | same Image |
| `img.text(text, x, y, scale, color)` | Built-in 8x8 font, printable ASCII U+0020..U+007E plus newline, at most 1024 bytes. Coordinates -16384..16384; integer scale 1..16; RGBA color. Unsupported characters fail before drawing. Newline advances 8 × scale pixels. | same Image |
| `img.adjust(options)` | Color changes described below, performed in one pixel pass. | same Image |
| `img.save(path, format, options)` | Host `path_write`; stream to a same-directory temporary file, close it, then atomically replace the destination. Parent directory must exist. | same Image |
| `img.encode(format, options)` | Encoded BT Bytes up to 16,777,152 bytes; use `save` for larger outputs. | Bytes |
| `img.pixel(x, y)` | Read one straight RGBA pixel. Out-of-bounds coordinates return `empty`. | array or empty |
| `img.close()` | Immediately free retained pixels and invalidate the handle. Further calls, including a second close, fail. | true |

### Image.info result

| Field | Type | Required / always present | Default | Range / values | Meaning |
|---|---|---|---|---|---|
| `width` | int | Yes | None | 1..16384 | Current pixel width. |
| `height` | int | Yes | None | 1..16384 | Current pixel height. |
| `channels` | int | Yes | 4 | 4 | RGBA channel count. |
| `pixel_bytes` | int | Yes | None | 4..67108864 | Retained RGBA allocation size, width × height × 4. |
| `color_space` | string | Yes | `srgb` | `srgb` | Interpretation of encoded RGB channel values. |
| `alpha` | string | Yes | `straight` | `straight` | Alpha association in retained pixels. |

### Image.adjust options

All fields are optional, unknown and duplicate fields are errors. Adjustment
order is contrast → brightness → clamp → gamma → saturation/grayscale → invert
→ final round/clamp. Gamma uses `255 * (channel / 255) ** (1 / gamma)`. Saturation
interpolates from luma using coefficients 0.2126, 0.7152, 0.0722. Grayscale takes
precedence over saturation. Alpha is preserved, including fully transparent RGB.

| Field | Type | Required | Default | Range / values | Meaning |
|---|---|---|---|---|---|
| `brightness` | number | No | 0 | -255..255 | Additive channel offset after contrast. |
| `contrast` | number | No | 1 | 0..4 | Contrast multiplier around channel value 127.5. |
| `saturation` | number | No | 1 | 0..4 | 0 produces grayscale; 1 preserves saturation. |
| `gamma` | number | No | 1 | 0.1..10 | Power adjustment; values above 1 brighten midtones. |
| `grayscale` | bool | No | false | true / false | Replace RGB with luma. |
| `invert` | bool | No | false | true / false | Replace adjusted RGB with 255 minus channel. |

### Image.save / Image.encode options

Unknown fields and fields inapplicable to the selected codec are rejected.
Compression does not guarantee a smaller file than an existing source, and
JPEG quality 100 is still lossy. Saving never changes the retained RGBA pixels.

| Field | Type | Required | Default | Range / values | Meaning |
|---|---|---|---|---|---|
| `quality` | int | No | 85 | 1..100; JPEG only | JPEG encoding quality. |
| `compression` | string | No | `default` | `fast`, `default`, `best`; PNG only | Lossless PNG encoding effort. |
| `background` | array | No | `[255,255,255,255]` | Four integers 0..255; alpha must be 255; JPEG only | Opaque matte for removing alpha. |

## Example

```bt
img = image('preview.png').create(640, 360, [20, 40, 80, 255])
img.text('BT image', 20, 20, 3, [255, 255, 255, 220])
   .adjust({brightness: 8, saturation: 1.1})
   .resize(320, 180, 'triangle')
   .save('preview.png', 'png', {compression: 'best'})

encoded = img.encode('jpeg', {quality: 85})
copy = image('copy.jpg').decode(encoded)
// Output: 320
print copy.info().width
copy.close()
img.close()

source = image('preview.png')
source.crop(0, 0, 100, 80).rotate(90)
      .watermark('preview.png', -20, -20, 0.25)
      .save('converted.webp', 'webp', {})
source.close()
```

File access is restricted by the host's project preopen and BT permission checks.
`path_read` and `path_write` are explicitly declared in the bindings. No filesystem
path is hidden inside an options object. A failed lookup such as `pixel(-1,0)`
returns `empty`; processing failures are errors, not ambiguous `null` values.

## Resources and execution

- One image: maximum dimension 16384 on either axis and maximum 16,777,216 pixels.
- One worker: at most 32 handles (including unloaded objects), 128 KiB of path
  contents and 33,554,432 retained pixels (128 MiB RGBA8).
- Two isolated workers: at most 64 handles, queue 16, at most 4 in-flight calls,
  per-call timeout 30 seconds, idle lifetime 300 seconds. Handles pin their worker.
- Encoded file input: regular files up to 64 MiB, streamed with a seekable byte
  cap. Decoder allocation budget: 128 MiB. Header pixel limits are checked before
  decoding. Encoded file output: at most 128 MiB, enforced by the writer.
- Transform scratch buffers, codec allocations and ABI buffers are additional to
  retained pixels. Resize may hold source, premultiplied source and destination;
  JPEG uses an additional RGB matte buffer. Atomic create/decode replacement may temporarily hold both old and new pixels.
  No full video is loaded, no encoded
  cache is retained, and no Base64 transport is used.
- Explicit `close` releases the allocation for reuse. WASM linear memory may
  retain its high-water capacity until worker destruction; `pixel_bytes` measures
  live pixels, not process RSS. Close handles promptly in long-lived applications.
- Image calls are synchronous to the BT caller and run in the host's bounded
  extension workers. Use the host's background-task mechanism for request flows
  that should not wait for processing; the package creates no private threads.
  A host timeout interrupts and invalidates that worker, freeing its objects.
- Argument/option validation precedes pixel mutation. Failed ordinary operations
  retain the original image; failed atomic saves remove the staging file. An
  externally interrupted/crashed process can leave a bounded-size temporary file
  named `.<destination>.bt-image-<slot>.tmp` beside the intended output. Four fixed
  slots (0..3) bound abandoned staging files per destination. A full staging pool
  returns an error; remove stale files only after confirming no writer owns them.

## Build, package and validation

Requirements: Rust 1.88 or newer, the `wasm32-wasip1` target, and a BT binary with
WASM extensions enabled. This development package requires this checkout's
shared-handle identity and empty-array-result fixes; the `bt_min_version` value
1.1.4 alone does not imply compatibility with an older published 1.1.4 binary.
No WASI C compiler is required. Dependencies are pinned
in the independent Cargo.lock and never add codecs to the BT executable.

From the BT repository root:

```powershell
rustup target add wasm32-wasip1
extension/image/build.ps1 -BtPath target/debug/bt.exe
extension/image/verify.ps1 -BtPath target/debug/bt.exe
target/debug/bt.exe ext install extension/image/target/image-1.0.0.bts path/to/project
cargo test --locked --manifest-path extension/image/Cargo.toml
cargo test --locked --manifest-path extension/image/Cargo.toml --release -- --ignored --nocapture
```

The build script checks formatting, runs native tests, builds release WASI, stages
only runtime metadata/notices/README and module.wasm under `target/package`, then
builds and checks `target/image-1.0.0.bts`. All generated binaries are ignored.
Unit tests use synthetic pixels, round-trip all four formats, inspect alpha,
geometry, text and color changes, and cover malformed input, unsupported options,
limits, repeated release, out-of-bounds `empty`, and atomic-save cleanup. Lazy
object tests cover absent/malformed paths, retry after failed loading, reuse after
source removal, atomic create/decode replacement, pixel accounting and bounded
unloaded objects. The installed BT smoke also verifies 256 replacements on one
object, 1,000 chained calls and fourteen negative cases.

The explicit release benchmark performs ten iterations of 1920×1080 PNG decode,
triangle resize to 960×540, color adjustment/text and JPEG quality-85 encoding.
It prints timing and live resource counters; it does not make universal speed or
RSS claims. See the workspace acceptance record for measured results and the
installed-package BT smoke evidence. WASI output is platform-portable in a
compatible BT host; only platforms actually listed in that record are verified.

## Source and licenses

| Path | Purpose |
|---|---|
| `src/lib.rs` | Public dispatch, pixel ownership, codec and image operations. |
| `src/tests.rs` | Deterministic functionality, resource and performance checks. |
| `bindings.json` / `manifest.json` | API contract, permissions and bounded runtime configuration. |
| `build.ps1` | Reproducible standalone WASI build and checked package. |
| `smoke.bt` | Full public BT API acceptance using locally generated fixtures. |
| `verify.ps1` | Isolated package installation, complete smoke and negative BT cases and lazy object lifecycle checks. |
| `Cargo.toml` / `Cargo.lock` | Exact direct versions and locked transitive dependency graph. |

Source is Copyright 2026 Lifeng Yan, **MIT OR Apache-2.0**. The package includes
`LICENSE-MIT`, `LICENSE-APACHE`, `COPYRIGHT` and `THIRD_PARTY_LICENSES.txt`.
The selected image-rs codecs are pure Rust; `image` 0.25.10 is MIT OR Apache-2.0
and requires Rust 1.88. `font8x8` 0.3.1 is MIT and embeds public-domain IBM-derived
bitmap glyph data. No external fonts or image assets are bundled.

Dependency sources checked before selection:
[image manifest](https://docs.rs/crate/image/0.25.10/source/Cargo.toml),
[font8x8 source/license](https://docs.rs/crate/font8x8/0.3.1/source/LICENSE),
[pure-Rust WebP codec](https://github.com/image-rs/image-webp).
The repository's compliance tool generates the complete WASI dependency notices.
