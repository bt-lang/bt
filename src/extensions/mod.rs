//! Compilation boundary for the BT extension system.
//!
//! Current extension capabilities include `.bts` package parsing, manifest/bindings validation,
//! project extension scanning, global entry injection, the pure BT Runner, the WASM/WASI Runner,
//! the project-level shared service, the CLI toolchain, and BtValueBinary encoding/decoding.
//! Default builds do not enable the `extensions` feature, so they do not pull in Wasmtime, WASI, or
//! extra startup paths.

#![allow(dead_code)]

pub mod bindings;
pub mod bt_runner;
pub mod cli;
pub mod manager;
pub mod manifest;
pub mod package;
pub mod registry;
pub mod service;
pub mod value_codec;
pub mod wasm_runner;

/// Cargo feature name for the extension system.
pub const FEATURE_NAME: &str = "extensions";

/// Project-level extensions directory name.
pub const PROJECT_EXTENSIONS_DIR: &str = "extensions";

/// BT extension package file suffix, excluding the dot.
pub const PACKAGE_EXTENSION: &str = "bts";

/// Check whether the name is a lowercase identifier used by extension manifests.
pub(crate) fn is_lower_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut previous_underscore = false;
    for ch in chars {
        if ch == '_' {
            if previous_underscore {
                return false;
            }
            previous_underscore = true;
            continue;
        }
        previous_underscore = false;
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() {
            return false;
        }
    }
    !name.ends_with('_')
}

/// Check whether the name is a snake_case identifier used by the BT API.
pub(crate) fn is_snake_case_identifier(name: &str) -> bool {
    is_lower_identifier(name)
}

/// Check whether the name is a capitalized identifier used by extension object types.
pub(crate) fn is_type_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Check whether a package path is a safe relative path.
pub(crate) fn is_safe_package_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains('\\') {
        return false;
    }
    let mut saw_component = false;
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            return false;
        }
        saw_component = true;
    }
    saw_component
}
