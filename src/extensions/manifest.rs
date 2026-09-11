//! Parsing and validation for `.bts` extension package manifests.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::extensions::is_lower_identifier;
use crate::extensions::is_safe_package_path;

/// Supported extension package format version.
pub const SUPPORTED_FORMAT_VERSION: u32 = 1;
/// Supported bindings semantic version.
pub const SUPPORTED_API_VERSION: u32 = 1;
/// Host hard limit for extension call arguments.
pub const HOST_MAX_ARGS_BYTES: u64 = 16 * 1024 * 1024;
/// Host hard limit for extension call return values.
pub const HOST_MAX_RESULT_BYTES: u64 = 16 * 1024 * 1024;
/// Host hard limit for the number of shared extension service workers.
pub const HOST_MAX_RUNTIME_WORKERS: u32 = 64;
/// Host hard limit for the shared extension service queue length.
pub const HOST_MAX_RUNTIME_QUEUE_LIMIT: u32 = 65_536;
/// Host hard limit for a single shared extension service call timeout.
pub const HOST_MAX_RUNTIME_CALL_TIMEOUT_MS: u64 = 300_000;
/// Host hard limit for shared extension service idle retention time.
pub const HOST_MAX_RUNTIME_IDLE_TTL_MS: u64 = 3_600_000;
/// Host hard limit for the total number of shared extension service objects.
pub const HOST_MAX_RUNTIME_OBJECTS: u32 = 65_536;
/// Host hard limit for the number of objects inside one shared worker.
pub const HOST_MAX_RUNTIME_WORKER_OBJECTS: u32 = 4_096;
/// Host hard limit for the number of in-flight calls in the shared extension service.
pub const HOST_MAX_RUNTIME_INFLIGHT_CALLS: u32 = 4_096;

/// Default number of shared extension service workers.
const DEFAULT_RUNTIME_WORKERS: u32 = 1;
/// Default shared extension service queue length.
const DEFAULT_RUNTIME_QUEUE_LIMIT: u32 = 1_024;
/// Default single-call timeout for the shared extension service.
const DEFAULT_RUNTIME_CALL_TIMEOUT_MS: u64 = 30_000;
/// Default shared extension service idle retention time.
const DEFAULT_RUNTIME_IDLE_TTL_MS: u64 = 300_000;
/// Default shared extension service object limit.
const DEFAULT_RUNTIME_MAX_OBJECTS: u32 = 65_536;
/// Default object limit inside one shared worker.
const DEFAULT_RUNTIME_MAX_WORKER_OBJECTS: u32 = 4_096;
/// Default in-flight call limit for the shared extension service.
const DEFAULT_RUNTIME_MAX_INFLIGHT_CALLS: u32 = 64;

/// Maximum length for string fields in the manifest.
const MAX_TEXT_FIELD_CHARS: usize = 512;
/// Maximum length for name fields in the manifest.
const MAX_NAME_FIELD_CHARS: usize = 64;
/// Maximum length for in-package path fields in the manifest.
const MAX_PACKAGE_PATH_CHARS: usize = 256;

/// Extension package manifest metadata.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionManifest {
    /// Package format identifier, currently must be `bts`.
    pub format: String,
    /// Package format version, currently must be `1`.
    pub format_version: u32,
    /// Extension name, allowing only lowercase letters, digits, and underscores.
    pub name: String,
    /// Extension version, using three-part SemVer.
    pub version: String,
    /// Extension description text.
    #[serde(default)]
    pub description: Option<String>,
    /// Short extension summary used by catalogs and tooling.
    #[serde(default)]
    pub summary: Option<String>,
    /// Extension author text.
    #[serde(default)]
    pub author: Option<String>,
    /// Public extension developer or publisher identity.
    #[serde(default)]
    pub developer: Option<ExtensionDeveloper>,
    /// Public source repository URL.
    #[serde(default)]
    pub repository: Option<String>,
    /// SPDX license expression for the extension package.
    #[serde(default)]
    pub license: Option<String>,
    /// Localized display metadata keyed by normalized BCP 47 language tag.
    #[serde(default)]
    pub locales: BTreeMap<String, ExtensionLocaleMetadata>,
    /// Extension backend type.
    pub kind: ExtensionKind,
    /// Extension ABI name, which must match the backend type.
    pub abi: String,
    /// Minimum BT version required by the extension.
    pub bt_min_version: String,
    /// Bindings semantic version, currently must be `1`.
    pub api_version: u32,
    /// In-package path to the backend entry file.
    pub entry: String,
    /// In-package path to the bindings description file.
    pub bindings: String,
    /// Call resource limits declared by the extension.
    #[serde(default)]
    pub limits: ExtensionLimits,
    /// Extension runtime mode and shared runtime resource configuration.
    #[serde(default)]
    pub runtime: ExtensionRuntime,
}

/// Public developer identity carried with an extension package.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDeveloper {
    /// Stable lowercase developer identifier.
    pub id: String,
    /// Default developer display name.
    pub name: String,
    /// Optional public developer homepage.
    #[serde(default)]
    pub homepage: Option<String>,
}

/// Localized extension display metadata that never changes API identifiers.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionLocaleMetadata {
    /// Localized short summary.
    #[serde(default)]
    pub summary: Option<String>,
    /// Localized full description.
    #[serde(default)]
    pub description: Option<String>,
    /// Localized developer display name.
    #[serde(default)]
    pub developer_name: Option<String>,
}

/// Extension backend type.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionKind {
    /// Pure BT source extension package.
    Bt,
    /// WASM/WASI extension package.
    Wasm,
}

impl ExtensionKind {
    /// Return the ABI name for this backend type.
    pub fn expected_abi(self) -> &'static str {
        match self {
            ExtensionKind::Bt => "bts-bt-1",
            ExtensionKind::Wasm => "bts-wasi-1",
        }
    }

    /// Return the backend name used in error messages.
    pub fn name(self) -> &'static str {
        match self {
            ExtensionKind::Bt => "bt",
            ExtensionKind::Wasm => "wasm",
        }
    }
}

/// Extension runtime mode.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionRuntimeMode {
    /// Current thread-local runtime, preserving the existing Runner cache model.
    ThreadLocal,
    /// Project-level shared runtime, handled later by ExtensionService.
    Shared,
}

impl Default for ExtensionRuntimeMode {
    /// Return the default runtime mode.
    fn default() -> Self {
        Self::ThreadLocal
    }
}

impl ExtensionRuntimeMode {
    /// Return the lowercase name used for manifests and error messages.
    pub fn name(self) -> &'static str {
        match self {
            ExtensionRuntimeMode::ThreadLocal => "thread_local",
            ExtensionRuntimeMode::Shared => "shared",
        }
    }

    /// Check whether this mode requires a project-level shared service.
    pub fn is_shared(self) -> bool {
        matches!(self, ExtensionRuntimeMode::Shared)
    }
}

/// Extension call resource limits.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionLimits {
    /// Maximum encoded bytes for a single call's arguments.
    pub max_args_bytes: u64,
    /// Maximum encoded bytes for a single call's return value.
    pub max_result_bytes: u64,
}

impl Default for ExtensionLimits {
    /// Return the default extension call resource limits.
    fn default() -> Self {
        Self {
            max_args_bytes: HOST_MAX_ARGS_BYTES,
            max_result_bytes: HOST_MAX_RESULT_BYTES,
        }
    }
}

/// Extension runtime configuration.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionRuntime {
    /// Runtime mode, defaulting to the current thread-local cache.
    pub mode: ExtensionRuntimeMode,
    /// Number of workers in shared mode.
    pub workers: u32,
    /// Bounded wait queue length in shared mode.
    pub queue_limit: u32,
    /// Single-call timeout in shared mode, in milliseconds.
    pub call_timeout_ms: u64,
    /// Idle service retention time in shared mode, in milliseconds.
    pub idle_ttl_ms: u64,
    /// Service-level object limit in shared mode.
    pub max_objects: u32,
    /// Object limit per worker in shared mode.
    pub max_worker_objects: u32,
    /// In-flight call limit in shared mode.
    pub max_inflight_calls: u32,
}

impl Default for ExtensionRuntime {
    /// Return the default extension runtime configuration.
    fn default() -> Self {
        Self {
            mode: ExtensionRuntimeMode::ThreadLocal,
            workers: DEFAULT_RUNTIME_WORKERS,
            queue_limit: DEFAULT_RUNTIME_QUEUE_LIMIT,
            call_timeout_ms: DEFAULT_RUNTIME_CALL_TIMEOUT_MS,
            idle_ttl_ms: DEFAULT_RUNTIME_IDLE_TTL_MS,
            max_objects: DEFAULT_RUNTIME_MAX_OBJECTS,
            max_worker_objects: DEFAULT_RUNTIME_MAX_WORKER_OBJECTS,
            max_inflight_calls: DEFAULT_RUNTIME_MAX_INFLIGHT_CALLS,
        }
    }
}

impl ExtensionManifest {
    /// Parse and validate a manifest from JSON text.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(raw)
            .map_err(|err| format!("Failed to parse manifest.json: {}", err))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate manifest fields and ranges.
    pub fn validate(&self) -> Result<(), String> {
        if self.format != "bts" {
            return Err(format!(
                "manifest.format must be `bts`, got `{}`",
                self.format
            ));
        }
        if self.format_version != SUPPORTED_FORMAT_VERSION {
            return Err(format!(
                "manifest.format_version only supports {}, got {}",
                SUPPORTED_FORMAT_VERSION, self.format_version
            ));
        }
        validate_name_field("manifest.name", &self.name)?;
        if !is_lower_identifier(&self.name) {
            return Err(format!(
                "manifest.name `{}` may contain only lowercase letters, digits, and underscores, and must start with a lowercase letter",
                self.name
            ));
        }
        validate_version_field("manifest.version", &self.version)?;
        if let Some(description) = &self.description {
            validate_text_field("manifest.description", description)?;
        }
        if let Some(summary) = &self.summary {
            validate_text_field("manifest.summary", summary)?;
        }
        if let Some(author) = &self.author {
            validate_text_field("manifest.author", author)?;
        }
        if let Some(developer) = &self.developer {
            validate_name_field("manifest.developer.id", &developer.id)?;
            if !is_lower_identifier(&developer.id) {
                return Err("manifest.developer.id must be a lowercase identifier".to_string());
            }
            validate_text_field("manifest.developer.name", &developer.name)?;
            if let Some(homepage) = &developer.homepage {
                validate_text_field("manifest.developer.homepage", homepage)?;
            }
        }
        if let Some(repository) = &self.repository {
            validate_text_field("manifest.repository", repository)?;
        }
        if let Some(license) = &self.license {
            validate_text_field("manifest.license", license)?;
        }
        if self.locales.len() > 16 {
            return Err("manifest.locales may contain at most 16 languages".to_string());
        }
        for (locale, metadata) in &self.locales {
            validate_locale_tag(locale)?;
            if let Some(summary) = &metadata.summary {
                validate_text_field(&format!("manifest.locales.{}.summary", locale), summary)?;
            }
            if let Some(description) = &metadata.description {
                validate_text_field(
                    &format!("manifest.locales.{}.description", locale),
                    description,
                )?;
            }
            if let Some(developer_name) = &metadata.developer_name {
                validate_text_field(
                    &format!("manifest.locales.{}.developer_name", locale),
                    developer_name,
                )?;
            }
        }
        if self.abi != self.kind.expected_abi() {
            return Err(format!(
                "manifest.kind={} must use ABI `{}`, got `{}`",
                self.kind.name(),
                self.kind.expected_abi(),
                self.abi
            ));
        }
        validate_version_field("manifest.bt_min_version", &self.bt_min_version)?;
        validate_bt_min_version(&self.bt_min_version)?;
        if self.api_version != SUPPORTED_API_VERSION {
            return Err(format!(
                "manifest.api_version only supports {}, got {}",
                SUPPORTED_API_VERSION, self.api_version
            ));
        }
        validate_package_path_field("manifest.entry", &self.entry)?;
        validate_package_path_field("manifest.bindings", &self.bindings)?;
        self.validate_limits()?;
        self.validate_runtime()?;
        Ok(())
    }

    /// Check that manifest-declared resource limits stay within host hard limits.
    fn validate_limits(&self) -> Result<(), String> {
        if self.limits.max_args_bytes == 0 || self.limits.max_args_bytes > HOST_MAX_ARGS_BYTES {
            return Err(format!(
                "manifest.limits.max_args_bytes must be in the range 1..={}",
                HOST_MAX_ARGS_BYTES
            ));
        }
        if self.limits.max_result_bytes == 0 || self.limits.max_result_bytes > HOST_MAX_RESULT_BYTES
        {
            return Err(format!(
                "manifest.limits.max_result_bytes must be in the range 1..={}",
                HOST_MAX_RESULT_BYTES
            ));
        }
        Ok(())
    }

    /// Validate the manifest runtime mode and shared runtime resource limits.
    fn validate_runtime(&self) -> Result<(), String> {
        if self.runtime.mode.is_shared() && self.kind != ExtensionKind::Wasm {
            return Err(format!(
                "manifest.runtime.mode=shared in v1 only supports kind=wasm, current kind={}",
                self.kind.name()
            ));
        }
        validate_runtime_u32_range(
            "manifest.runtime.workers",
            self.runtime.workers,
            HOST_MAX_RUNTIME_WORKERS,
        )?;
        validate_runtime_u32_range(
            "manifest.runtime.queue_limit",
            self.runtime.queue_limit,
            HOST_MAX_RUNTIME_QUEUE_LIMIT,
        )?;
        validate_runtime_u64_range(
            "manifest.runtime.call_timeout_ms",
            self.runtime.call_timeout_ms,
            HOST_MAX_RUNTIME_CALL_TIMEOUT_MS,
        )?;
        validate_runtime_u64_range(
            "manifest.runtime.idle_ttl_ms",
            self.runtime.idle_ttl_ms,
            HOST_MAX_RUNTIME_IDLE_TTL_MS,
        )?;
        validate_runtime_u32_range(
            "manifest.runtime.max_objects",
            self.runtime.max_objects,
            HOST_MAX_RUNTIME_OBJECTS,
        )?;
        validate_runtime_u32_range(
            "manifest.runtime.max_worker_objects",
            self.runtime.max_worker_objects,
            HOST_MAX_RUNTIME_WORKER_OBJECTS,
        )?;
        validate_runtime_u32_range(
            "manifest.runtime.max_inflight_calls",
            self.runtime.max_inflight_calls,
            HOST_MAX_RUNTIME_INFLIGHT_CALLS,
        )?;
        if self.runtime.max_worker_objects > self.runtime.max_objects {
            return Err(
                "manifest.runtime.max_worker_objects cannot exceed manifest.runtime.max_objects"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Validate a shared runtime u32 field range.
fn validate_runtime_u32_range(field: &str, value: u32, max: u32) -> Result<(), String> {
    if value == 0 || value > max {
        return Err(format!("{} must be in the range 1..={}", field, max));
    }
    Ok(())
}

/// Validate a shared runtime u64 field range.
fn validate_runtime_u64_range(field: &str, value: u64, max: u64) -> Result<(), String> {
    if value == 0 || value > max {
        return Err(format!("{} must be in the range 1..={}", field, max));
    }
    Ok(())
}

/// Validate a plain text field length.
fn validate_text_field(field: &str, value: &str) -> Result<(), String> {
    if value.chars().count() > MAX_TEXT_FIELD_CHARS {
        return Err(format!(
            "{} length cannot exceed {} characters",
            field, MAX_TEXT_FIELD_CHARS
        ));
    }
    Ok(())
}

/// Validate a name field for length and non-emptiness.
fn validate_name_field(field: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{} cannot be empty", field));
    }
    if value.chars().count() > MAX_NAME_FIELD_CHARS {
        return Err(format!(
            "{} length cannot exceed {} characters",
            field, MAX_NAME_FIELD_CHARS
        ));
    }
    Ok(())
}

/// Validate a canonical-cased BCP 47 language tag used as a localization key.
fn validate_locale_tag(value: &str) -> Result<(), String> {
    let parts: Vec<&str> = value.split('-').collect();
    let language_valid = parts.first().is_some_and(|part| {
        (2..=3).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_lowercase())
    });
    let subtags_valid = parts.iter().skip(1).all(|part| match part.len() {
        2 => part.bytes().all(|byte| byte.is_ascii_uppercase()),
        3 => part.bytes().all(|byte| byte.is_ascii_digit()),
        4 => {
            let bytes = part.as_bytes();
            bytes[0].is_ascii_uppercase() && bytes[1..].iter().all(|byte| byte.is_ascii_lowercase())
        }
        5..=8 => part
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        _ => false,
    });
    if value.len() > 35 || !language_valid || !subtags_valid {
        return Err(format!(
            "manifest.locales key `{}` must be a canonical-cased BCP 47 language tag",
            value
        ));
    }
    Ok(())
}

/// Validate that a version field is a three-part SemVer.
fn validate_version_field(field: &str, value: &str) -> Result<(), String> {
    parse_semver_triplet(value)
        .map(|_| ())
        .map_err(|err| format!("{} {}", field, err))
}

/// Validate that the minimum BT version does not exceed the current interpreter version.
fn validate_bt_min_version(version: &str) -> Result<(), String> {
    let required = parse_semver_triplet(version)?;
    let current = parse_semver_triplet(env!("CARGO_PKG_VERSION"))?;
    if required > current {
        return Err(format!(
            "manifest.bt_min_version `{}` is higher than the current BT version `{}`",
            version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    Ok(())
}

/// Validate an in-package path field.
fn validate_package_path_field(field: &str, value: &str) -> Result<(), String> {
    if value.chars().count() > MAX_PACKAGE_PATH_CHARS {
        return Err(format!(
            "{} length cannot exceed {} characters",
            field, MAX_PACKAGE_PATH_CHARS
        ));
    }
    if !is_safe_package_path(value) {
        return Err(format!(
            "{} `{}` must be a safe relative path and cannot contain empty segments, `.`, `..`, backslashes, absolute paths, or drive letters",
            field, value
        ));
    }
    Ok(())
}

/// Parse the major, minor, and patch parts of a SemVer value.
fn parse_semver_triplet(version: &str) -> Result<(u64, u64, u64), String> {
    let core = version
        .split(|ch| ch == '-' || ch == '+')
        .next()
        .unwrap_or_default();
    let mut parts = core.split('.');
    let major = parse_semver_part(parts.next(), version)?;
    let minor = parse_semver_part(parts.next(), version)?;
    let patch = parse_semver_part(parts.next(), version)?;
    if parts.next().is_some() {
        return Err(format!("`{}` is not a three-part SemVer", version));
    }
    Ok((major, minor, patch))
}

/// Parse one numeric SemVer field.
fn parse_semver_part(part: Option<&str>, version: &str) -> Result<u64, String> {
    let Some(part) = part else {
        return Err(format!("`{}` is not a three-part SemVer", version));
    };
    if part.is_empty()
        || !part.chars().all(|ch| ch.is_ascii_digit())
        || (part.len() > 1 && part.starts_with('0'))
    {
        return Err(format!("`{}` is not a three-part SemVer", version));
    }
    part.parse::<u64>()
        .map_err(|_| format!("`{}` is not a valid SemVer number", version))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return a valid manifest JSON text.
    fn valid_manifest_json() -> String {
        r#"{
            "format": "bts",
            "format_version": 1,
            "name": "calc",
            "version": "1.0.0",
            "summary": "Calculator extension",
            "description": "BT extension loader verification package",
            "author": "BT Team",
            "developer": {
                "id": "bt_team",
                "name": "BT Team",
                "homepage": "https://btlang.org"
            },
            "repository": "https://github.com/bt-lang/bt",
            "license": "MIT OR Apache-2.0",
            "locales": {
                "zh-CN": {
                    "summary": "计算器扩展",
                    "description": "用于测试的计算器扩展。",
                    "developer_name": "BT 团队"
                }
            },
            "kind": "bt",
            "abi": "bts-bt-1",
            "bt_min_version": "1.1.0",
            "api_version": 1,
            "entry": "src/lib.bt",
            "bindings": "bindings.json",
            "limits": {
                "max_args_bytes": 4096,
                "max_result_bytes": 4096
            }
        }"#
        .to_string()
    }

    /// Append a runtime object to a valid manifest JSON string.
    fn manifest_with_runtime(runtime: &str) -> String {
        valid_manifest_json().replace(
            r#""limits": {
                "max_args_bytes": 4096,
                "max_result_bytes": 4096
            }"#,
            &format!(
                r#""limits": {{
                "max_args_bytes": 4096,
                "max_result_bytes": 4096
            }},
            "runtime": {}"#,
                runtime
            ),
        )
    }

    /// A valid manifest should parse successfully.
    #[test]
    fn parses_valid_manifest() {
        let manifest = ExtensionManifest::parse(&valid_manifest_json()).unwrap();
        assert_eq!(manifest.name, "calc");
        assert_eq!(manifest.kind, ExtensionKind::Bt);
        assert_eq!(manifest.summary.as_deref(), Some("Calculator extension"));
        assert_eq!(
            manifest.locales["zh-CN"].summary.as_deref(),
            Some("计算器扩展")
        );
        assert_eq!(manifest.runtime.mode, ExtensionRuntimeMode::ThreadLocal);
        assert_eq!(manifest.runtime.workers, DEFAULT_RUNTIME_WORKERS);
    }

    /// Locale keys must use canonical casing so package and registry lookups are deterministic.
    #[test]
    fn rejects_non_canonical_locale_tag() {
        let raw = valid_manifest_json().replace("\"zh-CN\"", "\"zh-cn\"");
        let error = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(error.contains("canonical-cased BCP 47"));
    }

    /// Removed package-level permission metadata must be rejected as an unknown field.
    #[test]
    fn rejects_removed_permissions_field() {
        let raw = valid_manifest_json().replace(
            "\"bindings\": \"bindings.json\",",
            "\"bindings\": \"bindings.json\",\n            \"permissions\": [\"fs_read\"],",
        );
        let error = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(error.contains("unknown field `permissions`"));
    }

    /// A kind/abi mismatch should fail.
    #[test]
    fn rejects_mismatched_abi() {
        let raw = valid_manifest_json().replace("\"abi\": \"bts-bt-1\"", "\"abi\": \"bts-wasi-1\"");
        let err = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(err.contains("must use ABI"));
    }

    /// In-package entry paths must not escape their boundary.
    #[test]
    fn rejects_unsafe_entry_path() {
        let raw =
            valid_manifest_json().replace("\"entry\": \"src/lib.bt\"", "\"entry\": \"../lib.bt\"");
        let err = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(err.contains("safe relative path"));
    }

    /// A valid WASM shared runtime configuration should parse successfully.
    #[test]
    fn parses_valid_shared_runtime_manifest() {
        let raw = valid_manifest_json()
            .replace("\"kind\": \"bt\"", "\"kind\": \"wasm\"")
            .replace("\"abi\": \"bts-bt-1\"", "\"abi\": \"bts-wasi-1\"")
            .replace("\"entry\": \"src/lib.bt\"", "\"entry\": \"module.wasm\"")
            .replace(
                r#""limits": {
                "max_args_bytes": 4096,
                "max_result_bytes": 4096
            }"#,
                r#""limits": {
                "max_args_bytes": 4096,
                "max_result_bytes": 4096
            },
            "runtime": {
                "mode": "shared",
                "workers": 4,
                "queue_limit": 1024,
                "call_timeout_ms": 30000,
                "idle_ttl_ms": 300000,
                "max_objects": 65536,
                "max_worker_objects": 4096,
                "max_inflight_calls": 64
            }"#,
            );
        let manifest = ExtensionManifest::parse(&raw).unwrap();
        assert_eq!(manifest.runtime.mode, ExtensionRuntimeMode::Shared);
        assert_eq!(manifest.runtime.workers, 4);
    }

    /// runtime mode may only use known lowercase strings.
    #[test]
    fn rejects_invalid_runtime_mode() {
        let raw = manifest_with_runtime(
            r#"{
                "mode": "global"
            }"#,
        );
        let err = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(err.contains("unknown variant") || err.contains("global"));
    }

    /// runtime numeric fields must not exceed host hard limits.
    #[test]
    fn rejects_runtime_field_above_host_limit() {
        let raw = manifest_with_runtime(&format!(
            r#"{{
                "workers": {}
            }}"#,
            HOST_MAX_RUNTIME_WORKERS + 1
        ));
        let err = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(err.contains("manifest.runtime.workers"));
    }

    /// Pure BT extensions cannot declare shared runtime settings.
    #[test]
    fn rejects_bt_shared_runtime() {
        let raw = manifest_with_runtime(
            r#"{
                "mode": "shared"
            }"#,
        );
        let err = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(err.contains("shared"));
        assert!(err.contains("kind=wasm"));
    }

    /// Unknown fields in the runtime object should be rejected so extension authors do not assume the setting took effect.
    #[test]
    fn rejects_unknown_runtime_field() {
        let raw = manifest_with_runtime(
            r#"{
                "mode": "thread_local",
                "unknown": 1
            }"#,
        );
        let err = ExtensionManifest::parse(&raw).unwrap_err();
        assert!(err.contains("unknown field"));
    }
}
