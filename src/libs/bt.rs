//! BT runtime standard library.
//!
//! `BT` is a global stateless object. Constant properties return compile-time information directly, while runtime information and the environment overlay
//! are initialized on demand to avoid unnecessary system calls during interpreter startup.

use crate::path as bt_path;
use crate::value::Value;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// BT runtime static object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtRuntime;

/// `BT.info()` The boot snapshot returned.
#[derive(Debug, Clone)]
struct RuntimeInfoSnapshot {
    /// BT runtime version.
    version: String,
    /// Current binary logic name.
    name: String,
    /// Operating system name.
    os: String,
    /// CPU architecture name.
    arch: String,
    /// Operating system family.
    family: String,
    /// Whether to build for debug.
    debug: bool,
    /// The current actual running file name.
    exe_name: String,
    /// Executable file extension.
    exe_ext: String,
    /// Dynamic-library extension.
    lib_ext: String,
    /// Pointer width.
    pointer_width: i64,
    /// Number of available parallel threads.
    threads: i64,
    /// The process starts a snapshot of the working directory.
    cwd: String,
    /// The full path of the current actual running file.
    exe_path: String,
}

/// BT runtime environment variable override items.
#[derive(Debug, Clone)]
enum EnvOverlayValue {
    /// Variable value after setting or overwriting.
    Set(String),
    /// Remove flag for masking OS environment variables.
    Removed,
}

static THREADS: OnceLock<i64> = OnceLock::new();
static START_TIME: OnceLock<u64> = OnceLock::new();
static START_CWD: OnceLock<String> = OnceLock::new();
static INFO: OnceLock<RuntimeInfoSnapshot> = OnceLock::new();
static RUNTIME_ID: OnceLock<String> = OnceLock::new();
static ENV_OVERLAY: OnceLock<RwLock<HashMap<String, EnvOverlayValue>>> = OnceLock::new();

impl BtRuntime {
    /// Initializes a runtime lightweight startup snapshot.
    ///
    /// This function is called when VM is constructed. It only records the second-level startup time and startup working directory, and does not scan hardware, network or disk details.
    pub fn init_start_snapshot() {
        let _ = START_TIME.get_or_init(current_timestamp_secs);
        let _ = START_CWD.get_or_init(current_dir_text);
    }

    /// Reads BT compile-time constant attributes.
    pub fn get_property(&self, name: &str) -> Option<Value> {
        let value = match name {
            "VERSION" => Value::Str(env!("CARGO_PKG_VERSION").to_string()),
            "NAME" => Value::Str(runtime_name().to_string()),
            "OS" => Value::Str(env::consts::OS.to_string()),
            "ARCH" => Value::Str(env::consts::ARCH.to_string()),
            "FAMILY" => Value::Str(env::consts::FAMILY.to_string()),
            "DEBUG" => Value::Bool(cfg!(debug_assertions)),
            "EXE_EXT" => Value::Str(exe_ext()),
            "LIB_EXT" => Value::Str(lib_ext()),
            "POINTER_WIDTH" => Value::Int(pointer_width()),
            "THREADS" => Value::Int(threads()),
            _ => return None,
        };
        Some(value)
    }

    /// Determines whether the name is a BT static object method.
    pub fn is_method(name: &str) -> bool {
        matches!(
            name,
            "info"
                | "env"
                | "set_env"
                | "remove_env"
                | "has_env"
                | "envs"
                | "path_entries"
                | "add_path"
                | "remove_path"
                | "has_path"
                | "system"
                | "runtime"
                | "stats"
                | "has"
                | "features"
                | "runtime_id"
        )
    }

    /// Dispatches a BT runtime-object method.
    pub fn call_method_with_paths(
        &self,
        method: &str,
        args: Vec<Value>,
        source_dir: &Path,
        project_root: &Path,
    ) -> Result<Value, String> {
        match method {
            "info" => Ok(info_value()),
            "env" => env_value(&args),
            "set_env" => set_env_value(&args),
            "remove_env" => remove_env_value(&args),
            "has_env" => has_env_value(&args),
            "envs" => Ok(envs_value()),
            "path_entries" => path_entries_value(),
            "add_path" => add_path_value(&args, source_dir, project_root),
            "remove_path" => remove_path_value(&args, source_dir, project_root),
            "has_path" => has_path_value(&args, source_dir, project_root),
            "system" => Ok(system_value()),
            "runtime" => Ok(runtime_value()),
            "stats" => Ok(stats_value()),
            "has" => Ok(has_feature_value(&args)),
            "features" => Ok(features_value()),
            "runtime_id" => Ok(Value::Str(runtime_id())),
            _ => Err(format!("BT has no method `{}`", method)),
        }
    }
}

/// Applies the BT runtime environment overlay to a child-process command.
pub fn apply_env_overlay(command: &mut Command) {
    let Ok(overlay) = env_overlay().read() else {
        return;
    };
    for (key, value) in overlay.iter() {
        match value {
            EnvOverlayValue::Set(value) => {
                command.env(key, value);
            }
            EnvOverlayValue::Removed => {
                command.env_remove(key);
            }
        }
    }
}

/// Reads the runtime environment variable overlay.
fn env_overlay() -> &'static RwLock<HashMap<String, EnvOverlayValue>> {
    ENV_OVERLAY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Reads and caches the available parallelism.
fn threads() -> i64 {
    *THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|value| value.get() as i64)
            .unwrap_or(1)
    })
}

/// Returns the logical name of the current binary.
fn runtime_name() -> &'static str {
    if cfg!(feature = "desktop") {
        "bt-app"
    } else {
        "bt"
    }
}

/// Returns the executable-file extension.
fn exe_ext() -> String {
    dotted_ext(env::consts::EXE_EXTENSION)
}

/// Returns the dynamic-library extension.
fn lib_ext() -> String {
    dotted_ext(env::consts::DLL_EXTENSION)
}

/// Add a dot before the non-empty platform extension.
fn dotted_ext(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!(".{}", value)
    }
}

/// Returns the current platform pointer width.
fn pointer_width() -> i64 {
    if cfg!(target_pointer_width = "64") {
        64
    } else {
        32
    }
}

/// Returns the current Unix timestamp seconds.
fn current_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

/// Returns the current Unix timestamp in nanoseconds.
fn current_timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

/// Returns the current working directory as text.
fn current_dir_text() -> String {
    env::current_dir()
        .map(|path| bt_path::path_text(&path))
        .unwrap_or_default()
}

/// Reads and caches a snapshot of runtime information.
fn info_snapshot() -> &'static RuntimeInfoSnapshot {
    INFO.get_or_init(|| {
        BtRuntime::init_start_snapshot();
        let exe_path = env::current_exe().ok();
        let exe_name = exe_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let exe_path = exe_path
            .as_ref()
            .map(|path| bt_path::path_text(path))
            .unwrap_or_default();
        RuntimeInfoSnapshot {
            version: env!("CARGO_PKG_VERSION").to_string(),
            name: runtime_name().to_string(),
            os: env::consts::OS.to_string(),
            arch: env::consts::ARCH.to_string(),
            family: env::consts::FAMILY.to_string(),
            debug: cfg!(debug_assertions),
            exe_name,
            exe_ext: exe_ext(),
            lib_ext: lib_ext(),
            pointer_width: pointer_width(),
            threads: threads(),
            cwd: START_CWD.get_or_init(current_dir_text).clone(),
            exe_path,
        }
    })
}

/// Convert runtime information snapshot into BT object.
fn info_value() -> Value {
    let info = info_snapshot();
    let mut object = IndexMap::with_capacity(13);
    object.insert("version".to_string(), Value::Str(info.version.clone()));
    object.insert("name".to_string(), Value::Str(info.name.clone()));
    object.insert("os".to_string(), Value::Str(info.os.clone()));
    object.insert("arch".to_string(), Value::Str(info.arch.clone()));
    object.insert("family".to_string(), Value::Str(info.family.clone()));
    object.insert("debug".to_string(), Value::Bool(info.debug));
    object.insert("exe_name".to_string(), Value::Str(info.exe_name.clone()));
    object.insert("exe_ext".to_string(), Value::Str(info.exe_ext.clone()));
    object.insert("lib_ext".to_string(), Value::Str(info.lib_ext.clone()));
    object.insert("pointer_width".to_string(), Value::Int(info.pointer_width));
    object.insert("threads".to_string(), Value::Int(info.threads));
    object.insert("cwd".to_string(), Value::Str(info.cwd.clone()));
    object.insert("exe_path".to_string(), Value::Str(info.exe_path.clone()));
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Reads valid environment variables.
fn env_value(args: &[Value]) -> Result<Value, String> {
    let name = required_env_name(args, "BT.env()")?;
    Ok(effective_env(&name).map(Value::Str).unwrap_or(Value::Empty))
}

/// Sets runtime environment variable overrides.
fn set_env_value(args: &[Value]) -> Result<Value, String> {
    let name = required_env_name(args, "BT.set_env()")?;
    let value = args.get(1).map(Value::to_string).unwrap_or_default();
    let old = effective_env(&name).map(Value::Str).unwrap_or(Value::Empty);
    let mut overlay = env_overlay()
        .write()
        .map_err(|_| "BT environment variable override lock is poisoned".to_string())?;
    overlay.insert(name, EnvOverlayValue::Set(value));
    Ok(old)
}

/// Removes runtime environment variable overrides and shadows the OS environment.
fn remove_env_value(args: &[Value]) -> Result<Value, String> {
    let name = required_env_name(args, "BT.remove_env()")?;
    let old = effective_env(&name).map(Value::Str).unwrap_or(Value::Empty);
    let mut overlay = env_overlay()
        .write()
        .map_err(|_| "BT environment variable override lock is poisoned".to_string())?;
    overlay.insert(name, EnvOverlayValue::Removed);
    Ok(old)
}

/// Determines whether valid environment variables exist.
fn has_env_value(args: &[Value]) -> Result<Value, String> {
    let name = required_env_name(args, "BT.has_env()")?;
    Ok(Value::Bool(effective_env(&name).is_some()))
}

/// Merges OS environment variables and BT overlays.
fn envs_value() -> Value {
    let mut values = IndexMap::new();
    for (key, value) in env::vars() {
        values.insert(key, Value::Str(value));
    }
    if let Ok(overlay) = env_overlay().read() {
        for (key, value) in overlay.iter() {
            match value {
                EnvOverlayValue::Set(value) => {
                    values.insert(key.clone(), Value::Str(value.clone()));
                }
                EnvOverlayValue::Removed => {
                    values.shift_remove(key);
                }
            }
        }
    }
    Value::Object(Rc::new(RefCell::new(values)))
}

/// Reads the text of a valid environment variable.
fn effective_env(name: &str) -> Option<String> {
    if let Ok(overlay) = env_overlay().read() {
        if let Some(value) = overlay.get(name) {
            return match value {
                EnvOverlayValue::Set(value) => Some(value.clone()),
                EnvOverlayValue::Removed => None,
            };
        }
    }
    env::var(name).ok()
}

/// Reads a required environment-variable name.
fn required_env_name(args: &[Value], method: &str) -> Result<String, String> {
    args.first()
        .map(Value::to_string)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{} requires a non-empty environment variable name", method))
}

/// Reads a valid PATH entry.
fn path_entries_value() -> Result<Value, String> {
    let items = effective_path_entries()?
        .into_iter()
        .map(|path| Value::Str(bt_path::path_text(&path)))
        .collect::<Vec<_>>();
    Ok(Value::Array(Rc::new(RefCell::new(items))))
}

/// Appends or prepends PATH entries.
fn add_path_value(args: &[Value], source_dir: &Path, project_root: &Path) -> Result<Value, String> {
    let path = required_bt_path(args, "BT.add_path()", source_dir, project_root)?;
    let path_text = bt_path::path_text(&path);
    let mode = args.get(1).map(Value::to_string).unwrap_or_default();
    if !mode.is_empty() && mode != "append" && mode != "prepend" {
        return Err("BT.add_path() mode must be `append` or `prepend`".to_string());
    }
    let mut entries = effective_path_entries()?;
    if entries
        .iter()
        .any(|entry| same_path_text(&bt_path::path_text(entry), &path_text))
    {
        return Ok(Value::Bool(false));
    }
    if mode == "prepend" {
        entries.insert(0, path);
    } else {
        entries.push(path);
    }
    write_path_overlay(entries)?;
    Ok(Value::Bool(true))
}

/// Removes a `PATH` entry.
fn remove_path_value(
    args: &[Value],
    source_dir: &Path,
    project_root: &Path,
) -> Result<Value, String> {
    let path = required_bt_path(args, "BT.remove_path()", source_dir, project_root)?;
    let path_text = bt_path::path_text(&path);
    let mut entries = effective_path_entries()?;
    let old_len = entries.len();
    entries.retain(|entry| !same_path_text(&bt_path::path_text(entry), &path_text));
    let removed = entries.len() != old_len;
    if removed {
        write_path_overlay(entries)?;
    }
    Ok(Value::Bool(removed))
}

/// Determines whether the PATH entry exists.
fn has_path_value(args: &[Value], source_dir: &Path, project_root: &Path) -> Result<Value, String> {
    let path = required_bt_path(args, "BT.has_path()", source_dir, project_root)?;
    let path_text = bt_path::path_text(&path);
    Ok(Value::Bool(effective_path_entries()?.iter().any(|entry| {
        same_path_text(&bt_path::path_text(entry), &path_text)
    })))
}

/// Reads the effective PATH and splits it according to platform rules.
fn effective_path_entries() -> Result<Vec<PathBuf>, String> {
    let Some(path) = effective_env("PATH") else {
        return Ok(Vec::new());
    };
    Ok(env::split_paths(&path).collect())
}

/// Writes back PATH to the BT environment variable overlay.
fn write_path_overlay(entries: Vec<PathBuf>) -> Result<(), String> {
    let joined = env::join_paths(entries.iter())
        .map_err(|err| format!("Failed to update BT PATH: {}", err))?
        .to_string_lossy()
        .to_string();
    let mut overlay = env_overlay()
        .write()
        .map_err(|_| "BT environment variable override lock is poisoned".to_string())?;
    overlay.insert("PATH".to_string(), EnvOverlayValue::Set(joined));
    Ok(())
}

/// Read and parse BT path parameters.
fn required_bt_path(
    args: &[Value],
    method: &str,
    source_dir: &Path,
    project_root: &Path,
) -> Result<PathBuf, String> {
    let path = args
        .first()
        .map(Value::to_string)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| format!("{} requires a non-empty path", method))?;
    Ok(bt_path::resolve_path(&path, project_root, source_dir))
}

/// Compares two PATH entry texts.
fn same_path_text(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

/// Returns lightweight system fields.
fn system_value() -> Value {
    let mut object = IndexMap::with_capacity(4);
    object.insert("os".to_string(), Value::Str(env::consts::OS.to_string()));
    object.insert(
        "arch".to_string(),
        Value::Str(env::consts::ARCH.to_string()),
    );
    object.insert(
        "family".to_string(),
        Value::Str(env::consts::FAMILY.to_string()),
    );
    object.insert("threads".to_string(), Value::Int(threads()));
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Returns runtime fields.
fn runtime_value() -> Value {
    BtRuntime::init_start_snapshot();
    let start_time = *START_TIME.get_or_init(current_timestamp_secs);
    let uptime = current_timestamp_secs().saturating_sub(start_time);
    let mut object = IndexMap::with_capacity(3);
    object.insert("uptime".to_string(), Value::Int(uptime as i64));
    object.insert("threads".to_string(), Value::Int(threads()));
    object.insert("start_time".to_string(), Value::Int(start_time as i64));
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Returns the runtime resource-statistics object.
fn stats_value() -> Value {
    BtRuntime::init_start_snapshot();
    let start_time = *START_TIME.get_or_init(current_timestamp_secs);
    let uptime = current_timestamp_secs().saturating_sub(start_time);
    let mut object = IndexMap::with_capacity(13);
    object.insert("uptime".to_string(), Value::Int(uptime as i64));
    object.insert("threads".to_string(), Value::Int(threads()));
    object.insert("start_time".to_string(), Value::Int(start_time as i64));
    object.insert("io".to_string(), io_stats_value());
    object.insert("task".to_string(), task_stats_value());
    object.insert("timer".to_string(), timer_stats_value());
    object.insert("net".to_string(), net_stats_value());
    object.insert("http".to_string(), http_stats_value());
    object.insert("mysql".to_string(), mysql_stats_value());
    object.insert("bytes".to_string(), bytes_stats_value());
    object.insert("ffi".to_string(), ffi_stats_value());
    object.insert("cache".to_string(), cache_stats_value());
    object.insert("permission".to_string(), permission_stats_value());
    object_value(object)
}

/// Returns minimal FFI statistics; when FFI is disabled, all capability counters remain zero.
fn ffi_stats_value() -> Value {
    let mut object = IndexMap::with_capacity(4);
    #[cfg(feature = "ffi")]
    {
        let stats = crate::libs::ffi::stats();
        object.insert("enabled".to_string(), Value::Bool(stats.enabled));
        object.insert(
            "open_libraries".to_string(),
            usize_value(stats.open_libraries),
        );
        object.insert("buffers".to_string(), usize_value(stats.buffers));
        object.insert("buffer_bytes".to_string(), usize_value(stats.buffer_bytes));
    }
    #[cfg(not(feature = "ffi"))]
    {
        object.insert("enabled".to_string(), Value::Bool(false));
        object.insert("open_libraries".to_string(), Value::Int(0));
        object.insert("buffers".to_string(), Value::Int(0));
        object.insert("buffer_bytes".to_string(), Value::Int(0));
    }
    object_value(object)
}

/// Returns the I/O run boundary statistics object.
fn io_stats_value() -> Value {
    let stats = crate::io::stats();
    let mut config = IndexMap::with_capacity(5);
    config.insert(
        "async_workers".to_string(),
        usize_value(stats.config.async_workers),
    );
    config.insert(
        "blocking_workers".to_string(),
        usize_value(stats.config.blocking_workers),
    );
    config.insert(
        "blocking_queue_limit".to_string(),
        usize_value(stats.config.blocking_queue_limit),
    );
    config.insert(
        "default_timeout_ms".to_string(),
        u64_value(stats.config.default_timeout_ms),
    );
    config.insert(
        "shutdown_timeout_ms".to_string(),
        u64_value(stats.config.shutdown_timeout_ms),
    );

    let mut object = IndexMap::with_capacity(14);
    object.insert(
        "async_runtime_started".to_string(),
        Value::Bool(stats.async_runtime_started),
    );
    object.insert("async_active".to_string(), usize_value(stats.async_active));
    object.insert(
        "async_completed".to_string(),
        usize_value(stats.async_completed),
    );
    object.insert("async_failed".to_string(), usize_value(stats.async_failed));
    object.insert(
        "async_timeouts".to_string(),
        usize_value(stats.async_timeouts),
    );
    object.insert(
        "async_rejected".to_string(),
        usize_value(stats.async_rejected),
    );
    object.insert(
        "blocking_pool_started".to_string(),
        Value::Bool(stats.blocking_pool_started),
    );
    object.insert(
        "blocking_queued".to_string(),
        usize_value(stats.blocking_queued),
    );
    object.insert(
        "blocking_running".to_string(),
        usize_value(stats.blocking_running),
    );
    object.insert(
        "blocking_completed".to_string(),
        usize_value(stats.blocking_completed),
    );
    object.insert(
        "blocking_rejected".to_string(),
        usize_value(stats.blocking_rejected),
    );
    object.insert(
        "blocking_timeouts".to_string(),
        usize_value(stats.blocking_timeouts),
    );
    object.insert(
        "blocking_shutdown".to_string(),
        Value::Bool(stats.blocking_shutdown),
    );
    object.insert("config".to_string(), object_value(config));
    object_value(object)
}

/// Returns background-task runtime statistics.
fn task_stats_value() -> Value {
    let stats = crate::task::stats();
    let mut object = IndexMap::with_capacity(7);
    object.insert(
        "executor_started".to_string(),
        Value::Bool(stats.executor_started),
    );
    object.insert("queue_limit".to_string(), usize_value(stats.queue_limit));
    object.insert("workers".to_string(), usize_value(stats.workers));
    object.insert("queued".to_string(), usize_value(stats.queued));
    object.insert("running".to_string(), usize_value(stats.running));
    object.insert("completed".to_string(), usize_value(stats.completed));
    object.insert("rejected".to_string(), usize_value(stats.rejected));
    object_value(object)
}

/// Returns the timer runtime statistics object.
fn timer_stats_value() -> Value {
    let stats = crate::timer::stats();
    let mut object = IndexMap::with_capacity(5);
    object.insert(
        "runtime_started".to_string(),
        Value::Bool(stats.runtime_started),
    );
    object.insert("active".to_string(), usize_value(stats.active));
    object.insert("queued".to_string(), usize_value(stats.queued));
    object.insert("limit".to_string(), usize_value(stats.limit));
    object.insert(
        "event_queue_limit".to_string(),
        usize_value(stats.event_queue_limit),
    );
    object_value(object)
}

/// Returns the network runtime statistics object.
fn net_stats_value() -> Value {
    let stats = crate::net::stats();
    let mut object = IndexMap::with_capacity(12);
    object.insert(
        "runtime_started".to_string(),
        Value::Bool(stats.runtime_started),
    );
    object.insert("web_services".to_string(), usize_value(stats.web_services));
    object.insert(
        "event_services".to_string(),
        usize_value(stats.event_services),
    );
    object.insert(
        "event_queue_bounded".to_string(),
        Value::Bool(stats.event_queue_bounded),
    );
    object.insert(
        "event_queue_limit".to_string(),
        optional_usize_value(stats.event_queue_limit),
    );
    object.insert(
        "event_queue_queued".to_string(),
        usize_value(stats.event_queue_queued),
    );
    object.insert(
        "event_queue_sent".to_string(),
        usize_value(stats.event_queue_sent),
    );
    object.insert(
        "event_queue_rejected".to_string(),
        usize_value(stats.event_queue_rejected),
    );
    object.insert(
        "connection_limit".to_string(),
        usize_value(stats.connection_limit),
    );
    object.insert(
        "message_limit".to_string(),
        usize_value(stats.message_limit),
    );
    object.insert(
        "write_queue_limit".to_string(),
        usize_value(stats.write_queue_limit),
    );
    object.insert("idle_ttl_ms".to_string(), u64_value(stats.idle_ttl_ms));
    object_value(object)
}

/// Returns the HTTP client pool statistics object.
fn http_stats_value() -> Value {
    let stats = crate::libs::reqwest::stats();
    let mut config = IndexMap::with_capacity(5);
    config.insert("enabled".to_string(), Value::Bool(stats.config.enabled));
    config.insert(
        "pool_limit".to_string(),
        usize_value(stats.config.pool_limit),
    );
    config.insert(
        "idle_ttl_ms".to_string(),
        u64_value(stats.config.idle_ttl_ms),
    );
    config.insert("slow_ms".to_string(), u64_value(stats.config.slow_ms));
    config.insert(
        "config_error".to_string(),
        optional_string_value(stats.config.config_error),
    );

    let mut object = IndexMap::with_capacity(10);
    object.insert("pool_started".to_string(), Value::Bool(stats.pool_started));
    object.insert("entries".to_string(), usize_value(stats.entries));
    object.insert("hits".to_string(), usize_value(stats.hits));
    object.insert("misses".to_string(), usize_value(stats.misses));
    object.insert("created".to_string(), usize_value(stats.created));
    object.insert("evicted".to_string(), usize_value(stats.evicted));
    object.insert("bypassed".to_string(), usize_value(stats.bypassed));
    object.insert("build_failed".to_string(), usize_value(stats.build_failed));
    object.insert("slow_calls".to_string(), usize_value(stats.slow_calls));
    object.insert("config".to_string(), object_value(config));
    object_value(object)
}

/// Returns the MySQL connection pool statistics object.
fn mysql_stats_value() -> Value {
    let stats = crate::libs::mysql::stats();
    let mut config = IndexMap::with_capacity(9);
    config.insert("enabled".to_string(), Value::Bool(stats.config.enabled));
    config.insert(
        "pool_limit".to_string(),
        usize_value(stats.config.pool_limit),
    );
    config.insert(
        "min_connections".to_string(),
        usize_value(stats.config.min_connections),
    );
    config.insert(
        "max_connections".to_string(),
        usize_value(stats.config.max_connections),
    );
    config.insert(
        "idle_ttl_ms".to_string(),
        u64_value(stats.config.idle_ttl_ms),
    );
    config.insert(
        "connect_timeout_ms".to_string(),
        u64_value(stats.config.connect_timeout_ms),
    );
    config.insert(
        "query_timeout_ms".to_string(),
        u64_value(stats.config.query_timeout_ms),
    );
    config.insert("slow_ms".to_string(), u64_value(stats.config.slow_ms));
    config.insert(
        "config_error".to_string(),
        optional_string_value(stats.config.config_error),
    );

    let mut object = IndexMap::with_capacity(19);
    object.insert("pool_started".to_string(), Value::Bool(stats.pool_started));
    object.insert("entries".to_string(), usize_value(stats.entries));
    object.insert("connections".to_string(), usize_value(stats.connections));
    object.insert(
        "idle_connections".to_string(),
        usize_value(stats.idle_connections),
    );
    object.insert("hits".to_string(), usize_value(stats.hits));
    object.insert("misses".to_string(), usize_value(stats.misses));
    object.insert("created".to_string(), usize_value(stats.created));
    object.insert("evicted".to_string(), usize_value(stats.evicted));
    object.insert("bypassed".to_string(), usize_value(stats.bypassed));
    object.insert("build_failed".to_string(), usize_value(stats.build_failed));
    object.insert("slow_calls".to_string(), usize_value(stats.slow_calls));
    object.insert(
        "transactions_active".to_string(),
        usize_value(stats.transactions_active),
    );
    object.insert(
        "transactions_started".to_string(),
        usize_value(stats.transactions_started),
    );
    object.insert(
        "transactions_committed".to_string(),
        usize_value(stats.transactions_committed),
    );
    object.insert(
        "transactions_rolled_back".to_string(),
        usize_value(stats.transactions_rolled_back),
    );
    object.insert(
        "transactions_closed".to_string(),
        usize_value(stats.transactions_closed),
    );
    object.insert(
        "transactions_failed".to_string(),
        usize_value(stats.transactions_failed),
    );
    object.insert("config".to_string(), object_value(config));
    object_value(object)
}

/// Returns Bytes resource configuration and statistics.
fn bytes_stats_value() -> Value {
    let stats = crate::libs::bytes::stats();
    let mut object = IndexMap::with_capacity(2);
    object.insert("limit".to_string(), usize_value(stats.limit));
    object.insert(
        "config_error".to_string(),
        optional_string_value(stats.config_error),
    );
    object_value(object)
}

/// Returns the VM cache statistics object.
fn cache_stats_value() -> Value {
    let stats = crate::vm::cache_stats();
    let mut object = IndexMap::with_capacity(18);
    object.insert(
        "compiled_file_entries".to_string(),
        usize_value(stats.compiled_file_entries),
    );
    object.insert(
        "compiled_file_limit".to_string(),
        usize_value(stats.compiled_file_limit),
    );
    object.insert(
        "compiled_file_bytes".to_string(),
        usize_value(stats.compiled_file_bytes),
    );
    object.insert(
        "compiled_file_bytes_limit".to_string(),
        usize_value(stats.compiled_file_bytes_limit),
    );
    object.insert(
        "compiled_file_hits".to_string(),
        usize_value(stats.compiled_file_hits),
    );
    object.insert(
        "compiled_file_misses".to_string(),
        usize_value(stats.compiled_file_misses),
    );
    object.insert(
        "compiled_file_invalidations".to_string(),
        usize_value(stats.compiled_file_invalidations),
    );
    object.insert(
        "compiled_file_evictions".to_string(),
        usize_value(stats.compiled_file_evictions),
    );
    object.insert(
        "compiled_file_fingerprint_checks".to_string(),
        usize_value(stats.compiled_file_fingerprint_checks),
    );
    object.insert(
        "template_fragment_entries".to_string(),
        usize_value(stats.template_fragment_entries),
    );
    object.insert(
        "template_fragment_limit".to_string(),
        usize_value(stats.template_fragment_limit),
    );
    object.insert(
        "template_fragment_bytes".to_string(),
        usize_value(stats.template_fragment_bytes),
    );
    object.insert(
        "template_fragment_bytes_limit".to_string(),
        usize_value(stats.template_fragment_bytes_limit),
    );
    object.insert(
        "template_fragment_hits".to_string(),
        usize_value(stats.template_fragment_hits),
    );
    object.insert(
        "template_fragment_misses".to_string(),
        usize_value(stats.template_fragment_misses),
    );
    object.insert(
        "template_fragment_evictions".to_string(),
        usize_value(stats.template_fragment_evictions),
    );
    object.insert(
        "template_fragment_bypassed".to_string(),
        usize_value(stats.template_fragment_bypassed),
    );
    object.insert(
        "template_fragment_max_code_bytes".to_string(),
        usize_value(stats.template_fragment_max_code_bytes),
    );
    object_value(object)
}

/// Returns permission configuration and denial statistics.
fn permission_stats_value() -> Value {
    let stats = crate::permission::stats();
    let mut config = IndexMap::with_capacity(5);
    config.insert(
        "allow_configured".to_string(),
        Value::Bool(stats.config.allow_configured),
    );
    config.insert("allow".to_string(), string_array_value(stats.config.allow));
    config.insert("deny".to_string(), string_array_value(stats.config.deny));
    config.insert(
        "allowed".to_string(),
        string_array_value(stats.config.allowed),
    );
    config.insert(
        "config_error".to_string(),
        optional_string_value(stats.config.config_error),
    );

    let mut object = IndexMap::with_capacity(2);
    object.insert("denied".to_string(), usize_value(stats.denied));
    object.insert("config".to_string(), object_value(config));
    object_value(object)
}

/// Determines whether the specified capability exists.
fn has_feature_value(args: &[Value]) -> Value {
    let name = args
        .first()
        .map(Value::to_string)
        .unwrap_or_default()
        .to_ascii_lowercase();
    Value::Bool(feature_enabled(&name))
}

/// Returns the static capability table.
fn features_value() -> Value {
    let mut object = IndexMap::with_capacity(FEATURES.len());
    for (name, enabled) in FEATURES {
        object.insert((*name).to_string(), Value::Bool(*enabled));
    }
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Determines whether the static capability is enabled.
fn feature_enabled(name: &str) -> bool {
    FEATURES
        .iter()
        .find_map(|(feature, enabled)| (*feature == name).then_some(*enabled))
        .unwrap_or(false)
}

/// Static capability table.
const FEATURES: &[(&str, bool)] = &[
    ("web", true),
    ("desktop", cfg!(feature = "desktop")),
    ("ffi", cfg!(feature = "ffi")),
    ("net", true),
    ("device", true),
    ("mysql", true),
    ("reqwest", true),
    ("crypto", true),
    ("fs", true),
    ("process", true),
    ("stats", true),
    ("bytes", true),
    ("modbus", true),
];

/// Returns the ID of this runtime instance.
fn runtime_id() -> String {
    RUNTIME_ID
        .get_or_init(|| {
            BtRuntime::init_start_snapshot();
            let info = info_snapshot();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            info.os.hash(&mut hasher);
            info.arch.hash(&mut hasher);
            info.name.hash(&mut hasher);
            info.exe_path.hash(&mut hasher);
            START_TIME
                .get_or_init(current_timestamp_secs)
                .hash(&mut hasher);
            current_timestamp_nanos().hash(&mut hasher);
            format!("bt-{}-{:016x}", runtime_name(), hasher.finish())
        })
        .clone()
}

/// Convert object fields to BT object values.
fn object_value(object: IndexMap<String, Value>) -> Value {
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Convert usize to a BT integer value.
fn usize_value(value: usize) -> Value {
    Value::Int(value.min(i64::MAX as usize) as i64)
}

/// Convert u64 to BT integer value.
fn u64_value(value: u64) -> Value {
    Value::Int(value.min(i64::MAX as u64) as i64)
}

/// Convert optional usize to a BT value.
fn optional_usize_value(value: Option<usize>) -> Value {
    value.map(usize_value).unwrap_or(Value::Empty)
}

/// Convert optional string to BT value.
fn optional_string_value(value: Option<String>) -> Value {
    value.map(Value::Str).unwrap_or(Value::Empty)
}

/// Converts a list of strings to a BT array.
fn string_array_value(values: Vec<&'static str>) -> Value {
    Value::Array(Rc::new(RefCell::new(
        values
            .into_iter()
            .map(|value| Value::Str(value.to_string()))
            .collect(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Constant attributes should return compile-time information.
    #[test]
    fn constants_return_compile_time_values() {
        let bt = BtRuntime;
        assert_eq!(
            bt.get_property("VERSION"),
            Some(Value::Str(env!("CARGO_PKG_VERSION").to_string()))
        );
        assert_eq!(bt.get_property("THREADS"), Some(Value::Int(threads())));
        assert_eq!(bt.get_property("NOPE"), None);
    }

    /// Environment variable overlays should distinguish between settings, removals, and OS fallbacks.
    #[test]
    fn env_overlay_supports_set_and_remove() {
        let bt = BtRuntime;
        let name = "BT_TEST_RUNTIME_ENV_OVERLAY";
        assert_eq!(
            bt.call_method_with_paths(
                "set_env",
                vec![Value::Str(name.to_string()), Value::Str("one".to_string())],
                Path::new("."),
                Path::new("."),
            ),
            Ok(Value::Empty)
        );
        assert_eq!(
            bt.call_method_with_paths(
                "env",
                vec![Value::Str(name.to_string())],
                Path::new("."),
                Path::new("."),
            ),
            Ok(Value::Str("one".to_string()))
        );
        assert_eq!(
            bt.call_method_with_paths(
                "remove_env",
                vec![Value::Str(name.to_string())],
                Path::new("."),
                Path::new("."),
            ),
            Ok(Value::Str("one".to_string()))
        );
        assert_eq!(
            bt.call_method_with_paths(
                "has_env",
                vec![Value::Str(name.to_string())],
                Path::new("."),
                Path::new("."),
            ),
            Ok(Value::Bool(false))
        );
    }

    /// `stats` returns resource sections whose field names remain in snake_case.
    #[test]
    fn stats_returns_resource_sections() {
        let bt = BtRuntime;
        let value = bt
            .call_method_with_paths("stats", Vec::new(), Path::new("."), Path::new("."))
            .unwrap();
        let Value::Object(values) = value else {
            panic!("expects a stats return object");
        };
        let values = values.borrow();
        for key in [
            "io",
            "task",
            "timer",
            "net",
            "http",
            "mysql",
            "ffi",
            "cache",
            "permission",
        ] {
            assert!(values.contains_key(key), "is missing stats.{}", key);
        }
        let Value::Object(io) = values.get("io").unwrap() else {
            panic!("expects a stats.io return object");
        };
        let io = io.borrow();
        assert!(io.contains_key("blocking_queued"));
        let Value::Object(config) = io.get("config").unwrap() else {
            panic!("Expect stats.io.config to return object");
        };
        assert!(config.borrow().contains_key("blocking_queue_limit"));
    }
}
