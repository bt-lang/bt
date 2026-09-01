use crate::app::config::AppJson;
use crate::app::resource::ResourceSource;
use crate::error::BtError;
use std::path::{Path, PathBuf};
use tauri::image::Image;

/// The bt_app runtime default icon must share the same source as the exe resource icon.
const DEFAULT_BT_APP_ICON_BYTES: &[u8] = include_bytes!("../../src-tauri/icons/app.ico");

/// Fixed resource group ID used for the application's main icon in Windows PE.
#[cfg(any(windows, test))]
const APP_ICON_GROUP_ID: u16 = 1;

/// Resource group ID used for the first file-association icon in Windows PE.
const FILE_ASSOCIATION_ICON_GROUP_ID_BASE: usize = 1000;

/// Load the runtime window icon.
///
/// Use the project icon when `app.icon` is configured and the resource exists; when it is not configured or the resource is missing during development,
/// fall back to the compile-time built-in `src-tauri/icons/app.ico` so the Tauri default icon never reaches the window or taskbar.
pub fn load_window_icon(
    resource: &ResourceSource,
    config: &AppJson,
) -> Result<Image<'static>, BtError> {
    if let Some(icon) = config.app.icon.as_deref() {
        if resource.exists(icon) {
            let bytes = resource.read(icon)?;
            return decode_window_icon(&bytes, icon);
        }
    }
    decode_window_icon(DEFAULT_BT_APP_ICON_BYTES, "src-tauri/icons/app.ico")
}

/// Decode a window icon and convert it into an image object that Tauri can keep alive.
fn decode_window_icon(bytes: &[u8], label: &str) -> Result<Image<'static>, BtError> {
    let image = Image::from_bytes(bytes)
        .map_err(|err| BtError::Config(format!("Failed to read app.icon `{}`: {}", label, err)))?;
    Ok(image.to_owned())
}

/// Validate the build-time icon configuration and return the real file path.
pub fn build_icon_path(project_dir: &Path, config: &AppJson) -> Result<Option<PathBuf>, BtError> {
    let Some(icon) = config.app.icon.as_deref() else {
        return Ok(None);
    };
    let path = project_dir.join(icon);
    if !path.is_file() {
        return Err(BtError::Config(format!(
            "The configured app.icon file does not exist: {}",
            icon
        )));
    }
    Ok(Some(path))
}

/// Return the stable icon resource group ID used by the specified file association in Windows PE.
pub fn file_association_icon_group_id(index: usize) -> Result<u16, BtError> {
    let id = FILE_ASSOCIATION_ICON_GROUP_ID_BASE
        .checked_add(index)
        .ok_or_else(|| BtError::Config("File-association icon resource ID overflow".to_string()))?;
    u16::try_from(id).map_err(|_| BtError::Config("Too many file-association icons".to_string()))
}

/// Validate a file-association icon file and return the PE resource group ID and real path.
pub fn build_file_association_icon_paths(
    project_dir: &Path,
    config: &AppJson,
) -> Result<Vec<(u16, PathBuf)>, BtError> {
    let mut icons = Vec::new();
    for (index, association) in config.app.file_associations.iter().enumerate() {
        let Some(icon) = association.icon.as_deref() else {
            continue;
        };
        let path = project_dir.join(icon);
        if !path.is_file() {
            return Err(BtError::Config(format!(
                "The configured app.file_associations[{}].icon file does not exist: {}",
                index, icon
            )));
        }
        icons.push((file_association_icon_group_id(index)?, path));
    }
    Ok(icons)
}

/// Write the app main icon and file-association icons into separate resource groups in the packaged exe.
#[cfg(windows)]
pub fn apply_exe_icons(
    output: &Path,
    app_icon: Option<&Path>,
    association_icons: &[(u16, PathBuf)],
) -> Result<(), BtError> {
    let mut icons = Vec::with_capacity(association_icons.len() + usize::from(app_icon.is_some()));
    if let Some(icon) = app_icon {
        icons.push((APP_ICON_GROUP_ID, icon));
    }
    icons.extend(
        association_icons
            .iter()
            .map(|(group_id, path)| (*group_id, path.as_path())),
    );
    if icons.is_empty() {
        return Ok(());
    }
    exe_resource::apply_exe_icons(output, &icons)
}

/// Windows PE icon resource writing implementation.
#[cfg(windows)]
mod exe_resource {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::{
        BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{RT_GROUP_ICON, RT_ICON};

    /// A single ICO resource group with resolved PE resource IDs.
    struct ParsedIconResource {
        /// Resource group ID used by `RT_GROUP_ICON`.
        group_id: u16,
        /// Multi-size images inside the ICO.
        images: Vec<IcoImage>,
        /// `RT_ICON` resource ID for each image.
        image_ids: Vec<u16>,
        /// Serialized `RT_GROUP_ICON` data.
        group: Vec<u8>,
    }

    /// Write the app main icon and all file-association icons in a single PE resource update transaction.
    pub fn apply_exe_icons(output: &Path, icons: &[(u16, &Path)]) -> Result<(), BtError> {
        let mut parsed = Vec::with_capacity(icons.len());
        let mut image_index = 0usize;
        for (group_id, icon) in icons {
            let bytes = std::fs::read(icon).map_err(|err| {
                BtError::Io(std::io::Error::new(
                    err.kind(),
                    format!("Failed to read icon `{}`: {}", icon.display(), err),
                ))
            })?;
            let images = parse_ico(&bytes)?;
            let mut image_ids = Vec::with_capacity(images.len());
            for _ in &images {
                image_ids.push(icon_resource_id(image_index)?);
                image_index = image_index
                    .checked_add(1)
                    .ok_or_else(|| BtError::Bundle("Icon image count overflow".to_string()))?;
            }
            let group = build_group_icon_resource(&images, &image_ids)?;
            parsed.push(ParsedIconResource {
                group_id: *group_id,
                images,
                image_ids,
                group,
            });
        }
        let wide_output = wide_path(output);

        unsafe {
            let handle = BeginUpdateResourceW(wide_output.as_ptr(), 0);
            if handle.is_null() {
                return Err(last_resource_error(
                    "failed to start updating exe icon resources",
                ));
            }

            for resource in &parsed {
                for (image, image_id) in resource.images.iter().zip(&resource.image_ids) {
                    if UpdateResourceW(
                        handle,
                        RT_ICON,
                        int_resource(*image_id),
                        0,
                        image.data.as_ptr().cast(),
                        u32::try_from(image.data.len()).map_err(|_| {
                            BtError::Bundle("Icon image resource is too large".to_string())
                        })?,
                    ) == 0
                    {
                        let err = last_resource_error("failed to write exe image resources");
                        EndUpdateResourceW(handle, 1);
                        return Err(err);
                    }
                }

                if UpdateResourceW(
                    handle,
                    RT_GROUP_ICON,
                    int_resource(resource.group_id),
                    0,
                    resource.group.as_ptr().cast(),
                    u32::try_from(resource.group.len()).map_err(|_| {
                        BtError::Bundle("Icon group resource is too large".to_string())
                    })?,
                ) == 0
                {
                    let err = last_resource_error("failed to write exe icon group resources");
                    EndUpdateResourceW(handle, 1);
                    return Err(err);
                }
            }

            if EndUpdateResourceW(handle, 0) == 0 {
                return Err(last_resource_error("Failed to save EXE icon resources"));
            }
        }

        Ok(())
    }

    /// Integer resource name used by the Windows API.
    fn int_resource(id: u16) -> windows_sys::core::PCWSTR {
        id as usize as windows_sys::core::PCWSTR
    }

    /// Generate stable `RT_ICON` resource IDs and avoid the low-numbered defaults.
    fn icon_resource_id(index: usize) -> Result<u16, BtError> {
        let id = 200usize
            .checked_add(index)
            .ok_or_else(|| BtError::Bundle("Icon resource ID overflow".to_string()))?;
        u16::try_from(id).map_err(|_| BtError::Bundle("Too many icon images".to_string()))
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

    /// A single image in an ICO file.
    #[derive(Debug, Clone)]
    struct IcoImage {
        /// Width byte, where 0 means 256.
        width: u8,
        /// Height byte, where 0 means 256.
        height: u8,
        /// Number of colors, usually 0.
        color_count: u8,
        /// Reserved byte, must be 0.
        reserved: u8,
        /// The ICO entry's `planes` field.
        planes: u16,
        /// The ICO entry's `bit_count` field.
        bit_count: u16,
        /// Raw image data.
        data: Vec<u8>,
    }

    /// Parse the image entries in an ICO file.
    fn parse_ico(bytes: &[u8]) -> Result<Vec<IcoImage>, BtError> {
        if bytes.len() < 6 {
            return Err(BtError::Bundle("Icon file is not a valid ICO".to_string()));
        }
        let reserved = read_u16(bytes, 0)?;
        let image_type = read_u16(bytes, 2)?;
        let count = read_u16(bytes, 4)? as usize;
        if reserved != 0 || image_type != 1 || count == 0 {
            return Err(BtError::Bundle(
                "icon file is not a valid ICO icon".to_string(),
            ));
        }

        let table_len = 6usize
            .checked_add(count.saturating_mul(16))
            .ok_or_else(|| BtError::Bundle("Icon directory is too large".to_string()))?;
        if table_len > bytes.len() {
            return Err(BtError::Bundle(
                "icon directory data is truncated".to_string(),
            ));
        }

        let mut images = Vec::with_capacity(count);
        for index in 0..count {
            let offset = 6 + index * 16;
            let width = bytes[offset];
            let height = bytes[offset + 1];
            let color_count = bytes[offset + 2];
            let reserved = bytes[offset + 3];
            let planes = read_u16(bytes, offset + 4)?;
            let bit_count = read_u16(bytes, offset + 6)?;
            let bytes_in_res = read_u32(bytes, offset + 8)? as usize;
            let image_offset = read_u32(bytes, offset + 12)? as usize;
            if reserved != 0 || bytes_in_res == 0 {
                return Err(BtError::Bundle("Invalid icon image entry".to_string()));
            }
            let end = image_offset
                .checked_add(bytes_in_res)
                .ok_or_else(|| BtError::Bundle("Icon image offset overflow".to_string()))?;
            if end > bytes.len() {
                return Err(BtError::Bundle("Icon image data is truncated".to_string()));
            }
            images.push(IcoImage {
                width,
                height,
                color_count,
                reserved,
                planes,
                bit_count,
                data: bytes[image_offset..end].to_vec(),
            });
        }
        Ok(images)
    }

    /// Build `RT_GROUP_ICON` resource contents using explicit `RT_ICON` IDs.
    fn build_group_icon_resource(
        images: &[IcoImage],
        image_ids: &[u16],
    ) -> Result<Vec<u8>, BtError> {
        if images.len() != image_ids.len() {
            return Err(BtError::Bundle(
                "Icon image count does not match resource ID count".to_string(),
            ));
        }
        let count = u16::try_from(images.len())
            .map_err(|_| BtError::Bundle("Icon image count exceeds u16".to_string()))?;
        let mut output = Vec::with_capacity(6 + images.len() * 14);
        output.extend_from_slice(&0u16.to_le_bytes());
        output.extend_from_slice(&1u16.to_le_bytes());
        output.extend_from_slice(&count.to_le_bytes());
        for (image, image_id) in images.iter().zip(image_ids) {
            output.push(image.width);
            output.push(image.height);
            output.push(image.color_count);
            output.push(image.reserved);
            output.extend_from_slice(&image.planes.to_le_bytes());
            output.extend_from_slice(&image.bit_count.to_le_bytes());
            output.extend_from_slice(
                &u32::try_from(image.data.len())
                    .map_err(|_| BtError::Bundle("Icon image resource is too large".to_string()))?
                    .to_le_bytes(),
            );
            output.extend_from_slice(&image_id.to_le_bytes());
        }
        Ok(output)
    }

    /// Read a little-endian u16 from the given offset.
    fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, BtError> {
        let slice = checked_slice(bytes, offset, 2)?;
        Ok(u16::from_le_bytes([slice[0], slice[1]]))
    }

    /// Read a little-endian u32 from the given offset.
    fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, BtError> {
        let slice = checked_slice(bytes, offset, 4)?;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    /// Get a byte slice for the requested range.
    fn checked_slice(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], BtError> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| BtError::Bundle("Icon offset overflow".to_string()))?;
        bytes
            .get(offset..end)
            .ok_or_else(|| BtError::Bundle("Icon data is truncated".to_string()))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// ICO parsing should read the image data and generate a group icon resource.
        #[test]
        fn parses_ico_and_builds_group_resource() {
            let mut ico = Vec::new();
            ico.extend_from_slice(&0u16.to_le_bytes());
            ico.extend_from_slice(&1u16.to_le_bytes());
            ico.extend_from_slice(&1u16.to_le_bytes());
            ico.extend_from_slice(&[16, 16, 0, 0]);
            ico.extend_from_slice(&1u16.to_le_bytes());
            ico.extend_from_slice(&32u16.to_le_bytes());
            ico.extend_from_slice(&4u32.to_le_bytes());
            ico.extend_from_slice(&22u32.to_le_bytes());
            ico.extend_from_slice(&[1, 2, 3, 4]);

            let images = parse_ico(&ico).unwrap();
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].width, 16);
            assert_eq!(images[0].data, vec![1, 2, 3, 4]);

            let group = build_group_icon_resource(&images, &[4321]).unwrap();
            assert_eq!(&group[0..6], &[0, 0, 1, 0, 1, 0]);
            assert_eq!(group.len(), 20);
            assert_eq!(&group[18..20], &4321u16.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default window icon should come directly from the compile-time built-in `src-tauri/icons/app.ico`.
    #[test]
    fn decodes_default_window_icon() {
        decode_window_icon(DEFAULT_BT_APP_ICON_BYTES, "src-tauri/icons/app.ico").unwrap();
    }

    /// File-association icon resource group IDs must be assigned stably in configuration order and avoid the app main icon.
    #[test]
    fn file_association_icon_group_ids_are_stable() {
        assert_eq!(file_association_icon_group_id(0).unwrap(), 1000);
        assert_eq!(file_association_icon_group_id(2).unwrap(), 1002);
        assert_ne!(
            file_association_icon_group_id(0).unwrap(),
            APP_ICON_GROUP_ID
        );
    }
}
