//! Building, reading, and security validation for `.btr` desktop application packages.
//!
//! BTR uses ZIP as its outer container, with a runtime-generated `btr.json` and the project's
//! `app.json` at the root. Reading first scans every entry and limits the file count, individual file
//! size, and total expanded size. The protocol layer may then decompress resources on demand,
//! preventing uncontrolled allocation and path traversal.

use crate::app::config::{load_app_json_from_str, AppJson};
use crate::bundle::builder::collect_resource_files;
use crate::error::BtError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// BTR file extension without the leading dot.
pub const BTR_EXTENSION: &str = "btr";
/// Path of the BTR container descriptor.
pub const BTR_MANIFEST_PATH: &str = "btr.json";
/// Currently supported BTR format version.
pub const BTR_FORMAT_VERSION: u32 = 1;
/// Maximum compressed size of a BTR file in bytes.
pub const MAX_BTR_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum number of file entries in a BTR.
pub const MAX_BTR_ENTRIES: usize = 4096;
/// Maximum expanded size of one BTR resource in bytes.
pub const MAX_BTR_ENTRY_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum total expanded size of all resources in one BTR, in bytes.
pub const MAX_BTR_TOTAL_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
/// Maximum size of the BTR descriptor in bytes.
pub const MAX_BTR_DESCRIPTOR_BYTES: u64 = 1024 * 1024;
/// Maximum app.icon size that may be read and returned to the toolbar.
pub const MAX_BTR_ICON_BYTES: u64 = 16 * 1024 * 1024;

/// BTR container metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BtrManifest {
    /// Fixed format name, currently required to be `btr`.
    pub format: String,
    /// BTR container format version.
    pub format_version: u32,
    /// Version of the BT runtime that generated this package.
    pub bt_version: String,
    /// Minimum BT version capable of running this BTR.
    pub bt_min_version: String,
}

impl BtrManifest {
    /// Creates BTR metadata for the current runtime.
    fn current() -> Self {
        Self {
            format: BTR_EXTENSION.to_string(),
            format_version: BTR_FORMAT_VERSION,
            bt_version: env!("CARGO_PKG_VERSION").to_string(),
            bt_min_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Validates that the current runtime can read the BTR metadata.
    fn validate(&self) -> Result<(), BtError> {
        if self.format != BTR_EXTENSION {
            return Err(BtError::Bundle(format!(
                "BTR format must be `{}`; current value: {}",
                BTR_EXTENSION, self.format
            )));
        }
        if self.format_version != BTR_FORMAT_VERSION {
            return Err(BtError::Bundle(format!(
                "Unsupported BTR format version: {}; only {} is currently supported",
                self.format_version, BTR_FORMAT_VERSION
            )));
        }
        if self.bt_version.trim().is_empty() {
            return Err(BtError::Bundle(
                "BTR bt_version cannot be empty".to_string(),
            ));
        }
        if !version_at_least(env!("CARGO_PKG_VERSION"), &self.bt_min_version)? {
            return Err(BtError::Bundle(format!(
                "BTR requires at least BT {}; current runtime is {}",
                self.bt_min_version,
                env!("CARGO_PKG_VERSION")
            )));
        }
        Ok(())
    }
}

/// Result of building a BTR.
pub struct BuiltBtr {
    /// Complete ZIP container bytes.
    pub bytes: Vec<u8>,
    /// Project resource names, excluding the internal `btr.json`.
    pub file_names: Vec<String>,
}

/// A validated BTR package whose resources can be read on demand.
#[derive(Clone)]
pub struct BtrPackage {
    /// Read-only package state shared by cloned resource sources.
    inner: Arc<BtrPackageInner>,
}

/// Shared internal BTR state.
struct BtrPackageInner {
    /// Original package path displayed in errors and validation results.
    path: PathBuf,
    /// Parsed and validated container metadata.
    manifest: BtrManifest,
    /// Parsed and validated application configuration.
    config: AppJson,
    /// Index from safely normalized paths to original ZIP entry names.
    entries: BTreeMap<String, String>,
    /// Serializes access while reusing the ZIP central directory and compressed bytes to avoid reparsing each request.
    archive: Mutex<ZipArchive<BtrArchiveReader>>,
    /// Compressed package size in bytes.
    package_bytes: u64,
    /// Total expanded size of all entries in bytes.
    uncompressed_bytes: u64,
}

/// Seekable ZIP source: external BTRs reuse a file handle; executable trailers use shared memory.
enum BtrArchiveReader {
    /// Standalone `.btr` file that is not fully preloaded into memory.
    File(File),
    /// BTR bytes read from an executable trailer.
    Memory(Cursor<Arc<[u8]>>),
}

impl Read for BtrArchiveReader {
    /// Forwards a read request to the current source.
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.read(buffer),
            Self::Memory(cursor) => cursor.read(buffer),
        }
    }
}

impl Seek for BtrArchiveReader {
    /// Forwards a seek request to the current source.
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::File(file) => file.seek(position),
            Self::Memory(cursor) => cursor.seek(position),
        }
    }
}

impl std::fmt::Debug for BtrPackage {
    /// Produces concise debug output without resource contents.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtrPackage")
            .field("path", &self.inner.path)
            .field("manifest", &self.inner.manifest)
            .field("files", &self.inner.entries.len())
            .field("package_bytes", &self.inner.package_bytes)
            .field("uncompressed_bytes", &self.inner.uncompressed_bytes)
            .finish()
    }
}

impl BtrPackage {
    /// Reads and validates a `.btr` file from disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BtError> {
        let path = path.as_ref();
        validate_btr_file_path(path)?;
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(BtError::Bundle(format!(
                "BTR path is not a regular file: {}",
                path.display()
            )));
        }
        if metadata.len() > MAX_BTR_FILE_BYTES {
            return Err(BtError::Bundle(format!(
                "BTR file size exceeds {} bytes: {}",
                MAX_BTR_FILE_BYTES,
                path.display()
            )));
        }
        let file = File::open(path)?;
        let archive = ZipArchive::new(BtrArchiveReader::File(file))
            .map_err(|err| BtError::Bundle(format!("Failed to read BTR ZIP: {}", err)))?;
        Self::from_archive(archive, path.to_path_buf(), metadata.len())
    }

    /// Reads and validates BTR bytes from an executable trailer or test buffer.
    pub fn from_bytes(bytes: Vec<u8>, path: PathBuf) -> Result<Self, BtError> {
        if bytes.len() as u64 > MAX_BTR_FILE_BYTES {
            return Err(BtError::Bundle(format!(
                "BTR file size exceeds {} bytes",
                MAX_BTR_FILE_BYTES
            )));
        }
        let package_bytes = bytes.len() as u64;
        let shared: Arc<[u8]> = Arc::from(bytes);
        let archive = ZipArchive::new(BtrArchiveReader::Memory(Cursor::new(shared)))
            .map_err(|err| BtError::Bundle(format!("Failed to read BTR ZIP: {}", err)))?;
        Self::from_archive(archive, path, package_bytes)
    }

    /// Performs entry, descriptor, and app.json validation shared by both read sources.
    fn from_archive(
        mut archive: ZipArchive<BtrArchiveReader>,
        path: PathBuf,
        package_bytes: u64,
    ) -> Result<Self, BtError> {
        let (entries, uncompressed_bytes) = scan_entries(&mut archive)?;

        let manifest_raw = read_indexed_string(
            &mut archive,
            &entries,
            BTR_MANIFEST_PATH,
            MAX_BTR_DESCRIPTOR_BYTES,
        )?;
        let manifest: BtrManifest = serde_json::from_str(&manifest_raw)
            .map_err(|err| BtError::Bundle(format!("Failed to parse btr.json: {}", err)))?;
        manifest.validate()?;

        let app_raw =
            read_indexed_string(&mut archive, &entries, "app.json", MAX_BTR_DESCRIPTOR_BYTES)?;
        let config = load_app_json_from_str(&app_raw)?;

        Ok(Self {
            inner: Arc::new(BtrPackageInner {
                path,
                manifest,
                config,
                entries,
                archive: Mutex::new(archive),
                package_bytes,
                uncompressed_bytes,
            }),
        })
    }

    /// Returns whether a byte stream has a standard ZIP/BTR local file header.
    pub fn has_zip_header(bytes: &[u8]) -> bool {
        bytes.starts_with(b"PK\x03\x04")
    }

    /// Returns the validated app.json configuration.
    pub fn config(&self) -> &AppJson {
        &self.inner.config
    }

    /// Returns the BTR container metadata.
    pub fn manifest(&self) -> &BtrManifest {
        &self.inner.manifest
    }

    /// Returns the original package path.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Returns the compressed package size in bytes.
    pub fn package_bytes(&self) -> u64 {
        self.inner.package_bytes
    }

    /// Returns the total expanded size of all entries in bytes.
    pub fn uncompressed_bytes(&self) -> u64 {
        self.inner.uncompressed_bytes
    }

    /// Reads a resource at a safe relative path.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, BtError> {
        self.read_limited(path, MAX_BTR_ENTRY_BYTES)
    }

    /// Reads within a caller-supplied limit so small resources such as icons are not overallocated.
    pub fn read_limited(&self, path: &str, max_bytes: u64) -> Result<Vec<u8>, BtError> {
        validate_entry_path(path)?;
        let max_bytes = max_bytes.min(MAX_BTR_ENTRY_BYTES);
        let raw_name = self
            .inner
            .entries
            .get(path)
            .ok_or_else(|| BtError::Bundle(format!("BTR resource does not exist: {}", path)))?
            .clone();
        let mut archive = self
            .inner
            .archive
            .lock()
            .map_err(|_| BtError::Runtime("BTR read lock is poisoned".to_string()))?;
        let mut entry = archive.by_name(&raw_name).map_err(|err| {
            BtError::Bundle(format!("Failed to read BTR resource `{}`: {}", path, err))
        })?;
        if entry.size() > max_bytes {
            return Err(BtError::Bundle(format!(
                "Expanded size of BTR resource `{}` exceeds {} bytes",
                path, max_bytes
            )));
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .by_ref()
            .take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| {
                BtError::Bundle(format!("Failed to read BTR resource `{}`: {}", path, err))
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(BtError::Bundle(format!(
                "BTR resource `{}` exceeds {} bytes after reading",
                path, max_bytes
            )));
        }
        Ok(bytes)
    }

    /// Returns whether the package contains the specified resource.
    pub fn exists(&self, path: &str) -> bool {
        validate_entry_path(path).is_ok() && self.inner.entries.contains_key(path)
    }

    /// Lists project resources without exposing the internal `btr.json` to the application.
    pub fn list(&self) -> Vec<String> {
        self.inner
            .entries
            .keys()
            .filter(|name| name.as_str() != BTR_MANIFEST_PATH)
            .cloned()
            .collect()
    }
}

/// Builds BTR ZIP bytes from the resource rules in app.json.
pub fn build_btr(project_dir: &Path, config: &AppJson) -> Result<BuiltBtr, BtError> {
    let files = collect_resource_files(project_dir, config)?;
    if files.contains_key(BTR_MANIFEST_PATH) {
        return Err(BtError::Bundle(format!(
            "{} is reserved for BTR internals and cannot be used as a project resource",
            BTR_MANIFEST_PATH
        )));
    }
    if files.len().saturating_add(1) > MAX_BTR_ENTRIES {
        return Err(BtError::Bundle(format!(
            "BTR file entry count exceeds {}",
            MAX_BTR_ENTRIES
        )));
    }

    let cursor = Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let manifest = serde_json::to_vec_pretty(&BtrManifest::current())
        .map_err(|err| BtError::Bundle(format!("Failed to serialize btr.json: {}", err)))?;
    writer
        .start_file(BTR_MANIFEST_PATH, options)
        .map_err(zip_write_error)?;
    writer.write_all(&manifest)?;

    let mut file_names = Vec::with_capacity(files.len());
    let mut total_uncompressed = manifest.len() as u64;
    for (name, path) in files {
        let metadata = fs::metadata(&path)?;
        if config.app.icon.as_deref() == Some(name.as_str()) && metadata.len() > MAX_BTR_ICON_BYTES
        {
            return Err(BtError::Bundle(format!(
                "BTR app.icon `{}` exceeds {} bytes",
                name, MAX_BTR_ICON_BYTES
            )));
        }
        if metadata.len() > MAX_BTR_ENTRY_BYTES {
            return Err(BtError::Bundle(format!(
                "BTR resource `{}` exceeds {} bytes",
                name, MAX_BTR_ENTRY_BYTES
            )));
        }
        total_uncompressed = total_uncompressed
            .checked_add(metadata.len())
            .ok_or_else(|| BtError::Bundle("BTR total resource size overflowed".to_string()))?;
        if total_uncompressed > MAX_BTR_TOTAL_UNCOMPRESSED_BYTES {
            return Err(BtError::Bundle(format!(
                "Total expanded size of BTR resources exceeds {} bytes",
                MAX_BTR_TOTAL_UNCOMPRESSED_BYTES
            )));
        }
        writer.start_file(&name, options).map_err(zip_write_error)?;
        let mut file = File::open(&path)?;
        std::io::copy(&mut file, &mut writer)?;
        file_names.push(name);
    }
    let bytes = writer.finish().map_err(zip_write_error)?.into_inner();
    if bytes.len() as u64 > MAX_BTR_FILE_BYTES {
        return Err(BtError::Bundle(format!(
            "Compressed BTR size exceeds {} bytes",
            MAX_BTR_FILE_BYTES
        )));
    }
    Ok(BuiltBtr { bytes, file_names })
}

/// Validates an external BTR path and file extension.
fn validate_btr_file_path(path: &Path) -> Result<(), BtError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case(BTR_EXTENSION) {
        return Err(BtError::Bundle(format!(
            "Application packages must use the .{} extension: {}",
            BTR_EXTENSION,
            path.display()
        )));
    }
    Ok(())
}

/// Scans ZIP entries and builds a safe path index.
fn scan_entries(
    archive: &mut ZipArchive<BtrArchiveReader>,
) -> Result<(BTreeMap<String, String>, u64), BtError> {
    if archive.len() > MAX_BTR_ENTRIES {
        return Err(BtError::Bundle(format!(
            "BTR file entry count exceeds {}",
            MAX_BTR_ENTRIES
        )));
    }
    let mut entries = BTreeMap::new();
    let mut folded_paths = BTreeSet::new();
    let mut total_uncompressed = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|err| BtError::Bundle(format!("Failed to read BTR ZIP entry: {}", err)))?;
        if entry.is_dir() {
            continue;
        }
        if entry.encrypted() {
            return Err(BtError::Bundle(format!(
                "BTR does not support encrypted resources: {}",
                entry.name()
            )));
        }
        if !matches!(
            entry.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(BtError::Bundle(format!(
                "BTR resource `{}` uses an unsupported compression method",
                entry.name()
            )));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(BtError::Bundle(format!(
                "BTR does not allow symbolic-link resources: {}",
                entry.name()
            )));
        }
        let raw_name = entry.name().to_string();
        let name = normalize_entry_path(&raw_name)?;
        let folded = name.to_ascii_lowercase();
        if !folded_paths.insert(folded) {
            return Err(BtError::Bundle(format!(
                "BTR contains duplicate resources equivalent under Windows case rules: {}",
                name
            )));
        }
        if entry.size() > MAX_BTR_ENTRY_BYTES {
            return Err(BtError::Bundle(format!(
                "Expanded size of BTR resource `{}` exceeds {} bytes",
                name, MAX_BTR_ENTRY_BYTES
            )));
        }
        total_uncompressed = total_uncompressed
            .checked_add(entry.size())
            .ok_or_else(|| BtError::Bundle("BTR total expanded size overflowed".to_string()))?;
        if total_uncompressed > MAX_BTR_TOTAL_UNCOMPRESSED_BYTES {
            return Err(BtError::Bundle(format!(
                "BTR total expanded size exceeds {} bytes",
                MAX_BTR_TOTAL_UNCOMPRESSED_BYTES
            )));
        }
        if entries.insert(name.clone(), raw_name).is_some() {
            return Err(BtError::Bundle(format!(
                "BTR contains a duplicate resource: {}",
                name
            )));
        }
    }
    let names: Vec<&str> = folded_paths.iter().map(String::as_str).collect();
    for pair in names.windows(2) {
        if pair[1].starts_with(pair[0]) && pair[1].as_bytes().get(pair[0].len()) == Some(&b'/') {
            return Err(BtError::Bundle(format!(
                "BTR resource file and directory prefix conflict: {} and {}",
                pair[0], pair[1]
            )));
        }
    }
    if !entries.contains_key(BTR_MANIFEST_PATH) {
        return Err(BtError::Bundle(format!(
            "BTR is missing the internal descriptor {}",
            BTR_MANIFEST_PATH
        )));
    }
    if !entries.contains_key("app.json") {
        return Err(BtError::Bundle(
            "BTR is missing app.json and cannot start the desktop application".to_string(),
        ));
    }
    Ok((entries, total_uncompressed))
}

/// Reads a bounded UTF-8 descriptor from the entry index.
fn read_indexed_string(
    archive: &mut ZipArchive<BtrArchiveReader>,
    entries: &BTreeMap<String, String>,
    path: &str,
    max_bytes: u64,
) -> Result<String, BtError> {
    let raw_name = entries
        .get(path)
        .ok_or_else(|| BtError::Bundle(format!("BTR is missing {}", path)))?;
    let mut entry = archive
        .by_name(raw_name)
        .map_err(|err| BtError::Bundle(format!("Failed to read BTR `{}`: {}", path, err)))?;
    if entry.size() > max_bytes {
        return Err(BtError::Bundle(format!(
            "BTR `{}` exceeds {} bytes",
            path, max_bytes
        )));
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .by_ref()
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| BtError::Bundle(format!("Failed to read BTR `{}`: {}", path, err)))?;
    if bytes.len() as u64 > max_bytes {
        return Err(BtError::Bundle(format!(
            "BTR `{}` exceeds {} bytes after reading",
            path, max_bytes
        )));
    }
    String::from_utf8(bytes)
        .map_err(|err| BtError::Bundle(format!("BTR `{}` is not UTF-8: {}", path, err)))
}

/// Normalizes ZIP entry paths and rejects absolute paths, backslashes, and parent traversal.
fn normalize_entry_path(raw_name: &str) -> Result<String, BtError> {
    if raw_name.trim().is_empty() || raw_name.as_bytes().contains(&0) {
        return Err(BtError::Bundle(
            "BTR contains an empty resource path".to_string(),
        ));
    }
    if raw_name.len() > 512 || raw_name.contains('\\') {
        return Err(BtError::Bundle(format!(
            "BTR resource path is too long or does not use `/`: {}",
            raw_name
        )));
    }
    validate_entry_path(raw_name)?;
    let mut parts = Vec::new();
    for component in Path::new(raw_name).components() {
        if let Component::Normal(value) = component {
            let part = value.to_str().ok_or_else(|| {
                BtError::Bundle(format!("BTR resource path is not UTF-8: {}", raw_name))
            })?;
            validate_portable_path_segment(part, raw_name)?;
            parts.push(part);
        }
    }
    if parts.len() > 64 {
        return Err(BtError::Bundle(format!(
            "BTR resource path depth exceeds 64: {}",
            raw_name
        )));
    }
    Ok(parts.join("/"))
}

/// Validates that a path component cannot alias another path or device on supported platforms.
fn validate_portable_path_segment(segment: &str, full_path: &str) -> Result<(), BtError> {
    if segment.is_empty()
        || segment.len() > 255
        || segment.ends_with([' ', '.'])
        || segment
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return Err(BtError::Bundle(format!(
            "BTR resource path contains non-portable component `{}`: {}",
            segment, full_path
        )));
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let reserved = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if reserved {
        return Err(BtError::Bundle(format!(
            "BTR resource path uses reserved Windows device name `{}`: {}",
            segment, full_path
        )));
    }
    Ok(())
}

/// Validates that an in-package resource path is a safe relative path.
fn validate_entry_path(path: &str) -> Result<(), BtError> {
    if path.trim().is_empty() || path.as_bytes().contains(&0) {
        return Err(BtError::Bundle(
            "BTR resource path cannot be empty".to_string(),
        ));
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(BtError::Bundle(format!(
            "BTR does not allow absolute resource paths: {}",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "BTR does not allow unsafe resource paths: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Converts a ZIP write error into a standard bundle error.
fn zip_write_error(error: zip::result::ZipError) -> BtError {
    BtError::Bundle(format!("Failed to write BTR ZIP: {}", error))
}

/// Compares numeric BT version cores; prerelease suffixes do not affect minimum-version checks.
fn version_at_least(current: &str, required: &str) -> Result<bool, BtError> {
    fn parse(value: &str) -> Result<Vec<u64>, BtError> {
        let core = value.split_once('-').map(|item| item.0).unwrap_or(value);
        let values: Result<Vec<u64>, _> = core.split('.').map(str::parse::<u64>).collect();
        let values = values.map_err(|_| {
            BtError::Bundle(format!("BTR contains an invalid BT version: {}", value))
        })?;
        if values.is_empty() || values.len() > 4 {
            return Err(BtError::Bundle(format!(
                "BTR contains an invalid BT version: {}",
                value
            )));
        }
        Ok(values)
    }
    let mut current = parse(current)?;
    let mut required = parse(required)?;
    let width = current.len().max(required.len());
    current.resize(width, 0);
    required.resize(width, 0);
    Ok(current >= required)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A BTR can be built, reopened, and read on demand.
    #[test]
    fn builds_and_reads_btr_package() {
        let dir = fresh_temp_dir("roundtrip");
        fs::write(
            dir.join("app.json"),
            r#"{"app":{"name":"BtrDemo","entry":"index.html","main":false},"resources":["app.json","index.html"]}"#,
        )
        .unwrap();
        fs::write(dir.join("index.html"), "<h1>BTR</h1>").unwrap();
        let config =
            load_app_json_from_str(&fs::read_to_string(dir.join("app.json")).unwrap()).unwrap();

        let built = build_btr(&dir, &config).unwrap();
        let memory_package =
            BtrPackage::from_bytes(built.bytes.clone(), dir.join("memory.btr")).unwrap();
        assert_eq!(memory_package.config().app.name, "BtrDemo");
        let package_path = dir.join("BtrDemo.btr");
        fs::write(&package_path, built.bytes).unwrap();
        let package = BtrPackage::open(&package_path).unwrap();

        assert_eq!(package.manifest().format_version, BTR_FORMAT_VERSION);
        assert_eq!(package.config().app.name, "BtrDemo");
        assert_eq!(package.read("index.html").unwrap(), b"<h1>BTR</h1>");
        assert!(!package.list().contains(&BTR_MANIFEST_PATH.to_string()));
        let _ = fs::remove_dir_all(dir);
    }

    /// BTR path-traversal entries are rejected while reading.
    #[test]
    fn rejects_parent_directory_entry() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("../outside.txt", options).unwrap();
        writer.write_all(b"bad").unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let error = BtrPackage::from_bytes(bytes, PathBuf::from("bad.btr")).unwrap_err();

        assert!(error.to_string().contains("unsafe resource paths"));
    }

    /// File/directory prefix conflicts equivalent under Windows case rules are rejected before use.
    #[test]
    fn rejects_case_folded_file_directory_conflict() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file("A", options).unwrap();
        writer.write_all(b"file").unwrap();
        writer.start_file("a/child.txt", options).unwrap();
        writer.write_all(b"child").unwrap();
        let bytes = writer.finish().unwrap().into_inner();

        let error = BtrPackage::from_bytes(bytes, PathBuf::from("bad.btr")).unwrap_err();

        assert!(error
            .to_string()
            .contains("file and directory prefix conflict"));
    }

    /// Creates a unique test directory.
    fn fresh_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bt-btr-package-test-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
