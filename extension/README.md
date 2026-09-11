# Official BT extensions

This directory contains independently built and versioned official BT extension libraries. Each extension owns its source, tests, package manifest, bindings, locked dependencies, and license notices.

| Extension | Simplified Chinese | Purpose |
|---|---|---|
| [SQLite](sqlite/README.md) | [简体中文](sqlite/README.zh-CN.md) | SQLite file databases with chained queries, parameter binding, and transactions. |
| [Image](image/README.md) | [简体中文](image/README.zh-CN.md) | Bounded image objects, conversion, geometry, watermarks, and color adjustments. |
| [Video](video/README.md) | [简体中文](video/README.zh-CN.md) | Asynchronous FFmpeg jobs for probing, editing, frames, and audio tracks. |

Each extension's `manifest.json`, `bindings.json`, `README.md`, and `README.zh-CN.md` are its only maintained metadata, editor-contract, and documentation sources. Simplified Chinese consistently uses the `zh-CN` manifest locale and `README.zh-CN.md` filename; bindings stay language-independent because API identifiers never change by locale. The website publishes exact immutable copies with the same filenames for one extension version, and later changes on `main` do not alter the website until the developer publishes another version.

The SDK lives in `crates/bt-extension-sdk/`, and the host loader and runners live in `src/extensions/`. A BT application's `extensions/` directory holds installed `.bts` packages; this repository's `extension/` directory holds their source.

Follow each extension's README to build and test it. Generated modules, packages, and build output are not tracked in Git.

Official extensions use their package name as the primary constructor: `image(path)`, `video(path, options)`, and `sqlite(path, options)`. Operations belong to the returned objects: for example `image(path).create(width, height, color)` and `clip.job(id)`. The previously published `sqlite_open` entry remains a deprecated compatibility alias.

Image and video are independently versioned optional packages. Exchange a project file path between them: video extracts a frame, then image opens that frame for pixel processing. Small encoded images may also use Bytes within the 16 MiB ABI limit. Extension object handles cannot be transferred between these libraries. Video requires a BT build with the optional SDK process import and separately installed FFmpeg/ffprobe executables; see its README for the tested configuration.
