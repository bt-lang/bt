use crate::error::BtError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path};
use url::Url;

/// BT desktop application configuration.
///
/// Corresponds to app.json in the project root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppJson {
    /// Basic application information; omitted fields use desktop runtime defaults.
    #[serde(default)]
    pub app: AppInfo,

    /// Main window configuration; omitted fields use desktop runtime defaults.
    #[serde(default)]
    pub window: WindowConfig,

    /// Development-mode configuration; production Bundle runtime ignores the file-watching switch.
    #[serde(default)]
    pub dev: DevConfig,

    /// Resource rules collected during packaging.
    #[serde(default)]
    pub resources: Vec<String>,

    /// Resource rules excluded from packaging and development watching; these take precedence over resources.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Basic application information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    /// Stable application identifier used for BTR metadata, instance recognition, and application-level isolation.
    #[serde(default)]
    pub id: String,

    /// Whether this ID was generated from an omitted app.id; not written to app.json during serialization.
    #[serde(skip)]
    pub(crate) id_generated: bool,

    /// Application name, also used as the default packaged executable name.
    #[serde(default = "default_app_name")]
    pub name: String,

    /// Main window title.
    #[serde(default = "default_app_title")]
    pub title: String,

    /// Application version.
    #[serde(default = "default_app_version")]
    pub version: String,

    /// Description for the packaged output, written to FileDescription on Windows.
    #[serde(default)]
    pub description: Option<String>,

    /// Copyright text for the packaged output, written to LegalCopyright on Windows.
    #[serde(default)]
    pub copyright: Option<String>,

    /// static | server | remote
    #[serde(default = "default_app_mode")]
    pub mode: String,

    /// static: HTML entry point; server/remote: URL loaded by the window.
    #[serde(default)]
    pub entry: String,

    /// Optional application icon path; currently limited to project-relative `.ico` files.
    #[serde(default)]
    pub icon: Option<String>,

    /// WebView storage policy: app, private, or global.
    #[serde(default = "default_app_storage")]
    pub storage: String,

    /// File associations registered for the packaged application under the current Windows user.
    #[serde(default)]
    pub file_associations: Vec<FileAssociationConfig>,

    /// BT main script run once at startup to register functions callable from JavaScript.
    #[serde(
        default,
        deserialize_with = "deserialize_app_main",
        serialize_with = "serialize_app_main"
    )]
    pub main: AppMain,
}

/// Windows file association configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAssociationConfig {
    /// File extensions without leading dots.
    #[serde(default)]
    pub extensions: Vec<String>,

    /// Optional file type icon path; defaults to the main application icon.
    #[serde(default)]
    pub icon: Option<String>,

    /// File type description shown in Explorer; defaults to "Application Title Document".
    #[serde(default)]
    pub description: Option<String>,

    /// Explorer context-menu text; defaults to "Open with Application Title".
    #[serde(default)]
    pub context_menu: Option<String>,
}

/// Desktop development-mode configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevConfig {
    /// Whether to watch project resources and automatically refresh the window.
    #[serde(default = "default_true")]
    pub watch: bool,

    /// Debounce delay after file changes, in milliseconds.
    #[serde(default = "default_dev_delay")]
    pub delay: u64,

    /// Whether the desktop WebView may open developer tools.
    #[serde(default)]
    pub devtools: bool,

    /// Whether to enable the desktop application's debug console; when enabled, `echo()` writes to it.
    #[serde(default = "default_true")]
    pub console: bool,
}

/// Desktop window configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    /// Initial main window width.
    #[serde(default = "default_window_width")]
    pub width: u32,

    /// Initial main window height.
    #[serde(default = "default_window_height")]
    pub height: u32,

    /// Whether the main window is resizable.
    #[serde(default = "default_true")]
    pub resizable: bool,

    /// Whether the main window starts fullscreen.
    #[serde(default)]
    pub fullscreen: bool,

    /// Whether to hide system decorations.
    #[serde(default)]
    pub hide_titlebar: bool,

    /// Whether to enable transparent backgrounds for the native window and WebView.
    #[serde(default)]
    pub transparent: bool,

    /// Whether the main window is always on top.
    #[serde(default)]
    pub always_on_top: bool,
}

/// Default configuration written when index.html exists but app.json does not.
///
/// This is the sole fallback after legacy HTML-tag configuration was removed and must match the startup documentation.
const DEFAULT_INDEX_APP_JSON: &str = r#"{
  "app": {
    "id": "org.btlang.bt_app",
    "name": "BT-APP",
    "title": "BT-APP",
    "version": "1.0.0",
    "description": "BT desktop application",
    "copyright": "Copyright 2026 BT",
    "mode": "static",
    "entry": "index.html",
    "storage": "app",
    "main": "main.bt"
  },
  "window": {
    "width": 800,
    "height": 500,
    "resizable": true,
    "fullscreen": false,
    "hide_titlebar": false,
    "transparent": false,
    "always_on_top": false
  },
  "dev": {
    "watch": true,
    "delay": 500,
    "devtools": true,
    "console": true
  },
  "resources": [
    "app.json",
    "index.html",
    "assets/**"
  ],
  "exclude": []
}"#;

/// Execution policy for app.main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppMain {
    /// When absent or true/null, the runtime automatically tries the current project's `main.bt`.
    Auto,

    /// When false or an empty string, no main script is run.
    Disabled,

    /// When a non-empty string, only the specified script is run.
    File(String),
}

impl Default for AppMain {
    /// Defaults to discovering `main.bt` automatically for compatibility with projects without app.main.
    fn default() -> Self {
        AppMain::Auto
    }
}

impl Default for AppJson {
    /// Return a runnable default desktop application configuration.
    fn default() -> Self {
        Self {
            app: AppInfo::default(),
            window: WindowConfig::default(),
            dev: DevConfig::default(),
            resources: Vec::new(),
            exclude: Vec::new(),
        }
    }
}

impl Default for AppInfo {
    /// Return the default basic application information.
    fn default() -> Self {
        Self {
            id: default_app_id_for_name(&default_app_name()),
            id_generated: true,
            name: default_app_name(),
            title: default_app_title(),
            version: default_app_version(),
            description: None,
            copyright: None,
            mode: default_app_mode(),
            entry: String::new(),
            icon: None,
            storage: default_app_storage(),
            file_associations: Vec::new(),
            main: AppMain::Auto,
        }
    }
}

impl Default for DevConfig {
    /// Return the default development-mode configuration.
    fn default() -> Self {
        Self {
            watch: true,
            delay: default_dev_delay(),
            devtools: false,
            console: true,
        }
    }
}

impl Default for WindowConfig {
    /// Return the default desktop main window configuration.
    fn default() -> Self {
        Self {
            width: default_window_width(),
            height: default_window_height(),
            resizable: true,
            fullscreen: false,
            hide_titlebar: false,
            transparent: false,
            always_on_top: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Default development-mode file-change debounce delay.
fn default_dev_delay() -> u64 {
    500
}

/// Default application name.
fn default_app_name() -> String {
    "BTApp".to_string()
}

impl AppInfo {
    /// Return the application identity key used by persistent data, the WebView profile, and system registrations.
    ///
    /// Legacy app.json files without an explicit migration receive an ID generated from name and continue using
    /// the original name to preserve login state and application data after upgrades. New applications with an explicit ID use it for isolation.
    pub fn identity_key(&self) -> &str {
        if self.id_generated {
            &self.name
        } else {
            &self.id
        }
    }
}

/// Generate a stable default identifier compatible with legacy app.json files from the application name.
///
/// An explicit `app.id` still takes precedence. This only provides a deterministic fallback for legacy projects;
/// authors of public BTR releases should supply their own reverse-domain name to prevent collisions between same-named applications from different sources.
pub(crate) fn default_app_id_for_name(name: &str) -> String {
    let mut segment = String::with_capacity(name.len());
    let mut previous_separator = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            segment.push(ch.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator && !segment.is_empty() {
            segment.push('_');
            previous_separator = true;
        }
    }
    while segment.ends_with('_') {
        segment.pop();
    }
    if segment.is_empty() {
        segment.push_str("app");
    }
    format!("org.btlang.{}", segment)
}

/// Default window title.
fn default_app_title() -> String {
    "BT Desktop Application".to_string()
}

/// Default application version.
fn default_app_version() -> String {
    "1.0.0".to_string()
}

/// Default runtime mode.
fn default_app_mode() -> String {
    "static".to_string()
}

/// Default WebView storage policy.
fn default_app_storage() -> String {
    "app".to_string()
}

/// Default window width.
fn default_window_width() -> u32 {
    800
}

/// Default window height.
fn default_window_height() -> u32 {
    500
}

/// Read and validate a BT desktop application configuration from a string.
pub fn load_app_json_from_str(raw: &str) -> Result<AppJson, BtError> {
    let mut config: AppJson = serde_json::from_str(raw)?;
    finalize_app_json(&mut config)?;
    Ok(config)
}

/// Read and validate a BT desktop application configuration from a file path.
pub fn load_app_json_from_path(path: &Path) -> Result<AppJson, BtError> {
    let raw = fs::read_to_string(path)?;
    load_app_json_from_str(&raw)
}

/// Return the default app.json configuration used by index.html fallback projects.
pub fn default_index_app_config() -> Result<AppJson, BtError> {
    load_app_json_from_str(DEFAULT_INDEX_APP_JSON)
}

/// Create a default app.json for a directory containing only index.html and return the validated configuration.
///
/// app.main in the default configuration points to `main.bt`. To let legacy projects containing only index.html start directly,
/// an empty file is created when `main.bt` is missing; existing user files are never overwritten.
pub fn create_default_app_json_for_index(project_dir: &Path) -> Result<AppJson, BtError> {
    let config = default_index_app_config()?;
    ensure_default_main_file(project_dir, &config)?;

    let app_json_path = project_dir.join("app.json");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&app_json_path)
    {
        Ok(mut file) => file.write_all(DEFAULT_INDEX_APP_JSON.as_bytes())?,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return load_app_json_from_path(&app_json_path);
        }
        Err(err) => return Err(BtError::Io(err)),
    }

    Ok(config)
}

/// Add the runnable but empty main.bt file required by the default configuration.
fn ensure_default_main_file(project_dir: &Path, config: &AppJson) -> Result<(), BtError> {
    let AppMain::File(main) = &config.app.main else {
        return Ok(());
    };
    let main_path = project_dir.join(main);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&main_path)
    {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && main_path.is_file() => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(BtError::Config(format!(
            "app.main is not a file: {}",
            main_path.display()
        ))),
        Err(err) => Err(BtError::Io(err)),
    }
}

/// Normalize and validate a desktop application configuration.
pub fn finalize_app_json(config: &mut AppJson) -> Result<(), BtError> {
    normalize_app_json(config);
    validate_app_json(config)
}

/// Normalize fields in app.json that may be omitted or blank.
fn normalize_app_json(config: &mut AppJson) {
    normalize_text_or_default(&mut config.app.name, default_app_name);
    let app_id = config.app.id.trim().to_ascii_lowercase();
    config.app.id_generated = config.app.id_generated || app_id.is_empty();
    config.app.id = if config.app.id_generated {
        default_app_id_for_name(&config.app.name)
    } else {
        app_id
    };
    normalize_text_or_default(&mut config.app.title, default_app_title);
    normalize_text_or_default(&mut config.app.version, default_app_version);
    normalize_text_or_default(&mut config.app.mode, default_app_mode);
    normalize_text_or_default(&mut config.app.storage, default_app_storage);
    normalize_optional_text(&mut config.app.description);
    normalize_optional_text(&mut config.app.copyright);
    for association in &mut config.app.file_associations {
        if let Some(icon) = association.icon.take() {
            let icon = icon.trim();
            if !icon.is_empty() {
                association.icon = Some(normalize_project_resource_name(icon));
            }
        }
        normalize_optional_text(&mut association.description);
        normalize_optional_text(&mut association.context_menu);
        for extension in &mut association.extensions {
            *extension = extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase();
        }
        association.extensions.sort_unstable();
        association.extensions.dedup();
    }

    config.app.mode = config.app.mode.to_ascii_lowercase();
    config.app.storage = config.app.storage.to_ascii_lowercase();
    if config.app.entry.trim().is_empty() {
        config.app.entry = default_entry_for_mode(&config.app.mode);
    } else {
        config.app.entry = config.app.entry.trim().to_string();
    }

    if config.app.mode == "static" {
        config.app.entry = normalize_project_resource_name(&config.app.entry);
    }

    if let Some(icon) = config.app.icon.take() {
        let icon = icon.trim();
        if !icon.is_empty() {
            config.app.icon = Some(normalize_project_resource_name(icon));
        }
    }

    if let AppMain::File(path) = &mut config.app.main {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            config.app.main = AppMain::Disabled;
        } else {
            *path = normalize_project_resource_name(trimmed);
        }
    }

    if config.window.width == 0 {
        config.window.width = default_window_width();
    }
    if config.window.height == 0 {
        config.window.height = default_window_height();
    }

    normalize_resource_rules(&mut config.resources);
    normalize_resource_rules(&mut config.exclude);
}

/// Trim resource rules and consistently use `/` separators.
fn normalize_resource_rules(values: &mut Vec<String>) {
    for value in values {
        *value = normalize_project_resource_name(value);
    }
}

/// Replace a blank field with its default text.
fn normalize_text_or_default(value: &mut String, default: fn() -> String) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        *value = default();
    } else if trimmed.len() != value.len() {
        *value = trimmed.to_string();
    }
}

/// Return the default entry point for a runtime mode.
fn default_entry_for_mode(mode: &str) -> String {
    match mode {
        "server" => "http://127.0.0.1:18280".to_string(),
        "remote" => "https://example.com".to_string(),
        _ => "index.html".to_string(),
    }
}

/// Normalize project resource paths to the `/`-separated format used by Bundle and bt://.
fn normalize_project_resource_name(value: &str) -> String {
    value.trim().replace('\\', "/")
}

/// Validate basic app.json values.
fn validate_app_json(config: &AppJson) -> Result<(), BtError> {
    validate_app_id(&config.app.id)?;
    validate_output_name_fragment(&config.app.name, "app.name")?;

    match config.app.mode.as_str() {
        "static" | "server" | "remote" => {}
        other => {
            return Err(BtError::Config(format!(
                "app.mode must be static, server, or remote; current value: {}",
                other
            )));
        }
    }

    match config.app.storage.as_str() {
        "app" | "private" | "global" => {}
        other => {
            return Err(BtError::Config(format!(
                "app.storage must be app, private, or global; current value: {}",
                other
            )));
        }
    }

    if config.app.mode == "static" {
        validate_relative_project_path(&config.app.entry, "app.entry")?;
    } else {
        validate_http_entry(&config.app.entry, &config.app.mode)?;
    }

    if let AppMain::File(main) = &config.app.main {
        validate_relative_project_path(main, "app.main")?;
    }

    if let Some(icon) = &config.app.icon {
        validate_relative_project_path(icon, "app.icon")?;
        validate_icon_extension(icon, "app.icon")?;
    }
    validate_optional_metadata_text(config.app.description.as_deref(), "app.description")?;
    validate_optional_metadata_text(config.app.copyright.as_deref(), "app.copyright")?;
    validate_file_associations(config)?;

    if config.window.width == 0 {
        return Err(BtError::Config(
            "window.width must be greater than 0".to_string(),
        ));
    }
    if config.window.height == 0 {
        return Err(BtError::Config(
            "window.height must be greater than 0".to_string(),
        ));
    }
    if config.window.transparent && !config.window.hide_titlebar {
        return Err(BtError::Config(
            "window.hide_titlebar=true is required when window.transparent=true".to_string(),
        ));
    }

    for resource in &config.resources {
        validate_resource_rule(resource, "resources")?;
    }
    for exclude in &config.exclude {
        validate_resource_rule(exclude, "exclude")?;
    }

    Ok(())
}

/// Validate that app.id is a stable reverse-domain identifier.
fn validate_app_id(value: &str) -> Result<(), BtError> {
    if value.len() > 255 || value.split('.').count() < 2 {
        return Err(BtError::Config(
            "app.id must be a reverse-domain identifier of at most 255 bytes with at least two segments".to_string(),
        ));
    }
    for segment in value.split('.') {
        let bytes = segment.as_bytes();
        if bytes.is_empty()
            || !bytes[0].is_ascii_alphanumeric()
            || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
            || bytes
                .iter()
                .any(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'-')
        {
            return Err(BtError::Config(format!(
                "app.id contains invalid segment `{}`; only letters, digits, _, and - are allowed, and segments must begin and end with a letter or digit",
                segment
            )));
        }
    }
    Ok(())
}

/// Validate Windows file association fields to prevent duplicate extensions and unusable registry paths.
fn validate_file_associations(config: &AppJson) -> Result<(), BtError> {
    let mut seen = HashSet::new();
    for (association_index, association) in config.app.file_associations.iter().enumerate() {
        if association.extensions.is_empty() {
            return Err(BtError::Config(format!(
                "app.file_associations[{}].extensions requires at least one extension",
                association_index
            )));
        }
        for extension in &association.extensions {
            if extension.is_empty()
                || extension.len() > 32
                || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
            {
                return Err(BtError::Config(format!(
                    "app.file_associations[{}] contains an invalid extension: {}",
                    association_index, extension
                )));
            }
            if !seen.insert(extension) {
                return Err(BtError::Config(format!(
                    "app.file_associations contains a duplicate extension: {}",
                    extension
                )));
            }
        }
        validate_optional_metadata_text(
            association.description.as_deref(),
            &format!("app.file_associations[{}].description", association_index),
        )?;
        validate_optional_metadata_text(
            association.context_menu.as_deref(),
            &format!("app.file_associations[{}].context_menu", association_index),
        )?;
        if let Some(icon) = association.icon.as_deref() {
            let field_name = format!("app.file_associations[{}].icon", association_index);
            validate_relative_project_path(icon, &field_name)?;
            validate_icon_extension(icon, &field_name)?;
        }
    }
    Ok(())
}

/// Normalize an optional text field, treating blank values as unset.
fn normalize_optional_text(value: &mut Option<String>) {
    if let Some(text) = value.take() {
        let text = text.trim();
        if !text.is_empty() {
            *value = Some(text.to_string());
        }
    }
}

/// Validate that entry points for URL-loading modes are HTTP or HTTPS URLs.
fn validate_http_entry(entry: &str, mode: &str) -> Result<(), BtError> {
    let url = Url::parse(entry.trim())
        .map_err(|err| BtError::Config(format!("Invalid {} entry URL: {}", mode, err)))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(BtError::Config(format!(
            "{} mode entry point must begin with http:// or https://",
            mode
        )));
    }
    Ok(())
}

/// Validate that a project-relative path is non-empty and cannot escape the project.
pub fn validate_relative_project_path(value: &str, name: &str) -> Result<(), BtError> {
    if value.trim().is_empty() {
        return Err(BtError::Config(format!("{} cannot be empty", name)));
    }
    if value.as_bytes().contains(&0) {
        return Err(BtError::Config(format!("{} contains a null byte", name)));
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(BtError::Config(format!(
            "{} must be a relative path: {}",
            name, value
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(BtError::Config(format!(
                    "{} cannot contain ..: {}",
                    name, value
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Config(format!(
                    "{} cannot use a root path: {}",
                    name, value
                )));
            }
        }
    }
    Ok(())
}

/// Validate that the specified icon field uses the currently supported `.ico` format.
fn validate_icon_extension(icon: &str, name: &str) -> Result<(), BtError> {
    let ext = Path::new(icon)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    if !ext.eq_ignore_ascii_case("ico") {
        return Err(BtError::Config(format!(
            "{} currently supports only .ico files: {}",
            name, icon
        )));
    }
    Ok(())
}

/// Validate that an application name can be used as a single filename component.
fn validate_output_name_fragment(value: &str, name: &str) -> Result<(), BtError> {
    if value.trim().is_empty() {
        return Err(BtError::Config(format!("{} cannot be empty", name)));
    }
    let path = Path::new(value);
    if path.components().count() != 1 || path.is_absolute() {
        return Err(BtError::Config(format!(
            "{} must be a filename and cannot contain a path: {}",
            name, value
        )));
    }
    if value.chars().any(|ch| {
        matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control()
    }) {
        return Err(BtError::Config(format!(
            "{} contains characters invalid in Windows filenames: {}",
            name, value
        )));
    }
    Ok(())
}

/// Validate that text written to system resources contains no null bytes.
fn validate_optional_metadata_text(value: Option<&str>, name: &str) -> Result<(), BtError> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.as_bytes().contains(&0) {
        return Err(BtError::Config(format!("{} contains a null byte", name)));
    }
    Ok(())
}

/// Validate that a resource rule is a safe project-relative path or glob.
fn validate_resource_rule(rule: &str, name: &str) -> Result<(), BtError> {
    if rule.trim().is_empty() {
        return Err(BtError::Config(format!(
            "{} cannot contain an empty path",
            name
        )));
    }
    if rule.as_bytes().contains(&0) {
        return Err(BtError::Config(format!(
            "{} contains an invalid null byte: {}",
            name, rule
        )));
    }
    let path = Path::new(rule);
    if path.is_absolute() {
        return Err(BtError::Config(format!(
            "{} cannot use an absolute path: {}",
            name, rule
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(BtError::Config(format!(
                    "{} cannot contain a .. path: {}",
                    name, rule
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Config(format!(
                    "{} cannot contain a root path: {}",
                    name, rule
                )));
            }
        }
    }
    Ok(())
}

/// Deserialize app.main with support for strings, false, true, and null.
fn deserialize_app_main<'de, D>(deserializer: D) -> Result<AppMain, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(AppMain::Auto),
        serde_json::Value::Bool(false) => Ok(AppMain::Disabled),
        serde_json::Value::Bool(true) => Ok(AppMain::Auto),
        serde_json::Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                Ok(AppMain::Disabled)
            } else {
                Ok(AppMain::File(value.to_string()))
            }
        }
        _ => Err(serde::de::Error::custom(
            "app.main must be a string, false, true, or null",
        )),
    }
}

/// Serialize app.main in an app.json-compatible format.
fn serialize_app_main<S>(main: &AppMain, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match main {
        AppMain::Auto => serializer.serialize_none(),
        AppMain::Disabled => serializer.serialize_bool(false),
        AppMain::File(path) => serializer.serialize_str(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// An omitted app.json configuration is completed with runnable static defaults.
    #[test]
    fn defaults_all_missing_fields() {
        let config = load_app_json_from_str("{}").unwrap();
        assert_eq!(config.app.id, "org.btlang.btapp");
        assert!(config.app.id_generated);
        assert_eq!(config.app.identity_key(), "BTApp");
        assert_eq!(config.app.name, "BTApp");
        assert_eq!(config.app.title, "BT Desktop Application");
        assert_eq!(config.app.mode, "static");
        assert_eq!(config.app.entry, "index.html");
        assert_eq!(config.app.storage, "app");
        assert_eq!(config.app.description, None);
        assert_eq!(config.app.copyright, None);
        assert_eq!(config.app.main, AppMain::Auto);
        assert_eq!(config.window.width, 800);
        assert_eq!(config.window.height, 500);
        assert!(!config.window.transparent);
        assert!(config.dev.watch);
        assert_eq!(config.dev.delay, 500);
        assert!(!config.dev.devtools);
        assert!(config.dev.console);
        assert!(config.exclude.is_empty());
    }

    /// app.id is normalized to lowercase and rejects unsafe or unstable segments.
    #[test]
    fn normalizes_and_validates_app_id() {
        let config = load_app_json_from_str(r#"{"app":{"id":" COM.Example.Demo_App "}}"#).unwrap();
        assert_eq!(config.app.id, "com.example.demo_app");
        assert!(!config.app.id_generated);
        assert_eq!(config.app.identity_key(), "com.example.demo_app");

        let explicit_default =
            load_app_json_from_str(r#"{"app":{"id":"org.btlang.btapp"}}"#).unwrap();
        assert!(!explicit_default.app.id_generated);
        assert_eq!(explicit_default.app.identity_key(), "org.btlang.btapp");

        let error = load_app_json_from_str(r#"{"app":{"id":"bad/path"}}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("app.id"));
    }

    /// app.description and app.copyright preserve non-empty text while trimming surrounding whitespace.
    #[test]
    fn normalizes_app_metadata_text() {
        let config = load_app_json_from_str(
            r#"{"app":{"description":"  File description  ","copyright":"  Copyright 2026  "}}"#,
        )
        .unwrap();
        assert_eq!(config.app.description.as_deref(), Some("File description"));
        assert_eq!(config.app.copyright.as_deref(), Some("Copyright 2026"));
    }

    /// app.main=false explicitly disables the main script.
    #[test]
    fn app_main_false_disables_script() {
        let config = load_app_json_from_str(r#"{"app":{"main":false}}"#).unwrap();
        assert_eq!(config.app.main, AppMain::Disabled);
    }

    /// An empty app.main string explicitly disables the main script.
    #[test]
    fn app_main_empty_string_disables_script() {
        let config = load_app_json_from_str(r#"{"app":{"main":""}}"#).unwrap();
        assert_eq!(config.app.main, AppMain::Disabled);
    }

    /// A non-empty app.main string is retained as the specified script path.
    #[test]
    fn app_main_string_selects_file() {
        let config = load_app_json_from_str(r#"{"app":{"main":"scripts\\boot.bt"}}"#).unwrap();
        assert_eq!(
            config.app.main,
            AppMain::File("scripts/boot.bt".to_string())
        );
    }

    /// Server mode without an entry point falls back to the default local address.
    #[test]
    fn server_mode_defaults_entry_url() {
        let config = load_app_json_from_str(r#"{"app":{"mode":"server"}}"#).unwrap();
        assert_eq!(config.app.entry, "http://127.0.0.1:18280");
    }

    /// A transparent window must also hide system decorations so the non-client area does not remain opaque.
    #[test]
    fn transparent_window_requires_hidden_titlebar() {
        let error = load_app_json_from_str(r#"{"window":{"transparent":true}}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("window.hide_titlebar=true"));

        let config =
            load_app_json_from_str(r#"{"window":{"hide_titlebar":true,"transparent":true}}"#)
                .unwrap();
        assert!(config.window.hide_titlebar);
        assert!(config.window.transparent);
    }

    /// icon accepts only project-relative ICO files.
    #[test]
    fn rejects_non_ico_icon() {
        let error = load_app_json_from_str(r#"{"app":{"icon":"logo.png"}}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("app.icon currently supports only .ico"));
    }

    /// app.storage accepts only defined WebView storage policies.
    #[test]
    fn rejects_invalid_storage_policy() {
        let error = load_app_json_from_str(r#"{"app":{"storage":"bad"}}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("app.storage must be app, private, or global"));
    }

    /// File association extensions lose leading dots, are lowercased, and are deduplicated.
    #[test]
    fn normalizes_file_association_extensions() {
        let config = load_app_json_from_str(
            r#"{"app":{"file_associations":[{"extensions":[".MD","md","Markdown"],"icon":"  icons\\markdown.ico  ","context_menu":"  Open with Demo  "}]}}"#,
        )
        .unwrap();

        assert_eq!(
            config.app.file_associations[0].extensions,
            vec!["markdown".to_string(), "md".to_string()]
        );
        assert_eq!(
            config.app.file_associations[0].context_menu.as_deref(),
            Some("Open with Demo")
        );
        assert_eq!(
            config.app.file_associations[0].icon.as_deref(),
            Some("icons/markdown.ico")
        );
    }

    /// A file association icon must be a safe project-relative ICO file.
    #[test]
    fn rejects_invalid_file_association_icon() {
        let format_error = load_app_json_from_str(
            r#"{"app":{"file_associations":[{"extensions":["md"],"icon":"markdown.png"}]}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(format_error.contains("app.file_associations[0].icon currently supports only .ico"));

        let path_error = load_app_json_from_str(
            r#"{"app":{"file_associations":[{"extensions":["md"],"icon":"../markdown.ico"}]}}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(path_error.contains("app.file_associations[0].icon"));
    }

    /// An extension cannot be declared by multiple associations, preventing later startup entries from overwriting earlier ones.
    #[test]
    fn rejects_duplicate_file_association_extensions() {
        let error = load_app_json_from_str(
            r#"{"app":{"file_associations":[{"extensions":["md"]},{"extensions":[".MD"]}]}}"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("app.file_associations contains a duplicate extension: md"));
    }

    /// The index.html fallback configuration stays aligned with the default app.json under the new rules.
    #[test]
    fn default_index_app_config_matches_spec() {
        let config = default_index_app_config().unwrap();

        assert_eq!(config.app.name, "BT-APP");
        assert_eq!(config.app.title, "BT-APP");
        assert_eq!(
            config.app.description.as_deref(),
            Some("BT desktop application")
        );
        assert_eq!(config.app.copyright.as_deref(), Some("Copyright 2026 BT"));
        assert_eq!(config.app.entry, "index.html");
        assert_eq!(config.app.storage, "app");
        assert_eq!(config.app.main, AppMain::File("main.bt".to_string()));
        assert!(!config.window.transparent);
        assert!(config.dev.watch);
        assert_eq!(config.dev.delay, 500);
        assert!(config.dev.devtools);
        assert!(config.dev.console);
        assert_eq!(
            config.resources,
            vec![
                "app.json".to_string(),
                "index.html".to_string(),
                "assets/**".to_string()
            ]
        );
        assert!(config.exclude.is_empty());
    }

    /// A directory containing only index.html receives a default app.json and an empty main.bt.
    #[test]
    fn create_default_app_json_for_index_writes_files() {
        let dir = fresh_temp_dir("default-index-config");
        fs::write(dir.join("index.html"), "<h1>BT</h1>").unwrap();

        let config = create_default_app_json_for_index(&dir).unwrap();

        assert_eq!(config.app.name, "BT-APP");
        assert!(dir.join("app.json").is_file());
        assert!(dir.join("main.bt").is_file());
        let raw = fs::read_to_string(dir.join("app.json")).unwrap();
        assert!(raw.contains(r#""name": "BT-APP""#));
        assert!(raw.contains(r#""storage": "app""#));
        assert!(raw.contains(r#""main": "main.bt""#));

        let _ = fs::remove_dir_all(dir);
    }

    /// Create a unique test directory.
    fn fresh_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "bt-app-config-test-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
