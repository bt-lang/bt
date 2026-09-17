//! Explicit online updates from verified release archives on the official website.

use super::{platform, read_version, TemporaryFile};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

/// Hard cap for both compressed downloads and the extracted interpreter.
const MAX_BYTES: u64 = 512 * 1024 * 1024;
/// Hard cap for release metadata, independent of Content-Length.
const METADATA_LIMIT: u64 = 2 * 1024 * 1024;
/// Website metadata is published only after all official archives pass release verification.
const RELEASE_URL: &str = "https://btlang.org/static/download/latest.json";

/// Release fields required to select a stable, versioned interpreter archive.
#[derive(Deserialize)]
struct Release {
    /// Exact v-prefixed semantic version.
    tag_name: String,
    /// Unpublished releases must never be installed.
    draft: bool,
    /// Prereleases are excluded even if the endpoint unexpectedly returns one.
    prerelease: bool,
    /// Published archives and their integrity metadata.
    assets: Vec<Asset>,
}

/// Integrity and location metadata for one official archive.
#[derive(Deserialize)]
struct Asset {
    /// Exact platform and version specific ZIP filename.
    name: String,
    /// Canonical official website download URL.
    browser_download_url: String,
    /// Exact compressed byte count.
    size: u64,
    /// GitHub's SHA-256 digest; absence is a hard failure.
    digest: Option<String>,
}

/// Updates only the fixed user installation, holding the install lock across the transaction.
pub(super) fn run() -> Result<(), String> {
    let directory = platform::install_dir()?;
    let target = directory.join(if cfg!(windows) { "bt.exe" } else { "bt" });
    let metadata = fs::symlink_metadata(&target)
        .map_err(|_| "BT is not installed for this user; run `bt install` first".to_string())?;
    if !metadata.file_type().is_file() {
        return Err("Installed BT must be a regular file".into());
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".bt-install.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|e| format!("Another BT installation may be running: {e}"))?;
    let installed = read_version(&target)?;
    let platform = platform_name(std::env::consts::OS, std::env::consts::ARCH)?;
    println!("Checking the latest stable BT release (installed: {installed})...");
    // This runtime exists only during an explicit CLI update, never on VM or request hot paths.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("Cannot start update runtime: {e}"))?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("bt-updater/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let release = runtime.block_on(async {
        let mut response = client
            .get(RELEASE_URL)
            .header("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| format!("Release lookup failed: {e}"))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() as u64 + chunk.len() as u64 > METADATA_LIMIT {
                return Err("Release metadata exceeds the size limit".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<Release>(&bytes)
            .map_err(|e| format!("Invalid release metadata: {e}"))
    })?;
    let (version, asset) = select_asset(&release, platform)?;
    if version.cmp_precedence(&installed) != Ordering::Greater {
        println!("BT {installed} is up to date (latest stable release: {version}).");
        return Ok(());
    }
    println!("Downloading {}...", asset.name);
    let (_archive_guard, mut archive_file) = TemporaryFile::create(&directory, "zip")?;
    runtime.block_on(download(&client, asset, &mut archive_file))?;
    archive_file
        .seek(SeekFrom::Start(0))
        .map_err(|e| e.to_string())?;
    let (staged, mut output) =
        TemporaryFile::create(&directory, if cfg!(windows) { "exe" } else { "bin" })?;
    extract_binary(
        archive_file,
        &mut output,
        if cfg!(windows) { "bt.exe" } else { "bt" },
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    output.sync_all().map_err(|e| e.to_string())?;
    drop(output);
    if read_version(&staged.path)? != version {
        return Err(
            "Downloaded BT version does not match the release; installed file was preserved".into(),
        );
    }
    replace_verified(&staged.path, &target)?;
    println!("Updated BT {installed} to {version}: {}. New invocations use the updated version; this session keeps its original version.", target.display());
    Ok(())
}

/// Selects only platforms for which the official release workflow publishes assets.
fn platform_name(os: &str, arch: &str) -> Result<&'static str, String> {
    match (os, arch) {
        ("windows", "x86_64") => Ok("windows-x64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        ("macos", "aarch64") => Ok("macos-arm64"),
        ("macos", "x86_64") => Ok("macos-x64"),
        _ => Err(format!("No official BT update archive for {os}/{arch}")),
    }
}

/// Rejects ambiguous archives, prereleases, foreign URLs, and unusable integrity metadata.
fn select_asset<'a>(release: &'a Release, platform: &str) -> Result<(Version, &'a Asset), String> {
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .ok_or("Invalid release tag")?,
    )
    .map_err(|e| e.to_string())?;
    if release.draft || release.prerelease || !version.pre.is_empty() || !version.build.is_empty() {
        return Err("Update requires a published stable BT release".into());
    }
    let name = format!("bt-{platform}-{version}.zip");
    let mut assets = release.assets.iter().filter(|asset| asset.name == name);
    let asset = assets
        .next()
        .ok_or_else(|| format!("Release is missing {name}; retry after publication completes"))?;
    if assets.next().is_some() || asset.size == 0 || asset.size > MAX_BYTES {
        return Err("Ambiguous or oversized release archive".into());
    }
    if asset.browser_download_url != format!("https://btlang.org/static/download/{name}") {
        return Err("Unexpected release download URL".into());
    }
    let digest = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .ok_or("Release archive has no SHA-256 digest")?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Invalid release SHA-256 digest".into());
    }
    Ok((version, asset))
}

/// Streams a bounded archive to disk and verifies its exact size and digest before extraction.
async fn download(
    client: &reqwest::Client,
    asset: &Asset,
    output: &mut File,
) -> Result<(), String> {
    let mut response = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| format!("Update download failed: {e}"))?;
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        length += chunk.len() as u64;
        if length > asset.size || length > MAX_BYTES {
            return Err("Update download exceeds the declared size".into());
        }
        hasher.update(&chunk);
        output.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    let actual = format!("sha256:{:x}", hasher.finalize());
    if length != asset.size
        || !asset
            .digest
            .as_deref()
            .is_some_and(|expected| expected.eq_ignore_ascii_case(&actual))
    {
        return Err("Update size or SHA-256 mismatch; installed file was preserved".into());
    }
    Ok(())
}

/// Extracts one exact root entry without trusting archive paths or unbounded decompressed sizes.
fn extract_binary(input: impl Read + Seek, output: &mut File, name: &str) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(input).map_err(|e| format!("Invalid update ZIP: {e}"))?;
    if archive.len() > 32 {
        return Err("Update ZIP has too many entries".into());
    }
    let mut selected = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|e| e.to_string())?;
        if entry.name() == name {
            if selected.is_some()
                || !entry.is_file()
                || entry.size() == 0
                || entry.size() > MAX_BYTES
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err("Invalid interpreter entry in update ZIP".into());
            }
            selected = Some(index);
        }
    }
    let entry = archive
        .by_index(selected.ok_or("Update ZIP is missing the interpreter")?)
        .map_err(|e| e.to_string())?;
    let expected = entry.size();
    let copied = std::io::copy(&mut entry.take(MAX_BYTES + 1), output)
        .map_err(|e| format!("Cannot extract update: {e}"))?;
    if copied != expected || copied > MAX_BYTES {
        return Err("Extracted interpreter size mismatch".into());
    }
    Ok(())
}

/// Publishes a verified sibling, retaining at most one Windows executable while it is running.
fn replace_verified(staged: &Path, target: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let backup = target.with_file_name(".bt-update-previous.exe");
        match fs::remove_file(&backup) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "Close the previous BT session before updating again: {e}"
                ))
            }
        }
        // Windows allows renaming a running executable but forbids overwriting its mapped image.
        fs::rename(target, &backup).map_err(|e| format!("Cannot preserve installed BT: {e}"))?;
        if let Err(error) = platform::replace(staged, target) {
            fs::rename(&backup, target).map_err(|rollback| {
                format!(
                    "{error}; restore {} to {}: {rollback}",
                    backup.display(),
                    target.display()
                )
            })?;
            return Err(error);
        }
        let _ = fs::remove_file(backup);
        Ok(())
    }
    #[cfg(not(windows))]
    platform::replace(staged, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Owns only temporary updater fixtures, including staged and preserved files.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        /// Creates a unique directory for each isolated update transaction.
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("bt-update-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        /// Removes only the directory created by this fixture.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Builds representative website metadata without using external services.
    fn release() -> Release {
        serde_json::from_value(serde_json::json!({
            "tag_name": "v1.2.0", "draft": false, "prerelease": false,
            "assets": [{"name": "bt-windows-x64-1.2.0.zip", "size": 123,
                "browser_download_url": "https://btlang.org/static/download/bt-windows-x64-1.2.0.zip",
                "digest": format!("sha256:{}", "a".repeat(64))}]
        })).unwrap()
    }

    /// Metadata must identify one stable platform archive with a trusted URL and usable digest.
    #[test]
    fn release_selection_rejects_untrusted_or_incomplete_metadata() {
        assert_eq!(
            select_asset(&release(), "windows-x64").unwrap().0,
            Version::new(1, 2, 0)
        );
        for mutate in [
            |r: &mut Release| r.draft = true,
            |r: &mut Release| r.prerelease = true,
            |r: &mut Release| r.tag_name = "v1.2.0-rc.1".into(),
            |r: &mut Release| r.assets[0].digest = None,
            |r: &mut Release| r.assets[0].size = MAX_BYTES + 1,
            |r: &mut Release| {
                r.assets[0].browser_download_url = "https://example.com/bt.zip".into()
            },
            |r: &mut Release| r.assets.push(release().assets.remove(0)),
        ] {
            let mut metadata = release();
            mutate(&mut metadata);
            assert!(select_asset(&metadata, "windows-x64").is_err());
        }
        assert!(select_asset(&release(), "linux-x64").is_err());
        assert!(platform_name("linux", "aarch64").is_err());
        assert_eq!(platform_name("macos", "aarch64").unwrap(), "macos-arm64");
    }

    /// Creates an in-memory ZIP with a caller-selected entry path.
    fn archive(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap().into_inner()
    }

    /// Archive paths are never extracted to disk; only the exact root executable is accepted.
    #[test]
    fn extraction_requires_the_exact_binary_and_rejects_corruption() {
        let fixture = Fixture::new();
        let mut output = File::create(fixture.0.join("output")).unwrap();
        extract_binary(
            Cursor::new(archive("bt.exe", b"executable")),
            &mut output,
            "bt.exe",
        )
        .unwrap();
        assert_eq!(fs::read(fixture.0.join("output")).unwrap(), b"executable");
        for bytes in [
            archive("../bt.exe", b"bad"),
            archive("bt-app.exe", b"wrong"),
            archive("bt.exe", b""),
            b"not a zip".to_vec(),
        ] {
            assert!(extract_binary(Cursor::new(bytes), &mut output, "bt.exe").is_err());
        }
    }

    /// A failed publish restores the original executable; a successful one replaces it.
    #[test]
    fn replacement_preserves_the_old_binary_on_failure() {
        let fixture = Fixture::new();
        let target = fixture.0.join("bt.exe");
        let staged = fixture.0.join("new.exe");
        fs::write(&target, b"old").unwrap();
        assert!(replace_verified(&staged, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        fs::write(&staged, b"new").unwrap();
        replace_verified(&staged, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    /// Holds a copied test executable open only when explicitly launched by the replacement test.
    #[cfg(windows)]
    #[test]
    #[ignore = "subprocess fixture for the Windows update regression"]
    fn running_binary_fixture() {
        let Some(ready) = std::env::var_os("BT_UPDATE_TEST_READY") else {
            return;
        };
        fs::write(ready, b"ready").unwrap();
        std::thread::sleep(Duration::from_secs(20));
    }

    /// A running Windows image can be updated without stopping it or accumulating old images.
    #[cfg(windows)]
    #[test]
    fn replaces_a_running_windows_executable() {
        let fixture = Fixture::new();
        let target = fixture.0.join("bt.exe");
        let staged = fixture.0.join("new.exe");
        let ready = fixture.0.join("ready");
        fs::copy(std::env::current_exe().unwrap(), &target).unwrap();
        fs::write(&staged, b"new").unwrap();
        let mut child = std::process::Command::new(&target)
            .args([
                "--exact",
                "install::update::tests::running_binary_fixture",
                "--ignored",
            ])
            .env("BT_UPDATE_TEST_READY", &ready)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = if ready.exists() {
            replace_verified(&staged, &target)
        } else {
            Err("Child did not start".into())
        };
        let still_running = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
        assert!(still_running);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        fs::write(&staged, b"next").unwrap();
        replace_verified(&staged, &target).unwrap();
        assert!(!fixture.0.join(".bt-update-previous.exe").exists());
    }

    /// Real HTTP reads reject truncated, oversized, and wrong-digest archives without publishing.
    #[test]
    fn download_checks_size_hash_and_http_status() {
        use std::net::TcpListener;
        let fixture = Fixture::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (status, body, size, valid_hash, success) in [
            ("200 OK", "payload", 7, true, true),
            ("200 OK", "payload", 7, false, false),
            ("200 OK", "payload", 6, true, false),
            ("200 OK", "payload", 8, true, false),
            ("404 Not Found", "payload", 7, true, false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            let asset = Asset {
                name: "fixture".into(),
                browser_download_url: format!("http://{address}"),
                size,
                digest: Some(if valid_hash {
                    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
                } else {
                    format!("sha256:{}", "0".repeat(64))
                }),
            };
            let mut output = File::create(fixture.0.join("download")).unwrap();
            let _ = rustls::crypto::ring::default_provider().install_default();
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            assert_eq!(
                runtime
                    .block_on(download(&client, &asset, &mut output))
                    .is_ok(),
                success
            );
            server.join().unwrap();
        }
    }
}
