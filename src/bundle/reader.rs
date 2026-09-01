use crate::bundle::footer::read_bundle_from_exe;
use crate::bundle::vfs::VirtualFileSystem;
use crate::error::BtError;
use std::path::Path;

/// Read a Bundle from the end of an executable and parse it as a VFS.
#[allow(dead_code)]
pub fn read_vfs_from_exe(exe_path: &Path) -> Result<Option<VirtualFileSystem>, BtError> {
    let Some(bundle) = read_bundle_from_exe(exe_path)? else {
        return Ok(None);
    };
    Ok(Some(VirtualFileSystem::from_bundle(&bundle)?))
}
