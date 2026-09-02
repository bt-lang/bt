//! Desktop API implementation for `bt.app`.

use crate::app::api::{absolute_path_text, map_error, required_text};
use crate::app::runtime::AppState;
use crate::bundle::package::{BtrPackage, MAX_BTR_ICON_BYTES};
use base64::Engine;
use serde::Serialize;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;
use url::Url;

/// Returns the current application version.
pub fn version(state: &AppState) -> Result<String, String> {
    let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
    Ok(runtime.config.app.version.clone())
}

/// Information about a BTR application that can be displayed and launched directly.
#[derive(Debug, Clone, Serialize)]
pub struct BtrAppInfo {
    /// Canonical absolute path to the BTR application file.
    pub path: String,
    /// Stable application identifier from app.json.
    pub id: String,
    /// Application name.
    pub name: String,
    /// Window title.
    pub title: String,
    /// Application version.
    pub version: String,
    /// Optional application description.
    pub description: Option<String>,
    /// Application run mode.
    pub mode: String,
    /// Application entry point.
    pub entry: String,
    /// Relative icon path configured in app.json.
    pub icon: Option<String>,
    /// ICO data URL suitable for `<img src>`; `None` when no icon is configured.
    pub icon_data_url: Option<String>,
    /// BTR container format version.
    pub format_version: u32,
    /// BT version used to build the BTR.
    pub bt_version: String,
    /// Minimum BT version capable of running the BTR.
    pub bt_min_version: String,
    /// Number of resource files in the BTR project.
    pub file_count: usize,
    /// Compressed size of the BTR in bytes.
    pub package_bytes: u64,
    /// Total uncompressed size of all BTR entries in bytes.
    pub uncompressed_bytes: u64,
}

/// A successfully spawned standalone BTR application process.
#[derive(Debug, Clone, Serialize)]
pub struct BtrRunResult {
    /// Child process ID; this only indicates that the process was created, not that the WebView finished loading.
    pub pid: u32,
    /// Canonical absolute path of the BTR being run.
    pub path: String,
    /// Stable application identifier from the BTR's app.json.
    pub id: String,
}

/// Returns the bt-app engine version.
pub fn engine_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Returns the current system platform.
pub fn platform() -> String {
    if cfg!(target_os = "windows") {
        "windows".to_string()
    } else if cfg!(target_os = "macos") {
        "macos".to_string()
    } else if cfg!(target_os = "linux") {
        "linux".to_string()
    } else {
        std::env::consts::OS.to_string()
    }
}

/// Opens a URL in the system's default browser.
pub fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    let url = required_text(url, "URL")?;
    let parsed = Url::parse(&url).map_err(|err| map_error("Parse URL", err))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("URL must start with http:// or https://".to_string());
    }
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|err| map_error("Open URL", err))
}

/// Opens a file or directory with the system's default application.
pub fn open_path(app: AppHandle, path: String) -> Result<(), String> {
    let path = required_text(path, "Path")?;
    app.opener()
        .open_path(path, None::<String>)
        .map_err(|err| map_error("Open path", err))
}

/// Reveals a file or directory in the system file manager.
pub fn reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    let path = PathBuf::from(required_text(path, "Path")?);
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|err| map_error("Reveal path", err))
}

/// Exits the application.
pub fn quit(app: AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}

/// Returns application arguments with runtime commands and the BTR path removed.
pub fn args(state: &AppState) -> Result<Vec<String>, String> {
    let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
    Ok(runtime.app_args.clone())
}

/// Reads BTR application information and its default icon without executing app.main, server.bt, or page scripts.
pub fn info(path: String) -> Result<BtrAppInfo, String> {
    let path = PathBuf::from(required_text(path, "BTR path")?);
    let path = path
        .canonicalize()
        .map_err(|err| map_error("Read BTR path", err))?;
    let package = BtrPackage::open(&path).map_err(|err| err.to_string())?;
    let config = package.config();
    let icon_data_url = match config.app.icon.as_deref() {
        Some(icon) if package.exists(icon) => {
            let bytes = package
                .read_limited(icon, MAX_BTR_ICON_BYTES)
                .map_err(|err| err.to_string())?;
            Some(format!(
                "data:image/x-icon;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            ))
        }
        _ => None,
    };
    Ok(BtrAppInfo {
        path: absolute_path_text(path)?,
        id: config.app.id.clone(),
        name: config.app.name.clone(),
        title: config.app.title.clone(),
        version: config.app.version.clone(),
        description: config.app.description.clone(),
        mode: config.app.mode.clone(),
        entry: config.app.entry.clone(),
        icon: config.app.icon.clone(),
        icon_data_url,
        format_version: package.manifest().format_version,
        bt_version: package.manifest().bt_version.clone(),
        bt_min_version: package.manifest().bt_min_version.clone(),
        file_count: package.list().len(),
        package_bytes: package.package_bytes(),
        uncompressed_bytes: package.uncompressed_bytes(),
    })
}

/// Launches a standalone BTR application process with the current bt-app runtime.
pub fn run(path: String, args: Vec<String>) -> Result<BtrRunResult, String> {
    validate_btr_run_args(&args)?;
    let path = PathBuf::from(required_text(path, "BTR path")?);
    let path = path
        .canonicalize()
        .map_err(|err| map_error("Read BTR path", err))?;
    let package = BtrPackage::open(&path).map_err(|err| err.to_string())?;
    let id = package.config().app.id.clone();
    let show_console = package.config().dev.console;
    let exe = std::env::current_exe().map_err(|err| map_error("Read bt-app path", err))?;
    let mut command = Command::new(exe);
    command
        .arg(crate::bt_app::INTERNAL_RUN_BTR_ARG)
        .arg(&path)
        .arg("--")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_btr_child_console(&mut command, show_console);
    let child = command
        .spawn()
        .map_err(|err| map_error("Launch BTR application", err))?;
    Ok(BtrRunResult {
        pid: child.id(),
        path: absolute_path_text(path)?,
        id,
    })
}

/// Validates the count and length limits of arguments passed to the BTR application.
fn validate_btr_run_args(args: &[String]) -> Result<(), String> {
    if args.len() > 256 {
        return Err("BTR launch arguments cannot exceed 256 entries".to_string());
    }
    if args.iter().any(|value| value.len() > 32_768) {
        return Err("Each BTR launch argument cannot exceed 32768 bytes".to_string());
    }
    Ok(())
}

/// Suppresses startup console flashing on Windows according to the target app.json.
#[cfg(windows)]
fn configure_btr_child_console(command: &mut Command, show_console: bool) {
    if show_console {
        return;
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

/// Non-Windows platforms require no additional child-process console configuration.
#[cfg(not(windows))]
fn configure_btr_child_console(_command: &mut Command, _show_console: bool) {}

/// Returns the user's Documents directory as resolved by the operating system.
///
/// The system API resolves this path, accounting for localized directory names, OneDrive, and
/// user redirection. Callers should not construct it manually from HOME or USERPROFILE.
pub fn documents_dir(app: &AppHandle) -> Result<String, String> {
    let directory = app
        .path()
        .document_dir()
        .map_err(|err| map_error("Read user Documents directory", err))?;
    absolute_path_text(directory)
}

#[cfg(test)]
mod tests {
    use super::validate_btr_run_args;

    /// BTR arguments accept ordinary arrays and reject excessive counts or item sizes.
    #[test]
    fn btr_run_arguments_are_bounded() {
        assert!(validate_btr_run_args(&["open.md".to_string()]).is_ok());
        assert!(validate_btr_run_args(&vec![String::new(); 257]).is_err());
        assert!(validate_btr_run_args(&["x".repeat(32_769)]).is_err());
    }
}
