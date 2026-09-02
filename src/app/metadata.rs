use crate::app::config::AppJson;
use crate::error::BtError;
use std::path::Path;

/// Language ID for Windows version resources, using en-US to match the default resource viewer.
const VERSION_LANG_ID: u16 = 0x0409;

/// Unicode code page for Windows version resources.
const VERSION_CODE_PAGE: u16 = 0x04b0;

/// Write application metadata into the packaged executable.
pub fn apply_exe_metadata(output: &Path, config: &AppJson) -> Result<(), BtError> {
    use windows_sys::Win32::System::LibraryLoader::{
        BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW,
    };

    let original_filename = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("BTApp.exe");
    let resource = build_version_resource(config, original_filename)?;
    let wide_output = wide_path(output);

    unsafe {
        let handle = BeginUpdateResourceW(wide_output.as_ptr(), 0);
        if handle.is_null() {
            return Err(last_resource_error(
                "failed to start updating exe version resources",
            ));
        }

        if UpdateResourceW(
            handle,
            int_resource(16),
            int_resource(1),
            VERSION_LANG_ID,
            resource.as_ptr().cast(),
            u32::try_from(resource.len())
                .map_err(|_| BtError::Bundle("Version resource is too large".to_string()))?,
        ) == 0
        {
            let err = last_resource_error("failed to write exe version resources");
            EndUpdateResourceW(handle, 1);
            return Err(err);
        }

        if EndUpdateResourceW(handle, 0) == 0 {
            return Err(last_resource_error("Failed to save EXE version resources"));
        }
    }

    Ok(())
}

/// Build the binary contents of a Windows `RT_VERSION` resource.
fn build_version_resource(config: &AppJson, original_filename: &str) -> Result<Vec<u8>, BtError> {
    let description = config
        .app
        .description
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&config.app.title);
    let product_name = if config.app.title.trim().is_empty() {
        config.app.name.as_str()
    } else {
        config.app.title.as_str()
    };
    let version = config.app.version.as_str();
    let version_numbers = parse_version_numbers(version);

    let mut output = Vec::with_capacity(512 + description.len() + product_name.len());
    let root = begin_version_block(&mut output, "VS_VERSION_INFO", 52, 0)?;
    append_fixed_file_info(&mut output, version_numbers);
    align_dword(&mut output);
    append_string_file_info(
        &mut output,
        &[
            ("FileDescription", description),
            ("FileVersion", version),
            ("InternalName", config.app.name.as_str()),
            ("OriginalFilename", original_filename),
            ("ProductName", product_name),
            ("ProductVersion", version),
        ],
        config.app.copyright.as_deref(),
    )?;
    append_var_file_info(&mut output)?;
    finish_block(&mut output, root)?;
    Ok(output)
}

/// Parse up to four version segments; unparseable segments are written as 0 into the fixed-size version fields.
fn parse_version_numbers(version: &str) -> [u16; 4] {
    let mut numbers = [0u16; 4];
    for (index, part) in version.split('.').take(4).enumerate() {
        let value = part
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if !value.is_empty() {
            numbers[index] = value.parse::<u16>().unwrap_or(u16::MAX);
        }
    }
    numbers
}

/// Append `VS_FIXEDFILEINFO` to provide the fixed-size version fields used by Windows property pages.
fn append_fixed_file_info(output: &mut Vec<u8>, version: [u16; 4]) {
    let file_ms = ((version[0] as u32) << 16) | version[1] as u32;
    let file_ls = ((version[2] as u32) << 16) | version[3] as u32;
    for value in [
        0xFEEF04BDu32,
        0x00010000,
        file_ms,
        file_ls,
        file_ms,
        file_ls,
        0x0000003F,
        0,
        0x00040004,
        0x00000001,
        0,
        0,
        0,
    ] {
        output.extend_from_slice(&value.to_le_bytes());
    }
}

/// Append the `StringFileInfo` node and all of its string key-value pairs.
fn append_string_file_info(
    output: &mut Vec<u8>,
    base_entries: &[(&str, &str)],
    copyright: Option<&str>,
) -> Result<(), BtError> {
    let start = begin_version_block(output, "StringFileInfo", 0, 1)?;
    let table_key = format!("{:04X}{:04X}", VERSION_LANG_ID, VERSION_CODE_PAGE);
    let table = begin_version_block(output, &table_key, 0, 1)?;
    for (key, value) in base_entries {
        append_string_entry(output, key, value)?;
    }
    if let Some(value) = copyright.filter(|value| !value.trim().is_empty()) {
        append_string_entry(output, "LegalCopyright", value)?;
    }
    finish_block(output, table)?;
    finish_block(output, start)?;
    Ok(())
}

/// Append a single Windows version resource string.
fn append_string_entry(output: &mut Vec<u8>, key: &str, value: &str) -> Result<(), BtError> {
    let value_len = u16::try_from(value.encode_utf16().count() + 1)
        .map_err(|_| BtError::Bundle(format!("Version resource field too long: {}", key)))?;
    let start = begin_version_block(output, key, value_len, 1)?;
    push_wide_z(output, value);
    align_dword(output);
    finish_block(output, start)
}

/// Append the `VarFileInfo` node to declare the language and code page used by the string table.
fn append_var_file_info(output: &mut Vec<u8>) -> Result<(), BtError> {
    let start = begin_version_block(output, "VarFileInfo", 0, 1)?;
    let translation = begin_version_block(output, "Translation", 4, 0)?;
    output.extend_from_slice(&VERSION_LANG_ID.to_le_bytes());
    output.extend_from_slice(&VERSION_CODE_PAGE.to_le_bytes());
    align_dword(output);
    finish_block(output, translation)?;
    finish_block(output, start)
}

/// Start a version resource block and reserve space for the length field.
fn begin_version_block(
    output: &mut Vec<u8>,
    key: &str,
    value_len: u16,
    value_type: u16,
) -> Result<usize, BtError> {
    let start = output.len();
    output.extend_from_slice(&0u16.to_le_bytes());
    output.extend_from_slice(&value_len.to_le_bytes());
    output.extend_from_slice(&value_type.to_le_bytes());
    push_wide_z(output, key);
    align_dword(output);
    Ok(start)
}

/// Finish a version resource block and backfill its length.
fn finish_block(output: &mut [u8], start: usize) -> Result<(), BtError> {
    let len = output.len().checked_sub(start).ok_or_else(|| {
        BtError::Bundle("Failed to calculate version resource length".to_string())
    })?;
    let len = u16::try_from(len)
        .map_err(|_| BtError::Bundle("Version resource is too large".to_string()))?;
    output[start..start + 2].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

/// Append a NUL-terminated UTF-16LE string.
fn push_wide_z(output: &mut Vec<u8>, value: &str) {
    for word in value.encode_utf16().chain(Some(0)) {
        output.extend_from_slice(&word.to_le_bytes());
    }
}

/// Pad the current position to a DWORD boundary.
fn align_dword(output: &mut Vec<u8>) {
    while output.len() % 4 != 0 {
        output.push(0);
    }
}

/// Integer resource name used by the Windows API.
fn int_resource(id: u16) -> windows_sys::core::PCWSTR {
    id as usize as windows_sys::core::PCWSTR
}

/// Convert a path to a Windows wide string.
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Build a system error for the resource update API.
fn last_resource_error(context: &str) -> BtError {
    use windows_sys::Win32::Foundation::GetLastError;

    let code = unsafe { GetLastError() };
    BtError::Bundle(format!(
        "{}: {}",
        context,
        std::io::Error::from_raw_os_error(code as i32)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `app.description` and `app.copyright` should be written into the version resource string table.
    #[test]
    fn builds_version_resource_with_app_metadata() {
        let mut config = AppJson::default();
        config.app.name = "MetaDemo".to_string();
        config.app.title = "Metadata Demo".to_string();
        config.app.version = "2.3.4.5".to_string();
        config.app.description = Some("File description".to_string());
        config.app.copyright = Some("Copyright 2026".to_string());

        let resource = build_version_resource(&config, "MetaDemo.exe").unwrap();

        assert_eq!(read_u16(&resource, 0), resource.len() as u16);
        assert!(contains_wide_text(&resource, "FileDescription"));
        assert!(contains_wide_text(&resource, "File description"));
        assert!(contains_wide_text(&resource, "LegalCopyright"));
        assert!(contains_wide_text(&resource, "Copyright 2026"));
    }

    /// The default description should fall back to the window title so user apps do not keep the bt-app engine description.
    #[test]
    fn version_resource_description_falls_back_to_title() {
        let mut config = AppJson::default();
        config.app.title = "Window title".to_string();

        let resource = build_version_resource(&config, "BTApp.exe").unwrap();

        assert!(contains_wide_text(&resource, "Window title"));
    }

    /// Read a little-endian u16 for tests.
    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
    }

    /// Check whether the resource contains the specified UTF-16LE text.
    fn contains_wide_text(bytes: &[u8], text: &str) -> bool {
        let mut needle = Vec::new();
        push_wide_z(&mut needle, text);
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
    }
}
