//! BT file system standard library.
//!
//! `fs(path)` creates a path object whose methods operate relative to that path. Results use only the VM's core `Value` type,
//! keeping legacy asynchronous `Env` and `Libs` structures out of the current runtime.

use crate::path as bt_path;
use crate::value::Value;
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use sha2::{Digest, Sha256};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

/// File system path object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtFs {
    /// The path to which the current object is bound.
    path: PathBuf,
}

impl BtFs {
    /// Creates a file system object from a Rust path.
    ///
    /// Web file upload will first complete the temporary file placement at the service layer, and then package the path into a `fs` object and hand it to the script.
    /// In this way, the script side can continue to use `file.move()` and `file.copy()`, which are equivalent to a set of file APIs.
    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// Creates a path object.
    #[allow(dead_code)]
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let path = args.first().map(Value::to_string).unwrap_or_default();
        if path.is_empty() {
            return Err("fs() requires a path argument".to_string());
        }
        Ok(Value::Fs(Self {
            path: PathBuf::from(path),
        }))
    }

    /// Dispatches a file-system method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "path" | "to_string" => Ok(Value::Str(self.path_text())),
            "read" => fs::read_to_string(&self.path)
                .map(Value::Str)
                .map_err(|err| format!("Failed to read file `{}`: {}", self.path_text(), err)),
            "binary" => fs::read(&self.path)
                .map(|bytes| {
                    Value::Array(Rc::new(RefCell::new(
                        bytes.into_iter().map(|b| Value::Int(b as i64)).collect(),
                    )))
                })
                .map_err(|err| {
                    format!("Failed to read binary file `{}`: {}", self.path_text(), err)
                }),
            "lines" => {
                let text = fs::read_to_string(&self.path).map_err(|err| {
                    format!(
                        "Failed to read `{}` line by line: {}",
                        self.path_text(),
                        err
                    )
                })?;
                Ok(Value::Array(Rc::new(RefCell::new(
                    text.lines()
                        .map(|line| Value::Str(line.to_string()))
                        .collect(),
                ))))
            }
            "write" => {
                let bytes = Self::arg_bytes(&args, 0);
                fs::write(&self.path, &bytes)
                    .map(|_| Value::Int(bytes.len() as i64))
                    .map_err(|err| format!("Failed to write file `{}`: {}", self.path_text(), err))
            }
            "atomic_write" => self.atomic_write(args),
            "append" => self.write_with_mode(args, true, false),
            "prepend" => {
                let mut bytes = Self::arg_bytes(&args, 0);
                if self.path.exists() {
                    let mut old = fs::read(&self.path).map_err(|err| {
                        format!(
                            "Failed to read existing file `{}`: {}",
                            self.path_text(),
                            err
                        )
                    })?;
                    bytes.append(&mut old);
                }
                fs::write(&self.path, &bytes)
                    .map(|_| Value::Int(bytes.len() as i64))
                    .map_err(|err| format!("Failed to append to `{}`: {}", self.path_text(), err))
            }
            "rename" => {
                let to = Self::required_path(&args, 0, "rename")?;
                fs::rename(&self.path, &to)
                    .map(|_| Value::Fs(Self { path: to }))
                    .map_err(|err| format!("Failed to rename `{}`: {}", self.path_text(), err))
            }
            "move" => {
                let dir = Self::required_path(&args, 0, "move")?;
                let name = self
                    .path
                    .file_name()
                    .ok_or_else(|| "move() could not determine the file name".to_string())?;
                let to = dir.join(name);
                fs::create_dir_all(&dir).map_err(|err| {
                    format!("Creating directory `{}` failed: {}", dir.display(), err)
                })?;
                fs::rename(&self.path, &to)
                    .map(|_| Value::Fs(Self { path: to }))
                    .map_err(|err| format!("Failed to move `{}`: {}", self.path_text(), err))
            }
            "copy" => {
                let to = Self::required_path(&args, 0, "copy")?;
                self.copy_to(&to)?;
                Ok(Value::Bool(true))
            }
            "delete" => self.delete().map(|_| Value::Bool(true)),
            "size" => fs::metadata(&self.path)
                .map(|meta| Value::Int(meta.len() as i64))
                .map_err(|err| {
                    format!("Failed to read the size of `{}`: {}", self.path_text(), err)
                }),
            "create_dir" => fs::create_dir_all(&self.path)
                .map(|_| Value::Bool(true))
                .map_err(|err| {
                    format!("Creating directory `{}` failed: {}", self.path_text(), err)
                }),
            "create_file" => self.create_file(args),
            "list" => self.list(),
            "is_dir" => Ok(Value::Bool(self.path.is_dir())),
            "is_file" => Ok(Value::Bool(self.path.is_file())),
            "is_relative" => Ok(Value::Bool(self.path.is_relative())),
            "is_absolute" => Ok(Value::Bool(self.path.is_absolute())),
            "is_symlink" => Ok(Value::Bool(self.path.is_symlink())),
            "is_exists" => Ok(Value::Bool(self.path.exists())),
            "real_path" => fs::canonicalize(&self.path)
                .map(|path| Value::Str(bt_path::path_text(&path)))
                .map_err(|err| {
                    format!("Resolving real path `{}` failed: {}", self.path_text(), err)
                }),
            "basename" => Ok(Value::Str(self.file_part(|path| path.file_name()))),
            "filename" => Ok(Value::Str(self.file_part(|path| path.file_stem()))),
            "extension" => Ok(Value::Str(self.file_part(|path| path.extension()))),
            _ => Err(format!("fs has no method `{}`", method)),
        }
    }

    /// Writes a file in append mode.
    fn write_with_mode(
        &self,
        args: Vec<Value>,
        append: bool,
        truncate: bool,
    ) -> Result<Value, String> {
        let bytes = Self::arg_bytes(&args, 0);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(truncate)
            .open(&self.path)
            .map_err(|err| format!("Failed to open file `{}`: {}", self.path_text(), err))?;
        file.write_all(&bytes)
            .map_err(|err| format!("Writing file `{}` failed: {}", self.path_text(), err))?;
        Ok(Value::Int(bytes.len() as i64))
    }

    /// Atomic replacement of the target with a temporary file in the same directory, with the option of rejecting concurrent overwrites by SHA-256 of the old content.
    ///
    /// The temporary file is on the same file system as the target; the target is not replaced until writing and flushing are complete. When the caller passes in the old digest,
    /// will read the target again before replacing it, ensuring that common editor or external process rewrites will not be silently overwritten.
    fn atomic_write(&self, args: Vec<Value>) -> Result<Value, String> {
        let bytes = Self::arg_bytes(&args, 0);
        let expected = args
            .get(1)
            .filter(|value| !matches!(value, Value::Empty | Value::Null))
            .map(Value::to_string);
        if let Some(expected) = &expected {
            if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(
                    "fs.atomic_write() expected_sha256 must be a 64-character hexadecimal string"
                        .to_string(),
                );
            }
        }
        if self.path.is_dir() {
            return Err(format!(
                "Atomic write target `{}` cannot be a directory",
                self.path_text()
            ));
        }
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(format!(
                "Atomic write target directory `{}` does not exist",
                parent.display()
            ));
        }

        let previous = if self.path.exists() {
            Some(fs::read(&self.path).map_err(|err| {
                format!(
                    "Failed to read atomic-write target `{}`: {}",
                    self.path_text(),
                    err
                )
            })?)
        } else {
            None
        };
        let previous_sha256 = previous.as_deref().map(sha256_hex);
        if let Some(expected) = &expected {
            if previous_sha256.as_deref() != Some(expected.to_ascii_lowercase().as_str()) {
                return Err(format!(
                    "Atomic write conflict: the current SHA-256 of `{}` does not match expected_sha256",
                    self.path_text()
                ));
            }
        }

        let temp_path = self.unique_atomic_temp_path(parent)?;
        let write_result = (|| -> Result<(), String> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .map_err(|err| format!("Failed to create atomic write temp file: {}", err))?;
            file.write_all(&bytes)
                .map_err(|err| format!("Failed to write atomic temp file: {}", err))?;
            file.sync_all()
                .map_err(|err| format!("Failed to refresh atomic temp file: {}", err))?;

            // Check the summary again after the temporary file is placed, shortening the race window between external rewriting and final replacement.
            if let Some(expected) = &expected {
                let current = fs::read(&self.path).map_err(|err| {
                    format!(
                        "Failed to recheck `{}` before replacement: {}",
                        self.path_text(),
                        err
                    )
                })?;
                if sha256_hex(&current) != expected.to_ascii_lowercase() {
                    return Err(format!(
                        "Atomic write conflict: `{}` has been modified externally during write",
                        self.path_text()
                    ));
                }
            }
            atomic_replace(&temp_path, &self.path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result?;

        let mut result = IndexMap::new();
        result.insert("bytes".to_string(), Value::Int(bytes.len() as i64));
        result.insert("sha256".to_string(), Value::Str(sha256_hex(&bytes)));
        result.insert(
            "previous_sha256".to_string(),
            previous_sha256.map(Value::Str).unwrap_or(Value::Empty),
        );
        Ok(Value::Object(Rc::new(RefCell::new(result))))
    }

    /// Generates a temporary path to the same directory for atomic writes that does not overwrite existing files.
    fn unique_atomic_temp_path(&self, parent: &Path) -> Result<PathBuf, String> {
        let name = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file");
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for attempt in 0..32_u32 {
            let candidate = parent.join(format!(
                ".{}.bt-atomic-{}-{}-{}",
                name,
                std::process::id(),
                seed,
                attempt
            ));
            if !candidate.exists() {
                return Ok(candidate);
            }
        }
        Err("Unable to allocate atomic write temporary file name".to_string())
    }

    /// Creates file and writes optional content.
    fn create_file(&self, args: Vec<Value>) -> Result<Value, String> {
        let text = args.first().map(Self::file_text).unwrap_or_default();
        let mut file = File::create(&self.path)
            .map_err(|err| format!("Failed to create file `{}`: {}", self.path_text(), err))?;
        file.write_all(text.as_bytes())
            .map_err(|err| format!("Failed to initialize file `{}`: {}", self.path_text(), err))?;
        Ok(Value::Int(text.len() as i64))
    }

    /// List directory entries.
    fn list(&self) -> Result<Value, String> {
        let mut items = Vec::new();
        for entry in fs::read_dir(&self.path)
            .map_err(|err| format!("Failed to read directory `{}`: {}", self.path_text(), err))?
        {
            let entry = entry.map_err(|err| format!("Failed to read directory entry: {}", err))?;
            items.push(Value::Fs(Self { path: entry.path() }));
        }
        Ok(Value::Array(Rc::new(RefCell::new(items))))
    }

    /// Delete file or directory.
    fn delete(&self) -> Result<(), String> {
        if self.path.is_dir() {
            fs::remove_dir_all(&self.path)
        } else {
            fs::remove_file(&self.path)
        }
        .map_err(|err| format!("Failed to remove `{}`: {}", self.path_text(), err))
    }

    /// Copying files or directories.
    fn copy_to(&self, to: &Path) -> Result<(), String> {
        if self.path.is_dir() {
            fs::create_dir_all(to).map_err(|err| {
                format!(
                    "Creating target directory `{}` failed: {}",
                    to.display(),
                    err
                )
            })?;
            for entry in fs::read_dir(&self.path).map_err(|err| {
                format!(
                    "Reading source directory `{}` failed: {}",
                    self.path_text(),
                    err
                )
            })? {
                let entry =
                    entry.map_err(|err| format!("Failed to read directory entry: {}", err))?;
                let child_to = to.join(entry.file_name());
                Self { path: entry.path() }.copy_to(&child_to)?;
            }
            Ok(())
        } else {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    format!(
                        "Creating target directory `{}` failed: {}",
                        parent.display(),
                        err
                    )
                })?;
            }
            fs::copy(&self.path, to).map(|_| ()).map_err(|err| {
                format!(
                    "Copying `{}` to `{}` failed: {}",
                    self.path_text(),
                    to.display(),
                    err
                )
            })
        }
    }

    /// Path to string.
    fn path_text(&self) -> String {
        bt_path::path_text(&self.path)
    }

    /// Read path components.
    fn file_part(&self, f: impl FnOnce(&Path) -> Option<&std::ffi::OsStr>) -> String {
        f(&self.path)
            .and_then(|part| part.to_str())
            .unwrap_or("")
            .to_string()
    }

    /// Reads a required path argument.
    fn required_path(args: &[Value], index: usize, method: &str) -> Result<PathBuf, String> {
        let text = args
            .get(index)
            .map(Value::to_string)
            .ok_or_else(|| format!("fs.{}() missing path parameter", method))?;
        Ok(PathBuf::from(text))
    }

    /// Convert script value to byte array.
    fn arg_bytes(args: &[Value], index: usize) -> Vec<u8> {
        args.get(index)
            .map(Self::file_text)
            .unwrap_or_default()
            .into_bytes()
    }

    /// Converts script values to file content text.
    ///
    /// The file library uses standard JSON when writing arrays and objects, ensuring that subsequent `include()` or `String.parse_json()` can be read back in a stable
    /// format; ordinary strings are still written as original text, avoiding additional quotation marks for text such as templates, HTML, and logs.
    fn file_text(value: &Value) -> String {
        match value {
            Value::Array(_) | Value::Object(_) | Value::Instance(_) => value.to_json_string(),
            other => other.to_string(),
        }
    }
}

/// Computes the SHA-256 lowercase hexadecimal digest of the byte contents.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Submits temporary files on the same volume as target files using operating system atomic replacement semantics.
#[cfg(windows)]
fn atomic_replace(from: &Path, to: &Path) -> Result<(), String> {
    let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH;
    if unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), flags) } == 0 {
        return Err(format!(
            "Atomic replacement `{}` failed: {}",
            to.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Unix's rename will atomically replace the target within the same file system.
#[cfg(not(windows))]
fn atomic_replace(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to)
        .map_err(|err| format!("Atomic replacement `{}` failed: {}", to.display(), err))
}

impl std::fmt::Display for BtFs {
    /// Outputs the current path text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a unique temporary directory used only for current testing.
    fn test_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("bt-fs-{}-{}-{}", name, std::process::id(), stamp))
    }

    /// `real_path` returns the absolute path resolved by the operating system.
    #[test]
    fn real_path_returns_canonical_absolute_path() {
        let root = test_dir("real-path");
        fs::create_dir_all(root.join("child")).expect("The test directory should be created");
        let object = BtFs::from_path(root.join("child").join(".."));
        let actual = object
            .call_method("real_path", Vec::new())
            .expect("real_path should succeed");
        assert_eq!(
            actual,
            Value::Str(bt_path::path_text(&fs::canonicalize(&root).unwrap()))
        );
        fs::remove_dir_all(&root).expect("The test directory should be cleaned");
    }

    /// `atomic_write` replaces a matching target and preserves content when the old digest is stale.
    #[test]
    fn atomic_write_replaces_and_rejects_stale_hash() {
        let root = test_dir("atomic-write");
        fs::create_dir_all(&root).expect("The test directory should be created");
        let target = root.join("data.txt");
        fs::write(&target, b"old").expect("The old content should be written");
        let object = BtFs::from_path(target.clone());
        let old_hash = sha256_hex(b"old");
        let result = object
            .call_method(
                "atomic_write",
                vec![Value::Str("new".to_string()), Value::Str(old_hash.clone())],
            )
            .expect("The atomic replacement should succeed");
        let Value::Object(result) = result else {
            panic!("atomic_write should return the result object");
        };
        assert_eq!(
            result.borrow().get("sha256"),
            Some(&Value::Str(sha256_hex(b"new")))
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");

        let error = object
            .call_method(
                "atomic_write",
                vec![Value::Str("lost".to_string()), Value::Str(old_hash)],
            )
            .expect_err("The expired digest must trigger a conflict");
        assert!(error.contains("conflict"));
        assert_eq!(fs::read(&target).unwrap(), b"new");
        fs::remove_dir_all(&root).expect("The test directory should be cleaned");
    }
}
