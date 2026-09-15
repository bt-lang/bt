//! Isolated Windows installer acceptance harness; never changes the real user PATH or associations.
//!
//! Compile with the repository's semver, uuid, and windows-sys dependencies and
//! CARGO_PKG_VERSION set to the root package version. Run the resulting executable
//! as a separate process. Its predefined registry handles are redirected only
//! inside this process, and all files reside in its unique temporary directory.

#[path = "../../src/install.rs"]
mod install;

use std::ffi::OsStr;
use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegOpenKeyExW,
    RegOverridePredefKey, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CLASSES_ROOT,
    HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ,
};

/// Own all test state and restore predefined handles before removing the isolated hive.
struct Sandbox {
    /// Unique temporary user profile containing only this test's files.
    directory: PathBuf,
    /// Unique key under the real current-user hive, never a production settings key.
    registry_path: String,
    /// The replacement current-user root retained for the entire test.
    root: HKEY,
    /// The replacement merged classes root retained for the entire test.
    classes: HKEY,
}

impl Sandbox {
    /// Prepare isolated filesystem and registry roots before invoking installation.
    fn create() -> Result<Self, String> {
        let id = uuid::Uuid::new_v4();
        let directory = std::env::temp_dir().join(format!("bt-install-acceptance-{id}"));
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let registry_path = format!(r"Software\BTLang.InstallAcceptance.{id}");
        let root = create_key(HKEY_CURRENT_USER, &registry_path)?;
        let classes = create_key(root, r"Software\Classes")?;
        check(unsafe { RegOverridePredefKey(HKEY_CURRENT_USER, root) })?;
        let sandbox = Self {
            directory,
            registry_path,
            root,
            classes,
        };
        check(unsafe { RegOverridePredefKey(HKEY_CLASSES_ROOT, classes) })?;
        std::env::set_var("USERPROFILE", &sandbox.directory);
        Ok(sandbox)
    }
}

impl Drop for Sandbox {
    /// Undo process-local overrides and delete only the exact test-owned roots.
    fn drop(&mut self) {
        unsafe {
            RegOverridePredefKey(HKEY_CLASSES_ROOT, std::ptr::null_mut());
            RegOverridePredefKey(HKEY_CURRENT_USER, std::ptr::null_mut());
            RegCloseKey(self.classes);
            RegCloseKey(self.root);
            RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.registry_path).as_ptr());
            RegDeleteKeyW(HKEY_CURRENT_USER, wide(&self.registry_path).as_ptr());
        }
        // This directory was created by this process from a fixed prefix and UUID.
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Supply deterministic probe versions, otherwise execute the isolated acceptance checks.
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--hold") {
        std::thread::sleep(std::time::Duration::from_secs(60));
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("-v") {
        let is_installed =
            std::env::current_exe().unwrap().file_name() == Some(OsStr::new("bt.exe"));
        let version = if is_installed {
            std::env::var("BT_INSTALL_TEST_INSTALLED_VERSION")
                .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
        } else {
            env!("CARGO_PKG_VERSION").to_string()
        };
        println!("v{version}");
        return;
    }
    if let Err(error) = verify() {
        eprintln!("Windows installation acceptance failed: {error}");
        std::process::exit(1);
    }
    println!("Windows installation acceptance passed; isolated state was removed.");
}

/// Exercise complete first install, integration, idempotence, upgrade, and preservation paths.
fn verify() -> Result<(), String> {
    let sandbox = Sandbox::create()?;
    let environment = create_key(HKEY_CURRENT_USER, "Environment")?;
    let existing_path = r"%SystemRoot%\System32;C:\Existing Tools";
    write_value(environment, "Path", existing_path, REG_EXPAND_SZ)?;
    unsafe {
        RegCloseKey(environment);
    }
    install::run()?;
    let installed = sandbox.directory.join(".bt").join("bin").join("bt.exe");
    let output = Command::new(&installed)
        .arg("-v")
        .output()
        .map_err(|error| error.to_string())?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("v{}", env!("CARGO_PKG_VERSION"))
    );
    let (path, kind) = read_value("Environment", "Path")?;
    assert_eq!(kind, REG_EXPAND_SZ);
    assert_eq!(
        path,
        format!("{existing_path};{}", installed.parent().unwrap().display())
    );
    assert_eq!(
        read_value(r"Software\Classes\BTLang.Script\shell\open\command", "")?.0,
        format!("\"{}\" --open-script \"%1\"", installed.display())
    );
    assert_eq!(read_value(r"Software\Classes\.bt", "")?.0, "BTLang.Script");
    assert_eq!(
        read_value(r"Software\BTLang\Capabilities\FileAssociations", ".bt")?.0,
        "BTLang.Script"
    );
    let baseline = fs::read(&installed).map_err(|error| error.to_string())?;
    let modified = fs::metadata(&installed).unwrap().modified().unwrap();
    install::run()?;
    assert_eq!(
        fs::metadata(&installed).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(read_value("Environment", "Path")?.0, path);

    // A different existing default remains intact, including protected UserChoice.
    let extension = create_key(HKEY_CURRENT_USER, r"Software\Classes\.bt")?;
    write_value(extension, "", "Existing.Editor", REG_SZ)?;
    unsafe {
        RegCloseKey(extension);
    }
    let choice_path = r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.bt\UserChoice";
    let choice = create_key(HKEY_CURRENT_USER, choice_path)?;
    write_value(choice, "ProgId", "Existing.Editor", REG_SZ)?;
    write_value(choice, "Hash", "PreserveThisHash", REG_SZ)?;
    unsafe {
        RegCloseKey(choice);
    }
    std::env::set_var("BT_INSTALL_TEST_INSTALLED_VERSION", "999999.0.0");
    install::run()?;
    assert_eq!(
        fs::metadata(&installed).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(
        read_value(r"Software\Classes\.bt", "")?.0,
        "Existing.Editor"
    );
    assert_eq!(read_value(choice_path, "ProgId")?.0, "Existing.Editor");
    assert_eq!(read_value(choice_path, "Hash")?.0, "PreserveThisHash");

    std::env::set_var("BT_INSTALL_TEST_INSTALLED_VERSION", "0.0.0");
    // A real running PE image must survive a failed update, and stopping only
    // this test-owned child must make the same upgrade succeed on retry.
    use std::os::windows::process::CommandExt;
    let mut running = Command::new(&installed)
        .arg("--hold")
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .spawn()
        .map_err(|error| error.to_string())?;
    let replacement = install::run();
    let _ = running.kill();
    let _ = running.wait();
    assert!(replacement.is_err());
    assert_eq!(fs::read(&installed).unwrap(), baseline);
    assert_no_temporary_files(installed.parent().unwrap());
    install::run()?;
    assert_eq!(fs::read(&installed).unwrap(), baseline);
    std::env::set_var("BT_INSTALL_TEST_INSTALLED_VERSION", "invalid");
    assert!(install::run().is_err());
    assert_eq!(fs::read(&installed).unwrap(), baseline);
    std::env::remove_var("BT_INSTALL_TEST_INSTALLED_VERSION");

    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(installed.parent().unwrap().join(".bt-install.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(install::run().is_err());
    drop(lock);
    assert_no_temporary_files(installed.parent().unwrap());
    drop(sandbox);
    Ok(())
}

/// Verify probe files and staging executables are removed after all success/error paths.
fn assert_no_temporary_files(directory: &Path) {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [OsStr::new(".bt-install.lock"), OsStr::new("bt.exe")]
    );
}

/// Create a key beneath an explicit root using only the separate process's test hive.
fn create_key(root: HKEY, path: &str) -> Result<HKEY, String> {
    let mut result = std::ptr::null_mut();
    check(unsafe {
        RegCreateKeyExW(
            root,
            wide(path).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            std::ptr::null(),
            &mut result,
            std::ptr::null_mut(),
        )
    })?;
    Ok(result)
}

/// Write a test string to a test-owned key.
fn write_value(key: HKEY, name: &str, value: &str, kind: u32) -> Result<(), String> {
    let data = wide(value);
    check(unsafe {
        RegSetValueExW(
            key,
            wide(name).as_ptr(),
            0,
            kind,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    })
}

/// Read a bounded test value and return its stored registry type for assertions.
fn read_value(path: &str, name: &str) -> Result<(String, u32), String> {
    let mut key = std::ptr::null_mut();
    check(unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            KEY_READ,
            &mut key,
        )
    })?;
    let mut data = [0u16; 32768];
    let mut length = (data.len() * 2) as u32;
    let mut kind = 0;
    let result = unsafe {
        RegQueryValueExW(
            key,
            wide(name).as_ptr(),
            std::ptr::null(),
            &mut kind,
            data.as_mut_ptr().cast(),
            &mut length,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    check(result)?;
    Ok((
        String::from_utf16(&data[..length as usize / 2 - 1]).unwrap(),
        kind,
    ))
}

/// Convert a Win32 failure into a visible acceptance failure.
fn check(status: u32) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(status as i32).to_string())
    }
}

/// Terminate a UTF-16 Win32 string without shell interpolation.
fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
