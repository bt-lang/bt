//! CLI regressions for explicit installation and desktop-only wait behavior.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Owns an isolated fixture directory without modifying real user installation settings.
struct Fixture(PathBuf);

impl Fixture {
    /// Creates a unique directory for script inputs and captured output.
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bt-cli-install-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    /// Starts BT with an invalid installation root, making accidental installation visible and harmless.
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bt"));
        command
            .current_dir(&self.0)
            .env("USERPROFILE", "relative-invalid-home")
            .env("HOME", "relative-invalid-home");
        command
    }
}

impl Drop for Fixture {
    /// Deletes only the unique directory created by this test.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Script, version, and no-argument startup stay independent of installation settings.
#[test]
fn ordinary_invocations_do_not_install() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("demo.bt"), "print 'ordinary-script-ok'").unwrap();
    let output = fixture.command().arg("demo.bt").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "ordinary-script-ok"
    );
    let output = fixture.command().arg("-v").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        concat!("v", env!("CARGO_PKG_VERSION"))
    );
    let output = fixture.command().stdin(Stdio::null()).output().unwrap();
    assert!(output.status.success());
    assert!(!String::from_utf8(output.stdout)
        .unwrap()
        .contains("Installation target"));
    assert!(!fixture.0.join("relative-invalid-home").exists());
}

/// Both explicit spellings enter interpreter installation and failures return a nonzero status.
#[test]
fn explicit_install_and_alias_validate_user_root() {
    let fixture = Fixture::new();
    for argument in ["install", "--install"] {
        let output = fixture.command().arg(argument).output().unwrap();
        assert!(!output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("absolute"), "{output}");
        assert!(!output.contains("extension support"));
    }
    let output = fixture
        .command()
        .args(["--install", "sqlite"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Usage: bt --install"));
    assert!(!fixture.0.join("relative-invalid-home").exists());
}

/// Interactive installation reports a failure and continues in the same interpreter session.
#[test]
fn interactive_install_keeps_the_current_session() {
    let fixture = Fixture::new();
    let mut child = fixture
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"install\n--install\n-v\n-e\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.matches("absolute").count(), 2, "{text}");
    assert!(text.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
    assert!(text.contains("bye"));
}

/// Installs a real executable and platform integration into an isolated Unix home.
#[cfg(unix)]
#[test]
fn unix_user_install_is_repeatable_and_keeps_newer_binary() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let home = fixture.0.join("home space ' $ 中文");
    fs::create_dir(&home).unwrap();
    let mut command = fixture.command();
    command
        .env("HOME", &home)
        .env("ZDOTDIR", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .arg("install");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let binary = home.join(if cfg!(target_os = "macos") {
        ".bt/bin/bt"
    } else {
        ".local/bin/bt"
    });
    let output = Command::new(&binary).arg("-v").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        concat!("v", env!("CARGO_PKG_VERSION"))
    );
    let profile = fs::read(home.join(".profile")).unwrap();
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("Kept installed BT"));
    assert_eq!(fs::read(home.join(".profile")).unwrap(), profile);
    // A future installation must survive running this older installer unchanged.
    let future = "#!/bin/sh\nprintf 'v999.0.0\\n'\n";
    fs::write(&binary, future).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&binary).unwrap(), future);
    #[cfg(target_os = "macos")]
    {
        let app = home.join("Applications/BT.app");
        assert!(app.join("Contents/MacOS/applet").is_file());
        assert!(Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app)
            .status()
            .unwrap()
            .success());
        // Remove only this fixture's LaunchServices registration before fixture cleanup.
        let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
            .arg("-u").arg(app).status();
    }
}

/// Association execution displays success, errors, and explicit exit output before waiting for Enter.
#[test]
fn association_waits_without_changing_script_source() {
    let fixture = Fixture::new();
    for (index, source, expected) in [
        (0, "print 'desktop-ok'", "desktop-ok"),
        (1, "exit('exit-ok')", "exit-ok"),
        (
            2,
            "print 'before-error'\nthis is invalid syntax !",
            "Press Enter",
        ),
    ] {
        // Spaces, Unicode, and shell metacharacters must remain literal path characters.
        let script = fixture.0.join(format!("-script 中文 & $ ( {index}).bt"));
        fs::write(&script, source).unwrap();
        let capture = fixture.0.join(format!("capture-{index}"));
        let output = fs::File::create(&capture).unwrap();
        let mut child = fixture
            .command()
            .arg("--open-script")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = fs::read_to_string(&capture).unwrap();
            if text.contains("Press Enter") {
                assert!(text.contains(expected), "{text}");
                break;
            }
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Desktop wrapper did not wait: {text}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(child.try_wait().unwrap().is_none());
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(fs::read_to_string(&script).unwrap(), source);
    }
}
