//! Explicit user installation integration for Linux and macOS.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::{Command, Stdio};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Duration, Instant};

/// Identifies the bounded, installer-owned shell initialization block.
const PROFILE_MARKER: &str = "# BT user installation PATH";
/// Bounds configuration reads before appending a small initialization block.
const CONFIG_LIMIT: u64 = 1024 * 1024;

/// Returns the current user's absolute home directory without shell expansion.
fn home_dir() -> Result<PathBuf, String> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| "HOME must identify an absolute user directory".to_string())?;
    path_text(&home)?;
    Ok(home)
}

/// Selects a fixed user executable directory for the current platform.
pub(super) fn install_dir() -> Result<PathBuf, String> {
    let home = home_dir()?;
    if cfg!(target_os = "macos") {
        Ok(home.join(".bt/bin"))
    } else {
        Ok(home.join(".local/bin"))
    }
}

/// Replaces an executable atomically while existing Unix processes keep their inode.
pub(super) fn replace(staged: &Path, target: &Path) -> Result<(), String> {
    fs::rename(staged, target).map_err(|error| {
        format!(
            "Cannot replace installed executable {}: {error}",
            target.display()
        )
    })
}

/// Configures login shells and the desktop only during explicit installation.
pub(super) fn integrate(executable: &Path) -> Result<Vec<String>, String> {
    let home = home_dir()?;
    let directory = executable
        .parent()
        .ok_or_else(|| "The installed executable has no parent directory".to_string())?;
    configure_shells(&home, directory)?;
    let messages = vec![
        "User PATH configured for sh, bash, zsh and fish. Open a new terminal to use bt."
            .to_string(),
    ];
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let messages = {
        let mut messages = messages;
        #[cfg(target_os = "linux")]
        integrate_linux(&home, executable, &mut messages)?;
        #[cfg(target_os = "macos")]
        integrate_macos(&home, executable, &mut messages)?;
        messages
    };
    Ok(messages)
}

/// Rejects paths that cannot be represented safely in PATH or line-based metadata.
fn path_text(path: &Path) -> Result<&str, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "Installation paths must contain valid UTF-8".to_string())?;
    if text.contains(':') || text.chars().any(char::is_control) {
        return Err("Installation paths cannot contain colons or control characters".to_string());
    }
    Ok(text)
}

/// Quotes a literal word for POSIX shells without interpreting its contents.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Quotes a fish literal, whose single-quoted backslash rules differ from sh.
fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Builds a PATH guard that treats glob characters in the install directory literally.
fn shell_block(directory: &str) -> String {
    let quoted = shell_quote(directory);
    format!(
        "{PROFILE_MARKER}\ncase \":$PATH:\" in\n    *:{quoted}:*) ;;\n    *) export PATH={quoted}:\"$PATH\" ;;\nesac\n# End BT user installation PATH\n"
    )
}

/// Resolves an XDG directory, ignoring relative environment values as required by XDG.
fn xdg_dir(variable: &str, fallback: PathBuf) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or(fallback)
}

/// Appends only a missing installer block while preserving all existing profile bytes.
fn append_profile(path: &Path, block: &str) -> Result<(), String> {
    let mut existing = Vec::new();
    match fs::File::open(path) {
        Ok(file) => {
            file.take(CONFIG_LIMIT + 1)
                .read_to_end(&mut existing)
                .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
            if existing.len() as u64 > CONFIG_LIMIT {
                return Err(format!(
                    "Shell configuration exceeds 1 MiB: {}",
                    path.display()
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Cannot read {}: {error}", path.display())),
    }
    if existing
        .windows(block.len())
        .any(|window| window == block.as_bytes())
    {
        return Ok(());
    }
    if existing
        .windows(PROFILE_MARKER.len())
        .any(|window| window == PROFILE_MARKER.as_bytes())
    {
        return Err(format!("An edited BT PATH block exists in {}; restore or remove that block before reinstalling", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Cannot create {}: {error}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("Cannot update {}: {error}", path.display()))?;
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        file.write_all(b"\n").map_err(|error| error.to_string())?;
    }
    file.write_all(block.as_bytes())
        .map_err(|error| format!("Cannot update {}: {error}", path.display()))
}

/// Configures standard shells without creating a bash login file that masks .profile.
fn configure_shells(home: &Path, directory: &Path) -> Result<(), String> {
    let directory = path_text(directory)?;
    let block = shell_block(directory);
    append_profile(&home.join(".profile"), &block)?;
    append_profile(&home.join(".bashrc"), &block)?;
    // Bash reads only the first existing login file; preserve that selection.
    for name in [".bash_profile", ".bash_login"] {
        let path = home.join(name);
        if path.exists() {
            append_profile(&path, &block)?;
            break;
        }
    }
    let zsh = xdg_dir("ZDOTDIR", home.to_path_buf());
    append_profile(&zsh.join(".zprofile"), &block)?;
    append_profile(&zsh.join(".zshrc"), &block)?;
    let fish = xdg_dir("XDG_CONFIG_HOME", home.join(".config")).join("fish/conf.d/bt-path.fish");
    let quoted = fish_quote(directory);
    append_profile(&fish, &format!("{PROFILE_MARKER}\nif not contains -- {quoted} $PATH\n    set -gx PATH {quoted} $PATH\nend\n# End BT user installation PATH\n"))
}

/// Runs an installation helper with inherited diagnostics and a bounded lifetime.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_helper(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("Cannot run {program}: {error}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("{program} failed with {status}"))
                }
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Cannot wait for {program}: {error}"));
            }
        }
        if start.elapsed() >= Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{program} exceeded the 30-second installation timeout"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Writes installer-owned metadata, refusing to follow an unexpected symbolic link.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_metadata(path: &Path, contents: &str) -> Result<(), String> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "Refusing to overwrite metadata symlink {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(path, contents).map_err(|error| format!("Cannot write {}: {error}", path.display()))
}

/// Escapes both desktop-entry string decoding and Exec argument decoding.
#[cfg(any(target_os = "linux", test))]
fn desktop_quote(text: &str) -> String {
    let mut result = String::from("\"");
    for character in text.chars() {
        match character {
            '\\' => result.push_str("\\\\\\\\"),
            '"' | '`' | '$' => {
                result.push_str("\\\\");
                result.push(character);
            }
            '%' => result.push_str("%%"),
            _ => result.push(character),
        }
    }
    result.push('"');
    result
}

/// Produces a direct, terminal-backed file opener without a shell command layer.
#[cfg(any(target_os = "linux", test))]
fn desktop_entry(executable: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName=BT\nComment=Run BT scripts\nExec={} --open-script %f\nTerminal=true\nMimeType=text/x-bt;\nCategories=Development;ConsoleOnly;\nNoDisplay=true\n", desktop_quote(executable))
}

/// Registers the user's MIME type and opener when a graphical session is available.
#[cfg(target_os = "linux")]
fn integrate_linux(
    home: &Path,
    executable: &Path,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let desktop = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()));
    if !desktop {
        messages.push("No graphical session detected; desktop file association skipped. Run bt install in a desktop session to add it.".to_string());
        return Ok(());
    }
    let executable = path_text(executable)?;
    // Desktop Entry disallows '=' in the executable token, even inside quotes.
    if executable.contains('=') {
        return Err("The desktop executable path cannot contain an equals sign".to_string());
    }
    let data = xdg_dir("XDG_DATA_HOME", home.join(".local/share"));
    let applications = data.join("applications");
    let mime = data.join("mime");
    write_metadata(
        &applications.join("org.btlang.BT.desktop"),
        &desktop_entry(executable),
    )?;
    write_metadata(&mime.join("packages/org.btlang.BT.xml"), "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\"><mime-type type=\"text/x-bt\"><comment>BT script</comment><sub-class-of type=\"text/plain\"/><glob pattern=\"*.bt\"/></mime-type></mime-info>\n")?;
    for (program, directory) in [
        ("update-mime-database", mime),
        ("update-desktop-database", applications),
    ] {
        if let Err(error) = run_helper(Command::new(program).arg(directory)) {
            messages.push(format!("Desktop integration warning: {error}. Install the desktop MIME utilities and run bt install again."));
        }
    }
    messages.push("BT is registered as a .bt file opener. If needed, choose BT once in your file manager's Open With settings; existing defaults are preserved.".to_string());
    Ok(())
}

/// Escapes an AppleScript string literal before the runtime quotes shell arguments.
#[cfg(any(target_os = "macos", test))]
fn applescript_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Handles Finder open events and interactive launches through the installed binary.
#[cfg(any(target_os = "macos", test))]
fn applescript_launcher(executable: &str) -> String {
    format!(
        r#"property btExecutable : {}
on run
    my launchBT("")
end run
on open scriptFiles
    repeat with scriptFile in scriptFiles
        my launchBT(POSIX path of scriptFile)
    end repeat
end open
on launchBT(scriptPath)
    set launchCommand to "exec " & quoted form of btExecutable
    if scriptPath is not "" then
        set launchCommand to launchCommand & " --open-script " & quoted form of scriptPath
    end if
    tell application "Terminal"
        activate
        do script launchCommand
    end tell
end launchBT
"#,
        applescript_quote(executable)
    )
}

/// Declares BT's document type and the standard osacompile applet executable.
#[cfg(any(target_os = "macos", test))]
fn macos_plist() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.btlang.BT</string>
<key>CFBundleName</key><string>BT</string>
<key>CFBundleDisplayName</key><string>BT</string>
<key>CFBundleExecutable</key><string>applet</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>NSAppleEventsUsageDescription</key><string>BT opens Terminal to run your BT scripts and show their output.</string>
<key>CFBundleDocumentTypes</key><array><dict>
<key>CFBundleTypeName</key><string>BT script</string>
<key>CFBundleTypeRole</key><string>Shell</string>
<key>LSHandlerRank</key><string>Default</string>
<key>LSItemContentTypes</key><array><string>org.btlang.script</string></array>
</dict></array>
<key>UTExportedTypeDeclarations</key><array><dict>
<key>UTTypeIdentifier</key><string>org.btlang.script</string>
<key>UTTypeDescription</key><string>BT script</string>
<key>UTTypeConformsTo</key><array><string>public.plain-text</string></array>
<key>UTTypeTagSpecification</key><dict>
<key>public.filename-extension</key><array><string>bt</string></array>
<key>public.mime-type</key><string>text/x-bt</string>
</dict></dict></array>
</dict></plist>
"#
}

/// Generates a user applet and registers it without replacing the user's default choice.
#[cfg(target_os = "macos")]
fn integrate_macos(
    home: &Path,
    executable: &Path,
    messages: &mut Vec<String>,
) -> Result<(), String> {
    let applications = home.join("Applications");
    fs::create_dir_all(&applications).map_err(|error| error.to_string())?;
    let app = applications.join("BT.app");
    let staging = applications.join(format!(".bt-install-{}.app", std::process::id()));
    let backup = applications.join(".BT-install-backup.app");
    if staging.exists() || backup.exists() {
        return Err("A previous BT app installation staging directory exists in ~/Applications; inspect it before retrying".to_string());
    }
    if app.exists() {
        let metadata = fs::symlink_metadata(&app).map_err(|error| error.to_string())?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || fs::read_to_string(app.join("Contents/Resources/bt-install-owner"))
                .ok()
                .as_deref()
                != Some("org.btlang.BT\n")
        {
            return Err(
                "~/Applications/BT.app already exists and is not owned by this installer"
                    .to_string(),
            );
        }
    }
    let result = (|| {
        run_helper(
            Command::new("/usr/bin/osacompile")
                .arg("-o")
                .arg(&staging)
                .arg("-e")
                .arg(applescript_launcher(path_text(executable)?)),
        )?;
        write_metadata(&staging.join("Contents/Info.plist"), macos_plist())?;
        write_metadata(
            &staging.join("Contents/Resources/bt-install-owner"),
            "org.btlang.BT\n",
        )?;
        // Info.plist is part of the signature: sign after adding document declarations.
        run_helper(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&staging),
        )?;
        if app.exists() {
            fs::rename(&app, &backup).map_err(|error| error.to_string())?;
        }
        if let Err(error) = fs::rename(&staging, &app) {
            if backup.exists() {
                let _ = fs::rename(&backup, &app);
            }
            return Err(format!("Cannot activate BT.app: {error}"));
        }
        if backup.exists() {
            fs::remove_dir_all(&backup)
                .map_err(|error| format!("Cannot remove previous BT app backup: {error}"))?;
        }
        Ok(())
    })();
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    run_helper(Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
        .arg("-f").arg(&app))?;
    messages.push("Created ~/Applications/BT.app. If needed, choose BT in Finder's Open With settings. Allow BT to control Terminal when macOS asks.".to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures shell metacharacters remain literal in the generated PATH guard.
    #[test]
    fn shell_literals_and_path_guard() {
        // Keep the integration call graph type-checked by host-only unit test builds.
        let _ = install_dir as fn() -> Result<PathBuf, String>;
        let _ = replace as fn(&Path, &Path) -> Result<(), String>;
        let _ = integrate as fn(&Path) -> Result<Vec<String>, String>;
        assert_eq!(shell_quote("a'b$`\\*"), "'a'\\''b$`\\*'");
        assert_eq!(fish_quote("a'b\\c"), "'a\\'b\\\\c'");
        let block = shell_block("/home/a[*]/bin");
        assert!(block.contains("*:'/home/a[*]/bin':*)"));
        assert!(block.contains("export PATH='/home/a[*]/bin':\"$PATH\""));
    }

    /// Keeps field expansion outside quotes and escapes desktop parser metacharacters.
    #[test]
    fn desktop_arguments_are_literal() {
        assert_eq!(
            desktop_quote("/home/a $`\"\\%/bt"),
            "\"/home/a \\\\$\\\\`\\\\\"\\\\\\\\%%/bt\""
        );
        let entry = desktop_entry("/home/a b/bin/bt");
        assert!(entry.contains("Exec=\"/home/a b/bin/bt\" --open-script %f\n"));
        assert!(entry.contains("Terminal=true\n"));
        assert!(!entry.contains("sh -c"));
    }

    /// Finder paths are quoted at runtime rather than interpolated into source code.
    #[test]
    fn finder_launcher_quotes_both_boundaries() {
        assert_eq!(applescript_quote("a\"\\b"), "\"a\\\"\\\\b\"");
        let launcher = applescript_launcher("/Users/a '$/bin/bt");
        assert!(launcher.contains("quoted form of btExecutable"));
        assert!(launcher.contains("quoted form of scriptPath"));
        assert!(launcher.contains("on open scriptFiles"));
        assert!(macos_plist().contains("org.btlang.script"));
        assert!(macos_plist().contains("NSAppleEventsUsageDescription"));
    }

    /// Rejects installation directories that cannot be encoded as a PATH entry.
    #[test]
    fn rejects_ambiguous_install_paths() {
        assert!(path_text(Path::new("/home/a:b/bin")).is_err());
        assert!(path_text(Path::new("/home/a\nb/bin")).is_err());
        assert_eq!(
            path_text(Path::new("/home/a ' $ %/bin")).unwrap(),
            "/home/a ' $ %/bin"
        );
    }

    /// Repeated installs preserve user configuration and add the managed block only once.
    #[test]
    fn profile_append_is_idempotent_and_preserves_bytes() {
        let directory =
            env::temp_dir().join(format!("bt-unix-profile-test-{}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("profile");
        fs::write(&path, b"# user data\nexport EDITOR=vim").unwrap();
        let block = shell_block("/home/a ' b/bin");
        append_profile(&path, &block).unwrap();
        let first = fs::read(&path).unwrap();
        append_profile(&path, &block).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);
        assert!(first.starts_with(b"# user data\nexport EDITOR=vim\n"));
        assert!(append_profile(&path, &shell_block("/different/bin")).is_err());
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    /// Executes the real POSIX parser to verify literal PATH entries and deduplication.
    #[cfg(unix)]
    #[test]
    fn shell_path_block_executes_without_expansion() {
        let directory = "/tmp/BT ' $HOME `uname` \\ [*] % bin";
        let block = shell_block(directory);
        let script = format!("{block}\n{block}\nprintf '%s' \"$PATH\"");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{directory}:/usr/bin:/bin")
        );
    }
}
