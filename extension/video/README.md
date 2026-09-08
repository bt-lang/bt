# BT video extension 1.0.0

Independent `kind=wasm` official extension for asynchronous local-file video work.
Install only when needed. It does not depend on the image or SQLite extensions.
FFmpeg and ffprobe must be installed on the host's PATH; no FFmpeg executable or
codec library is included or downloaded by this package. The tested backend is
FFmpeg 8.1.1 on Windows x64. The WASI module builds independently. Linux and macOS
are intended host targets, but their end-to-end behavior has not been verified.
This extension requires a BT build with the optional `bts_host.process_request`
import added alongside this extension; previously released BT 1.1.4 binaries lack
that import despite satisfying the manifest's minimum version number.

## API

All names use snake_case. Every processing method returns a `VideoJob` immediately;
decoding and encoding execute in bounded native background processes. `info()` is
also asynchronous. Pass `{}` for default option objects.
`video` is the sole global entry point; use names such as `clip` or `source` for
the returned object so that a local variable does not shadow the entry function.

| Call | Parameters and behavior | Return |
|---|---|---|
| `video(path, options)` | Existing media filename; open options below. Only validates filesystem metadata. | `Video` |
| `clip.job(id)` | Positive integer from `job.id()`; requires a live source for the same normalized project-relative path, project and live worker. | Existing `VideoJob` alias |
| `clip.info()` | Probe duration, container and stream metadata. | `VideoJob` |
| `clip.transcode(output, options)` | Encode MP4 or WebM; encoding options below. | `VideoJob` |
| `clip.trim(output, start, duration, options)` | Start in seconds, 0..86400; duration in seconds, 0.001..86400; range must fit the source. | `VideoJob` |
| `clip.concat(paths, output, options)` | Array of 1..7 additional project-relative filenames; this source is first. | `VideoJob` |
| `clip.frame(output, time)` | One PNG/JPEG frame; seconds, 0 inclusive to source duration exclusive. | `VideoJob` |
| `clip.resize(output, width, height, options)` | Even integer dimensions, 2..8192 each, at most 16,777,216 pixels. | `VideoJob` |
| `clip.extract_audio(output)` | First audio stream to WAV, FLAC or M4A. | `VideoJob` |
| `clip.replace_audio(audio, output, options)` | Replace audio from an existing media file; retain video duration. | `VideoJob` |
| `clip.close()` | Release the source descriptor; existing jobs retain their own source snapshot. | `true` |
| `job.status()` | Poll once and advance preflight/encoding stages. Never waits for media processing. | Status object |
| `job.cancel()` | Cancel unfinished work and return its snapshot; completed output survives. | Status object |
| `job.id()` | Identifier for restoring the job in subsequent requests. | Positive integer |
| `job.close()` | Cancel unfinished work and release the handle; all aliases become invalid and restoring a closed ID fails. | `true` |

Top-level path parameters use BT `path_read`/`path_write` roles, so `@/` refers to
the project root and source-relative paths follow ordinary BT resolution. Items
inside `concat(paths, ...)` are **project-relative**, without `@/`. All paths must
remain inside the project. URL syntax, parent traversal, percent signs, symbolic
links and control characters are rejected. Output parent directories must exist.
Output files must not exist: jobs reserve them with `create_new` and never replace
caller-owned files. Files are exchanged with `image` using their paths; no media
bytes, Base64 strings or cross-extension object handles are transferred.

### Open options

| Field | Type | Required | Default | Valid range | Meaning |
|---|---|---|---|---|---|
| `timeout_ms` | int | No | 60000 | 1..300000 | One deadline for the whole job, including all probes, encoding and time between polling calls. |

### Encoding options

| Field | Type | Required | Default | Valid range | Meaning |
|---|---|---|---|---|---|
| `quality` | int | No | 5 | 2..31 | MPEG-4 `q:v`; VP9 CRF is twice this number (4..62). Lower means higher quality; not a percentage or size guarantee. |
| `fps` | number | No | None | 1..120, finite | Force constant output frame rate. Omitted: preserve source timing except concat, which uses the first source's average frame rate. |
| `audio` | bool | No | true | true/false | Retain first audio stream; concat requires audio in every input when true. False produces video-only output. `replace_audio` always includes the replacement audio. |

Unknown option fields and unsupported combinations fail with English errors.

### Status object

| Field | Type | Required | Default | Values/range | Meaning |
|---|---|---|---|---|---|
| `state` | string | Yes | `probing` | `probing`, `running`, `succeeded`, `failed`, `cancelled`, `timed_out` | Job lifecycle. |
| `elapsed_ms` | int | Yes | 0 | Nonnegative | Wall time since job creation. |
| `processed_seconds` | number | Yes | 0 | Nonnegative | Latest encoded output timeline position from FFmpeg progress; remains zero during probing. Not a percentage. |
| `error` | string | Yes | Empty string | Bounded by host stderr (1 MiB tail) | English failure details; empty on success. |
| `result` | object or empty | Yes | `empty` | Metadata or output object | Present only after successful completion. Missing stream fields remain absent; external JSON null remains BT `null`. |

An output result contains:

| Field | Type | Required | Default | Range | Meaning |
|---|---|---|---|---|---|
| `path` | string | Yes | None | Project-relative filename | Completed output file. |
| `size_bytes` | int | Yes | None | 1 to less than 2 GiB | Encoded file size. |

An `info()` result contains:

| Field | Type | Required | Default | Range | Meaning |
|---|---|---|---|---|---|
| `duration` | number | Yes | None | 0.001..86400 seconds | Container duration. |
| `format` | string | Yes | None | FFprobe format identifier | Container name; may contain aliases separated by commas. |
| `size_bytes` | int | Yes | 0 if unreported | 0..32 GiB | Input size reported by ffprobe. |
| `streams` | array of objects | Yes | None | 1..16 streams | Stream descriptors below. |

Stream objects expose only these ffprobe fields; unavailable fields are absent.
Rational/time strings retain backend precision without lossy normalization.

| Field | Type | Required | Default | Range | Meaning |
|---|---|---|---|---|---|
| `index` | int | Backend dependent | None | Nonnegative | Container stream index. |
| `codec_type` | string | Backend dependent | None | `video`, `audio`, other FFprobe types | Media kind. |
| `codec_name` | string | Backend dependent | None | Backend codec identifier | Decoder codec. |
| `width`, `height` | int | Video only | None | 1..8192; product <=16,777,216 | Encoded frame dimensions before display rotation. |
| `avg_frame_rate` | string | Video only | None | Rational; >0 and <=240 fps | Average source frame rate. |
| `sample_rate` | string | Audio only | None | Integer text 8000..192000 | Audio sample rate in Hz. |
| `channels` | int | Audio only | None | 1..8 | Source channel count. |
| `duration` | string | Backend dependent | None | Seconds text | Per-stream duration. |
| `start_time` | string | Backend dependent | None | Seconds text | Per-stream starting timestamp. |

## Formats and timeline rules

Inputs are local MP4/MOV/MKV/WebM/AVI video or WAV/MP3/M4A/FLAC/OGG audio. An explicit
demuxer is selected from the filename; disguised playlists, network protocols and
device inputs are not supported. Actual decoder availability depends on the host
FFmpeg installation. Unsupported codecs fail through `job.status().error`.

| Output suffix | Encoding |
|---|---|
| `.mp4` | Native MPEG-4 Part 2 video + AAC audio, faststart; not H.264. |
| `.webm` | `libvpx-vp9` video + `libopus` audio; these encoders must be present. |
| `.png` | One lossless RGB frame. |
| `.jpg`, `.jpeg` | One lossy JPEG frame using FFmpeg's encoder defaults. |
| `.wav` | PCM signed 16-bit audio. |
| `.flac` | FLAC audio. |
| `.m4a` | AAC audio. |

Video output is 8-bit YUV 4:2:0 without alpha; audio is normalized to stereo 48 kHz.
Metadata, chapters, subtitles, attachments and additional audio/video tracks are
discarded. This version does not perform HDR tone mapping or preserve color
metadata. Odd source dimensions are rounded down to even dimensions; resize uses
the exact requested dimensions, and can alter aspect ratio. No GPU is required.

Trim seeks by seconds and re-encodes, so it is frame-accurate rather than limited
to keyframes; endpoints are quantized to decoded frame/audio sample boundaries.
Frame extraction selects the first decoded frame at/after the requested time.
Concat resets each segment's timestamps, scales to the first input's even geometry,
normalizes pixel aspect ratio/frame rate and resamples audio. With audio enabled,
the shorter stream in a segment can be padded by the concat filter; total duration
can differ from the sum by frame/audio rounding. Use `audio:false` when any segment
lacks audio. The sum of input durations must not exceed 86400 seconds.
Replacement audio begins at time zero, pads silence when short and is cut to the
video's duration when long. Existing video is re-encoded so container/codec
combinations remain deterministic. Millisecond-exact duration is not promised.

## Jobs, Web requests and resource bounds

Call `status()` periodically (for example every 50–250 ms). Polling advances one
stage at a time: each input is probed, validated, then encoded. If polling stops,
the active native process still finishes or times out; the next stage waits for
another status call. There is no blocking `wait()` API. For Web handlers, create
a job, return `job.id()`, and restore it with `source.job(id)` in later short requests.
Reopen the same source path first; a different source or a closed source is rejected.
Path identity ignores redundant `.` and separators, but preserves case. Restoration
does not copy the job or start another process. Closing the source leaves the job
alive; closing any job alias invalidates every alias. The reopened source's timeout
does not change the original job's deadline. For example, in a later request:

```bt
source = video('@/source.mp4', {})
job = source.job(id)
source.close()
snapshot = job.status()
// Output: the current job state; close the job after it reaches a terminal state.
print snapshot.state
```

Never run a sleep/poll loop inside a Web request. IDs are **not authentication
tokens**; applications must authorize ownership before accepting an ID from clients.
IDs expire when closed, on service shutdown or after the shared worker is evicted.

The worker retains at most 64 sources and 32 jobs. Close completed jobs to reclaim
their slots. The process host permits 4 active processes per worker, 32 handles per
worker, and 32 active processes across the host; overload is rejected rather than
queued without a bound. The shared call queue is limited to 32, with a 2-second
call timeout and 5-minute idle TTL. All operations have a maximum 5-minute deadline.
The host retains only the latest 1 MiB of stdout and stderr for each process;
truncated probe JSON is rejected. Media payloads never enter WASM.

Input files are limited to 32 GiB, 24 hours, 16 streams, 8192 pixels per axis,
16,777,216 pixels per frame, 240 fps, and 8 audio channels/192 kHz. Probing uses
5 MB / 5 seconds analysis limits. FFmpeg uses one decoding thread per input,
two encoding threads, one filter thread, one complex-filter thread, a 64 MiB
maximum individual allocation and a 128-packet muxing queue. These constrain
work but are **not a hard total RSS quota** for FFmpeg; decoder internals and
multiple input streams can retain several frames. Output is capped at 2 GiB;
reaching that cap is a failed job and its file is removed.

Cancel/close requests return promptly; the native worker kills and waits for the
exact child process, closes pipes and removes incomplete owned files afterward.
Cleanup can therefore be observed shortly after cancellation. Completed files
remain after close. Extension/service shutdown also cancels owned native work.
Filesystem permission checks and declared paths are enforced by the host. Granting
`process` nevertheless permits native execution outside the WASI sandbox: it is
a trusted-extension capability, not an OS sandbox for arbitrary downloaded code.

## Example (CLI)

```bt
clip = video('@/source.mp4', {timeout_ms: 60000})
job = clip.resize('@/small.mp4', 640, 360, {quality: 5})
snapshot = job.status()
while snapshot.state == 'probing' || snapshot.state == 'running' {
    sleep(50)
    snapshot = job.status()
}
assert(snapshot.state == 'succeeded', snapshot.error)
// Output: the completed file's metadata.
print json(snapshot.result)
job.close()
clip.close()
```

Use `clip.frame('@/frame.png', 0.5)`, poll to success, then open `@/frame.png`
with the independently installed image extension for further editing.

## Build and verification

From this directory, `build.ps1 -BtExe <path-to-current-bt.exe>` builds with the
locked WASI dependencies, stages only runtime files and licenses under `target/`,
creates `video-1.0.0.bts`, then checks it. The `.bts` is independently versioned.
`verify.ps1 -BtExe <path> -Package <package>` creates an independent project under
`target/`, installs the package and runs `smoke.bt` against generated test media,
then independently decodes and probes its outputs. The scripts require ffmpeg
and ffprobe on PATH. They do not commit, publish, download codecs or upload files.

```powershell
cargo test --locked --manifest-path extension/video/Cargo.toml -- --test-threads=1
cargo fmt --manifest-path extension/video/Cargo.toml -- --check
cargo build --locked --manifest-path extension/video/Cargo.toml --target wasm32-wasip1 --release
```

Native tests cover all processing methods, container/codec outputs, geometry,
trim/concat/replacement durations, PNG/JPEG signatures, full output decode,
invalid paths/options/metadata, existing-output protection, cancel while encoding,
timeouts, same-source ID restoration, mismatched sources, invalid/missing IDs,
closed sources, shared job aliases and repeated release/reuse. These run
the actual native host protocol, not mocks. Real installed-BT package checks are
recorded in the workspace `docs/` acceptance record. Installed-BT verification
requires the explicit `VIDEO_SMOKE_PASS` marker and checks five error scenarios;
a zero CLI exit code alone does not establish that a script succeeded.

## Licenses and backend provenance

Source: Copyright 2026 Lifeng Yan, MIT OR Apache-2.0. `LICENSE-MIT`,
`LICENSE-APACHE`, `COPYRIGHT` and `THIRD_PARTY_LICENSES.txt` accompany the package.
The Rust JSON dependency is pinned to serde_json 1.0.151 (MIT OR Apache-2.0,
Rust 1.71 minimum) and all transitive versions/checksums are locked in Cargo.lock.
See [serde_json's official manifest](https://github.com/serde-rs/json/blob/master/Cargo.toml).

FFmpeg is a separately installed program, invoked with argument arrays without a
shell. [FFmpeg's official legal page](https://ffmpeg.org/legal.html) documents the
LGPL/GPL distinction based on build configuration. The tested Gyan 8.1.1 Windows
build enables GPL and version3; it is not redistributed here. Anyone distributing
a backend must independently satisfy that exact build's notices and source
obligations. Supported arguments follow the official
[FFmpeg CLI](https://ffmpeg.org/ffmpeg.html) and
[FFprobe CLI](https://ffmpeg.org/ffprobe.html) documentation.
