//! Explicit, per-user interpreter installation; ordinary execution never enters this module.

#[cfg(unix)]
#[path = "install/unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "install/windows.rs"]
mod windows;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

use semver::Version;
use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Installs this binary and refreshes user integration only on an explicit request.
pub(crate) fn run() -> Result<(), String> {
    let source = std::env::current_exe().map_err(|e| format!("Cannot locate BT: {e}"))?;
    let directory = platform::install_dir()?;
    fs::create_dir_all(&directory).map_err(|e| format!("Cannot create install directory: {e}"))?;
    // The OS releases this lock even after a crash. Keep its inode in place to prevent
    // another installer from locking a different file while this guard is still live.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".bt-install.lock"))
        .map_err(|e| format!("Cannot open installation lock: {e}"))?;
    lock.try_lock()
        .map_err(|e| format!("Another BT installation may be running: {e}"))?;
    let destination = directory.join(if cfg!(windows) { "bt.exe" } else { "bt" });
    let current = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?;
    let outcome = install_binary(&source, &destination, &current, read_version)?;
    println!("{outcome}: {}", destination.display());
    for message in platform::integrate(&destination)? {
        println!("{message}");
    }
    println!("BT user installation is ready. Open a new terminal to use the updated PATH.");
    Ok(())
}

/// Owns an exclusively created staging file and removes it on every return path.
struct TemporaryFile {
    /// Path owned by this operation, never an existing user file.
    path: PathBuf,
}

impl TemporaryFile {
    /// Creates an unpredictable sibling without following an existing symlink.
    fn create(directory: &Path, extension: &str) -> Result<(Self, File), String> {
        let path = directory.join(format!(
            ".bt-install-{}.{}",
            uuid::Uuid::new_v4(),
            extension
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Cannot create installation temporary file: {e}"))?;
        Ok((Self { path }, file))
    }
}

impl Drop for TemporaryFile {
    /// Removes only the exact file created by this operation.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Keeps equal/newer installations and verifies a complete staged copy before replacement.
fn install_binary(
    source: &Path,
    destination: &Path,
    current: &Version,
    version_of: impl Fn(&Path) -> Result<Version, String>,
) -> Result<String, String> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(format!(
                    "Installation target is not a regular file: {}",
                    destination.display()
                ));
            }
            let installed = version_of(destination)?;
            if current.cmp_precedence(&installed) != Ordering::Greater {
                return Ok(format!(
                    "Kept installed BT {installed} (this interpreter is {current})"
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Cannot inspect installed BT: {e}")),
    }
    let directory = destination
        .parent()
        .ok_or("Installation target has no parent directory")?;
    let (staged, mut output) =
        TemporaryFile::create(directory, if cfg!(windows) { "exe" } else { "bin" })?;
    let mut input =
        File::open(source).map_err(|e| format!("Cannot read current BT executable: {e}"))?;
    std::io::copy(&mut input, &mut output)
        .map_err(|e| format!("Cannot copy BT executable: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Cannot set BT permissions: {e}"))?;
    }
    output
        .sync_all()
        .map_err(|e| format!("Cannot flush BT executable: {e}"))?;
    drop(output);
    if version_of(&staged.path)? != *current {
        return Err(
            "Copied BT version does not match this interpreter; installation was not replaced"
                .into(),
        );
    }
    platform::replace(&staged.path, destination)?;
    Ok(format!("Installed BT {current}"))
}

/// Checks response size and enforces a deadline while probing an installed executable.
fn read_version(executable: &Path) -> Result<Version, String> {
    let directory = executable
        .parent()
        .ok_or("BT executable has no parent directory")?;
    let (capture, mut file) = TemporaryFile::create(directory, "version")?;
    let mut command = Command::new(executable);
    command
        .arg("-v")
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("Cannot query BT version at {}: {e}", executable.display()))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Cannot query installed BT version: {e}"));
            }
        }
        if Instant::now() >= deadline || file.metadata().map(|m| m.len() > 1024).unwrap_or(true) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("BT version query timed out or exceeded its output limit; installed file was preserved".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(format!(
            "BT version query failed: {status}; installed file was preserved"
        ));
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut output = String::new();
    file.take(1025)
        .read_to_string(&mut output)
        .map_err(|e| format!("Cannot read BT version: {e}"))?;
    drop(capture);
    parse_version_output(&output)
}

/// Accepts the BT `-v` response without guessing the version of an unknown executable.
fn parse_version_output(output: &str) -> Result<Version, String> {
    if output.len() > 1024 {
        return Err("BT version response is too large; installed file was preserved".into());
    }
    let value = output.trim();
    Version::parse(value.strip_prefix('v').unwrap_or(value)).map_err(|e| {
        format!("Cannot recognize installed BT version: {e}; installed file was preserved")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Uses an isolated directory and simulated version probes to exercise actual file replacement.
    #[test]
    fn installation_preserves_newer_versions_and_replaces_only_verified_copies() {
        let directory =
            std::env::temp_dir().join(format!("bt-install-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("download");
        let target = directory.join("bt-target");
        fs::write(&source, "1.2.0").unwrap();
        let probe = |path: &Path| parse_version_output(&fs::read_to_string(path).unwrap());
        let current = Version::parse("1.2.0").unwrap();
        assert!(install_binary(&source, &target, &current, probe)
            .unwrap()
            .starts_with("Installed"));
        fs::write(&source, "broken").unwrap();
        assert!(install_binary(&source, &target, &current, probe)
            .unwrap()
            .starts_with("Kept"));
        assert!(
            install_binary(&source, &target, &Version::parse("1.1.0").unwrap(), probe)
                .unwrap()
                .starts_with("Kept")
        );
        assert!(
            install_binary(&source, &target, &Version::parse("1.3.0").unwrap(), probe).is_err()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "1.2.0");
        fs::write(&source, "1.10.0").unwrap();
        install_binary(&source, &target, &Version::parse("1.10.0").unwrap(), probe).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "1.10.0");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    /// Semantic precedence is numeric, understands prereleases, and ignores build metadata.
    #[test]
    fn semantic_versions_and_invalid_responses() {
        assert!(parse_version_output("v1.10.0\r\n").unwrap() > Version::parse("1.9.0").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("1.0.0-rc.1").unwrap());
        assert_eq!(
            Version::parse("1.0.0+one")
                .unwrap()
                .cmp_precedence(&Version::parse("1.0.0+two").unwrap()),
            Ordering::Equal
        );
        assert!(parse_version_output("1.2.3\nextra").is_err());
        assert!(parse_version_output("unknown").is_err());
    }
}
