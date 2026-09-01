//! Shared BT path-resolution helpers.
//!
//! This module applies lexical rules to script-visible paths without touching the file system or relying on the process working directory.
//! The VM, Web configuration, and standard libraries resolve paths here so base-path rules remain consistent.

use std::path::{Component, Path, PathBuf};

/// Returns whether script text represents an absolute path.
///
/// `Path::is_absolute()` follows the host platform, so Windows drive-letter and UNC
/// paths are recognized explicitly as well. This keeps BT path rules consistent in
/// cross-platform tests and configuration tooling.
pub fn is_absolute_path(path: &str) -> bool {
    Path::new(path).is_absolute() || is_windows_drive_absolute(path) || is_unc_path(path)
}

/// Returns whether a `process(program)` value clearly has path semantics.
///
/// Bare command names retain normal system `PATH` lookup. Unified resolution is
/// used only for separators, relative-path markers, the `@` project root, or an
/// absolute path.
pub fn is_process_program_path(program: &str) -> bool {
    has_path_semantics(program)
}

/// Determines whether text clearly has file-path semantics.
///
/// Process names and bare FFI library names retain system search behavior. Callers
/// use unified resolution only for separators, relative markers, `@`, or absolute paths.
pub(crate) fn has_path_semantics(value: &str) -> bool {
    is_absolute_path(value)
        || value.contains('/')
        || value.contains('\\')
        || value.starts_with('.')
        || value.starts_with('@')
}

/// Parses the script path according to BT rules.
///
/// Empty paths are left for callers to reject according to their own semantics. `@` points to the project root, `@/...` is resolved from that root,
/// and other relative paths are resolved from the directory of the source file currently being executed.
pub fn resolve_path(path: &str, project_root: &Path, source_dir: &Path) -> PathBuf {
    if is_absolute_path(path) {
        return normalize_path(PathBuf::from(path));
    }
    if path == "@" {
        return normalize_path(project_root);
    }
    if let Some(rest) = path.strip_prefix("@/").or_else(|| path.strip_prefix("@\\")) {
        return normalize_path(project_root.join(rest));
    }
    normalize_path(source_dir.join(path))
}

/// Normalizes a path lexically without accessing the file system.
///
/// This folds `.` and safe `..` components without calling `canonicalize()`, so the
/// result remains stable for nonexistent paths and never depends silently on the
/// process working directory.
pub fn normalize_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    output.push("..");
                }
            }
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                output.push(component.as_os_str());
            }
        }
    }
    if output.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        output
    }
}

/// Convert the path to the text displayed by BT.
///
/// All platforms use the `/` delimiter uniformly, and the Windows drive letter remains unchanged.
pub fn path_text(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
        format!("//{}", rest)
    } else if let Some(rest) = text.strip_prefix("//?/") {
        rest.to_string()
    } else {
        text
    }
}

/// Produces display text relative to the project root.
///
/// Returns the full path when the target path is not under the project root, to avoid forging a seemingly relative but inaccessible location.
pub fn relative_path_text(path: &Path, project_root: &Path) -> String {
    let relative = path.strip_prefix(project_root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path_text(relative)
    }
}

/// Returns whether the path is an absolute Windows drive-letter path.
fn is_windows_drive_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

/// Returns whether the path is an absolute UNC path.
fn is_unc_path(path: &str) -> bool {
    path.starts_with("\\\\") || path.starts_with("//")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `@` and `@/...` must be pinned to the project root, not the current source directory.
    #[test]
    fn resolve_project_root_paths() {
        let root = Path::new("E:/project");
        let dir = Path::new("E:/project/lib");

        assert_eq!(path_text(&resolve_path("@", root, dir)), "E:/project");
        assert_eq!(
            path_text(&resolve_path("@/config/app.json", root, dir)),
            "E:/project/config/app.json"
        );
    }

    /// Ordinary relative paths must be based on the current source file directory.
    #[test]
    fn resolve_relative_paths_from_source_dir() {
        let root = Path::new("E:/project");
        let dir = Path::new("E:/project/common");

        assert_eq!(
            path_text(&resolve_path("../log.bt", root, dir)),
            "E:/project/log.bt"
        );
    }

    /// The absolute path is used directly, and only the delimiter is adjusted.
    #[test]
    fn absolute_paths_stay_absolute() {
        let root = Path::new("E:/project");
        let dir = Path::new("E:/project/common");

        assert_eq!(
            path_text(&resolve_path("E:/other/a.txt", root, dir)),
            "E:/other/a.txt"
        );
    }

    /// The Windows verbatim prefix should not appear in script-visible paths.
    #[test]
    fn path_text_strips_windows_verbatim_prefix() {
        assert_eq!(
            path_text(Path::new(r"\\?\C:\project\a.txt")),
            "C:/project/a.txt"
        );
    }

    /// Naked commands and bare library names should be retained for system search, and only text with directory semantics will enter unified parsing.
    #[test]
    fn path_semantics_distinguish_bare_names_from_paths() {
        assert!(!has_path_semantics("user32.dll"));
        assert!(!is_process_program_path("cargo"));
        assert!(has_path_semantics("./native/sdk.dll"));
        assert!(has_path_semantics("@/native/sdk.dll"));
        assert!(is_process_program_path("tools/run.exe"));
    }
}
