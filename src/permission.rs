//! BT optional permission configuration.
//!
//! Permission checks run only at standard-library capability boundaries, never in the VM's ordinary instruction loop.
//! For compatibility, all capabilities are allowed by default and are restricted only when `BT_PERMISSION_ALLOW` or `BT_PERMISSION_DENY` is configured explicitly.

#[cfg(test)]
use std::cell::RefCell;
use std::env;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

/// Permission allow list environment variable.
pub const PERMISSION_ALLOW_ENV: &str = "BT_PERMISSION_ALLOW";
/// Permission denial list environment variable.
pub const PERMISSION_DENY_ENV: &str = "BT_PERMISSION_DENY";

/// Number of permission denials.
static PERMISSION_DENIED: AtomicUsize = AtomicUsize::new(0);
/// Process-level permission configuration cache.
static PERMISSION_CONFIG: OnceLock<Result<PermissionConfig, String>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Permission configuration overrides within the test thread.
    static TEST_PERMISSION_CONFIG: RefCell<Option<Result<PermissionConfig, String>>> = RefCell::new(None);
}

/// BT standard library capability classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// File-system access through `fs()`, including file and directory operations.
    Fs,
    /// Child-process access through `process()`.
    Process,
    /// Network services and connection capabilities, corresponding to `net()`, `net.listen()`, `net.connect()`, DNS and interface information.
    Net,
    /// HTTP client capability, corresponding to `reqwest()`.
    Http,
    /// MySQL database capability, corresponding to `mysql()`.
    Mysql,
    /// Device access through `device()`, including serial-port scanning and I/O.
    Device,
    /// Environment variable capabilities, corresponding to `BT.env()`, `BT.set_env()`, `BT.envs()` and PATH overlays.
    Env,
    /// Desktop API capabilities, reserved for `bt_app`'s window, tray, clipboard, notification, and dialog boundaries.
    Desktop,
    /// Screen capture capability, corresponding to `bt.screen`'s full-screen color selection and frame selection screenshots.
    Screen,
    /// Native dynamic library FFI capability, corresponding to global `ffi`.
    #[cfg(feature = "ffi")]
    Ffi,
}

impl Capability {
    /// Returns the stable capability name.
    pub fn name(self) -> &'static str {
        match self {
            Capability::Fs => "fs",
            Capability::Process => "process",
            Capability::Net => "net",
            Capability::Http => "http",
            Capability::Mysql => "mysql",
            Capability::Device => "device",
            Capability::Env => "env",
            Capability::Desktop => "desktop",
            Capability::Screen => "screen",
            #[cfg(feature = "ffi")]
            Capability::Ffi => "ffi",
        }
    }

    /// Returns the capability bit.
    fn bit(self) -> u64 {
        1_u64 << (self as u8)
    }
}

/// All known capabilities.
const ALL_CAPABILITIES: &[Capability] = &[
    Capability::Fs,
    Capability::Process,
    Capability::Net,
    Capability::Http,
    Capability::Mysql,
    Capability::Device,
    Capability::Env,
    Capability::Desktop,
    Capability::Screen,
    #[cfg(feature = "ffi")]
    Capability::Ffi,
];

/// A collection of bits of all known capabilities.
const ALL_CAPABILITY_MASK: u64 = (1_u64 << ALL_CAPABILITIES.len()) - 1;

/// Permission configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionConfig {
    /// Whether the allow list is explicitly configured.
    allow_configured: bool,
    /// Set of allowed capability bits.
    allow_mask: u64,
    /// Set of rejection capability bits.
    deny_mask: u64,
}

/// Permission configuration snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionConfigSnapshot {
    /// Whether the allow list is explicitly configured.
    pub allow_configured: bool,
    /// Capability names in the allow list.
    pub allow: Vec<&'static str>,
    /// Configures the capability names in the deny list.
    pub deny: Vec<&'static str>,
    /// The current actual allowed capability name.
    pub allowed: Vec<&'static str>,
    /// Configuration parsing error; empty if no error.
    pub config_error: Option<String>,
}

/// Permission statistics snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionStats {
    /// Current permission configuration snapshot.
    pub config: PermissionConfigSnapshot,
    /// Number of permission denials.
    pub denied: usize,
}

impl PermissionConfig {
    /// Reads permission configuration from environment variables.
    fn from_env() -> Result<Self, String> {
        let allow = env::var(PERMISSION_ALLOW_ENV).ok();
        let deny = env::var(PERMISSION_DENY_ENV).ok();
        Self::from_values(allow.as_deref(), deny.as_deref())
    }

    /// Builds permission configuration from string configuration.
    fn from_values(allow: Option<&str>, deny: Option<&str>) -> Result<Self, String> {
        let allow_mask = parse_capability_mask(PERMISSION_ALLOW_ENV, allow)?;
        let deny_mask = parse_capability_mask(PERMISSION_DENY_ENV, deny)?;
        Ok(Self {
            allow_configured: allow_mask.is_some(),
            allow_mask: allow_mask.unwrap_or(ALL_CAPABILITY_MASK),
            deny_mask: deny_mask.unwrap_or(0),
        })
    }

    /// Returns the default all-allowed configuration.
    fn allow_all() -> Self {
        Self {
            allow_configured: false,
            allow_mask: ALL_CAPABILITY_MASK,
            deny_mask: 0,
        }
    }

    /// Determine whether the ability is allowed.
    fn is_allowed(&self, capability: Capability) -> bool {
        let bit = capability.bit();
        (self.allow_mask & bit) != 0 && (self.deny_mask & bit) == 0
    }

    /// Convert to statistics snapshot.
    fn snapshot(&self, config_error: Option<String>) -> PermissionConfigSnapshot {
        PermissionConfigSnapshot {
            allow_configured: self.allow_configured,
            allow: capability_names(self.allow_mask),
            deny: capability_names(self.deny_mask),
            allowed: ALL_CAPABILITIES
                .iter()
                .copied()
                .filter(|capability| self.is_allowed(*capability))
                .map(Capability::name)
                .collect(),
            config_error,
        }
    }
}

/// Checks whether the specified capability is allowed.
pub fn check(capability: Capability) -> Result<(), String> {
    if with_permission_config(|config| config.is_allowed(capability))? {
        return Ok(());
    }
    PERMISSION_DENIED.fetch_add(1, Ordering::Relaxed);
    Err(format!(
        "Permission denied: capability `{}` is disabled; check {} and {}",
        capability.name(),
        PERMISSION_ALLOW_ENV,
        PERMISSION_DENY_ENV
    ))
}

/// Returns permission statistics.
pub fn stats() -> PermissionStats {
    let (config, config_error) = match with_permission_config(|config| config.clone()) {
        Ok(config) => (config, None),
        Err(err) => (PermissionConfig::allow_all(), Some(err)),
    };
    PermissionStats {
        config: config.snapshot(config_error),
        denied: PERMISSION_DENIED.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
/// Temporarily overrides permission configuration in the current test thread.
pub(crate) fn with_test_config<R>(
    allow: Option<&str>,
    deny: Option<&str>,
    f: impl FnOnce() -> R,
) -> R {
    let config = PermissionConfig::from_values(allow, deny);
    TEST_PERMISSION_CONFIG.with(|slot| {
        let previous = slot.replace(Some(config));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        slot.replace(previous);
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// Reads the permission configuration and executes the closure while the configuration exists.
fn with_permission_config<R>(f: impl FnOnce(&PermissionConfig) -> R) -> Result<R, String> {
    #[cfg(test)]
    if let Some(config) = TEST_PERMISSION_CONFIG.with(|slot| slot.borrow().clone()) {
        return match config {
            Ok(config) => Ok(f(&config)),
            Err(err) => Err(err),
        };
    }

    match PERMISSION_CONFIG.get_or_init(PermissionConfig::from_env) {
        Ok(config) => Ok(f(config)),
        Err(err) => Err(err.clone()),
    }
}

/// Parsing capability list.
fn parse_capability_mask(name: &str, value: Option<&str>) -> Result<Option<u64>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let mut mask = 0_u64;
    let mut saw_value = false;
    for item in value.split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace()) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        saw_value = true;
        if item.eq_ignore_ascii_case("all") {
            mask |= ALL_CAPABILITY_MASK;
            continue;
        }
        if item.eq_ignore_ascii_case("none") {
            continue;
        }
        let capability = parse_capability(item)
            .ok_or_else(|| format!("{} contains unknown permission capability `{}`", name, item))?;
        mask |= capability.bit();
    }
    if saw_value {
        Ok(Some(mask))
    } else {
        Ok(None)
    }
}

/// Parse a single capability name.
fn parse_capability(name: &str) -> Option<Capability> {
    match name.trim().to_ascii_lowercase().as_str() {
        "fs" | "file" | "files" => Some(Capability::Fs),
        "process" | "proc" => Some(Capability::Process),
        "net" | "network" => Some(Capability::Net),
        "http" | "reqwest" => Some(Capability::Http),
        "mysql" | "db" | "database" => Some(Capability::Mysql),
        "device" | "serial" => Some(Capability::Device),
        "env" | "environment" | "path" => Some(Capability::Env),
        "desktop" | "app" | "bt_app" => Some(Capability::Desktop),
        "screen" | "capture" | "screenshot" => Some(Capability::Screen),
        #[cfg(feature = "ffi")]
        "ffi" | "native" => Some(Capability::Ffi),
        _ => None,
    }
}

/// Convert the set of capability bits into a list of names.
fn capability_names(mask: u64) -> Vec<&'static str> {
    ALL_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| (mask & capability.bit()) != 0)
        .map(Capability::name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default configuration should maintain historical compatible behavior.
    #[test]
    fn default_config_allows_all_capabilities() {
        let config = PermissionConfig::from_values(None, None).unwrap();
        for capability in ALL_CAPABILITIES {
            assert!(config.is_allowed(*capability));
        }
    }

    /// The allow list should be tightened to the specified capabilities.
    #[test]
    fn allow_list_limits_capabilities() {
        let config = PermissionConfig::from_values(Some("fs,process"), None).unwrap();
        assert!(config.is_allowed(Capability::Fs));
        assert!(config.is_allowed(Capability::Process));
        assert!(!config.is_allowed(Capability::Net));
    }

    /// Deny lists take precedence over allow lists.
    #[test]
    fn deny_list_wins_over_allow_list() {
        let config = PermissionConfig::from_values(Some("all"), Some("process")).unwrap();
        assert!(config.is_allowed(Capability::Fs));
        assert!(!config.is_allowed(Capability::Process));
    }

    /// An unknown capability name should return a clear configuration error.
    #[test]
    fn unknown_capability_reports_config_error() {
        let err = PermissionConfig::from_values(Some("fs,nope"), None).unwrap_err();
        assert!(err.contains("unknown permission capability"));
    }

    /// Capability aliases should be normalized to stable names.
    #[test]
    fn capability_aliases_are_supported() {
        let config =
            PermissionConfig::from_values(Some("file,network,reqwest,database"), None).unwrap();
        assert!(config.is_allowed(Capability::Fs));
        assert!(config.is_allowed(Capability::Net));
        assert!(config.is_allowed(Capability::Http));
        assert!(config.is_allowed(Capability::Mysql));
    }

    /// The screen capture capability should support stable names and common screenshot aliases.
    #[test]
    fn screen_capability_and_alias_are_supported() {
        let screen = PermissionConfig::from_values(Some("screen"), None).unwrap();
        let screenshot = PermissionConfig::from_values(Some("screenshot"), None).unwrap();

        assert!(screen.is_allowed(Capability::Screen));
        assert!(screenshot.is_allowed(Capability::Screen));
        assert!(!screen.is_allowed(Capability::Desktop));
    }

    /// Stable capability names and native aliases should be recognized after enabling the FFI feature.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_capability_and_alias_are_supported() {
        let ffi = PermissionConfig::from_values(Some("ffi"), None).unwrap();
        let native = PermissionConfig::from_values(Some("native"), None).unwrap();

        assert!(ffi.is_allowed(Capability::Ffi));
        assert!(native.is_allowed(Capability::Ffi));
    }
}
