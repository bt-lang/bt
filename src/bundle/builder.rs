use crate::app::config::{AppJson, AppMain};
use crate::error::BtError;
use glob::glob;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Result of building a bundle.
#[cfg(test)]
pub struct BuiltBundle {
    /// Version 1 uncompressed bundle bytes, used only for legacy-format compatibility tests.
    #[allow(dead_code)]
    pub bytes: Vec<u8>,
    /// Ordered file list for a version 1 bundle.
    pub file_names: Vec<String>,
}

/// Builds a version 1 uncompressed bundle from the resources in app.json.
#[cfg(test)]
pub fn build_bundle(project_dir: &Path, config: &AppJson) -> Result<BuiltBundle, BtError> {
    let files = collect_resource_files(project_dir, config)?;
    let mut entries = Vec::with_capacity(files.len());
    let mut total_content_len = 0usize;
    for (name, path) in files {
        let content = fs::read(&path).map_err(|err| {
            BtError::Io(std::io::Error::new(
                err.kind(),
                format!("Failed to read resource `{}`: {}", path.display(), err),
            ))
        })?;
        total_content_len = total_content_len.saturating_add(content.len());
        entries.push((name, content));
    }

    serialize_bundle(entries, total_content_len)
}

/// Collects, deduplicates, and sorts the resource files to include in the bundle.
pub fn collect_resource_files(
    project_dir: &Path,
    config: &AppJson,
) -> Result<BTreeMap<String, PathBuf>, BtError> {
    let project_dir = project_dir.canonicalize()?;
    let mut patterns = config.resources.clone();
    if project_dir.join("app.json").is_file() {
        push_required_resource(&mut patterns, "app.json");
    }
    if config.app.mode == "static" {
        push_required_resource(&mut patterns, &config.app.entry);
    }
    match &config.app.main {
        AppMain::Auto => {
            if project_dir.join("main.bt").is_file() {
                push_required_resource(&mut patterns, "main.bt");
            }
        }
        AppMain::Disabled => {}
        AppMain::File(main) => push_required_resource(&mut patterns, main),
    }
    if project_dir.join("server.bt").is_file() {
        push_required_resource(&mut patterns, "server.bt");
    }
    if let Some(icon) = &config.app.icon {
        push_required_resource(&mut patterns, icon);
    }

    let mut files = BTreeMap::new();
    for pattern in patterns {
        validate_resource_pattern(&pattern, "resources")?;
        if should_skip_disabled_main_resource(project_dir.as_path(), config, &pattern) {
            continue;
        }
        if contains_glob(&pattern) {
            add_glob_matches(&project_dir, &pattern, &mut files)?;
        } else {
            add_plain_resource(&project_dir, &pattern, &mut files)?;
        }
    }
    apply_exclude_patterns(&project_dir, &config.exclude, &mut files)?;
    remove_build_outputs(&mut files);

    Ok(files)
}

/// Always excludes `dist/` outputs so broad resource rules cannot bundle old BTR or exe files.
fn remove_build_outputs(files: &mut BTreeMap<String, PathBuf>) {
    files.retain(|name, _| name != "dist" && !name.starts_with("dist/"));
}

/// Handles a legacy main.bt resource left in app.json when app.main=false.
///
/// Older defaults added `main.bt` to resources. If the user later disables `app.main` and deletes
/// `main.bt`, the build skips that stale resource instead of requiring an unused main script.
fn should_skip_disabled_main_resource(project_dir: &Path, config: &AppJson, pattern: &str) -> bool {
    matches!(config.app.main, AppMain::Disabled)
        && normalize_resource_pattern_text(pattern) == "main.bt"
        && !project_dir.join("main.bt").is_file()
}

/// Normalizes resource-pattern text for fixed-name comparisons.
fn normalize_resource_pattern_text(pattern: &str) -> String {
    pattern.trim().replace('\\', "/")
}

/// Adds a resource required by the build.
fn push_required_resource(patterns: &mut Vec<String>, resource: &str) {
    if !patterns.iter().any(|item| item == resource) {
        patterns.push(resource.to_string());
    }
}

/// Processes a plain file resource.
fn add_plain_resource(
    project_dir: &Path,
    resource: &str,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), BtError> {
    let path = project_dir.join(resource);
    if !path.exists() {
        return Err(BtError::Bundle(format!(
            "Resource file does not exist: {}",
            resource
        )));
    }
    if path.is_dir() {
        return add_directory_files(project_dir, &path, files);
    }
    add_checked_file(project_dir, &path, files)
}

/// Applies exclude rules to the collected resources.
fn apply_exclude_patterns(
    project_dir: &Path,
    patterns: &[String],
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), BtError> {
    if patterns.is_empty() {
        return Ok(());
    }
    let mut excluded = BTreeSet::new();
    for pattern in patterns {
        validate_resource_pattern(pattern, "exclude")?;
        if contains_glob(pattern) {
            add_glob_excludes(project_dir, pattern, &mut excluded)?;
        } else {
            add_plain_exclude(project_dir, pattern, &mut excluded)?;
        }
    }
    for name in excluded {
        files.remove(&name);
    }
    Ok(())
}

/// Processes a plain file or directory exclusion.
fn add_plain_exclude(
    project_dir: &Path,
    resource: &str,
    excluded: &mut BTreeSet<String>,
) -> Result<(), BtError> {
    let path = project_dir.join(resource);
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        return add_directory_excludes(project_dir, &path, excluded);
    }
    add_checked_exclude(project_dir, &path, excluded)
}

/// Processes a glob exclusion.
fn add_glob_excludes(
    project_dir: &Path,
    pattern: &str,
    excluded: &mut BTreeSet<String>,
) -> Result<(), BtError> {
    if let Some(base) = recursive_all_base(pattern) {
        let dir = project_dir.join(base);
        if dir.is_dir() {
            add_directory_excludes(project_dir, &dir, excluded)?;
            return Ok(());
        }
    }

    let abs_pattern = project_dir.join(pattern);
    let glob_pattern = abs_pattern.to_string_lossy().replace('\\', "/");
    for entry in glob(&glob_pattern).map_err(|err| BtError::Bundle(err.to_string()))? {
        let path = entry.map_err(|err| BtError::Bundle(err.to_string()))?;
        if path.is_file() {
            add_checked_exclude(project_dir, &path, excluded)?;
        }
    }
    Ok(())
}

/// Recursively adds every excluded file in a directory.
fn add_directory_excludes(
    project_dir: &Path,
    dir: &Path,
    excluded: &mut BTreeSet<String>,
) -> Result<(), BtError> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                add_checked_exclude(project_dir, &path, excluded)?;
            }
        }
    }
    Ok(())
}

/// Validates and inserts an excluded file within the project directory.
fn add_checked_exclude(
    project_dir: &Path,
    path: &Path,
    excluded: &mut BTreeSet<String>,
) -> Result<(), BtError> {
    let full_path = path.canonicalize()?;
    if !full_path.starts_with(project_dir) {
        return Err(BtError::Bundle(format!(
            "Cannot exclude a file outside the project directory: {}",
            full_path.display()
        )));
    }
    let relative = full_path.strip_prefix(project_dir).map_err(|_| {
        BtError::Bundle(format!(
            "Excluded resource path is outside the project directory: {}",
            full_path.display()
        ))
    })?;
    excluded.insert(normalize_bundle_name(relative)?);
    Ok(())
}

/// Processes a glob resource.
fn add_glob_matches(
    project_dir: &Path,
    pattern: &str,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), BtError> {
    if let Some(base) = recursive_all_base(pattern) {
        let dir = project_dir.join(base);
        if dir.is_dir() {
            add_directory_files(project_dir, &dir, files)?;
            return Ok(());
        }
    }

    let abs_pattern = project_dir.join(pattern);
    let glob_pattern = abs_pattern.to_string_lossy().replace('\\', "/");
    for entry in glob(&glob_pattern).map_err(|err| BtError::Bundle(err.to_string()))? {
        let path = entry.map_err(|err| BtError::Bundle(err.to_string()))?;
        if path.is_file() {
            add_checked_file(project_dir, &path, files)?;
        }
    }
    Ok(())
}

/// Recognizes resource entries such as `assets/**` that recursively collect a directory.
fn recursive_all_base(pattern: &str) -> Option<&str> {
    pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("\\**"))
        .map(|base| if base.is_empty() { "." } else { base })
}

/// Recursively adds every file in a directory.
fn add_directory_files(
    project_dir: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), BtError> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                add_checked_file(project_dir, &path, files)?;
            }
        }
    }
    Ok(())
}

/// Validates and inserts a file within the project directory.
fn add_checked_file(
    project_dir: &Path,
    path: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), BtError> {
    let full_path = path.canonicalize()?;
    if !full_path.starts_with(project_dir) {
        return Err(BtError::Bundle(format!(
            "Cannot bundle a file outside the project directory: {}",
            full_path.display()
        )));
    }
    let relative = full_path.strip_prefix(project_dir).map_err(|_| {
        BtError::Bundle(format!(
            "Resource path is outside the project directory: {}",
            full_path.display()
        ))
    })?;
    let name = normalize_bundle_name(relative)?;
    files.insert(name, full_path);
    Ok(())
}

/// Serializes the version 1 bundle format.
#[cfg(test)]
fn serialize_bundle(
    entries: Vec<(String, Vec<u8>)>,
    total_content_len: usize,
) -> Result<BuiltBundle, BtError> {
    let file_count = u32::try_from(entries.len())
        .map_err(|_| BtError::Bundle("Bundle file count exceeds the u32 limit".to_string()))?;
    let mut capacity = 4usize.saturating_add(total_content_len);
    for (name, content) in &entries {
        capacity = capacity
            .saturating_add(2)
            .saturating_add(name.len())
            .saturating_add(8)
            .saturating_add(content.len());
    }

    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(&file_count.to_le_bytes());
    let mut file_names = Vec::with_capacity(entries.len());
    for (name, content) in entries {
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len()).map_err(|_| {
            BtError::Bundle(format!(
                "Bundle file name is too long and exceeds the u16 limit: {}",
                name
            ))
        })?;
        bytes.extend_from_slice(&name_len.to_le_bytes());
        bytes.extend_from_slice(name_bytes);
        bytes.extend_from_slice(&(content.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&content);
        file_names.push(name);
    }

    Ok(BuiltBundle { bytes, file_names })
}

/// Returns whether a resource entry contains glob metacharacters.
fn contains_glob(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

/// Validates that a resources entry is a safe relative path or glob.
fn validate_resource_pattern(pattern: &str, name: &str) -> Result<(), BtError> {
    if pattern.trim().is_empty() {
        return Err(BtError::Bundle(format!(
            "{} cannot contain an empty path",
            name
        )));
    }
    if pattern.as_bytes().contains(&0) {
        return Err(BtError::Bundle(format!(
            "{} contains an invalid null byte: {}",
            name, pattern
        )));
    }
    let path = Path::new(pattern);
    if path.is_absolute() {
        return Err(BtError::Bundle(format!(
            "{} cannot use an absolute path: {}",
            name, pattern
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(BtError::Bundle(format!(
                    "{} cannot contain a .. path: {}",
                    name, pattern
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "{} cannot contain a root path: {}",
                    name, pattern
                )));
            }
        }
    }
    Ok(())
}

/// Normalizes a path to the bundle's canonical `/`-separated relative form.
pub fn normalize_bundle_name(path: &Path) -> Result<String, BtError> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    BtError::Bundle(format!(
                        "Resource path is not valid UTF-8: {}",
                        path.display()
                    ))
                })?;
                parts.push(value);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(BtError::Bundle(format!(
                    "Bundle path cannot contain ..: {}",
                    path.display()
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "Bundle path must be relative: {}",
                    path.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(BtError::Bundle("Bundle path cannot be empty".to_string()));
    }

    for part in &parts {
        if part.is_empty() {
            return Err(BtError::Bundle(format!(
                "Bundle path contains an invalid component: {}",
                path.display()
            )));
        }
    }

    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::{AppInfo, DevConfig, WindowConfig};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Server-mode bundles do not treat the app.entry HTTP URL as a local resource file.
    #[test]
    fn server_bundle_does_not_collect_entry_url() {
        let dir = fresh_temp_dir("server-entry-url");
        write_project_file(&dir, "app.json", "{}");
        write_project_file(&dir, "server.bt", "net.listen({type:'web'});");
        write_project_file(&dir, "main.bt", "fn ping(){ return 'pong'; }");
        write_project_file(&dir, "www/main.bt", "return 'ok';");

        let config = sample_config(
            "server",
            "http://127.0.0.1:18280",
            vec!["app.json", "server.bt", "main.bt", "www/**"],
        );
        let bundle = build_bundle(&dir, &config).unwrap();

        assert!(bundle.file_names.contains(&"app.json".to_string()));
        assert!(bundle.file_names.contains(&"server.bt".to_string()));
        assert!(bundle.file_names.contains(&"main.bt".to_string()));
        assert!(bundle.file_names.contains(&"www/main.bt".to_string()));
        assert!(!bundle
            .file_names
            .iter()
            .any(|name| name.contains("http://127.0.0.1:18280")));

        let _ = fs::remove_dir_all(dir);
    }

    /// Static mode still adds an entry HTML file not explicitly listed in resources.
    #[test]
    fn static_bundle_auto_collects_entry_file() {
        let dir = fresh_temp_dir("static-entry-file");
        write_project_file(&dir, "app.json", "{}");
        write_project_file(&dir, "index.html", "<h1>BT</h1>");
        write_project_file(&dir, "main.bt", "fn ping(){ return 'pong'; }");

        let config = sample_config("static", "index.html", vec!["app.json", "main.bt"]);
        let bundle = build_bundle(&dir, &config).unwrap();

        assert!(bundle.file_names.contains(&"app.json".to_string()));
        assert!(bundle.file_names.contains(&"index.html".to_string()));
        assert!(bundle.file_names.contains(&"main.bt".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    /// The default index.html configuration does not scan tags or local page references.
    #[test]
    fn default_index_bundle_does_not_scan_html_resources() {
        let dir = fresh_temp_dir("default-index");
        write_project_file(&dir, "app.json", "{}");
        write_project_file(
            &dir,
            "index.html",
            r#"<title>HTML</title><link href="style.css"><script src="main.js"></script>"#,
        );
        write_project_file(&dir, "style.css", "body{margin:0}");
        write_project_file(&dir, "main.js", "window.loaded=true;");
        write_project_file(&dir, "logo.ico", "fake icon bytes");
        write_project_file(&dir, "main.bt", "");

        let config = sample_config("static", "index.html", vec!["app.json"]);
        let bundle = build_bundle(&dir, &config).unwrap();

        assert!(bundle.file_names.contains(&"app.json".to_string()));
        assert!(bundle.file_names.contains(&"index.html".to_string()));
        assert!(!bundle.file_names.contains(&"style.css".to_string()));
        assert!(!bundle.file_names.contains(&"main.js".to_string()));
        assert!(!bundle.file_names.contains(&"logo.ico".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    /// A plain directory resource recursively collects its files.
    #[test]
    fn plain_directory_resource_collects_files_recursively() {
        let dir = fresh_temp_dir("plain-directory");
        write_project_file(&dir, "app.json", "{}");
        write_project_file(&dir, "assets/app.css", "body{}");
        write_project_file(&dir, "assets/img/logo.txt", "logo");
        write_project_file(&dir, "index.html", "<h1>BT</h1>");
        write_project_file(&dir, "main.bt", "");

        let config = sample_config("static", "index.html", vec!["app.json", "assets"]);
        let bundle = build_bundle(&dir, &config).unwrap();

        assert!(bundle.file_names.contains(&"assets/app.css".to_string()));
        assert!(bundle
            .file_names
            .contains(&"assets/img/logo.txt".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    /// Exclude rules remove matching files from the final resource set.
    #[test]
    fn exclude_removes_matching_resources() {
        let dir = fresh_temp_dir("exclude");
        write_project_file(&dir, "app.json", "{}");
        write_project_file(&dir, "index.html", "<h1>BT</h1>");
        write_project_file(&dir, "main.bt", "");
        write_project_file(&dir, "assets/app.css", "body{}");
        write_project_file(&dir, "assets/test/debug.css", "body{}");

        let mut config = sample_config("static", "index.html", vec!["app.json", "assets/**"]);
        config.exclude = vec!["assets/test/**".to_string()];
        let bundle = build_bundle(&dir, &config).unwrap();

        assert!(bundle.file_names.contains(&"assets/app.css".to_string()));
        assert!(!bundle
            .file_names
            .contains(&"assets/test/debug.css".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    /// Broad resource rules cannot re-add outputs from an earlier dist build.
    #[test]
    fn build_output_directory_is_always_excluded() {
        let dir = fresh_temp_dir("dist-exclude");
        fs::create_dir_all(dir.join("dist")).unwrap();
        fs::write(dir.join("app.json"), "{}").unwrap();
        fs::write(dir.join("index.html"), "<h1>demo</h1>").unwrap();
        fs::write(dir.join("main.bt"), "").unwrap();
        fs::write(dir.join("dist/old.btr"), "old").unwrap();
        let config = sample_config(
            "static",
            "index.html",
            vec!["app.json", "index.html", "dist/**"],
        );

        let files = collect_resource_files(&dir, &config).unwrap();

        assert!(files.contains_key("app.json"));
        assert!(!files.contains_key("dist/old.btr"));
        let _ = fs::remove_dir_all(dir);
    }

    /// With app.main=false, a missing legacy main.bt resource is skipped.
    #[test]
    fn disabled_main_skips_missing_legacy_main_resource() {
        let dir = fresh_temp_dir("disabled-main");
        write_project_file(&dir, "app.json", "{}");
        write_project_file(&dir, "index.html", "<h1>BT</h1>");

        let mut config = sample_config("static", "index.html", vec!["app.json", "main.bt"]);
        config.app.main = AppMain::Disabled;
        let bundle = build_bundle(&dir, &config).unwrap();

        assert!(bundle.file_names.contains(&"app.json".to_string()));
        assert!(bundle.file_names.contains(&"index.html".to_string()));
        assert!(!bundle.file_names.contains(&"main.bt".to_string()));

        let _ = fs::remove_dir_all(dir);
    }

    /// Creates a desktop application configuration for the specified mode.
    fn sample_config(mode: &str, entry: &str, resources: Vec<&str>) -> AppJson {
        AppJson {
            app: AppInfo {
                id: "org.btlang.demo".to_string(),
                id_generated: false,
                name: "demo".to_string(),
                title: "Demo".to_string(),
                version: "1.0.0".to_string(),
                description: None,
                copyright: None,
                mode: mode.to_string(),
                entry: entry.to_string(),
                icon: None,
                storage: "app".to_string(),
                file_associations: Vec::new(),
                main: AppMain::File("main.bt".to_string()),
            },
            window: WindowConfig {
                width: 900,
                height: 700,
                resizable: true,
                fullscreen: false,
                hide_titlebar: false,
                transparent: false,
                always_on_top: false,
            },
            dev: DevConfig {
                watch: true,
                delay: 500,
                devtools: true,
                console: true,
            },
            resources: resources.into_iter().map(str::to_string).collect(),
            exclude: Vec::new(),
        }
    }

    /// Writes a test project file, creating parent directories as needed.
    fn write_project_file(project_dir: &Path, relative: &str, content: &str) {
        let path = project_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// Creates a unique test directory.
    fn fresh_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "bt-bundle-builder-test-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
