use crate::error::BtError;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Magic value at the end of a BT desktop application Bundle.
pub const BUNDLE_MAGIC: &[u8; 16] = b"BT_APP_BUNDLE_V1";

/// Check whether a BT desktop application Bundle is already appended to the executable.
pub fn has_bundle_injected(exe_path: &Path) -> bool {
    let mut file = match File::open(exe_path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return false,
    };
    if len < BUNDLE_MAGIC.len() as u64 {
        return false;
    }
    if file
        .seek(SeekFrom::End(-(BUNDLE_MAGIC.len() as i64)))
        .is_err()
    {
        return false;
    }
    let mut magic = [0u8; 16];
    file.read_exact(&mut magic).is_ok() && &magic == BUNDLE_MAGIC
}

/// Append Bundle data to the end of an executable.
///
/// The footer format is fixed: `[bundle][bundle_size:u64 little endian][magic:16 bytes]`.
pub fn inject_bundle(exe_path: &Path, bundle: &[u8]) -> Result<(), BtError> {
    if has_bundle_injected(exe_path) {
        return Err(BtError::Bundle(format!(
            "The target file already contains a Bundle: {}",
            exe_path.display()
        )));
    }

    let mut file = OpenOptions::new().append(true).open(exe_path)?;
    file.write_all(bundle)?;
    file.write_all(&(bundle.len() as u64).to_le_bytes())?;
    file.write_all(BUNDLE_MAGIC)?;
    file.flush()?;
    Ok(())
}

/// Read Bundle data from the end of an executable.
pub fn read_bundle_from_exe(exe_path: &Path) -> Result<Option<Vec<u8>>, BtError> {
    let mut file = File::open(exe_path)?;
    let len = file.metadata()?.len();
    let footer_len = BUNDLE_MAGIC.len() as u64 + 8;
    if len < footer_len {
        return Ok(None);
    }

    file.seek(SeekFrom::End(-(BUNDLE_MAGIC.len() as i64)))?;
    let mut magic = [0u8; 16];
    file.read_exact(&mut magic)?;
    if &magic != BUNDLE_MAGIC {
        return Ok(None);
    }

    file.seek(SeekFrom::End(-(footer_len as i64)))?;
    let mut size_bytes = [0u8; 8];
    file.read_exact(&mut size_bytes)?;
    let bundle_size = u64::from_le_bytes(size_bytes);
    if bundle_size > len - footer_len {
        return Err(BtError::Bundle(format!(
            "Invalid Bundle size: {} bytes; file size: {} bytes",
            bundle_size, len
        )));
    }

    let start = len - footer_len - bundle_size;
    let size = usize::try_from(bundle_size).map_err(|_| {
        BtError::Bundle("Bundle is too large to read in one operation on this platform".to_string())
    })?;
    let mut bundle = vec![0u8; size];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bundle)?;
    Ok(Some(bundle))
}
