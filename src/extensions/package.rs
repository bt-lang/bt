//! `.bts` extension package loading and zip safety boundaries.

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use crate::extensions::bindings::ExtensionBindings;
use crate::extensions::manifest::{ExtensionKind, ExtensionManifest};
use crate::extensions::{is_safe_package_path, PACKAGE_EXTENSION};

/// Maximum byte size for a single `.bts` file.
pub const MAX_PACKAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum number of entries allowed in a single `.bts` package.
pub const MAX_PACKAGE_ENTRIES: usize = 256;
/// Maximum uncompressed size for a single file inside a package.
pub const MAX_PACKAGE_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum total uncompressed size for a single `.bts` package.
pub const MAX_PACKAGE_TOTAL_UNCOMPRESSED_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum byte size for manifest and bindings description files.
pub const MAX_DESCRIPTOR_BYTES: u64 = 1024 * 1024;
/// Maximum byte size for pure BT extension entry source.
pub const MAX_BT_ENTRY_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum byte size for a WASM extension entry module.
pub const MAX_WASM_ENTRY_BYTES: u64 = 32 * 1024 * 1024;

/// Parsed extension package.
#[derive(Debug, Clone)]
pub struct ExtensionPackage {
    /// Local package path.
    pub path: PathBuf,
    /// Validated manifest metadata.
    pub manifest: ExtensionManifest,
    /// Validated bindings metadata.
    pub bindings: ExtensionBindings,
    /// Pure BT extension entry source; non-BT backends do not read entry bodies.
    pub entry_source: Option<String>,
    /// WASM extension entry module binary; non-WASM backends do not read entry bodies.
    pub entry_wasm: Option<Vec<u8>>,
    /// Metadata for regular file entries inside the package.
    pub files: Vec<PackageFileEntry>,
}

/// Metadata for a file entry inside the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFileEntry {
    /// Safe relative path inside the package.
    pub path: String,
    /// Uncompressed byte count recorded in the zip.
    pub uncompressed_size: u64,
    /// Compressed byte count recorded in the zip.
    pub compressed_size: u64,
}

impl ExtensionPackage {
    /// Read, validate, and parse extension package metadata from a `.bts` file.
    pub fn read(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        validate_package_file_path(path)?;
        let metadata = fs::metadata(path)
            .map_err(|err| format!("Failed to read extension package metadata: {}", err))?;
        if !metadata.is_file() {
            return Err(format!(
                "Extension package `{}` is not a regular file",
                path.display()
            ));
        }
        if metadata.len() > MAX_PACKAGE_FILE_BYTES {
            return Err(format!(
                "Extension package `{}` exceeds {} bytes",
                path.display(),
                MAX_PACKAGE_FILE_BYTES
            ));
        }

        let file =
            File::open(path).map_err(|err| format!("Failed to open extension package: {}", err))?;
        let mut archive =
            ZipArchive::new(file).map_err(|err| format!("Failed to read .bts zip: {}", err))?;
        let (files, raw_names) = scan_zip_entries(&mut archive)?;

        let manifest_raw_name = raw_names
            .get("manifest.json")
            .ok_or_else(|| "Extension package is missing manifest.json".to_string())?;
        let manifest_raw = read_archive_entry_string(
            &mut archive,
            manifest_raw_name,
            MAX_DESCRIPTOR_BYTES,
            "manifest.json",
        )?;
        let manifest = ExtensionManifest::parse(&manifest_raw)?;

        if manifest.entry == manifest.bindings {
            return Err(
                "manifest.entry cannot point to the same file as manifest.bindings".to_string(),
            );
        }
        let entry_raw_name = raw_names.get(&manifest.entry).ok_or_else(|| {
            format!(
                "Extension package is missing manifest.entry target `{}`",
                manifest.entry
            )
        })?;
        let bindings_raw_name = raw_names.get(&manifest.bindings).ok_or_else(|| {
            format!(
                "Extension package is missing manifest.bindings target `{}`",
                manifest.bindings
            )
        })?;
        let bindings_raw = read_archive_entry_string(
            &mut archive,
            bindings_raw_name,
            MAX_DESCRIPTOR_BYTES,
            &manifest.bindings,
        )?;
        let bindings = ExtensionBindings::parse(&bindings_raw, &manifest)?;
        let entry_source = if manifest.kind == ExtensionKind::Bt {
            Some(read_archive_entry_string(
                &mut archive,
                entry_raw_name,
                MAX_BT_ENTRY_SOURCE_BYTES,
                &manifest.entry,
            )?)
        } else {
            None
        };
        let entry_wasm = if manifest.kind == ExtensionKind::Wasm {
            Some(read_archive_entry_bytes(
                &mut archive,
                entry_raw_name,
                MAX_WASM_ENTRY_BYTES,
                &manifest.entry,
            )?)
        } else {
            None
        };

        Ok(Self {
            path: path.to_path_buf(),
            manifest,
            bindings,
            entry_source,
            entry_wasm,
            files,
        })
    }

    /// Check whether the package contains the given safe relative path.
    pub fn has_file(&self, package_path: &str) -> bool {
        self.files.iter().any(|file| file.path == package_path)
    }
}

/// Validate the package file path and suffix.
fn validate_package_file_path(path: &Path) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case(PACKAGE_EXTENSION) {
        return Err(format!(
            "Extension package `{}` must use the .{} suffix",
            path.display(),
            PACKAGE_EXTENSION
        ));
    }
    Ok(())
}

/// Scan zip entries and validate count, path, and size limits.
fn scan_zip_entries(
    archive: &mut ZipArchive<File>,
) -> Result<(Vec<PackageFileEntry>, HashMap<String, String>), String> {
    if archive.len() > MAX_PACKAGE_ENTRIES {
        return Err(format!(
            "Package entry count exceeds {}",
            MAX_PACKAGE_ENTRIES
        ));
    }
    let mut total_uncompressed = 0_u64;
    let mut files = Vec::with_capacity(archive.len());
    let mut raw_names = HashMap::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|err| format!("Failed to read zip entry: {}", err))?;
        let is_dir = entry.is_dir();
        let raw_name = entry.name().to_string();
        let Some(path) = normalize_zip_entry_path(&raw_name, is_dir)? else {
            continue;
        };
        if raw_names.insert(path.clone(), raw_name).is_some() {
            return Err(format!(
                "Extension package contains a duplicate entry `{}`",
                path
            ));
        }
        let uncompressed_size = entry.size();
        if uncompressed_size > MAX_PACKAGE_ENTRY_BYTES {
            return Err(format!(
                "Package entry `{}` exceeds {} bytes after decompression",
                path, MAX_PACKAGE_ENTRY_BYTES
            ));
        }
        total_uncompressed = total_uncompressed
            .checked_add(uncompressed_size)
            .ok_or_else(|| "Package total uncompressed size overflow".to_string())?;
        if total_uncompressed > MAX_PACKAGE_TOTAL_UNCOMPRESSED_BYTES {
            return Err(format!(
                "Package total uncompressed size exceeds {} bytes",
                MAX_PACKAGE_TOTAL_UNCOMPRESSED_BYTES
            ));
        }
        files.push(PackageFileEntry {
            path,
            uncompressed_size,
            compressed_size: entry.compressed_size(),
        });
    }
    Ok((files, raw_names))
}

/// Normalize a zip entry path; directory entries return empty.
fn normalize_zip_entry_path(raw_name: &str, is_dir: bool) -> Result<Option<String>, String> {
    let path = if is_dir {
        raw_name.trim_end_matches('/')
    } else {
        raw_name
    };
    if path.is_empty() {
        return Err("Extension package contains an empty path entry".to_string());
    }
    if !is_safe_package_path(path) {
        return Err(format!(
            "Extension package entry `{}` is not a safe relative path",
            raw_name
        ));
    }
    if is_dir {
        return Ok(None);
    }
    Ok(Some(path.to_string()))
}

/// Read the specified text entry from the zip archive.
fn read_archive_entry_string(
    archive: &mut ZipArchive<File>,
    raw_name: &str,
    max_bytes: u64,
    display_name: &str,
) -> Result<String, String> {
    let mut entry = archive
        .by_name(raw_name)
        .map_err(|err| format!("Failed to read `{}`: {}", display_name, err))?;
    if entry.size() > max_bytes {
        return Err(format!("`{}` exceeds {} bytes", display_name, max_bytes));
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Failed to read `{}` contents: {}", display_name, err))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "`{}` exceeds {} bytes after reading",
            display_name, max_bytes
        ));
    }
    String::from_utf8(bytes).map_err(|err| format!("`{}` is not UTF-8 text: {}", display_name, err))
}

/// Read the specified binary entry from the zip archive.
fn read_archive_entry_bytes(
    archive: &mut ZipArchive<File>,
    raw_name: &str,
    max_bytes: u64,
    display_name: &str,
) -> Result<Vec<u8>, String> {
    let mut entry = archive
        .by_name(raw_name)
        .map_err(|err| format!("Failed to read `{}`: {}", display_name, err))?;
    if entry.size() > max_bytes {
        return Err(format!("`{}` exceeds {} bytes", display_name, max_bytes));
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Failed to read `{}` contents: {}", display_name, err))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "`{}` exceeds {} bytes after reading",
            display_name, max_bytes
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Return valid manifest JSON.
    fn valid_manifest_json() -> &'static str {
        r#"{
            "format": "bts",
            "format_version": 1,
            "name": "calc",
            "version": "1.0.0",
            "kind": "bt",
            "abi": "bts-bt-1",
            "bt_min_version": "1.1.0",
            "api_version": 1,
            "entry": "src/lib.bt",
            "bindings": "bindings.json",
            "permissions": []
        }"#
    }

    /// Return valid calc bindings JSON.
    fn valid_bindings_json() -> &'static str {
        r#"{
            "api_version": 1,
            "functions": [
                {
                    "name": "calc",
                    "id": 1,
                    "params": [
                        { "name": "value", "type": "int" }
                    ],
                    "returns": "Calc"
                }
            ],
            "objects": [
                {
                    "name": "Calc",
                    "type_id": 1,
                    "methods": [
                        {
                            "name": "add",
                            "id": 2,
                            "params": [
                                { "name": "value", "type": "int" }
                            ],
                            "returns": "Calc"
                        },
                        {
                            "name": "value",
                            "id": 3,
                            "params": [],
                            "returns": "int"
                        }
                    ]
                }
            ]
        }"#
    }

    /// Generate a temporary `.bts` file path.
    fn temp_package_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        std::env::temp_dir().join(format!("bt_{}_{}.bts", name, millis))
    }

    /// Write a test zip extension package.
    fn write_test_package(path: &Path, entries: &[(&str, &str)]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }

    /// A valid package should load manifest, bindings, and entry metadata.
    #[test]
    fn reads_valid_package() {
        let path = temp_package_path("valid");
        write_test_package(
            &path,
            &[
                ("manifest.json", valid_manifest_json()),
                ("bindings.json", valid_bindings_json()),
                ("src/lib.bt", "fn calc(value) { value }"),
            ],
        );

        let package = ExtensionPackage::read(&path).unwrap();
        assert_eq!(package.manifest.name, "calc");
        assert!(package.has_file("src/lib.bt"));
        let _ = fs::remove_file(path);
    }

    /// Path traversal inside the package should be rejected.
    #[test]
    fn rejects_path_traversal_entry() {
        let path = temp_package_path("traversal");
        write_test_package(
            &path,
            &[
                ("manifest.json", valid_manifest_json()),
                ("bindings.json", valid_bindings_json()),
                ("../evil.txt", "bad"),
                ("src/lib.bt", "fn calc(value) { value }"),
            ],
        );

        let err = ExtensionPackage::read(&path).unwrap_err();
        assert!(err.contains("safe relative path"));
        let _ = fs::remove_file(path);
    }

    /// Missing manifest entry targets should fail.
    #[test]
    fn rejects_missing_manifest_entry() {
        let path = temp_package_path("missing_entry");
        write_test_package(
            &path,
            &[
                ("manifest.json", valid_manifest_json()),
                ("bindings.json", valid_bindings_json()),
            ],
        );

        let err = ExtensionPackage::read(&path).unwrap_err();
        assert!(err.contains("manifest.entry"));
        let _ = fs::remove_file(path);
    }
}
