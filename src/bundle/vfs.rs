use crate::error::BtError;
use std::collections::HashMap;
use std::path::{Component, Path};

/// Bundle virtual file system.
#[derive(Debug, Clone)]
pub struct VirtualFileSystem {
    files: HashMap<String, Vec<u8>>,
}

impl VirtualFileSystem {
    /// Create an empty virtual file system.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    /// Parse a virtual file system from a version-one Bundle byte stream.
    pub fn from_bundle(bundle: &[u8]) -> Result<Self, BtError> {
        let mut offset = 0usize;
        let file_count = read_u32(bundle, &mut offset)? as usize;
        let mut files = HashMap::with_capacity(file_count);

        for _ in 0..file_count {
            let name_len = read_u16(bundle, &mut offset)? as usize;
            let name_bytes = read_bytes(bundle, &mut offset, name_len)?;
            let name = std::str::from_utf8(name_bytes).map_err(|err| {
                BtError::Bundle(format!("Bundle file name is not UTF-8: {}", err))
            })?;
            validate_vfs_path(name)?;

            let content_len = read_u64(bundle, &mut offset)?;
            let content_len = usize::try_from(content_len)
                .map_err(|_| BtError::Bundle("Bundle file is too large to read".to_string()))?;
            let content = read_bytes(bundle, &mut offset, content_len)?.to_vec();

            if files.insert(name.to_string(), content).is_some() {
                return Err(BtError::Bundle(format!(
                    "Bundle contains a duplicate file: {}",
                    name
                )));
            }
        }

        if offset != bundle.len() {
            return Err(BtError::Bundle(format!(
                "Bundle contains trailing data: {} bytes",
                bundle.len() - offset
            )));
        }

        Ok(Self { files })
    }

    /// Read file bytes.
    pub fn read(&self, path: &str) -> Option<&[u8]> {
        if validate_vfs_path(path).is_err() {
            return None;
        }
        self.files.get(path).map(Vec::as_slice)
    }

    /// Read a UTF-8 text file.
    pub fn read_string(&self, path: &str) -> Result<String, BtError> {
        let bytes = self
            .read(path)
            .ok_or_else(|| BtError::Bundle(format!("Bundle file does not exist: {}", path)))?;
        String::from_utf8(bytes.to_vec())
            .map_err(|err| BtError::Bundle(format!("Bundle text is not UTF-8: {}", err)))
    }

    /// Check whether a file exists.
    #[allow(dead_code)]
    pub fn exists(&self, path: &str) -> bool {
        self.read(path).is_some()
    }

    /// List all files in the Bundle.
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.files.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Read a little-endian u16.
fn read_u16(input: &[u8], offset: &mut usize) -> Result<u16, BtError> {
    let bytes = read_bytes(input, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Read a little-endian u32.
fn read_u32(input: &[u8], offset: &mut usize) -> Result<u32, BtError> {
    let bytes = read_bytes(input, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Read a little-endian u64.
fn read_u64(input: &[u8], offset: &mut usize) -> Result<u64, BtError> {
    let bytes = read_bytes(input, offset, 8)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

/// Read a slice of the requested length from the buffer.
fn read_bytes<'a>(input: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], BtError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| BtError::Bundle("Bundle offset overflow".to_string()))?;
    if end > input.len() {
        return Err(BtError::Bundle("Bundle data is truncated".to_string()));
    }
    let bytes = &input[*offset..end];
    *offset = end;
    Ok(bytes)
}

/// Validate that an internal VFS path is a safe relative path.
fn validate_vfs_path(path: &str) -> Result<(), BtError> {
    if path.trim().is_empty() {
        return Err(BtError::Bundle(
            "Bundle file name cannot be empty".to_string(),
        ));
    }
    if path.as_bytes().contains(&0) {
        return Err(BtError::Bundle(format!(
            "Bundle file name contains a null byte: {}",
            path
        )));
    }
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(BtError::Bundle(format!(
            "Bundle forbids absolute paths: {}",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(BtError::Bundle(format!(
                    "Bundle forbids `..` paths: {}",
                    path.display()
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "Bundle forbids root paths: {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}
