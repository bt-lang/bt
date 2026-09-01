//! Extension CLI toolchain.
//!
//! This module is compiled only with the `extensions` feature. It handles scaffolding local
//! extension projects, packaging `.bts` files, installation, inspection, and validation. The
//! lightweight CLI built with `--no-default-features` does not link this module, avoiding a
//! zip/Wasmtime dependency when extensions are not used.

use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use console::measure_text_width;
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use zip::write::SimpleFileOptions;

use crate::extensions::bindings::{
    BindingMethodLifecycle, BindingParam, BindingParamRole, ExtensionBindings,
};
use crate::extensions::bt_runner::BtRunnerModule;
use crate::extensions::manifest::{ExtensionKind, ExtensionManifest};
use crate::extensions::package::{
    ExtensionPackage, PackageFileEntry, MAX_BT_ENTRY_SOURCE_BYTES, MAX_DESCRIPTOR_BYTES,
    MAX_PACKAGE_ENTRIES, MAX_PACKAGE_ENTRY_BYTES, MAX_PACKAGE_FILE_BYTES,
    MAX_PACKAGE_TOTAL_UNCOMPRESSED_BYTES, MAX_WASM_ENTRY_BYTES,
};
use crate::extensions::registry::ExtensionRegistry;
use crate::extensions::wasm_runner::WasmRunnerModule;
use crate::extensions::{
    is_lower_identifier, is_safe_package_path, PACKAGE_EXTENSION, PROJECT_EXTENSIONS_DIR,
};
use crate::path as bt_path;
use crate::vm::Vm;

/// Base URL for the official extension registry API.
const REGISTRY_API_BASE: &str = "https://btlang.org/api/ext";

/// Network request timeout for registry installations.
const REGISTRY_INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Width of the official extension download progress bar.
const REGISTRY_INSTALL_PROGRESS_WIDTH: usize = 40;

/// Refresh interval for official extension download progress.
const REGISTRY_INSTALL_PROGRESS_INTERVAL: Duration = Duration::from_millis(80);

/// Maximum size of official extension metadata, in bytes.
const MAX_REGISTRY_INFO_BYTES: u64 = 1024 * 1024;

/// Maximum size of an official extension README, in bytes.
const MAX_REGISTRY_README_BYTES: u64 = 2 * 1024 * 1024;

/// Development directory to package.
struct ProjectPackage {
    /// Package metadata parsed according to extension runtime rules.
    package: ExtensionPackage,
    /// Local files to write to the zip archive.
    files: Vec<ProjectFileEntry>,
}

/// A local file in the development directory.
struct ProjectFileEntry {
    /// Safe relative path within the zip archive.
    package_path: String,
    /// Path on the host file system.
    host_path: PathBuf,
    /// File size in bytes.
    uncompressed_size: u64,
}

/// Parsed `bt ext new` options.
struct NewOptions {
    /// Extension project directory to create.
    project_dir: PathBuf,
    /// Extension backend type.
    kind: ExtensionKind,
}

/// Parsed `bt ext build` options.
struct BuildOptions {
    /// Extension development directory.
    project_dir: PathBuf,
    /// Output `.bts` file path.
    output_path: Option<PathBuf>,
}

/// Parsed `bt install` options.
struct RemoteInstallOptions {
    /// Name of the extension to install.
    name: String,
    /// Requested version; defaults to the registry's latest version.
    version: Option<String>,
    /// Project directory in which to install the extension.
    project_dir: PathBuf,
}

/// Extension version metadata returned by the official registry.
#[derive(Debug, Clone, Deserialize)]
struct RegistryVersion {
    /// Version number, which must initially be a three-part SemVer.
    version: String,
    /// Filename used to save the downloaded `.bts` file.
    file: String,
    /// Official download API URL.
    download_url: String,
    /// Hexadecimal SHA-256 digest of the `.bts` package.
    sha256: String,
    /// Size of the `.bts` package in bytes.
    size: u64,
    /// Minimum BT version required to run this extension.
    bt_min_version: String,
    /// Extension backend type.
    kind: String,
    /// Extension ABI name.
    abi: String,
    /// Whether this version has been withdrawn from the official installation channel.
    #[serde(default)]
    yanked: bool,
    /// Summary of permissions declared by the extension.
    permissions: Option<Vec<String>>,
}

/// Public extension information returned by the official registry.
#[derive(Debug, Clone, Deserialize)]
struct RegistryInfo {
    /// Extension name.
    name: String,
    /// Latest installable version.
    latest: String,
    /// Available versions.
    versions: Vec<RegistryVersion>,
}

/// Validation result for a downloaded temporary file.
struct DownloadedPackage {
    /// Path to the temporary `.bts` file.
    temp_path: PathBuf,
    /// SHA-256 digest computed during download.
    sha256: String,
    /// Number of bytes downloaded locally.
    size: u64,
}

/// State for the official extension download progress line.
struct DownloadProgress {
    /// Target byte count declared by registry metadata.
    total_size: u64,
    /// Number of bytes successfully written to the temporary file.
    downloaded_size: u64,
    /// Time at which the download started.
    started_at: Instant,
    /// Time of the previous progress-line refresh.
    last_draw_at: Instant,
    /// Terminal display width of the previous progress line.
    last_line_width: usize,
    /// Whether a progress line has been drawn in the terminal.
    has_drawn: bool,
    /// Whether standard output is an interactive terminal.
    interactive: bool,
}

impl DownloadProgress {
    /// Creates download progress state.
    fn new(total_size: u64) -> Self {
        let now = Instant::now();
        Self {
            total_size,
            downloaded_size: 0,
            started_at: now,
            last_draw_at: now,
            last_line_width: 0,
            has_drawn: false,
            interactive: io::stdout().is_terminal(),
        }
    }

    /// Records a written download chunk and refreshes terminal progress at the throttled interval.
    fn add(&mut self, chunk_size: usize) -> Result<(), String> {
        self.downloaded_size = self
            .downloaded_size
            .checked_add(chunk_size as u64)
            .ok_or_else(|| "Extension package download progress size overflow".to_string())?;
        if !self.interactive {
            return Ok(());
        }
        let now = Instant::now();
        if !self.has_drawn
            || now.duration_since(self.last_draw_at) >= REGISTRY_INSTALL_PROGRESS_INTERVAL
            || self.downloaded_size >= self.total_size
        {
            self.draw(now)?;
        }
        Ok(())
    }

    /// Prints the final progress line followed by a newline.
    fn finish(&mut self) -> Result<(), String> {
        if !self.interactive {
            println!(
                "{}",
                format_download_progress_line(
                    self.downloaded_size,
                    self.total_size,
                    Instant::now().duration_since(self.started_at),
                )
            );
            return Ok(());
        }
        if !self.has_drawn || self.downloaded_size < self.total_size {
            self.draw(Instant::now())?;
        }
        println!();
        Ok(())
    }

    /// Ends the current progress line after a failed download so subsequent errors start cleanly.
    fn abort(&mut self) {
        if self.has_drawn {
            println!();
            let _ = io::stdout().flush();
        }
    }

    /// Draws a single-line download progress indicator.
    fn draw(&mut self, now: Instant) -> Result<(), String> {
        let line = format_download_progress_line(
            self.downloaded_size,
            self.total_size,
            now.duration_since(self.started_at),
        );
        let width = measure_text_width(&line);
        let padding = self.last_line_width.saturating_sub(width);
        print!("\r{}{}", line, " ".repeat(padding));
        io::stdout()
            .flush()
            .map_err(|err| format!("Failed to refresh download progress: {}", err))?;
        self.last_line_width = width;
        self.last_draw_at = now;
        self.has_drawn = true;
        Ok(())
    }
}

/// Runs an extension toolchain command.
pub fn run(args: &[String]) -> Result<(), String> {
    if args.is_empty() || is_help_arg(&args[0]) {
        print_help();
        return Ok(());
    }
    if args.iter().skip(1).any(|arg| is_help_arg(arg)) {
        print_help();
        return Ok(());
    }

    match args[0].as_str() {
        "new" => handle_new(&args[1..]),
        "build" => handle_build(&args[1..]),
        "install" => handle_install(&args[1..]),
        "info" => handle_info(&args[1..]),
        "check" => handle_check(&args[1..]),
        other => Err(format!("Unknown extension command `{}`", other)),
    }
}

/// Runs the top-level `bt install <name> [version]` remote installation command.
pub fn install(args: &[String]) -> Result<(), String> {
    handle_remote_install(args)
}

/// Prints extension toolchain help.
fn print_help() {
    println!("BT Extension Toolchain");
    println!();
    println!("Usage:");
    println!("  bt ext new <dir> [--kind bt|wasm]");
    println!("  bt ext build [dir] [-o <file.bts>]");
    println!("  bt ext install <file.bts> [project_dir]");
    println!("  bt ext info <file.bts>");
    println!("  bt ext check [dir|file.bts]");
    println!();
}

/// Returns whether an argument is a help flag.
fn is_help_arg(arg: &str) -> bool {
    arg == "-h" || arg == "--help"
}

/// Runs `bt ext new`.
fn handle_new(args: &[String]) -> Result<(), String> {
    let options = parse_new_options(args)?;
    let name = extension_name_from_dir(&options.project_dir)?;
    if !is_lower_identifier(&name) {
        return Err(format!(
            "Extension name `{}` may contain only lowercase letters, digits, and underscores, and must start with a lowercase letter",
            name
        ));
    }
    if options.project_dir.exists() && !is_directory_empty(&options.project_dir)? {
        return Err(format!(
            "Extension project directory `{}` already exists and is not empty",
            options.project_dir.display()
        ));
    }

    fs::create_dir_all(&options.project_dir)
        .map_err(|err| format!("Failed to create extension project directory: {}", err))?;
    write_new_file(
        &options.project_dir.join("manifest.json"),
        &scaffold_manifest(&name, options.kind),
    )?;
    write_new_file(
        &options.project_dir.join("bindings.json"),
        &scaffold_bindings(&name),
    )?;
    match options.kind {
        ExtensionKind::Bt => {
            let src_dir = options.project_dir.join("src");
            fs::create_dir_all(&src_dir)
                .map_err(|err| format!("Failed to create src directory: {}", err))?;
            write_new_file(&src_dir.join("lib.bt"), &scaffold_bt_source(&name))?;
        }
        ExtensionKind::Wasm => {
            let src_dir = options.project_dir.join("src");
            fs::create_dir_all(&src_dir)
                .map_err(|err| format!("Failed to create src directory: {}", err))?;
            write_new_file(
                &options.project_dir.join("Cargo.toml"),
                &scaffold_wasm_cargo_toml(&name),
            )?;
            write_new_file(&src_dir.join("lib.rs"), &scaffold_wasm_source(&name))?;
            write_new_file(
                &options.project_dir.join("README.md"),
                &scaffold_wasm_readme(&name),
            )?;
        }
    }

    println!(
        "Created extension project: {}",
        bt_path::path_text(&bt_path::normalize_path(&options.project_dir))
    );
    Ok(())
}

/// Parses `bt ext new` arguments.
fn parse_new_options(args: &[String]) -> Result<NewOptions, String> {
    let mut project_dir = None;
    let mut kind = ExtensionKind::Bt;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--kind" {
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "Missing value for --kind: bt or wasm".to_string())?;
            kind = parse_kind(value)?;
        } else if let Some(value) = arg.strip_prefix("--kind=") {
            kind = parse_kind(value)?;
        } else if arg.starts_with('-') {
            return Err(format!("Unknown new argument `{}`", arg));
        } else if project_dir.is_none() {
            project_dir = Some(PathBuf::from(arg));
        } else {
            return Err(format!("Unexpected extra new argument `{}`", arg));
        }
        index += 1;
    }
    Ok(NewOptions {
        project_dir: project_dir
            .ok_or_else(|| "Missing extension project directory: bt ext new <dir>".to_string())?,
        kind,
    })
}

/// Parses an extension backend type.
fn parse_kind(value: &str) -> Result<ExtensionKind, String> {
    match value {
        "bt" => Ok(ExtensionKind::Bt),
        "wasm" => Ok(ExtensionKind::Wasm),
        _ => Err(format!(
            "Unsupported extension kind `{}`; expected bt or wasm",
            value
        )),
    }
}

/// Derives the extension manifest name from the project directory name.
fn extension_name_from_dir(project_dir: &Path) -> Result<String, String> {
    project_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .ok_or_else(|| {
            format!(
                "Could not derive extension name from `{}`",
                project_dir.display()
            )
        })
}

/// Returns whether a directory is empty.
fn is_directory_empty(path: &Path) -> Result<bool, String> {
    if !path.is_dir() {
        return Err(format!(
            "`{}` already exists but is not a directory",
            path.display()
        ));
    }
    let mut entries = fs::read_dir(path)
        .map_err(|err| format!("Failed to read directory `{}`: {}", path.display(), err))?;
    Ok(entries.next().is_none())
}

/// Writes a new project file without overwriting existing user content.
fn write_new_file(path: &Path, content: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "File `{}` already exists; refusing to overwrite it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create directory `{}`: {}", parent.display(), err))?;
    }
    fs::write(path, content).map_err(|err| format!("Failed to write `{}`: {}", path.display(), err))
}

/// Generates an extension manifest scaffold.
fn scaffold_manifest(name: &str, kind: ExtensionKind) -> String {
    let (kind_text, abi, entry) = match kind {
        ExtensionKind::Bt => ("bt", "bts-bt-1", "src/lib.bt"),
        ExtensionKind::Wasm => ("wasm", "bts-wasi-1", "module.wasm"),
    };
    format!(
        r#"{{
    "format": "bts",
    "format_version": 1,
    "name": "{name}",
    "version": "1.0.0",
    "description": "{name} extension",
    "author": "",
    "kind": "{kind_text}",
    "abi": "{abi}",
    "bt_min_version": "{bt_version}",
    "api_version": 1,
    "entry": "{entry}",
    "bindings": "bindings.json",
    "permissions": [],
    "limits": {{
        "max_args_bytes": 16777216,
        "max_result_bytes": 16777216
    }}
}}
"#,
        name = name,
        kind_text = kind_text,
        abi = abi,
        bt_version = env!("CARGO_PKG_VERSION"),
        entry = entry
    )
}

/// Generates an extension bindings scaffold.
fn scaffold_bindings(name: &str) -> String {
    let type_name = snake_to_type_name(name);
    format!(
        r#"{{
    "api_version": 1,
    "functions": [
        {{
            "name": "{name}",
            "id": 1,
            "params": [
                {{ "name": "value", "type": "int" }}
            ],
            "returns": "{type_name}"
        }}
    ],
    "objects": [
        {{
            "name": "{type_name}",
            "type_id": 1,
            "methods": [
                {{
                    "name": "add",
                    "id": 2,
                    "params": [
                        {{ "name": "value", "type": "int" }}
                    ],
                    "returns": "{type_name}"
                }},
                {{
                    "name": "value",
                    "id": 3,
                    "params": [],
                    "returns": "int"
                }},
                {{
                    "name": "close",
                    "id": 4,
                    "params": [],
                    "returns": "bool",
                    "lifecycle": "dispose"
                }}
            ]
        }}
    ]
}}
"#,
        name = name,
        type_name = type_name
    )
}

/// Generates scaffolded entry-point source for a pure BT extension.
fn scaffold_bt_source(name: &str) -> String {
    let type_name = snake_to_type_name(name);
    format!(
        r#"class {type_name} {{
    value_num: 0

    new(value) {{
        this.value_num = value
        this
    }}

    pub add(value) {{
        this.value_num += value
        this
    }}

    pub value() {{
        this.value_num
    }}

    pub close() {{
        true
    }}
}}

fn {name}(value) {{
    {type_name}::new(value)
}}
"#,
        name = name,
        type_name = type_name
    )
}

/// Generates the Cargo configuration scaffold for a WASM Rust SDK extension.
fn scaffold_wasm_cargo_toml(name: &str) -> String {
    let sdk_path =
        bt_path::path_text(&Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/bt-extension-sdk"));
    format!(
        r#"[package]
name = "{name}"
version = "1.0.0"
edition = "2021"
publish = false

[workspace]

[lib]
crate-type = ["cdylib"]

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true

[dependencies]
bt-extension-sdk = {{ path = "{sdk_path}" }}
"#,
        name = name,
        sdk_path = sdk_path.replace('"', "\\\"")
    )
}

/// Generates the source scaffold for a WASM Rust SDK extension.
fn scaffold_wasm_source(name: &str) -> String {
    let type_name = snake_to_type_name(name);
    format!(
        r#"use std::cell::RefCell;

use bt_extension_sdk::{{
    bt_extension, expect_arg_count, expect_ext_object_type, expect_int, BtResult, BtValue,
    ExtObject, ObjectStore,
}};

/// Object type_id declared in bindings.json.
const TYPE_ID: u32 = 1;
/// Object type name declared in bindings.json.
const TYPE_NAME: &str = "{type_name}";
/// Maximum number of objects retained by this extension.
const MAX_OBJECTS: usize = 4096;

thread_local! {{
    /// Object state table for the current WASM instance.
    static OBJECTS: RefCell<ObjectStore<i64>> = RefCell::new(ObjectStore::new(MAX_OBJECTS));
}}

bt_extension! {{
    1 => entry_create,
    2 => entry_add,
    3 => entry_value,
    4 => entry_close,
}}

/// Creates a chainable calculation object.
fn entry_create(args: Vec<BtValue>) -> BtResult<BtValue> {{
    expect_arg_count(&args, 1, "{name}")?;
    let value = expect_int(&args, 0, "value")?;
    OBJECTS.with(|objects| {{
        let mut objects = objects.borrow_mut();
        let object_id = objects.insert(value)?;
        Ok(ExtObject::new(TYPE_ID, object_id, TYPE_NAME).into())
    }})
}}

/// Adds a value and returns the same object handle.
fn entry_add(args: Vec<BtValue>) -> BtResult<BtValue> {{
    expect_arg_count(&args, 2, "{type_name}.add")?;
    let object = expect_ext_object_type(&args, 0, "self", TYPE_ID, TYPE_NAME)?;
    let value = expect_int(&args, 1, "value")?;
    OBJECTS.with(|objects| {{
        let mut objects = objects.borrow_mut();
        let current = objects.get_mut_required(object.object_id, TYPE_NAME)?;
        *current += value;
        Ok(object.into())
    }})
}}

/// Returns the current calculation result.
fn entry_value(args: Vec<BtValue>) -> BtResult<BtValue> {{
    expect_arg_count(&args, 1, "{type_name}.value")?;
    let object = expect_ext_object_type(&args, 0, "self", TYPE_ID, TYPE_NAME)?;
    OBJECTS.with(|objects| {{
        let objects = objects.borrow();
        let value = objects.get_required(object.object_id, TYPE_NAME)?;
        Ok(BtValue::Int(*value))
    }})
}}

/// Releases the current calculation object.
fn entry_close(args: Vec<BtValue>) -> BtResult<BtValue> {{
    expect_arg_count(&args, 1, "{type_name}.close")?;
    let object = expect_ext_object_type(&args, 0, "self", TYPE_ID, TYPE_NAME)?;
    OBJECTS.with(|objects| {{
        let mut objects = objects.borrow_mut();
        objects.remove_required(object.object_id, TYPE_NAME)?;
        Ok(BtValue::Bool(true))
    }})
}}
"#,
        name = name,
        type_name = type_name
    )
}

/// Generates the README for a WASM Rust SDK extension.
fn scaffold_wasm_readme(name: &str) -> String {
    format!(
        r#"# {name}

This is a BT WASM extension project. Its source explicitly registers call IDs with `bt-extension-sdk`.

## Build

```text
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
```

After building, copy the generated wasm file to the extension project root:

```text
copy target\wasm32-wasip1\release\{name}.wasm module.wasm
```

Then package it:

```text
bt ext build .
```
"#,
        name = name
    )
}

/// Converts a snake_case name to an extension object type name.
fn snake_to_type_name(name: &str) -> String {
    let mut output = String::with_capacity(name.len());
    for part in name.split('_').filter(|part| !part.is_empty()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            output.push(first.to_ascii_uppercase());
            output.extend(chars);
        }
    }
    output
}

/// Runs `bt ext build`.
fn handle_build(args: &[String]) -> Result<(), String> {
    let options = parse_build_options(args)?;
    let project_dir = canonical_existing_dir(&options.project_dir)?;
    let output_path = match options.output_path {
        Some(path) => path,
        None => {
            let manifest = read_project_manifest(&project_dir)?;
            PathBuf::from(format!("{}.{}", manifest.name, PACKAGE_EXTENSION))
        }
    };
    validate_output_package_path(&output_path)?;
    let output_abs = absolute_path(&output_path)?;
    let project = read_project_package(&project_dir, Some(&output_abs))?;
    validate_package_backend(&project.package, &project_dir)?;
    write_package_zip(&project, &output_abs)?;
    let checked = ExtensionPackage::read(&output_abs).map_err(|err| {
        format!(
            "Failed to read back extension package `{}`: {}",
            output_abs.display(),
            err
        )
    })?;
    validate_package_backend(&checked, &project_dir)?;
    println!(
        "Built extension package: {}",
        bt_path::path_text(&bt_path::normalize_path(&output_abs))
    );
    Ok(())
}

/// Parses `bt ext build` arguments.
fn parse_build_options(args: &[String]) -> Result<BuildOptions, String> {
    let mut project_dir = None;
    let mut output_path = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-o" || arg == "--out" {
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "Missing output path: -o <file.bts>".to_string())?;
            output_path = Some(PathBuf::from(value));
        } else if let Some(value) = arg.strip_prefix("--out=") {
            output_path = Some(PathBuf::from(value));
        } else if arg.starts_with('-') {
            return Err(format!("Unknown build argument `{}`", arg));
        } else if project_dir.is_none() {
            project_dir = Some(PathBuf::from(arg));
        } else {
            return Err(format!("Unexpected extra build argument `{}`", arg));
        }
        index += 1;
    }
    Ok(BuildOptions {
        project_dir: project_dir.unwrap_or_else(|| PathBuf::from(".")),
        output_path,
    })
}

/// Runs `bt ext install`.
fn handle_install(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err(
            "Missing extension package path: bt ext install <file.bts> [project_dir]".to_string(),
        );
    }
    if args.len() > 2 {
        return Err(format!("Unexpected extra install argument `{}`", args[2]));
    }
    let package_path = PathBuf::from(&args[0]);
    let project_dir = if let Some(project_dir) = args.get(1) {
        canonical_existing_dir(Path::new(project_dir))?
    } else {
        canonical_existing_dir(Path::new("."))?
    };
    let package = ExtensionPackage::read(&package_path).map_err(|err| {
        format!(
            "Failed to read extension package `{}`: {}",
            package_path.display(),
            err
        )
    })?;
    validate_package_backend(&package, &project_dir)?;

    let package_dir = installed_extension_dir(&project_dir, &package.manifest.name);
    fs::create_dir_all(&package_dir).map_err(|err| {
        format!(
            "Failed to create extension directory `{}`: {}",
            package_dir.display(),
            err
        )
    })?;
    let target_path = installed_package_path(
        &project_dir,
        &package.manifest.name,
        &package.manifest.version,
    );
    if absolute_path(&package_path)? == absolute_path(&target_path)? {
        println!(
            "Extension package is already at the target location: {}",
            bt_path::path_text(&bt_path::normalize_path(&target_path))
        );
        return Ok(());
    }
    remove_installed_versions(&package_dir, &package.manifest.name)?;
    fs::copy(&package_path, &target_path).map_err(|err| {
        format!(
            "Failed to copy extension package to `{}`: {}",
            target_path.display(),
            err
        )
    })?;
    let installed = ExtensionPackage::read(&target_path)
        .map_err(|err| format!("Failed to validate installed extension package: {}", err))?;
    validate_package_backend(&installed, &project_dir)?;
    println!(
        "Installed extension package: {}",
        bt_path::path_text(&bt_path::normalize_path(&target_path))
    );
    Ok(())
}

/// Returns the installation directory for an extension in a project.
fn installed_extension_dir(project_dir: &Path, name: &str) -> PathBuf {
    project_dir.join(PROJECT_EXTENSIONS_DIR).join(name)
}

/// Returns the installed file path for an extension version in a project.
fn installed_package_path(project_dir: &Path, name: &str, version: &str) -> PathBuf {
    installed_extension_dir(project_dir, name).join(installed_package_file_name(name, version))
}

/// Returns a versioned `.bts` filename.
fn installed_package_file_name(name: &str, version: &str) -> String {
    format!("{}-{}.{}", name, version, PACKAGE_EXTENSION)
}

/// Removes old `.bts` versions from the directory for an extension.
fn remove_installed_versions(package_dir: &Path, name: &str) -> Result<(), String> {
    if !package_dir.exists() {
        return Ok(());
    }
    let prefix = format!("{}-", name);
    let entries = fs::read_dir(package_dir)
        .map_err(|err| format!("Failed to read extension directory: {}", err))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("Failed to read extension directory entry: {}", err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("Failed to read type of `{}`: {}", path.display(), err))?;
        if !file_type.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if file_name.starts_with(&prefix)
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case(PACKAGE_EXTENSION))
        {
            fs::remove_file(&path).map_err(|err| {
                format!(
                    "Failed to remove old extension package `{}`: {}",
                    path.display(),
                    err
                )
            })?;
        }
    }
    Ok(())
}

/// Installs an extension from the official registry with `bt install`.
fn handle_remote_install(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| is_help_arg(arg)) {
        print_remote_install_help();
        return Ok(());
    }
    let options = parse_remote_install_options(args)?;
    if !is_lower_identifier(&options.name) {
        return Err(format!(
            "Extension name `{}` may contain only lowercase letters, digits, and underscores, and must start with a lowercase letter",
            options.name
        ));
    }
    if let Some(version) = &options.version {
        validate_semver_text(version, "Extension version")?;
    }

    let project_dir = canonical_existing_dir(&options.project_dir)?;
    let info = fetch_registry_info(&options.name, options.version.as_deref())?;
    if info.name != options.name {
        return Err(format!(
            "Registry metadata name `{}` does not match requested name `{}`",
            info.name, options.name
        ));
    }
    let selected = select_registry_version(&info, options.version.as_deref())?.clone();
    validate_registry_version(&options.name, &selected)?;
    print_remote_install_plan(&options.name, &selected);

    let package_dir = installed_extension_dir(&project_dir, &options.name);
    fs::create_dir_all(&package_dir).map_err(|err| {
        format!(
            "Failed to create extension directory `{}`: {}",
            package_dir.display(),
            err
        )
    })?;

    let target_path = installed_package_path(&project_dir, &options.name, &selected.version);
    let temp_path = package_dir.join(format!(".{}-{}.tmp.bts", options.name, selected.version));
    if temp_path.exists() {
        fs::remove_file(&temp_path).map_err(|err| {
            format!(
                "Failed to remove old temporary file `{}`: {}",
                temp_path.display(),
                err
            )
        })?;
    }

    println!("[1/3] Downloading");
    let downloaded =
        match download_registry_package(&selected.download_url, &temp_path, selected.size) {
            Ok(downloaded) => downloaded,
            Err(err) => {
                let _ = fs::remove_file(&temp_path);
                return Err(err);
            }
        };
    println!();

    println!("[2/3] Verifying checksum");
    if let Err(err) = validate_downloaded_package(&downloaded, &selected) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    println!("✔ SHA-256 OK");
    println!();

    println!("[3/3] Installing");
    let package = match ExtensionPackage::read(&downloaded.temp_path)
        .map_err(|err| format!("Failed to validate downloaded extension package: {}", err))
    {
        Ok(package) => package,
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            return Err(err);
        }
    };
    if let Err(err) = validate_package_matches_registry(&package, &options.name, &selected)
        .and_then(|_| validate_package_backend(&package, &project_dir))
    {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    remove_installed_versions(&package_dir, &options.name)?;
    fs::rename(&temp_path, &target_path).map_err(|err| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "Failed to write extension package `{}`: {}",
            target_path.display(),
            err
        )
    })?;
    write_installed_readme(&package_dir, &info, &selected)?;

    match crate::extensions::manager::ExtensionManager::load_project(
        &project_dir,
        Vm::system_environment_names().iter().copied(),
    ) {
        Ok(Some(manager)) => manager.shutdown(),
        Ok(None) => {}
        Err(err) => {
            let _ = fs::remove_file(&target_path);
            return Err(format!("Failed to scan project extensions after installation; the new package was removed: {}", err));
        }
    }

    println!("✔ completed");
    println!();
    println!("Done.");
    Ok(())
}

/// Prints `bt install` help.
fn print_remote_install_help() {
    println!("Install from the Official BT Extension Registry");
    println!();
    println!("Usage:");
    println!("  bt install <name> [version] [--project <dir>]");
    println!();
    println!("Examples:");
    println!("  bt install sqlite");
    println!("  bt install sqlite 1.0.0 --project examples/app");
    println!();
}

/// Parses `bt install` arguments.
fn parse_remote_install_options(args: &[String]) -> Result<RemoteInstallOptions, String> {
    let mut name = None;
    let mut version = None;
    let mut project_dir = PathBuf::from(".");
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--project" {
            index += 1;
            let value = args
                .get(index)
                .ok_or_else(|| "Missing directory argument for --project".to_string())?;
            project_dir = PathBuf::from(value);
        } else if let Some(value) = arg.strip_prefix("--project=") {
            if value.trim().is_empty() {
                return Err("Missing directory argument for --project".to_string());
            }
            project_dir = PathBuf::from(value);
        } else if arg.starts_with('-') {
            return Err(format!("Unknown install argument `{}`", arg));
        } else if name.is_none() {
            name = Some(arg.clone());
        } else if version.is_none() {
            version = Some(arg.clone());
        } else {
            return Err(format!("Unexpected extra install argument `{}`", arg));
        }
        index += 1;
    }
    Ok(RemoteInstallOptions {
        name: name
            .ok_or_else(|| "Missing extension name: bt install <name> [version]".to_string())?,
        version,
        project_dir,
    })
}

/// Fetches extension metadata from the official registry.
fn fetch_registry_info(name: &str, version: Option<&str>) -> Result<RegistryInfo, String> {
    let url = registry_info_url(name, version);
    let raw = fetch_registry_text(url, MAX_REGISTRY_INFO_BYTES, "Extension metadata")?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("Failed to parse official extension metadata: {}", err))
}

/// Reads an official registry text response with a size limit.
fn fetch_registry_text(url: String, max_bytes: u64, label: &'static str) -> Result<String, String> {
    crate::io::ensure_rustls_provider();
    crate::io::run_async(
        async move { fetch_registry_text_async(url, max_bytes, label).await },
        Some(REGISTRY_INSTALL_TIMEOUT),
    )
}

/// Asynchronously reads an official registry text response with a size limit.
async fn fetch_registry_text_async(
    url: String,
    max_bytes: u64,
    label: &'static str,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|err| format!("Failed to create HTTP client: {}", err))?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("Request to `{}` failed: {}", url, err))?;
    if !response.status().is_success() {
        return Err(format!(
            "Request to `{}` returned HTTP {}",
            url,
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(format!("{} size exceeds {} bytes", label, max_bytes));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("Failed to read {}: {}", label, err))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{} size exceeds {} bytes", label, max_bytes));
    }
    String::from_utf8(bytes.to_vec()).map_err(|err| format!("{} is not UTF-8 text: {}", label, err))
}

/// Selects the official registry version to install.
fn select_registry_version<'a>(
    info: &'a RegistryInfo,
    requested: Option<&str>,
) -> Result<&'a RegistryVersion, String> {
    let version = requested.unwrap_or(info.latest.as_str());
    if version.is_empty() {
        return Err("Registry metadata is missing the latest version".to_string());
    }
    info.versions
        .iter()
        .find(|item| item.version == version)
        .ok_or_else(|| {
            format!(
                "Official extension `{}` has no version `{}`",
                info.name, version
            )
        })
}

/// Validates official registry version metadata.
fn validate_registry_version(name: &str, version: &RegistryVersion) -> Result<(), String> {
    validate_semver_text(&version.version, "Registry version")?;
    validate_semver_text(&version.bt_min_version, "bt_min_version")?;
    if version.yanked {
        return Err(format!(
            "Official extension `{}` version `{}` has been withdrawn",
            name, version.version
        ));
    }
    let expected_file = installed_package_file_name(name, &version.version);
    if version.file != expected_file {
        return Err(format!(
            "Registry metadata file `{}` does not match expected filename `{}`",
            version.file, expected_file
        ));
    }
    let expected_url = registry_download_url(name, &version.version);
    if version.download_url != expected_url {
        return Err(format!(
            "Registry metadata download_url `{}` does not match official URL `{}`",
            version.download_url, expected_url
        ));
    }
    if version.size == 0 || version.size > MAX_PACKAGE_FILE_BYTES {
        return Err(format!(
            "Registry metadata size must be in the range 1..={}",
            MAX_PACKAGE_FILE_BYTES
        ));
    }
    validate_sha256_text(&version.sha256)?;
    match version.kind.as_str() {
        "bt" if version.abi == "bts-bt-1" => {}
        "wasm" if version.abi == "bts-wasi-1" => {}
        _ => {
            return Err(format!(
                "Registry metadata kind/abi mismatch: kind={} abi={}",
                version.kind, version.abi
            ));
        }
    }
    if compare_semver(env!("CARGO_PKG_VERSION"), &version.bt_min_version)? < 0 {
        return Err(format!(
            "Extension `{}` {} requires BT >= {}; current version is {}",
            name,
            version.version,
            version.bt_min_version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    Ok(())
}

/// Prints a pre-installation summary.
fn print_remote_install_plan(name: &str, version: &RegistryVersion) {
    println!("Installing {} {}...", name, version.version);
    println!();
}

/// Formats the official extension download progress line.
fn format_download_progress_line(
    downloaded_size: u64,
    total_size: u64,
    elapsed: Duration,
) -> String {
    let filled = download_progress_filled(downloaded_size, total_size);
    let empty = REGISTRY_INSTALL_PROGRESS_WIDTH.saturating_sub(filled);
    let percent = download_progress_percent(downloaded_size, total_size);
    let speed = download_speed_bytes(downloaded_size, elapsed);
    format!(
        "[{}{}] {} / {}  {}%  {}/s",
        "█".repeat(filled),
        " ".repeat(empty),
        format_byte_size(downloaded_size),
        format_byte_size(total_size),
        percent,
        format_byte_size(speed)
    )
}

/// Calculates the number of filled progress-bar cells.
fn download_progress_filled(downloaded_size: u64, total_size: u64) -> usize {
    if total_size == 0 {
        return REGISTRY_INSTALL_PROGRESS_WIDTH;
    }
    let clamped = downloaded_size.min(total_size) as u128;
    ((clamped * REGISTRY_INSTALL_PROGRESS_WIDTH as u128) / total_size as u128) as usize
}

/// Calculates the download percentage.
fn download_progress_percent(downloaded_size: u64, total_size: u64) -> u64 {
    if total_size == 0 {
        return 100;
    }
    let clamped = downloaded_size.min(total_size) as u128;
    ((clamped * 100) / total_size as u128) as u64
}

/// Calculates the average download speed in bytes per second.
fn download_speed_bytes(downloaded_size: u64, elapsed: Duration) -> u64 {
    let millis = elapsed.as_millis().max(1);
    let bytes_per_second = downloaded_size as u128 * 1000 / millis;
    bytes_per_second.min(u64::MAX as u128) as u64
}

/// Formats a byte count using decimal units.
fn format_byte_size(bytes: u64) -> String {
    if bytes < 1_000 {
        return format!("{}B", bytes);
    }
    let value = bytes as f64;
    if bytes < 1_000_000 {
        return format!("{:.1}KB", value / 1_000.0);
    }
    if bytes < 1_000_000_000 {
        return format!("{:.1}MB", value / 1_000_000.0);
    }
    format!("{:.1}GB", value / 1_000_000_000.0)
}

/// Downloads an official `.bts` package to a temporary file.
fn download_registry_package(
    url: &str,
    temp_path: &Path,
    expected_size: u64,
) -> Result<DownloadedPackage, String> {
    crate::io::ensure_rustls_provider();
    let url = url.to_string();
    let temp_path = temp_path.to_path_buf();
    crate::io::run_async(
        async move { download_registry_package_async(url, temp_path, expected_size).await },
        Some(REGISTRY_INSTALL_TIMEOUT),
    )
}

/// Asynchronously downloads an official `.bts` package to a temporary file.
async fn download_registry_package_async(
    url: String,
    temp_path: PathBuf,
    expected_size: u64,
) -> Result<DownloadedPackage, String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|err| format!("Failed to create HTTP client: {}", err))?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("Failed to download extension package `{}`: {}", url, err))?;
    if !response.status().is_success() {
        return Err(format!(
            "Extension package download `{}` returned HTTP {}",
            url,
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PACKAGE_FILE_BYTES)
    {
        return Err(format!(
            "Downloaded extension package size exceeds {} bytes",
            MAX_PACKAGE_FILE_BYTES
        ));
    }

    let mut file = tokio::fs::File::create(&temp_path).await.map_err(|err| {
        format!(
            "Failed to create temporary extension package `{}`: {}",
            temp_path.display(),
            err
        )
    })?;
    let mut hasher = Sha256::new();
    let mut progress = DownloadProgress::new(expected_size);
    let mut size = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                progress.abort();
                return Err(format!(
                    "Failed to read extension package download stream: {}",
                    err
                ));
            }
        };
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| "Extension package download size overflow".to_string())?;
        if size > MAX_PACKAGE_FILE_BYTES {
            progress.abort();
            return Err(format!(
                "Downloaded extension package size exceeds {} bytes",
                MAX_PACKAGE_FILE_BYTES
            ));
        }
        hasher.update(&chunk);
        if let Err(err) = file.write_all(&chunk).await {
            progress.abort();
            return Err(format!(
                "Failed to write temporary extension package: {}",
                err
            ));
        }
        progress.add(chunk.len())?;
    }
    if let Err(err) = file.flush().await {
        progress.abort();
        return Err(format!(
            "Failed to flush temporary extension package: {}",
            err
        ));
    }
    progress.finish()?;
    Ok(DownloadedPackage {
        temp_path,
        sha256: format!("{:x}", hasher.finalize()),
        size,
    })
}

/// Validates that a download matches the official registry metadata.
fn validate_downloaded_package(
    downloaded: &DownloadedPackage,
    version: &RegistryVersion,
) -> Result<(), String> {
    if downloaded.size != version.size {
        return Err(format!(
            "Extension package size mismatch: registry says {} bytes, actual size is {} bytes",
            version.size, downloaded.size
        ));
    }
    if downloaded.sha256 != version.sha256.to_ascii_lowercase() {
        return Err(format!(
            "Extension package SHA-256 mismatch: registry says {}, actual digest is {}",
            version.sha256, downloaded.sha256
        ));
    }
    Ok(())
}

/// Validates that a downloaded package manifest matches the official registry metadata.
fn validate_package_matches_registry(
    package: &ExtensionPackage,
    name: &str,
    version: &RegistryVersion,
) -> Result<(), String> {
    if package.manifest.name != name {
        return Err(format!(
            "Package manifest.name `{}` does not match registry name `{}`",
            package.manifest.name, name
        ));
    }
    if package.manifest.version != version.version {
        return Err(format!(
            "Package manifest.version `{}` does not match registry version `{}`",
            package.manifest.version, version.version
        ));
    }
    if package.manifest.kind.name() != version.kind {
        return Err(format!(
            "Package manifest.kind `{}` does not match registry kind `{}`",
            package.manifest.kind.name(),
            version.kind
        ));
    }
    if package.manifest.abi != version.abi {
        return Err(format!(
            "Package manifest.abi `{}` does not match registry abi `{}`",
            package.manifest.abi, version.abi
        ));
    }
    if package.manifest.bt_min_version != version.bt_min_version {
        return Err(format!(
            "Package bt_min_version `{}` does not match registry bt_min_version `{}`",
            package.manifest.bt_min_version, version.bt_min_version
        ));
    }
    Ok(())
}

/// Writes the README in the installation directory.
fn write_installed_readme(
    package_dir: &Path,
    info: &RegistryInfo,
    version: &RegistryVersion,
) -> Result<(), String> {
    let readme_path = package_dir.join("readme.md");
    let readme = fetch_registry_text(
        registry_readme_url(&info.name, &version.version),
        MAX_REGISTRY_README_BYTES,
        "Extension README",
    )
    .unwrap_or_else(|_| fallback_registry_readme(info, version));
    fs::write(&readme_path, readme).map_err(|err| {
        format!(
            "Failed to write extension README `{}`: {}",
            readme_path.display(),
            err
        )
    })
}

/// Generates a local summary when the README cannot be downloaded.
fn fallback_registry_readme(info: &RegistryInfo, version: &RegistryVersion) -> String {
    format!(
        "# {}\n\nVersion: {}\n\nInstallation source: {}\n\nFile: {}\n\nSHA-256: {}\n",
        info.name, version.version, REGISTRY_API_BASE, version.file, version.sha256
    )
}

/// Returns the official extension metadata API URL.
fn registry_info_url(name: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{}/{}/{}", REGISTRY_API_BASE, name, version),
        None => format!("{}/{}", REGISTRY_API_BASE, name),
    }
}

/// Returns the official extension package download API URL.
fn registry_download_url(name: &str, version: &str) -> String {
    format!("{}/{}/download/{}", REGISTRY_API_BASE, name, version)
}

/// Returns the official extension README API URL.
fn registry_readme_url(name: &str, version: &str) -> String {
    format!("{}/{}/readme/{}", REGISTRY_API_BASE, name, version)
}

/// Validates three-part SemVer text.
fn validate_semver_text(version: &str, label: &str) -> Result<(), String> {
    parse_semver(version)
        .map(|_| ())
        .map_err(|err| format!("Invalid {} `{}`: {}", label, version, err))
}

/// Validates hexadecimal SHA-256 text.
fn validate_sha256_text(value: &str) -> Result<(), String> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(format!(
        "Registry metadata SHA-256 `{}` is not a 64-digit hexadecimal value",
        value
    ))
}

/// Compares two three-part SemVer values, returning -1, 0, or 1.
fn compare_semver(left: &str, right: &str) -> Result<i8, String> {
    let left = parse_semver(left)?;
    let right = parse_semver(right)?;
    for index in 0..3 {
        if left[index] < right[index] {
            return Ok(-1);
        }
        if left[index] > right[index] {
            return Ok(1);
        }
    }
    Ok(0)
}

/// Parses the three-part SemVer format supported in the initial phase.
fn parse_semver(version: &str) -> Result<[u64; 3], String> {
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err("Version must use the three-part major.minor.patch format".to_string());
    }
    let mut values = [0_u64; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Version components may contain only digits".to_string());
        }
        values[index] = part
            .parse::<u64>()
            .map_err(|err| format!("Failed to parse version component: {}", err))?;
    }
    Ok(values)
}

/// Runs `bt ext info`.
fn handle_info(args: &[String]) -> Result<(), String> {
    if args.len() != 1 {
        return Err("Usage: bt ext info <file.bts>".to_string());
    }
    let path = PathBuf::from(&args[0]);
    let package = ExtensionPackage::read(&path).map_err(|err| {
        format!(
            "Failed to read extension package `{}`: {}",
            path.display(),
            err
        )
    })?;
    print_package_info(&package);
    Ok(())
}

/// Runs `bt ext check`.
fn handle_check(args: &[String]) -> Result<(), String> {
    if args.len() > 1 {
        return Err(format!("Unexpected extra check argument `{}`", args[1]));
    }
    let path = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if path.is_dir() {
        let project_dir = canonical_existing_dir(&path)?;
        let project = read_project_package(&project_dir, None)?;
        validate_package_backend(&project.package, &project_dir)?;
        println!(
            "Extension project check passed: {} {} ({})",
            project.package.manifest.name,
            project.package.manifest.version,
            project.package.manifest.kind.name()
        );
        return Ok(());
    }
    let package = ExtensionPackage::read(&path).map_err(|err| {
        format!(
            "Failed to read extension package `{}`: {}",
            path.display(),
            err
        )
    })?;
    let project_root = canonical_existing_dir(Path::new("."))?;
    validate_package_backend(&package, &project_root)?;
    println!(
        "Extension package check passed: {} {} ({})",
        package.manifest.name,
        package.manifest.version,
        package.manifest.kind.name()
    );
    Ok(())
}

/// Prints an extension package summary.
fn print_package_info(package: &ExtensionPackage) {
    println!("Extension package: {}", package.path.display());
    println!("Name: {}", package.manifest.name);
    println!("Version: {}", package.manifest.version);
    println!("Backend: {}", package.manifest.kind.name());
    println!("ABI: {}", package.manifest.abi);
    println!("Entry point: {}", package.manifest.entry);
    println!("Bindings: {}", package.manifest.bindings);
    let permissions = package.manifest.permissions.names();
    println!(
        "Permissions: {}",
        if permissions.is_empty() {
            "None".to_string()
        } else {
            permissions.join(", ")
        }
    );
    println!(
        "Limits: max_args_bytes={} max_result_bytes={}",
        package.manifest.limits.max_args_bytes, package.manifest.limits.max_result_bytes
    );
    println!(
        "Runtime: mode={} workers={} queue_limit={} call_timeout_ms={} idle_ttl_ms={} max_objects={} max_worker_objects={} max_inflight_calls={}",
        package.manifest.runtime.mode.name(),
        package.manifest.runtime.workers,
        package.manifest.runtime.queue_limit,
        package.manifest.runtime.call_timeout_ms,
        package.manifest.runtime.idle_ttl_ms,
        package.manifest.runtime.max_objects,
        package.manifest.runtime.max_worker_objects,
        package.manifest.runtime.max_inflight_calls
    );
    println!("File count: {}", package.files.len());
    println!("Public entry points:");
    for function in &package.bindings.functions {
        println!(
            "  - {}",
            callable_signature(&function.name, &function.params, &function.returns)
        );
    }
    if !package.bindings.objects.is_empty() {
        println!("Objects:");
        for object in &package.bindings.objects {
            println!("  - {}#{}", object.name, object.type_id);
            for method in &object.methods {
                println!(
                    "    - {}",
                    method_signature(
                        &method.name,
                        &method.params,
                        &method.returns,
                        method.lifecycle
                    )
                );
            }
        }
    }
}

/// Generates a function or method signature summary.
fn callable_signature(name: &str, params: &[BindingParam], returns: &str) -> String {
    let params = params
        .iter()
        .map(param_signature)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({}) -> {}", name, params, returns)
}

/// Generates an object method signature summary.
fn method_signature(
    name: &str,
    params: &[BindingParam],
    returns: &str,
    lifecycle: BindingMethodLifecycle,
) -> String {
    let signature = callable_signature(name, params, returns);
    if lifecycle.is_dispose() {
        format!("{} [{}]", signature, lifecycle.name())
    } else {
        signature
    }
}

/// Generates a signature summary for one parameter.
fn param_signature(param: &BindingParam) -> String {
    if param.role == BindingParamRole::Value {
        format!("{}:{}", param.name, param.value_type.name())
    } else {
        format!(
            "{}:{} {}",
            param.name,
            param.value_type.name(),
            param.role.name()
        )
    }
}

/// Validates that an extension package can be loaded by its runtime backend.
fn validate_package_backend(package: &ExtensionPackage, project_root: &Path) -> Result<(), String> {
    match package.manifest.kind {
        ExtensionKind::Bt => {
            let _ = BtRunnerModule::from_package(0, project_root, package)?;
        }
        ExtensionKind::Wasm => {
            if package.manifest.runtime.mode.is_shared() {
                let _ = WasmRunnerModule::from_shared_package(0, project_root, package)?;
            } else {
                let _ = WasmRunnerModule::from_package(0, project_root, package)?;
            }
        }
    }
    let _ = ExtensionRegistry::from_packages(
        vec![package.clone()],
        Vm::system_environment_names().iter().copied(),
    )?;
    Ok(())
}

/// Reads an extension project from a development directory, reusing manifest/bindings validation.
fn read_project_package(
    project_dir: &Path,
    skip_abs_path: Option<&Path>,
) -> Result<ProjectPackage, String> {
    let files = collect_project_files(project_dir, skip_abs_path)?;
    let manifest = read_project_manifest(project_dir)?;
    if manifest.entry == manifest.bindings {
        return Err(
            "manifest.entry and manifest.bindings cannot refer to the same file".to_string(),
        );
    }
    let bindings_path = project_dir.join(host_path_from_package_path(&manifest.bindings)?);
    let bindings_raw =
        read_text_file_limited(&bindings_path, MAX_DESCRIPTOR_BYTES, &manifest.bindings)?;
    let bindings = ExtensionBindings::parse(&bindings_raw, &manifest)?;
    let entry_path = project_dir.join(host_path_from_package_path(&manifest.entry)?);
    let entry_source = if manifest.kind == ExtensionKind::Bt {
        Some(read_text_file_limited(
            &entry_path,
            MAX_BT_ENTRY_SOURCE_BYTES,
            &manifest.entry,
        )?)
    } else {
        None
    };
    let entry_wasm = if manifest.kind == ExtensionKind::Wasm {
        Some(read_bytes_file_limited(
            &entry_path,
            MAX_WASM_ENTRY_BYTES,
            &manifest.entry,
        )?)
    } else {
        None
    };
    let package_files = files
        .iter()
        .map(|entry| PackageFileEntry {
            path: entry.package_path.clone(),
            uncompressed_size: entry.uncompressed_size,
            compressed_size: 0,
        })
        .collect::<Vec<_>>();
    let package = ExtensionPackage {
        path: project_dir.to_path_buf(),
        manifest,
        bindings,
        entry_source,
        entry_wasm,
        files: package_files,
    };
    ensure_project_package_has_file(&package, "manifest.json")?;
    let bindings_name = package.manifest.bindings.clone();
    let entry_name = package.manifest.entry.clone();
    ensure_project_package_has_file(&package, &bindings_name)?;
    ensure_project_package_has_file(&package, &entry_name)?;
    Ok(ProjectPackage { package, files })
}

/// Reads only the development manifest without recursively collecting files to package.
fn read_project_manifest(project_dir: &Path) -> Result<ExtensionManifest, String> {
    let manifest_path = project_dir.join("manifest.json");
    let manifest_raw =
        read_text_file_limited(&manifest_path, MAX_DESCRIPTOR_BYTES, "manifest.json")?;
    ExtensionManifest::parse(&manifest_raw)
}

/// Ensures that a development package contains the specified file.
fn ensure_project_package_has_file(
    package: &ExtensionPackage,
    package_path: &str,
) -> Result<(), String> {
    if package.has_file(package_path) {
        return Ok(());
    }
    Err(format!("Extension project is missing `{}`", package_path))
}

/// Reads a size-limited UTF-8 text file.
fn read_text_file_limited(path: &Path, max_bytes: u64, label: &str) -> Result<String, String> {
    let bytes = read_bytes_file_limited(path, max_bytes, label)?;
    String::from_utf8(bytes).map_err(|err| format!("`{}` is not UTF-8 text: {}", label, err))
}

/// Reads a size-limited binary file.
fn read_bytes_file_limited(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path)
        .map_err(|err| format!("Failed to read metadata for `{}`: {}", label, err))?;
    if !metadata.is_file() {
        return Err(format!("`{}` is not a regular file", label));
    }
    if metadata.len() > max_bytes {
        return Err(format!("`{}` exceeds {} bytes", label, max_bytes));
    }
    let file = File::open(path).map_err(|err| format!("Failed to open `{}`: {}", label, err))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Failed to read `{}`: {}", label, err))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "`{}` exceeds {} bytes after reading",
            label, max_bytes
        ));
    }
    Ok(bytes)
}

/// Collects regular files from the development directory for the zip archive.
fn collect_project_files(
    project_dir: &Path,
    skip_abs_path: Option<&Path>,
) -> Result<Vec<ProjectFileEntry>, String> {
    let mut files = Vec::new();
    collect_project_files_inner(project_dir, project_dir, skip_abs_path, &mut files)?;
    files.sort_by(|left, right| left.package_path.cmp(&right.package_path));
    if files.len() > MAX_PACKAGE_ENTRIES {
        return Err(format!(
            "Extension package entry count exceeds {}",
            MAX_PACKAGE_ENTRIES
        ));
    }
    let mut total = 0_u64;
    for file in &files {
        if file.uncompressed_size > MAX_PACKAGE_ENTRY_BYTES {
            return Err(format!(
                "Extension package entry `{}` exceeds {} bytes when uncompressed",
                file.package_path, MAX_PACKAGE_ENTRY_BYTES
            ));
        }
        total = total
            .checked_add(file.uncompressed_size)
            .ok_or_else(|| "Total uncompressed extension package size overflow".to_string())?;
        if total > MAX_PACKAGE_TOTAL_UNCOMPRESSED_BYTES {
            return Err(format!(
                "Total uncompressed extension package size exceeds {} bytes",
                MAX_PACKAGE_TOTAL_UNCOMPRESSED_BYTES
            ));
        }
    }
    Ok(files)
}

/// Recursively collects files from a development directory.
fn collect_project_files_inner(
    root: &Path,
    dir: &Path,
    skip_abs_path: Option<&Path>,
    files: &mut Vec<ProjectFileEntry>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|err| format!("Failed to read directory `{}`: {}", dir.display(), err))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Failed to read directory entry: {}", err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("Failed to read type of `{}`: {}", path.display(), err))?;
        if file_type.is_symlink() {
            return Err(format!(
                "Extension packaging does not support symbolic link `{}`",
                path.display()
            ));
        }
        if file_type.is_dir() {
            if should_skip_project_dir(&path) {
                continue;
            }
            collect_project_files_inner(root, &path, skip_abs_path, files)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let abs_path = absolute_path(&path)?;
        if skip_abs_path.is_some_and(|skip| same_absolute_path(skip, &abs_path)) {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|err| {
            format!(
                "Failed to compute relative path for `{}`: {}",
                path.display(),
                err
            )
        })?;
        let package_path = package_path_from_relative(relative)?;
        let metadata = entry
            .metadata()
            .map_err(|err| format!("Failed to read metadata for `{}`: {}", path.display(), err))?;
        files.push(ProjectFileEntry {
            package_path,
            host_path: path,
            uncompressed_size: metadata.len(),
        });
    }
    Ok(())
}

/// Returns whether a development subdirectory should be skipped during packaging.
fn should_skip_project_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|value| value.to_str()),
        Some("target" | ".git" | ".hg" | ".svn" | "node_modules")
    )
}

/// Converts a local relative path to a path within the zip archive.
fn package_path_from_relative(relative: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| format!("Path `{}` is not UTF-8", relative.display()))?;
                parts.push(value.to_string());
            }
            _ => {
                return Err(format!(
                    "Path `{}` cannot be converted to a safe extension package relative path",
                    relative.display()
                ));
            }
        }
    }
    let package_path = parts.join("/");
    if !is_safe_package_path(&package_path) {
        return Err(format!(
            "Extension package entry `{}` is not a safe relative path",
            package_path
        ));
    }
    Ok(package_path)
}

/// Converts an in-package path to a host-relative path.
fn host_path_from_package_path(package_path: &str) -> Result<PathBuf, String> {
    if !is_safe_package_path(package_path) {
        return Err(format!(
            "In-package path `{}` is not a safe relative path",
            package_path
        ));
    }
    let mut path = PathBuf::new();
    for part in package_path.split('/') {
        path.push(part);
    }
    Ok(path)
}

/// Writes a `.bts` zip package.
fn write_package_zip(project: &ProjectPackage, output_path: &Path) -> Result<(), String> {
    if let Some(parent) = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create output directory `{}`: {}",
                parent.display(),
                err
            )
        })?;
    }
    let file = File::create(output_path).map_err(|err| {
        format!(
            "Failed to create output package `{}`: {}",
            output_path.display(),
            err
        )
    })?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for entry in &project.files {
        writer
            .start_file(&entry.package_path, options)
            .map_err(|err| {
                format!(
                    "Failed to write zip entry `{}`: {}",
                    entry.package_path, err
                )
            })?;
        let mut input = File::open(&entry.host_path)
            .map_err(|err| format!("Failed to open `{}`: {}", entry.host_path.display(), err))?;
        io::copy(&mut input, &mut writer).map_err(|err| {
            format!(
                "Failed to copy contents of `{}`: {}",
                entry.host_path.display(),
                err
            )
        })?;
    }
    writer
        .finish()
        .map_err(|err| format!("Failed to finish writing zip archive: {}", err))?;
    Ok(())
}

/// Validates the output package path suffix.
fn validate_output_package_path(path: &Path) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case(PACKAGE_EXTENSION) {
        return Ok(());
    }
    Err(format!(
        "Output extension package `{}` must use the .{} suffix",
        path.display(),
        PACKAGE_EXTENSION
    ))
}

/// Returns the canonical path of an existing directory.
fn canonical_existing_dir(path: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(path)
        .map_err(|err| format!("Failed to resolve directory `{}`: {}", path.display(), err))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(format!("`{}` is not a directory", path.display()))
    }
}

/// Returns a normalized absolute path.
fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| format!("Failed to read current directory: {}", err))?
            .join(path)
    };
    Ok(bt_path::normalize_path(path))
}

/// Compares absolute paths using BT display paths to avoid Windows verbatim-prefix differences.
fn same_absolute_path(left: &Path, right: &Path) -> bool {
    path_compare_key(left) == path_compare_key(right)
}

/// Generates a normalized text key for comparing local paths.
///
/// Existing paths use system canonicalization first to reconcile Windows short-path aliases with
/// long path text. Nonexistent output paths remain text-normalized so building a new package does
/// not require the target file to exist.
fn path_compare_key(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap_or_else(|_| bt_path::normalize_path(path));
    let text = bt_path::path_text(&bt_path::normalize_path(path));
    if cfg!(windows) {
        text.to_ascii_lowercase()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Creates a unique temporary directory.
    fn temp_root(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        std::env::temp_dir().join(format!("bt_ext_cli_{}_{}", name, millis))
    }

    /// A pure BT scaffold should complete the new/build/check/install flow.
    #[test]
    fn creates_builds_checks_and_installs_bt_extension() {
        let root = temp_root("bt_flow");
        let project = root.join("calc_cli");
        let output = root.join("calc_cli.bts");
        fs::create_dir_all(&root).unwrap();

        handle_new(&[project.to_string_lossy().to_string()]).unwrap();
        handle_build(&[
            project.to_string_lossy().to_string(),
            "-o".to_string(),
            output.to_string_lossy().to_string(),
        ])
        .unwrap();
        assert!(output.is_file());
        handle_check(&[output.to_string_lossy().to_string()]).unwrap();
        handle_install(&[
            output.to_string_lossy().to_string(),
            root.to_string_lossy().to_string(),
        ])
        .unwrap();
        assert!(root
            .join("extensions")
            .join("calc_cli")
            .join("calc_cli-1.0.0.bts")
            .is_file());

        let _ = fs::remove_dir_all(root);
    }

    /// The official remote-install download progress line should retain a stable user-facing format.
    #[test]
    fn formats_remote_install_download_progress() {
        let line = format_download_progress_line(720_319, 720_319, Duration::from_millis(600));
        let bar = "█".repeat(REGISTRY_INSTALL_PROGRESS_WIDTH);
        assert!(line.contains(&format!("[{}]", bar)));
        assert!(line.contains("720.3KB / 720.3KB  100%"));
        assert!(line.ends_with("1.2MB/s"));
    }

    /// Repackaging should skip and overwrite an existing same-name output inside the development directory.
    #[test]
    fn build_skips_existing_output_inside_project_dir() {
        let root = temp_root("existing_output");
        let project = root.join("calc_existing");
        fs::create_dir_all(&root).unwrap();
        handle_new(&[project.to_string_lossy().to_string()]).unwrap();
        let output = project.join("calc_existing.bts");
        let old_output = std::fs::File::create(&output).unwrap();
        old_output
            .set_len(MAX_PACKAGE_ENTRY_BYTES.saturating_add(1))
            .unwrap();

        handle_build(&[
            project.to_string_lossy().to_string(),
            "-o".to_string(),
            output.to_string_lossy().to_string(),
        ])
        .unwrap();
        assert!(output.is_file());
        assert!(output.metadata().unwrap().len() < MAX_PACKAGE_ENTRY_BYTES);
        handle_check(&[output.to_string_lossy().to_string()]).unwrap();

        let _ = fs::remove_dir_all(root);
    }

    /// Invalid extension names should be rejected during scaffolding.
    #[test]
    fn rejects_invalid_new_extension_name() {
        let root = temp_root("bad_name");
        let project = root.join("CalcBad");
        let err = handle_new(&[project.to_string_lossy().to_string()]).unwrap_err();
        assert!(err.contains("may contain only lowercase letters"));
        let _ = fs::remove_dir_all(root);
    }

    /// A WASM scaffold should generate a Rust SDK project skeleton.
    #[test]
    fn creates_wasm_sdk_scaffold() {
        let root = temp_root("wasm_scaffold");
        let project = root.join("calc_wasm_sdk");
        fs::create_dir_all(&root).unwrap();

        handle_new(&[
            project.to_string_lossy().to_string(),
            "--kind".to_string(),
            "wasm".to_string(),
        ])
        .unwrap();

        let cargo = fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(cargo.contains("bt-extension-sdk"));
        let source = fs::read_to_string(project.join("src").join("lib.rs")).unwrap();
        assert!(source.contains("bt_extension!"));
        assert!(project.join("README.md").is_file());

        let _ = fs::remove_dir_all(root);
    }

    /// Packaging a shared WASM extension should use the shared-module validation backend.
    #[test]
    fn builds_shared_wasm_extension_package() {
        let root = temp_root("shared_wasm_build");
        let project = root.join("shared_answer");
        let output = root.join("shared_answer.bts");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("manifest.json"),
            r#"{
                "format": "bts",
                "format_version": 1,
                "name": "shared_answer",
                "version": "1.0.0",
                "kind": "wasm",
                "abi": "bts-wasi-1",
                "bt_min_version": "1.1.1",
                "api_version": 1,
                "entry": "module.wasm",
                "bindings": "bindings.json",
                "permissions": [],
                "runtime": {
                    "mode": "shared",
                    "workers": 1,
                    "queue_limit": 4,
                    "call_timeout_ms": 1000,
                    "idle_ttl_ms": 300000,
                    "max_objects": 16,
                    "max_worker_objects": 16,
                    "max_inflight_calls": 4
                }
            }"#,
        )
        .unwrap();
        fs::write(
            project.join("bindings.json"),
            r#"{
                "api_version": 1,
                "functions": [
                    {
                        "name": "answer",
                        "id": 1,
                        "params": [],
                        "returns": "int"
                    }
                ],
                "objects": []
            }"#,
        )
        .unwrap();
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "bts_alloc") (param $len i32) (result i32)
                    i32.const 0
                )
                (func (export "bts_free") (param i32) (param i32))
                (func (export "bts_call") (param i32) (param i32) (param i32) (result i64)
                    i64.const 0
                )
            )
            "#,
        )
        .unwrap();
        fs::write(project.join("module.wasm"), wasm).unwrap();

        handle_build(&[
            project.to_string_lossy().to_string(),
            "-o".to_string(),
            output.to_string_lossy().to_string(),
        ])
        .unwrap();
        assert!(output.is_file());
        handle_check(&[output.to_string_lossy().to_string()]).unwrap();

        let _ = fs::remove_dir_all(root);
    }

    /// Packaging a development directory should skip Cargo build artifacts.
    #[test]
    fn build_collection_skips_target_directory() {
        let root = temp_root("skip_target");
        let project = root.join("calc_skip");
        fs::create_dir_all(&root).unwrap();
        handle_new(&[project.to_string_lossy().to_string()]).unwrap();
        let target_dir = project.join("target").join("debug");
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(target_dir.join("junk.bin"), [1_u8, 2, 3]).unwrap();

        let project_package = read_project_package(&project, None).unwrap();
        assert!(!project_package
            .files
            .iter()
            .any(|entry| entry.package_path.starts_with("target/")));

        let _ = fs::remove_dir_all(root);
    }

    /// Package output must use the .bts suffix.
    #[test]
    fn build_rejects_non_bts_output() {
        let root = temp_root("bad_output");
        let project = root.join("calc_out");
        fs::create_dir_all(&root).unwrap();
        handle_new(&[project.to_string_lossy().to_string()]).unwrap();
        let err = handle_build(&[
            project.to_string_lossy().to_string(),
            "-o".to_string(),
            root.join("calc.zip").to_string_lossy().to_string(),
        ])
        .unwrap_err();
        assert!(err.contains(".bts"));
        let _ = fs::remove_dir_all(root);
    }
}
