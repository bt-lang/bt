//! Unified BT executable entry point.
//!
//! Both `bt.exe` and `bt_app.exe` are built from this entry point. At startup, it first checks for an embedded desktop Bundle,
//! then falls back to the executable name so a packaged `dist/AppName.exe` still starts in App mode.

mod app;
mod bt_app;
mod bt_cli;
mod bundle;
mod bytecode;
mod compiler;
mod context;
mod device;
mod error;
#[cfg(feature = "extensions")]
mod extensions;
mod io;
mod lexer;
mod libs;
mod net;
mod parser;
mod path;
mod permission;
mod source;
mod task;
mod timer;
mod value;
mod vm;
mod web;

fn main() {
    let current_exe = std::env::current_exe().ok();

    let has_bundle = current_exe
        .as_deref()
        .map(crate::bundle::footer::has_bundle_injected)
        .unwrap_or(false);

    let exe_name = current_exe
        .as_deref()
        .and_then(|path| path.file_stem())
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_lowercase();

    if has_bundle || exe_name.contains("bt_app") {
        bt_app::main();
    } else {
        bt_cli::main();
    }
}
