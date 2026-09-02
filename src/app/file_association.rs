//! Windows desktop file-association registration.
//!
//! Only packaged Bundle apps write to the current user's registry; development-directory runs do not register the generic
//! `bt-app.exe` as a business app. The registration scope is fixed to `HKCU\\Software\\Classes`, so administrator privileges are not required.

use crate::app::config::AppJson;
use crate::error::BtError;

/// Register the current generic bt-app runtime as the open handler for `.btr` files.
///
/// This only writes to the current user's registry and does not modify the Windows-protected `UserChoice`. If the user has already
/// chosen another default app for `.btr`, the system may still ask for one confirmation in "Open with".
#[cfg(windows)]
pub fn register_btr_runtime() -> Result<(), BtError> {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    let executable = std::env::current_exe()?;
    let executable = executable.to_string_lossy();
    let prog_id = "BTLang.BTR";
    let command = format!("\"{}\" run \"%1\"", executable);
    let icon = format!("\"{}\",0", executable);

    set_default_value(r"Software\Classes\.btr", prog_id)?;
    set_named_value(r"Software\Classes\.btr\OpenWithProgids", prog_id, "")?;
    set_default_value(&format!(r"Software\Classes\{}", prog_id), "BT BTR App")?;
    set_default_value(&format!(r"Software\Classes\{}\DefaultIcon", prog_id), &icon)?;
    set_default_value(
        &format!(r"Software\Classes\{}\shell\open\command", prog_id),
        &command,
    )?;

    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
    Ok(())
}

/// Non-Windows platforms do not currently support writing `.btr` file associations.
#[cfg(not(windows))]
pub fn register_btr_runtime() -> Result<(), BtError> {
    Err(BtError::Config(
        "bt-app associate currently only supports Windows".to_string(),
    ))
}

/// Register default open commands and context-menu entries for the configured extensions.
#[cfg(windows)]
pub fn register(config: &AppJson) -> Result<(), BtError> {
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    if config.app.file_associations.is_empty() {
        return Ok(());
    }

    let executable = std::env::current_exe()?;
    let executable = executable.to_string_lossy();
    let command = format!("\"{}\" \"%1\"", executable);
    let app_icon = format!("\"{}\",0", executable);
    let app_id = registry_id_fragment(config.app.identity_key());
    let verb_id = format!("BTApp.{}", app_id);

    for (association_index, association) in config.app.file_associations.iter().enumerate() {
        let description = association
            .description
            .clone()
            .unwrap_or_else(|| format!("{} Document", config.app.title));
        let context_menu = association
            .context_menu
            .clone()
            .unwrap_or_else(|| format!("Open with {}", config.app.title));
        let file_type_icon = if association.icon.is_some() {
            file_association_icon_location(&executable, association_index)?
        } else {
            app_icon.clone()
        };

        for extension in &association.extensions {
            let dotted_extension = format!(".{}", extension);
            let prog_id = format!("BTApp.{}.{}", app_id, extension);
            set_default_value(&format!(r"Software\Classes\{}", dotted_extension), &prog_id)?;
            set_named_value(
                &format!(r"Software\Classes\{}\OpenWithProgids", dotted_extension),
                &prog_id,
                "",
            )?;
            set_default_value(&format!(r"Software\Classes\{}", prog_id), &description)?;
            set_default_value(
                &format!(r"Software\Classes\{}\DefaultIcon", prog_id),
                &file_type_icon,
            )?;
            set_default_value(
                &format!(r"Software\Classes\{}\shell\open\command", prog_id),
                &command,
            )?;

            let menu_key = format!(
                r"Software\Classes\SystemFileAssociations\{}\shell\{}",
                dotted_extension, verb_id
            );
            set_default_value(&menu_key, &context_menu)?;
            // The context-menu action means "open with the app", so keep showing the app icon instead of a document-type icon.
            set_named_value(&menu_key, "Icon", &app_icon)?;
            set_default_value(&format!(r"{}\command", menu_key), &command)?;
        }
    }

    // File type information is cached by Explorer; broadcast once after all writes to avoid refreshing after each extension.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
    Ok(())
}

/// Build the location string Windows Shell uses to read a file-association icon via a negative resource ID.
#[cfg(any(windows, test))]
fn file_association_icon_location(
    executable: &str,
    association_index: usize,
) -> Result<String, BtError> {
    let group_id = crate::app::icon::file_association_icon_group_id(association_index)?;
    Ok(format!("\"{}\",-{}", executable, group_id))
}

/// Non-Windows platforms keep the same call boundary, but file-association configuration does not write to the system.
#[cfg(not(windows))]
pub fn register(_config: &AppJson) -> Result<(), BtError> {
    Ok(())
}

/// Convert the app name into a stable and safe registry ProgID fragment.
#[cfg(any(windows, test))]
fn registry_id_fragment(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }
    if result.is_empty() {
        result.push_str("App");
    }
    result
}

/// Create or open a current-user registry key and write the default string value.
#[cfg(windows)]
fn set_default_value(subkey: &str, value: &str) -> Result<(), BtError> {
    set_registry_value(subkey, None, value)
}

/// Create or open a current-user registry key and write a named string value.
#[cfg(windows)]
fn set_named_value(subkey: &str, name: &str, value: &str) -> Result<(), BtError> {
    set_registry_value(subkey, Some(name), value)
}

/// Write a `REG_SZ` with the Win32 Registry API and ensure every error path closes the handle.
#[cfg(windows)]
fn set_registry_value(subkey: &str, name: Option<&str>, value: &str) -> Result<(), BtError> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let subkey_wide = wide_null(subkey);
    let name_wide = name.map(wide_null);
    let value_wide = wide_null(value);
    let byte_len = value_wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| {
            BtError::Config("File-association registry value is too long".to_string())
        })?;
    let mut key: HKEY = std::ptr::null_mut();
    let create_status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey_wide.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if create_status != ERROR_SUCCESS {
        return Err(registry_error(
            "create file-association registry key",
            subkey,
            create_status,
        ));
    }

    let value_name = name_wide
        .as_ref()
        .map_or(std::ptr::null(), |wide| wide.as_ptr());
    let write_status = unsafe {
        RegSetValueExW(
            key,
            value_name,
            0,
            REG_SZ,
            value_wide.as_ptr().cast(),
            byte_len,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if write_status != ERROR_SUCCESS {
        return Err(registry_error(
            "write file-association registry value",
            subkey,
            write_status,
        ));
    }
    Ok(())
}

/// Build a file-association error with a system error code.
#[cfg(windows)]
fn registry_error(context: &str, subkey: &str, code: u32) -> BtError {
    BtError::Config(format!(
        "{} failed: HKCU\\{}: {}",
        context,
        subkey,
        std::io::Error::from_raw_os_error(code as i32)
    ))
}

/// Convert a Rust string to the NUL-terminated UTF-16 required by the Win32 API.
#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ProgID fragment must keep regular characters and replace characters such as plus signs that are outside registry separators.
    #[test]
    fn registry_id_replaces_special_characters() {
        assert_eq!(registry_id_fragment("M++"), "M__");
        assert_eq!(registry_id_fragment("my-app_2"), "my-app_2");
    }

    /// Standalone document icons must be addressed through a stable negative resource ID instead of depending on PE icon enumeration order.
    #[test]
    fn file_association_icon_uses_resource_id() {
        assert_eq!(
            file_association_icon_location(r"C:\Apps\M++.exe", 0).unwrap(),
            r#""C:\Apps\M++.exe",-1000"#
        );
        assert_eq!(
            file_association_icon_location(r"C:\Apps\M++.exe", 2).unwrap(),
            r#""C:\Apps\M++.exe",-1002"#
        );
    }
}
