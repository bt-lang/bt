use crate::app::config::{create_default_app_json_for_index, load_app_json_from_path, AppJson};
use crate::bundle::footer::{inject_bundle, read_bundle_from_exe};
use crate::bundle::package::{build_btr, BtrPackage, BTR_EXTENSION};
use crate::bundle::vfs::VirtualFileSystem;
use crate::error::BtError;
use std::env;
use std::fs;
#[cfg(windows)]
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Builds the BT desktop app in the current directory as a single executable.
pub fn build_project() -> Result<(), BtError> {
    let project_dir = env::current_dir()?;
    let config = load_build_config(&project_dir)?;
    validate_output_name(&config.app.name)?;
    let icon_path = crate::app::icon::build_icon_path(&project_dir, &config)?;
    let association_icon_paths =
        crate::app::icon::build_file_association_icon_paths(&project_dir, &config)?;
    let built = build_btr(&project_dir, &config)?;

    let dist_dir = project_dir.join("dist");
    fs::create_dir_all(&dist_dir)?;
    let output = dist_dir.join(format!("{}.exe", config.app.name));
    let temp_output = build_sidecar_path(&dist_dir, &config.app.name, "tmp");
    let backup_output = build_sidecar_path(&dist_dir, &config.app.name, "old");
    let runtime_exe = env::current_exe()?;

    cleanup_build_file(&temp_output, "Failed to remove stale temporary file")?;
    cleanup_build_file(&backup_output, "Failed to remove stale backup file")?;
    let file_count = match build_output_exe(
        &runtime_exe,
        &temp_output,
        &config,
        icon_path.as_deref(),
        &association_icon_paths,
        &built.bytes,
        built.file_names.len(),
    ) {
        Ok(file_count) => file_count,
        Err(err) => {
            let _ = fs::remove_file(&temp_output);
            return Err(err);
        }
    };
    if let Err(err) = replace_build_output(&temp_output, &output, &backup_output) {
        let _ = fs::remove_file(&temp_output);
        return Err(err);
    }

    println!("BT desktop app packaging complete");
    println!("Application name: {}", config.app.name);
    println!("Output file: {}", output.display());
    println!("BTR file count: {}", file_count);
    println!("BTR size: {} bytes", built.bytes.len());

    Ok(())
}

/// Builds and validates a complete temporary output executable.
fn build_output_exe(
    runtime_exe: &Path,
    output: &Path,
    config: &AppJson,
    icon_path: Option<&Path>,
    association_icon_paths: &[(u16, PathBuf)],
    btr: &[u8],
    expected_file_count: usize,
) -> Result<usize, BtError> {
    fs::copy(runtime_exe, output)?;
    if !config.dev.console {
        set_windows_gui_subsystem(output)?;
    }
    apply_windows_exe_resources(output, config, icon_path, association_icon_paths)?;
    inject_bundle(output, btr)?;

    let bytes = read_bundle_from_exe(output)?
        .ok_or_else(|| BtError::Bundle("Failed to read BTR after injection".to_string()))?;
    let package = BtrPackage::from_bytes(bytes, output.to_path_buf())?;
    let file_count = package.list().len();
    if file_count != expected_file_count {
        return Err(BtError::Bundle(format!(
            "BTR write validation failed: built {} files, read {} files",
            expected_file_count, file_count
        )));
    }
    Ok(file_count)
}

/// Packages the current directory as a standalone BTR file without the bt-app runtime.
pub fn pack_project() -> Result<(), BtError> {
    let project_dir = env::current_dir()?;
    let config = load_build_config(&project_dir)?;
    validate_output_name(&config.app.name)?;
    let built = build_btr(&project_dir, &config)?;
    let dist_dir = project_dir.join("dist");
    fs::create_dir_all(&dist_dir)?;
    let output = dist_dir.join(format!("{}.{}", config.app.name, BTR_EXTENSION));
    let temp_output = package_sidecar_path(&dist_dir, &config.app.name, "tmp");
    let backup_output = package_sidecar_path(&dist_dir, &config.app.name, "old");

    cleanup_build_file(&temp_output, "Failed to remove stale BTR temporary file")?;
    cleanup_build_file(&backup_output, "Failed to remove stale BTR backup file")?;
    fs::write(&temp_output, &built.bytes)?;
    let verified = BtrPackage::open(&temp_output)?;
    if verified.list().len() != built.file_names.len() {
        let _ = fs::remove_file(&temp_output);
        return Err(BtError::Bundle(
            "BTR build verification file count mismatch".to_string(),
        ));
    }
    if let Err(err) = replace_build_output(&temp_output, &output, &backup_output) {
        let _ = fs::remove_file(&temp_output);
        return Err(err);
    }

    println!("BT desktop app BTR packaging complete");
    println!("Application name: {}", config.app.name);
    println!("Application ID: {}", config.app.id);
    println!("Output file: {}", output.display());
    println!("BTR file count: {}", built.file_names.len());
    println!("BTR size: {} bytes", built.bytes.len());
    Ok(())
}

/// Reads and prints metadata for a BTR package without executing any of its scripts.
pub fn info_btr(path: &Path) -> Result<(), BtError> {
    let package = BtrPackage::open(path)?;
    let config = package.config();
    println!("BT desktop app BTR");
    println!("Path: {}", package.path().display());
    println!("Format version: {}", package.manifest().format_version);
    println!("BT build version: {}", package.manifest().bt_version);
    println!("Minimum BT version: {}", package.manifest().bt_min_version);
    println!("Application ID: {}", config.app.id);
    println!("Application name: {}", config.app.name);
    println!("Window title: {}", config.app.title);
    println!("Application version: {}", config.app.version);
    println!("Run mode: {}", config.app.mode);
    println!("File count: {}", package.list().len());
    println!("BTR size: {} bytes", package.package_bytes());
    println!("Uncompressed size: {} bytes", package.uncompressed_bytes());
    Ok(())
}

/// Windows packages require PE icon and version resources.
#[cfg(windows)]
fn apply_windows_exe_resources(
    output: &Path,
    config: &AppJson,
    icon_path: Option<&Path>,
    association_icon_paths: &[(u16, PathBuf)],
) -> Result<(), BtError> {
    crate::app::icon::apply_exe_icons(output, icon_path, association_icon_paths)?;
    crate::app::metadata::apply_exe_metadata(output, config)
}

/// Non-Windows platforms skip PE-specific resources and their Windows-only writer.
#[cfg(not(windows))]
fn apply_windows_exe_resources(
    _output: &Path,
    _config: &AppJson,
    _icon_path: Option<&Path>,
    _association_icon_paths: &[(u16, PathBuf)],
) -> Result<(), BtError> {
    Ok(())
}

/// Replaces the final output with the validated temporary executable.
///
/// Windows cannot overwrite a running executable. Move the old output to a backup before moving
/// the new one; if that fails, restore the old output when possible to avoid a partial result.
fn replace_build_output(temp: &Path, output: &Path, backup: &Path) -> Result<(), BtError> {
    let had_output = output.exists();
    if had_output {
        fs::rename(output, backup).map_err(|err| {
            build_file_error(
                "Failed to back up old executable before replacement",
                output,
                err,
            )
        })?;
    }

    match fs::rename(temp, output) {
        Ok(()) => {
            if had_output {
                let _ = fs::remove_file(backup);
            }
            Ok(())
        }
        Err(err) => {
            if had_output {
                let _ = fs::rename(backup, output);
            }
            Err(build_file_error(
                "Failed to write final output file",
                output,
                err,
            ))
        }
    }
}

/// Generates a sidecar file path for this build.
fn build_sidecar_path(dist_dir: &Path, app_name: &str, suffix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    dist_dir.join(format!(
        ".{}.exe.bt-build-{}-{}.{}",
        app_name,
        std::process::id(),
        stamp,
        suffix
    ))
}

/// Generates a sidecar file path for a BTR build.
fn package_sidecar_path(dist_dir: &Path, app_name: &str, suffix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    dist_dir.join(format!(
        ".{}.bt-build-{}-{}.{}.btr",
        app_name,
        std::process::id(),
        stamp,
        suffix
    ))
}

/// Removes a build sidecar, including its path in any cleanup error.
fn cleanup_build_file(path: &Path, context: &str) -> Result<(), BtError> {
    if path.exists() {
        fs::remove_file(path).map_err(|err| build_file_error(context, path, err))?;
    }
    Ok(())
}

/// Creates a package file-operation error with guidance when the target is in use.
fn build_file_error(context: &str, path: &Path, err: std::io::Error) -> BtError {
    let hint = if err.raw_os_error() == Some(32) {
        "; the target file may be running. Close it and try again"
    } else {
        ""
    };
    BtError::Bundle(format!("{}: {}{}: {}", context, path.display(), hint, err))
}

/// Loads package configuration, preferring app.json or generating it when only index.html exists.
fn load_build_config(project_dir: &Path) -> Result<AppJson, BtError> {
    let app_json_path = project_dir.join("app.json");
    if app_json_path.is_file() {
        return load_app_json_from_path(&app_json_path);
    }

    let index_html_path = project_dir.join("index.html");
    if index_html_path.is_file() {
        return create_default_app_json_for_index(project_dir);
    }

    Err(BtError::Config(format!(
        "The current directory contains neither app.json nor index.html: {}",
        project_dir.display()
    )))
}

/// Sets the packaged Windows executable to the GUI subsystem when `dev.console` is false.
#[cfg(windows)]
fn set_windows_gui_subsystem(path: &Path) -> Result<(), BtError> {
    const PE_SIGNATURE: &[u8; 4] = b"PE\0\0";
    const SUBSYSTEM_WINDOWS_GUI: u16 = 2;
    const DOS_PE_OFFSET: u64 = 0x3c;
    const OPTIONAL_HEADER_OFFSET_FROM_PE: u64 = 24;
    const SUBSYSTEM_OFFSET_IN_OPTIONAL_HEADER: u64 = 68;

    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(DOS_PE_OFFSET))?;
    let pe_offset = read_u32(&mut file)? as u64;

    file.seek(SeekFrom::Start(pe_offset))?;
    let mut signature = [0u8; 4];
    file.read_exact(&mut signature)?;
    if &signature != PE_SIGNATURE {
        return Err(BtError::Bundle(format!(
            "Output file is not a valid PE executable: {}",
            path.display()
        )));
    }

    file.seek(SeekFrom::Start(
        pe_offset + OPTIONAL_HEADER_OFFSET_FROM_PE + SUBSYSTEM_OFFSET_IN_OPTIONAL_HEADER,
    ))?;
    file.write_all(&SUBSYSTEM_WINDOWS_GUI.to_le_bytes())?;
    Ok(())
}

/// Non-Windows platforms do not need PE subsystem changes.
#[cfg(not(windows))]
fn set_windows_gui_subsystem(_path: &Path) -> Result<(), BtError> {
    Ok(())
}

/// Reads a little-endian `u32` from the file's current position.
#[cfg(windows)]
fn read_u32(file: &mut fs::File) -> Result<u32, BtError> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Checks the current executable for a bundle and prints its file list.
pub fn bundle_check() -> Result<(), BtError> {
    let current_exe = env::current_exe()?;
    let Some(bytes) = read_bundle_from_exe(&current_exe)? else {
        println!("No bundle detected");
        return Ok(());
    };

    println!("Bundle detected");
    println!("Files:");
    if BtrPackage::has_zip_header(&bytes) {
        let package = BtrPackage::from_bytes(bytes, current_exe)?;
        for name in package.list() {
            println!("- {}", name);
        }
    } else {
        let vfs = VirtualFileSystem::from_bundle(&bytes)?;
        for name in vfs.list() {
            println!("- {}", name);
        }
    }
    Ok(())
}

/// Placeholder entry point for platform exports.
pub fn export_project(platform: &str) -> Result<(), BtError> {
    println!(
        "bt-app export: platform export support will be added later; target platform: {}",
        platform
    );
    Ok(())
}

/// Validates the output executable name so `app.name` cannot be a path.
fn validate_output_name(name: &str) -> Result<(), BtError> {
    if name.trim().is_empty() {
        return Err(BtError::Config("app.name cannot be empty".to_string()));
    }
    let path = Path::new(name);
    if path.components().count() != 1 || path.is_absolute() {
        return Err(BtError::Config(format!(
            "app.name must be a file name and cannot contain a path: {}",
            name
        )));
    }
    if name.chars().any(|ch| {
        matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control()
    }) {
        return Err(BtError::Config(format!(
            "app.name contains characters that are invalid in Windows file names: {}",
            name
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Build configuration writes a default app.json when only index.html exists.
    #[test]
    fn build_config_creates_default_app_json_for_index_html() {
        let dir = fresh_temp_dir("html-build-config");
        fs::write(
            dir.join("index.html"),
            r#"<title>HTML Package</title><script src="main.js"></script>"#,
        )
        .unwrap();

        let config = load_build_config(&dir).unwrap();

        assert_eq!(config.app.name, "BT-APP");
        assert_eq!(config.app.title, "BT-APP");
        assert_eq!(config.app.entry, "index.html");
        assert!(dir.join("app.json").is_file());
        assert!(dir.join("main.bt").is_file());
        assert_eq!(
            config.resources,
            vec![
                "app.json".to_string(),
                "index.html".to_string(),
                "assets/**".to_string()
            ]
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// app.json takes precedence over index.html to preserve explicit configuration.
    #[test]
    fn build_config_prefers_app_json() {
        let dir = fresh_temp_dir("app-json-build-config");
        fs::write(dir.join("app.json"), r#"{"app":{"name":"JsonDemo"}}"#).unwrap();
        fs::write(dir.join("index.html"), "<title>HTML</title>").unwrap();

        let config = load_build_config(&dir).unwrap();

        assert_eq!(config.app.name, "JsonDemo");

        let _ = fs::remove_dir_all(dir);
    }

    /// Output replacement backs up the old file before moving the built temporary file.
    #[test]
    fn replace_build_output_swaps_temp_into_final_path() {
        let dir = fresh_temp_dir("replace-output");
        let output = dir.join("Demo.exe");
        let temp = dir.join(".Demo.exe.tmp");
        let backup = dir.join(".Demo.exe.old");
        fs::write(&output, "old").unwrap();
        fs::write(&temp, "new").unwrap();

        replace_build_output(&temp, &output, &backup).unwrap();

        assert_eq!(fs::read_to_string(&output).unwrap(), "new");
        assert!(!temp.exists());
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(dir);
    }

    /// A failed temporary-file move restores the old output at the final path.
    #[test]
    fn replace_build_output_restores_old_output_when_new_file_missing() {
        let dir = fresh_temp_dir("restore-output");
        let output = dir.join("Demo.exe");
        let temp = dir.join(".missing.tmp");
        let backup = dir.join(".Demo.exe.old");
        fs::write(&output, "old").unwrap();

        let err = replace_build_output(&temp, &output, &backup).unwrap_err();

        assert!(err
            .to_string()
            .contains("Failed to write final output file"));
        assert_eq!(fs::read_to_string(&output).unwrap(), "old");
        assert!(!backup.exists());
        let _ = fs::remove_dir_all(dir);
    }

    /// Creates a unique test directory.
    fn fresh_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "bt-app-build-test-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
