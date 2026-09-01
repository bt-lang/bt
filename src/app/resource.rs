use crate::bundle::package::BtrPackage;
use crate::bundle::vfs::VirtualFileSystem;
use crate::error::BtError;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Embedded text resource.
///
/// Used for static pages that never touch disk, such as the desktop setup page. The content is
/// compiled directly into the executable, avoiding a temporary file when `app.json` is missing.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedResource {
    /// Relative resource path exposed through the protocol layer.
    pub path: &'static str,
    /// UTF-8 text content.
    pub content: &'static str,
}

/// Resource source for a desktop application.
#[derive(Debug, Clone)]
pub enum ResourceSource {
    /// Development mode, read from the project directory.
    Directory(PathBuf),

    /// Legacy packaged mode, read from the uncompressed bundle appended to the executable.
    Bundle(VirtualFileSystem),

    /// BTR mode, read on demand from a standalone file or ZIP container appended to the executable.
    Btr(BtrPackage),

    /// Setup mode, read from resources embedded at compile time.
    Embedded(&'static [EmbeddedResource]),
}

impl ResourceSource {
    /// Reads a resource as bytes.
    pub fn read(&self, path: &str) -> Result<Vec<u8>, BtError> {
        validate_resource_path(path)?;
        match self {
            ResourceSource::Directory(project_dir) => {
                let path = checked_directory_path(project_dir, path)?;
                Ok(fs::read(path)?)
            }
            ResourceSource::Bundle(vfs) => vfs
                .read(path)
                .map(|bytes| bytes.to_vec())
                .ok_or_else(|| BtError::Bundle(format!("Bundle file does not exist: {}", path))),
            ResourceSource::Btr(package) => package.read(path),
            ResourceSource::Embedded(resources) => resources
                .iter()
                .find(|resource| resource.path == path)
                .map(|resource| resource.content.as_bytes().to_vec())
                .ok_or_else(|| {
                    BtError::Bundle(format!("Embedded resource does not exist: {}", path))
                }),
        }
    }

    /// Reads a UTF-8 text resource.
    #[allow(dead_code)]
    pub fn read_string(&self, path: &str) -> Result<String, BtError> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes)
            .map_err(|err| BtError::Bundle(format!("Resource is not UTF-8: {}", err)))
    }

    /// Returns whether a resource exists.
    #[allow(dead_code)]
    pub fn exists(&self, path: &str) -> bool {
        if validate_resource_path(path).is_err() {
            return false;
        }
        match self {
            ResourceSource::Directory(project_dir) => checked_directory_path(project_dir, path)
                .map(|path| path.is_file())
                .unwrap_or(false),
            ResourceSource::Bundle(vfs) => vfs.exists(path),
            ResourceSource::Btr(package) => package.exists(path),
            ResourceSource::Embedded(resources) => {
                resources.iter().any(|resource| resource.path == path)
            }
        }
    }

    /// Lists resource files.
    pub fn list(&self) -> Vec<String> {
        match self {
            ResourceSource::Directory(project_dir) => list_directory_files(project_dir),
            ResourceSource::Bundle(vfs) => vfs.list(),
            ResourceSource::Btr(package) => package.list(),
            ResourceSource::Embedded(resources) => {
                let mut files: Vec<String> = resources
                    .iter()
                    .map(|resource| resource.path.to_string())
                    .collect();
                files.sort();
                files
            }
        }
    }

    /// Returns whether this source is a development directory.
    pub fn is_dev(&self) -> bool {
        matches!(self, ResourceSource::Directory(_))
    }
}

/// Validates a resource path before exposing it to callers.
fn validate_resource_path(path: &str) -> Result<(), BtError> {
    if path.trim().is_empty() {
        return Err(BtError::Bundle("Resource path cannot be empty".to_string()));
    }
    if path.as_bytes().contains(&0) {
        return Err(BtError::Bundle(format!(
            "Resource path contains a null byte: {}",
            path
        )));
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(BtError::Bundle(format!(
            "Absolute resource paths cannot be read: {}",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(BtError::Bundle(format!(
                    "Resource paths containing .. cannot be read: {}",
                    path.display()
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "Rooted resource paths cannot be read: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Resolves a file path while keeping it within the project directory.
fn checked_directory_path(project_dir: &Path, resource: &str) -> Result<PathBuf, BtError> {
    let root = project_dir.canonicalize()?;
    let path = root.join(resource);
    let full_path = path.canonicalize()?;
    if !full_path.starts_with(&root) {
        return Err(BtError::Bundle(format!(
            "Resources outside the project directory cannot be read: {}",
            full_path.display()
        )));
    }
    Ok(full_path)
}

/// Recursively lists files in the development directory.
fn list_directory_files(project_dir: &Path) -> Vec<String> {
    let root = match project_dir.canonicalize() {
        Ok(root) => root,
        Err(_) => return Vec::new(),
    };
    let mut stack = vec![root.clone()];
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(full_path) = path.canonicalize() else {
                continue;
            };
            if !full_path.starts_with(&root) {
                continue;
            }
            if full_path.is_dir() {
                stack.push(full_path);
                continue;
            }
            if !full_path.is_file() {
                continue;
            }
            let Ok(relative) = full_path.strip_prefix(&root) else {
                continue;
            };
            if let Some(name) = normalize_list_name(relative) {
                files.push(name);
            }
        }
    }
    files.sort();
    files
}

/// Converts a development-directory file path to a `/`-separated relative path.
fn normalize_list_name(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => parts.push(value.to_str()?),
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}
