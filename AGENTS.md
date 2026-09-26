# BT Linux desktop policy

- Linux desktop support in this repository, including `bt`, `bt-app`, their runtime APIs, examples, packaging, and tests, targets native Wayland exclusively. This is an explicit user decision recorded on 2026-09-26.
- Do not implement, retain, or restore X11/XWayland compatibility, fallback, capture, global hotkey grabs, or forced backend launchers. Never clear Wayland environment variables or select `GDK_BACKEND=x11` to bypass compositor restrictions.
- Use native Wayland desktop portals for screen acquisition and global shortcuts. Window positioning support must not determine their backend. Keep authorization asynchronous, bounded, and explicit on failure.
- Respect compositor differences. GNOME-specific desktop placement requires native Shell integration; do not assume ordinary Wayland windows can set absolute positions or that GNOME supports layer-shell.
- Preserve Windows and macOS native behavior. Keep the CLI free of desktop initialization. Test Linux against native Wayland, preferably with `DISPLAY` unset, and validate single instance, actual screen tools, shortcuts, and docking together.

## Development builds and releases

- Choose development build settings by actual compilation time in the current environment. Prefer whichever completes valid verification fastest; do not trade iteration speed for a smaller executable. Size optimization is reserved for an explicitly requested new version release.

- Unless the user explicitly requests a new version release, use the fastest available dev/debug incremental build for development, fixes, debugging, and acceptance. Reuse the development dependency cache; keep application optimization and LTO disabled. Do not use `--release` or perform a full optimized release build for local verification.
- Development defaults: `cargo build --features desktop --bin bt-app`; use focused `cargo test`/`cargo check` for the affected code. On a fresh cache, a one-time dependency build may be needed; retain it for later iterations rather than rebuilding release dependencies.
- Only an explicit request to publish a new version authorizes the unchanged formal release profile and full release gates. Never upload a development executable as a release or deploy it to production.
- After runtime changes, validate the development executable, atomically replace the exact runtime used by the acceptance application, and rebuild that application. Never overwrite a running executable or keep testing an old runtime.
