//! Explicit, current-user Windows installation and shell integration.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM};
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
    REG_SZ,
};
use windows_sys::Win32::UI::Shell::{
    SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_FLUSHNOWAIT, SHCNF_IDLIST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    WM_SETTINGCHANGE,
};

/// Return the stable per-user executable directory without changing the environment.
pub(super) fn install_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or("USERPROFILE is unavailable; cannot locate the BT installation directory")?;
    if !home.is_absolute() {
        return Err("USERPROFILE must be an absolute path".to_string());
    }
    let directory = home.join(".bt").join("bin");
    installation_path(&directory)?;
    Ok(directory)
}

/// Configure the user PATH and register BT as an available script handler.
pub(super) fn integrate(executable: &Path) -> Result<Vec<String>, String> {
    let executable_text = installation_path(executable)?;
    let directory = executable
        .parent()
        .ok_or("BT executable has no parent directory")?;
    let directory_text = installation_path(directory)?;
    let environment = RegistryKey::create("Environment")?;
    let (old_path, path_type) = environment
        .read("Path")?
        .unwrap_or((String::new(), REG_EXPAND_SZ));
    let updated_path = append_path(&old_path, directory_text, path_type == REG_EXPAND_SZ)?;
    let mut messages = Vec::new();
    if let Some(updated_path) = updated_path {
        environment.write("Path", &updated_path, path_type)?;
        if !notify_environment() {
            messages.push("Some applications did not acknowledge the PATH update. Sign out and back in if a new terminal cannot find BT.".to_string());
        }
    }

    for (subkey, name, value) in association_entries(executable_text) {
        RegistryKey::create(subkey)?.write(name, &value, REG_SZ)?;
    }
    // Supply an initial association only when neither HKCU nor the merged HKCR has
    // an existing default. Never edit Explorer's protected UserChoice key.
    let extension = RegistryKey::create(r"Software\Classes\.bt")?;
    let existing_default = RegistryKey::open_at(
        windows_sys::Win32::System::Registry::HKEY_CLASSES_ROOT,
        ".bt",
    )?
    .map(|key| key.read(""))
    .transpose()?
    .flatten();
    if existing_default
        .as_ref()
        .is_none_or(|(value, _)| value.is_empty())
    {
        extension.write("", "BTLang.Script", REG_SZ)?;
    }
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST | SHCNF_FLUSHNOWAIT,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
    messages.push("Open a new terminal to use bt. If necessary, choose BT in Open with or Windows Settings > Apps > Default apps for .bt files.".to_string());
    Ok(messages)
}

/// Atomically rename a staged sibling over the target, leaving locked binaries intact.
pub(super) fn replace(staged: &Path, target: &Path) -> Result<(), String> {
    if staged.parent() != target.parent()
        || staged
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&target.as_os_str().to_string_lossy())
    {
        return Err("BT replacement requires distinct files in the same directory".to_string());
    }
    let staged_wide = wide(staged.as_os_str());
    let target_wide = wide(target.as_os_str());
    let succeeded = unsafe {
        MoveFileExW(
            staged_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        return Err(format!(
            "Cannot replace {}: {}. Close programs using the installed BT executable and retry installation from the downloaded executable; the existing installation was not removed.",
            target.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Reject paths that cannot be represented safely in a Windows PATH or shell command.
fn installation_path(path: &Path) -> Result<&str, String> {
    let text = path
        .to_str()
        .ok_or("BT installation path must contain valid Unicode")?;
    if !path.is_absolute() || text.contains([';', '"', '\0', '\r', '\n']) {
        return Err("BT installation requires an absolute path without semicolons, quotes, or control characters".to_string());
    }
    Ok(text)
}

/// Build registry strings without invoking a command shell or changing protected defaults.
fn association_entries(executable: &str) -> Vec<(&'static str, &'static str, String)> {
    let command = format!("\"{executable}\" --open-script \"%1\"");
    vec![
        (
            r"Software\Classes\BTLang.Script",
            "",
            "BT Script".to_string(),
        ),
        (
            r"Software\Classes\BTLang.Script\DefaultIcon",
            "",
            format!("\"{executable}\",0"),
        ),
        (
            r"Software\Classes\BTLang.Script\shell\open\command",
            "",
            command.clone(),
        ),
        (
            r"Software\Classes\.bt\OpenWithProgids",
            "BTLang.Script",
            String::new(),
        ),
        (
            r"Software\Classes\Applications\bt.exe",
            "FriendlyAppName",
            "BT".to_string(),
        ),
        (
            r"Software\Classes\Applications\bt.exe\SupportedTypes",
            ".bt",
            String::new(),
        ),
        (
            r"Software\Classes\Applications\bt.exe\shell\open\command",
            "",
            command,
        ),
        (
            r"Software\BTLang\Capabilities",
            "ApplicationName",
            "BT".to_string(),
        ),
        (
            r"Software\BTLang\Capabilities",
            "ApplicationDescription",
            "Run BT language scripts in a console window.".to_string(),
        ),
        (
            r"Software\BTLang\Capabilities\FileAssociations",
            ".bt",
            "BTLang.Script".to_string(),
        ),
        (
            r"Software\RegisteredApplications",
            "BT",
            r"Software\BTLang\Capabilities".to_string(),
        ),
    ]
}

/// Append one directory while preserving existing entries and avoiding equivalent duplicates.
fn append_path(
    existing: &str,
    directory: &str,
    expandable: bool,
) -> Result<Option<String>, String> {
    let expected = normalized_path(directory);
    if existing.split(';').any(|entry| {
        let expanded = if expandable {
            expand_for_comparison(entry)
        } else {
            entry.to_string()
        };
        normalized_path(&expanded) == expected
    }) {
        return Ok(None);
    }
    let separator = if existing.is_empty() || existing.ends_with(';') {
        ""
    } else {
        ";"
    };
    let result = format!("{existing}{separator}{directory}");
    if result.encode_utf16().count() >= 32767 {
        return Err("Adding BT would exceed the Windows PATH length limit".to_string());
    }
    Ok(Some(result))
}

/// Normalize spelling differences that do not affect Windows directory lookup.
fn normalized_path(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// Expand variable references solely for duplicate detection, preserving stored PATH text.
fn expand_for_comparison(value: &str) -> String {
    let mut result = String::new();
    let mut remaining = value;
    while let Some(start) = remaining.find('%') {
        result.push_str(&remaining[..start]);
        remaining = &remaining[start..];
        let Some(end) = remaining[1..].find('%').map(|index| index + 1) else {
            break;
        };
        let reference = &remaining[..=end];
        result
            .push_str(&std::env::var(&remaining[1..end]).unwrap_or_else(|_| reference.to_string()));
        remaining = &remaining[end + 1..];
    }
    result.push_str(remaining);
    result
}

/// Own a registry handle so every successful open is closed on all return paths.
struct RegistryKey(HKEY);

impl RegistryKey {
    /// Create a current-user key with only the value access required by installation.
    fn create(subkey: &str) -> Result<Self, String> {
        let mut key = std::ptr::null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                wide(OsStr::new(subkey)).as_ptr(),
                0,
                std::ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        registry_status(status, "create", subkey)?;
        Ok(Self(key))
    }

    /// Open a key without creating it or requesting write access.
    fn open_at(root: HKEY, subkey: &str) -> Result<Option<Self>, String> {
        let mut key = std::ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                root,
                wide(OsStr::new(subkey)).as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        registry_status(status, "open", subkey)?;
        Ok(Some(Self(key)))
    }

    /// Read bounded, valid UTF-16 string data without expanding or changing its registry type.
    fn read(&self, name: &str) -> Result<Option<(String, u32)>, String> {
        let name_wide = wide(OsStr::new(name));
        let mut kind = 0;
        let mut length = 0;
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                name_wide.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut length,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        registry_status(status, "read", name)?;
        if !matches!(kind, REG_SZ | REG_EXPAND_SZ) || length > 131072 || length % 2 != 0 {
            return Err(format!("Registry value {name} is not a supported bounded UTF-16 string; it was not modified"));
        }
        let mut buffer = vec![0u16; length as usize / 2 + 1];
        let capacity = length;
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                name_wide.as_ptr(),
                std::ptr::null(),
                &mut kind,
                buffer.as_mut_ptr().cast(),
                &mut length,
            )
        };
        registry_status(status, "read", name)?;
        if !matches!(kind, REG_SZ | REG_EXPAND_SZ) || length > capacity || length % 2 != 0 {
            return Err(format!(
                "Registry value {name} changed while being read; retry installation"
            ));
        }
        buffer.truncate(length as usize / 2);
        if buffer.last() == Some(&0) {
            buffer.pop();
        }
        if buffer.contains(&0) {
            return Err(format!(
                "Registry value {name} contains embedded NUL characters"
            ));
        }
        let value = String::from_utf16(&buffer)
            .map_err(|_| format!("Registry value {name} is not valid Unicode"))?;
        Ok(Some((value, kind)))
    }

    /// Store a string with its explicit registry type, including the terminating NUL.
    fn write(&self, name: &str, value: &str, kind: u32) -> Result<(), String> {
        let value_wide = wide(OsStr::new(value));
        let length =
            u32::try_from(value_wide.len() * 2).map_err(|_| "Registry value is too large")?;
        let status = unsafe {
            RegSetValueExW(
                self.0,
                wide(OsStr::new(name)).as_ptr(),
                0,
                kind,
                value_wide.as_ptr().cast(),
                length,
            )
        };
        registry_status(status, "write", name)
    }
}

impl Drop for RegistryKey {
    /// Release this owned registry handle without masking an earlier operation error.
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

/// Convert a Win32 registry status to an actionable error.
fn registry_status(status: u32, action: &str, name: &str) -> Result<(), String> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "Cannot {action} registry entry {name}: {}",
            std::io::Error::from_raw_os_error(status as i32)
        ))
    }
}

/// Convert an operating-system string to terminated UTF-16 without lossy conversion.
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

/// Carry a shared deadline across the synchronous top-level window enumeration.
struct EnvironmentNotification {
    /// Latest time at which another notification may begin.
    deadline: Instant,
    /// Whether every eligible window acknowledged the message.
    complete: bool,
    /// Stable storage for the system-marshalled Environment message string.
    text: Vec<u16>,
}

/// Notify external windows within a global budget instead of multiplying broadcast timeouts.
unsafe extern "system" fn notify_window(window: HWND, data: LPARAM) -> i32 {
    let state = unsafe { &mut *(data as *mut EnvironmentNotification) };
    let remaining = state
        .deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    if remaining == 0 {
        state.complete = false;
        return 0;
    }
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut process);
    }
    // Do not call back synchronously into this process's own UI thread.
    if process != std::process::id() {
        let result = unsafe {
            SendMessageTimeoutW(
                window,
                WM_SETTINGCHANGE,
                0,
                state.text.as_ptr() as LPARAM,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                remaining.min(100) as u32,
                std::ptr::null_mut(),
            )
        };
        if result == 0 {
            state.complete = false;
        }
    }
    1
}

/// Announce a persistent PATH change with at most two seconds of window-response waiting.
fn notify_environment() -> bool {
    let mut state = EnvironmentNotification {
        deadline: Instant::now() + Duration::from_secs(2),
        complete: true,
        text: wide(OsStr::new("Environment")),
    };
    let result = unsafe { EnumWindows(Some(notify_window), &mut state as *mut _ as LPARAM) };
    result != 0 && state.complete
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;

    /// Remove only the unique registry key owned by one isolated installer test.
    struct TestRegistryKey(String);

    impl Drop for TestRegistryKey {
        /// Clean the isolated test key even if a registry assertion fails.
        fn drop(&mut self) {
            unsafe {
                windows_sys::Win32::System::Registry::RegDeleteKeyW(
                    HKEY_CURRENT_USER,
                    wide(OsStr::new(&self.0)).as_ptr(),
                );
            }
        }
    }

    /// Registry string reads retain expansion syntax and the original storage type.
    #[test]
    fn registry_round_trip_preserves_expandable_path_type() {
        let cleanup = TestRegistryKey(format!(
            r"Software\BTLang.InstallTest.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let key = RegistryKey::create(&cleanup.0).unwrap();
        assert_eq!(key.read("Path").unwrap(), None);
        let original = r"%SystemRoot%\System32;C:\Tools";
        key.write("Path", original, REG_EXPAND_SZ).unwrap();
        let (value, kind) = key.read("Path").unwrap().unwrap();
        assert_eq!((value.as_str(), kind), (original, REG_EXPAND_SZ));
        let appended = append_path(&value, r"C:\Users\A B\.bt\bin", true)
            .unwrap()
            .unwrap();
        key.write("Path", &appended, kind).unwrap();
        assert_eq!(key.read("Path").unwrap(), Some((appended, REG_EXPAND_SZ)));
        key.write("Path", original, REG_SZ).unwrap();
        assert_eq!(
            key.read("Path").unwrap(),
            Some((original.to_string(), REG_SZ))
        );
        drop(key);
        drop(cleanup);
    }

    /// Existing expansion references and PATH contents survive installation unchanged.
    #[test]
    fn path_preserves_existing_entries_and_avoids_duplicates() {
        let directory = r"C:\Users\A B\.bt\bin";
        assert_eq!(
            append_path(r"%SystemRoot%\System32;C:\Tools", directory, true).unwrap(),
            Some(format!(r"%SystemRoot%\System32;C:\Tools;{directory}"))
        );
        assert_eq!(
            append_path(r#"C:\Tools;"c:/users/a b/.bt/bin/""#, directory, false).unwrap(),
            None
        );
        assert_eq!(
            append_path("", directory, true).unwrap(),
            Some(directory.to_string())
        );
        assert!(append_path(&"x".repeat(32766), directory, false).is_err());
        if let Ok(home) = std::env::var("USERPROFILE") {
            assert_eq!(
                append_path(r"%USERPROFILE%\.bt\bin", &format!(r"{home}\.bt\bin"), true).unwrap(),
                None
            );
        }
    }

    /// Association commands preserve spaces and metacharacters as one direct executable path.
    #[test]
    fn associations_quote_paths_and_do_not_write_user_choice() {
        let entries = association_entries(r"C:\Users\A & B\.bt\bin\bt.exe");
        let command = entries
            .iter()
            .find(|(key, _, _)| key.ends_with(r"BTLang.Script\shell\open\command"))
            .unwrap();
        assert_eq!(
            command.2,
            r#""C:\Users\A & B\.bt\bin\bt.exe" --open-script "%1""#
        );
        assert!(entries
            .iter()
            .all(|(key, _, _)| !key.contains("UserChoice")));
        assert!(installation_path(Path::new(r"C:\Users\A;B\.bt\bin")).is_err());
        assert!(installation_path(Path::new("relative\\bt.exe")).is_err());
    }

    /// Same-directory replacement preserves the previous binary when Windows denies sharing.
    #[test]
    fn replacement_is_complete_or_preserves_locked_target() {
        let directory = std::env::temp_dir().join(format!(
            "bt-install-replace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let target = directory.join("bt.exe");
        let staged = directory.join("staged.exe");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        assert!(replace(&staged, &target).is_err());
        drop(locked);
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert_eq!(std::fs::read(&staged).unwrap(), b"new");
        replace(&staged, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!staged.exists());
        assert!(replace(&target, &target).is_err());
        assert!(replace(&staged, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }
}
