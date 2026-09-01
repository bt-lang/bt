//! Standalone BT CLI entry point.
//!
//! The CLI does not link the Tauri/WebView runtime, so scripts and servers retain normal interpreter behavior
//! even when desktop dependencies are unavailable.

mod bt_cli;
mod bytecode;
mod compiler;
mod device;
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
    bt_cli::main();
}
