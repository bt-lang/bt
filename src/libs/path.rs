//! BT path text standard library.
//!
//! `path(text)` provides cross-platform joining, normalization, and file-name extraction. The object stores text only
//! and never accesses the file system, keeping path manipulation separate from I/O.

use crate::value::Value;
use std::path::{Component, Path, PathBuf};

/// Path text standard library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtPath {
    /// Current path text.
    text: String,
}

impl BtPath {
    /// Creates a path object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let text = args.first().map(Value::to_string).unwrap_or_default();
        Ok(Value::Path(Self { text }))
    }

    /// Call the path method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "join" => Ok(Value::Str(self.join(args))),
            "dirname" => Ok(Value::Str(
                Path::new(&self.text)
                    .parent()
                    .map(path_to_string)
                    .unwrap_or_default(),
            )),
            "basename" => Ok(Value::Str(
                Path::new(&self.text)
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )),
            "filename" => Ok(Value::Str(
                Path::new(&self.text)
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )),
            "extension" => Ok(Value::Str(
                Path::new(&self.text)
                    .extension()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )),
            "normalize" => Ok(Value::Str(normalize_path_text(Path::new(&self.text)))),
            "is_absolute" => Ok(Value::Bool(Path::new(&self.text).is_absolute())),
            "is_relative" => Ok(Value::Bool(Path::new(&self.text).is_relative())),
            "to_string" => Ok(Value::Str(self.text.clone())),
            _ => Err(format!("path has no method `{}`", method)),
        }
    }

    /// Joins the current path with additional fragments.
    fn join(&self, args: Vec<Value>) -> String {
        let mut path = PathBuf::from(&self.text);
        for arg in args {
            path.push(arg.to_string());
        }
        normalize_path_text(&path)
    }
}

/// Normalize path text, removing `.` and statically offsetable `..`.
fn normalize_path_text(path: &Path) -> String {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let last_component = output.components().last();
                if matches!(last_component, Some(Component::Normal(_))) {
                    output.pop();
                } else if !matches!(
                    last_component,
                    Some(Component::RootDir) | Some(Component::Prefix(_))
                ) {
                    output.push("..");
                }
            }
            other => output.push(other.as_os_str()),
        }
    }
    path_to_string(&output)
}

/// Convert the path to a script-visible string.
fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path library should support concatenation, normalization and filename splitting.
    #[test]
    fn path_methods_cover_text_operations() {
        let Value::Path(path) = BtPath::new(vec![Value::Str("root".to_string())])
            .expect("path() should create the object")
        else {
            panic!("path() should return the Path value");
        };

        assert_eq!(
            path.call_method(
                "join",
                vec![Value::Str("a".to_string()), Value::Str("b.txt".to_string())]
            ),
            Ok(Value::Str("root/a/b.txt".to_string()))
        );
        assert_eq!(normalize_path_text(Path::new("root/a/../b")), "root/b");
        assert_eq!(normalize_path_text(Path::new("../root/a")), "../root/a");
        assert_eq!(normalize_path_text(Path::new("root/../../b")), "../b");
    }
}
