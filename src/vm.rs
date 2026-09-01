//! BT register-based bytecode virtual machine.
//!
//! VM takes `Chunk` as input and executes instructions sequentially. At the current stage, only basic variable environment and arithmetic output are implemented.
//! Subsequent arrays, objects, function calls, closures, and coroutines should continue to be extended on this execution core.

use crate::bytecode::{
    Chunk, Instruction, Register, SourceSpan, SymbolId, BYTECODE_FORMAT_VERSION,
};
use crate::compiler::Compiler;
#[cfg(feature = "extensions")]
use crate::extensions::manager::{ExtObject, ExtensionManager};
use crate::lexer::tokenize;
use crate::lexer::TokenKind;
#[cfg(feature = "ffi")]
use crate::libs::ffi::BtFfiValue;
use crate::libs::{
    base64::BtBase64, bt::BtRuntime, bytes::BtBytes, crypto::BtCrypto, date::BtDate,
    device::BtDevice, fs::BtFs, html::BtHtml, math::BtMath, md5::BtMd5, modbus::BtModbus,
    mysql::BtMysql, net::BtNet, path::BtPath, process::BtProcess, reqwest::BtReqwest, system,
    url::BtUrl, web::BtWebResponse,
};
use crate::net::traits::{BtNetConnection, BtNetServer};
use crate::net::{self as net_runtime, NetEvent};
use crate::parser::{Expr, Parser, PosExpr, Statement};
use crate::path as bt_path;
use crate::permission::{self, Capability};
use crate::source::{analyze_source, SourceMode};
use crate::task::{
    self, BtTask, TaskCaptureScopeSnapshot, TaskCaptureSnapshot, TaskChunkSnapshot,
    TaskCompletionSubscription, TaskFunctionSnapshot, TaskRunOutcome, TaskValueSnapshot,
};
use crate::timer::{self, BtTimer, TimerEvent, TimerKind};
use crate::value::{ClassMember, InstanceObject, IterState, RangeState, Value};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
#[cfg(feature = "extensions")]
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Bytecode execution error.
#[derive(Debug, Clone)]
pub struct VmError {
    /// Error message.
    pub message: String,
    /// Instruction pointer at the time of error.
    pub ip: usize,
    /// The source code location where the error is located.
    pub span: Option<SourceSpan>,
    /// The function name where the error occurred.
    pub function: Option<String>,
    /// The value that the script actively throws; normal runtime errors are empty.
    throw_value: Option<Value>,
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(span) = &self.span {
            if let Some(function) = &self.function {
                write!(
                    f,
                    "{}:{}:{}: VM error (function {}, instruction @{}): {}{}",
                    span.file,
                    span.line,
                    span.column,
                    function,
                    self.ip,
                    self.message,
                    Self::format_source_hint(span)
                )
            } else {
                write!(
                    f,
                    "{}:{}:{}: VM error (command @{}): {}{}",
                    span.file,
                    span.line,
                    span.column,
                    self.ip,
                    self.message,
                    Self::format_source_hint(span)
                )
            }
        } else if let Some(function) = &self.function {
            write!(
                f,
                "VM error (function {}, instruction @{}): {}",
                function, self.ip, self.message
            )
        } else {
            write!(f, "VM error @{}: {}", self.ip, self.message)
        }
    }
}

impl std::error::Error for VmError {}

impl VmError {
    /// Generates source code location hints close to Rust style.
    ///
    /// Runtime errors already carry a file, line, and column. This helper also reads the matching
    /// source line and places `^` under the offending column, so users do not have to count spaces.
    fn format_source_hint(span: &SourceSpan) -> String {
        let Ok(source) = std::fs::read_to_string(&span.file) else {
            return String::new();
        };
        let Some(line_text) = source.lines().nth(span.line.saturating_sub(1)) else {
            return String::new();
        };
        let gutter_width = span.line.to_string().len();
        let caret_padding = " ".repeat(gutter_width + 3 + span.column.saturating_sub(1));
        format!("\n{} | {}\n{}^", span.line, line_text, caret_padding)
    }
}

/// Describes where a register value came from.
///
/// A value alone is not enough to diagnose a runtime error. Keeping the variable name and read
/// location lets errors in expressions such as `a + b + c` point to the missing `c`.
#[derive(Debug, Clone)]
struct ValueOrigin {
    /// Value comes from.
    span: SourceSpan,
    /// The variable name that the value comes from, empty if it is a non-variable expression.
    variable: Option<String>,
    /// If no definition was found in any scope when reading the variable.
    missing: bool,
}

/// TCP callback saved on the VM side.
#[derive(Debug, Clone, Default)]
struct VmTcpCallbacks {
    /// Whether to pass in message callback data as Bytes.
    binary: bool,
    /// Client connection success callback.
    on_connect: Option<Value>,
    /// Client sends data callback.
    on_message: Option<Value>,
    /// Client close callback.
    on_close: Option<Value>,
    /// TCP service or connection error callback.
    on_error: Option<Value>,
}

/// UDP callback saved on the VM side.
#[derive(Debug, Clone, Default)]
struct VmUdpCallbacks {
    /// Whether to pass in message callback data as Bytes.
    binary: bool,
    /// UDP socket receives message callback.
    on_message: Option<Value>,
    /// UDP socket error callback.
    on_error: Option<Value>,
}

/// WebSocket callback saved on the VM side.
#[derive(Debug, Clone, Default)]
struct VmWsCallbacks {
    /// Whether to pass in message callback data as Bytes.
    binary: bool,
    /// WebSocket client connection successful callback.
    on_connect: Option<Value>,
    /// WebSocket client sends message callback.
    on_message: Option<Value>,
    /// WebSocket client close callback.
    on_close: Option<Value>,
    /// WebSocket service or connection error callback.
    on_error: Option<Value>,
}

/// VM lazy initialized timer event channel.
struct VmTimerInbox {
    /// Bounded sender of scheduled thread delivery events.
    sender: SyncSender<TimerEvent>,
    /// The channel through which the current VM thread receives events.
    receiver: Receiver<TimerEvent>,
}

/// The timer callback saved on the VM side.
struct VmTimerCallback {
    /// Timer type.
    kind: TimerKind,
    /// Bound script callback.
    callback: Value,
    /// Timer handle visible to the script.
    timer: BtTimer,
    /// Fixed delay in milliseconds.
    delay_ms: u64,
    /// VM local expected next expiration time.
    next_due: Option<Instant>,
    /// Whether the current interval is executing a callback.
    running: bool,
}

/// VM lazy initialized task completion event channel.
struct VmTaskInbox {
    /// Bounded sender that delivers a lightweight event number when a background task completes.
    sender: SyncSender<usize>,
    /// The channel through which the current VM thread receives task completion events.
    receiver: Receiver<usize>,
}

/// Task completion callback saved on the VM side.
struct VmTaskCallback {
    /// The monitored task object.
    task: crate::task::BtTask,
    /// The script callback function of owner has been bound.
    callback: Value,
    /// Completion subscription hung on the task object; empty when registered after the task has been completed.
    subscription: Option<TaskCompletionSubscription>,
}

/// Callback collection parsed by a single net.listen call.
enum VmNetListenCallbacks {
    /// TCP service callback collection.
    Tcp(VmTcpCallbacks),
    /// UDP socket callback collection.
    Udp(VmUdpCallbacks),
    /// WebSocket service callback collection.
    Ws(VmWsCallbacks),
}

type LocalCell = Rc<RefCell<Option<Value>>>;
type LocalScope = Vec<Option<LocalCell>>;

/// Source frame currently executing in the VM.
///
/// Stores the active source file and directory. The project root belongs to the VM itself.
#[derive(Debug, Clone)]
struct SourceFrame {
    /// Source file currently executing.
    file: PathBuf,
    /// Directory containing the active source file.
    dir: PathBuf,
}

/// The configured version of the current file compilation cache.
///
/// Must be incremented when the semantics of the include, template access, or compilation options change to allow old caches in resident processes to naturally expire.
const COMPILE_CACHE_CONFIG_VERSION: u32 = 1;
/// The time window in which recently modified files trigger content fingerprint review.
///
/// The mtime accuracy of Windows and some remote file systems may be insufficient; only low-frequency hash verification is performed on files that have just been modified.
/// Stable production files still take the mtime + length fast path.
const COMPILED_FILE_RECENT_VERIFY_WINDOW: Duration = Duration::from_secs(2);
/// The maximum number of compiled files retained within a single thread.
///
/// The cache is thread-local, so its total size grows with the number of server workers. Keep the
/// limit conservative and evict the oldest entries one at a time to avoid a burst of recompilation.
const COMPILED_FILE_CACHE_LIMIT: usize = 32;
/// Estimated byte limit for compiled file cache within a single thread.
///
/// This works with the entry limit to keep a few large templates or include files from occupying
/// resident memory indefinitely.
const COMPILED_FILE_CACHE_BYTES_LIMIT: usize = 16 * 1024 * 1024;
/// The maximum number of template fragment bytecodes retained in a single thread.
///
/// `${...}` expressions and `${...}$` script blocks execute on the request hot path. Caching their
/// compiled fragments avoids allocating tokens, AST nodes, and chunks for every request, while the
/// entry limit keeps dynamic templates from growing the cache without bound.
const TEMPLATE_FRAGMENT_CACHE_LIMIT: usize = 512;
/// Estimated byte limit for the template-fragment cache within a single thread.
///
/// Dynamic fragments may be large, so estimated bytes provide a second bound alongside entry count.
const TEMPLATE_FRAGMENT_CACHE_BYTES_LIMIT: usize = 8 * 1024 * 1024;
/// Template fragments exceeding this number of bytes are not cached.
const TEMPLATE_FRAGMENT_CACHE_MAX_CODE_BYTES: usize = 16 * 1024;
/// The maximum number of task completion callbacks allowed to be registered for a single VM.
const TASK_CALLBACK_LIMIT: usize = 1024;
/// Single VM task completion event queue length.
const TASK_EVENT_QUEUE_LIMIT: usize = 1024;
/// Maximum number of disposed WASM extension object handles tracked by one VM.
#[cfg(feature = "extensions")]
const DISPOSED_EXTENSION_OBJECT_LIMIT: usize = 4096;
/// The longest waiting interval for back-up scanning when task completion events are lost.
const TASK_CALLBACK_SCAN_WAIT: Duration = Duration::from_millis(200);

thread_local! {
    /// Thread-local BT file bytecode cache.
    ///
    /// `Chunk` contains `Rc<RefCell<...>>` values and therefore cannot be shared across threads.
    /// Giving each Tokio worker its own cache avoids lock contention and repeated allocation while
    /// reading, parsing, and compiling the same files.
    static COMPILED_FILE_CACHE: RefCell<IndexMap<CompiledFileCacheKey, CachedChunk>> = RefCell::new(IndexMap::new());
    /// Thread-local template-fragment bytecode cache.
    ///
    /// Even when `include()` caches a web template shell, its `${...}` fragments still execute at
    /// runtime. Caching their compiled chunks avoids repeated parser/compiler work and allocator
    /// churn on hot pages.
    static TEMPLATE_FRAGMENT_CACHE: RefCell<IndexMap<TemplateFragmentCacheKey, CachedTemplateFragment>> = RefCell::new(IndexMap::new());
    /// Current thread VM cache hit, invalidation, and eviction count.
    static VM_CACHE_METRICS: RefCell<VmCacheMetrics> = RefCell::new(VmCacheMetrics::default());
}

/// Cache key for compiled files.
///
/// Regular script entries and template includes apply different admission rules to the same file.
/// Including `allow_template` in the key prevents an include-cached template from bypassing web-entry validation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CompiledFileCacheKey {
    /// The real file path after normalization.
    path: PathBuf,
    /// Whether the current caller allows the `# TPL` template file to be compiled into an executable template output.
    allow_template: bool,
}

/// Cache entry for compiled files.
#[derive(Clone)]
struct CachedChunk {
    /// The last modification time of the file, `None` when the read fails.
    modified: Option<SystemTime>,
    /// File byte length, used to match the modification time to determine whether the cache is still valid.
    len: u64,
    /// Source file type, used to distinguish ordinary scripts and `# TPL` template files.
    source_mode: SourceMode,
    /// Source-content fingerprint recorded during compilation.
    source_fingerprint: u64,
    /// Compiler configuration version used for this cache entry.
    compile_config_version: u32,
    /// The bytecode format version corresponding to the cache entry.
    bytecode_format_version: u32,
    /// The estimated number of cache bytes occupied by the current entry.
    estimated_bytes: usize,
    /// Compiled bytecode chunk.
    chunk: Rc<Chunk>,
}

/// Compiled template fragment cache entry.
#[derive(Clone)]
struct CachedTemplateFragment {
    /// The estimated number of cache bytes occupied by the current entry.
    estimated_bytes: usize,
    /// Compiled template fragment bytecode block.
    chunk: Rc<Chunk>,
}

/// Template fragment cache key.
///
/// The same source code in the same template file will generate stable keys; `is_script` distinguishes `${expr}` from `${...}The same source code in the same template file will generate stable keys; `is_script` distinguishes `${expr}` from  to avoid
/// The two tags contaminate each other when they are subsequently expanded with different compilation rules.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TemplateFragmentCacheKey {
    /// The file to which the template fragment belongs.
    file: String,
    /// Template fragment starting line.
    line: usize,
    /// Template fragment starting column.
    column: usize,
    /// Whether this is a script tag.
    is_script: bool,
    /// Template fragment source code.
    code: String,
}

/// Snapshot of thread-local VM cache statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmCacheStats {
    /// The number of compiled file cache entries for the current thread.
    pub compiled_file_entries: usize,
    /// The current thread's compiled file cache limit.
    pub compiled_file_limit: usize,
    /// Estimated bytes held by the current thread's compiled-file cache.
    pub compiled_file_bytes: usize,
    /// Byte limit for the current thread's compiled-file cache.
    pub compiled_file_bytes_limit: usize,
    /// The number of compiled file cache hits for the current thread.
    pub compiled_file_hits: usize,
    /// The number of compiled file cache misses for the current thread.
    pub compiled_file_misses: usize,
    /// Compiled-file cache invalidations on the current thread.
    pub compiled_file_invalidations: usize,
    /// Compiled-file cache evictions on the current thread.
    pub compiled_file_evictions: usize,
    /// Content-fingerprint checks on the current thread's compiled-file cache.
    pub compiled_file_fingerprint_checks: usize,
    /// Template-fragment cache entries on the current thread.
    pub template_fragment_entries: usize,
    /// Entry limit for the current thread's template-fragment cache.
    pub template_fragment_limit: usize,
    /// Estimated bytes held by the current thread's template-fragment cache.
    pub template_fragment_bytes: usize,
    /// Byte limit for the current thread's template-fragment cache.
    pub template_fragment_bytes_limit: usize,
    /// Template-fragment cache hits on the current thread.
    pub template_fragment_hits: usize,
    /// Template-fragment cache misses on the current thread.
    pub template_fragment_misses: usize,
    /// Template-fragment cache evictions on the current thread.
    pub template_fragment_evictions: usize,
    /// Fragments bypassed because they were too large or the cache lacked capacity.
    pub template_fragment_bypassed: usize,
    /// The maximum number of source code bytes allowed to be cached for a single template fragment.
    pub template_fragment_max_code_bytes: usize,
}

/// Current thread VM cache counter.
#[derive(Clone, Default)]
struct VmCacheMetrics {
    /// Number of compiled file cache hits.
    compiled_file_hits: usize,
    /// Number of compiled file cache misses.
    compiled_file_misses: usize,
    /// Number of compiled-file cache invalidations.
    compiled_file_invalidations: usize,
    /// Number of compiled-file cache evictions.
    compiled_file_evictions: usize,
    /// Number of compiled-file content-fingerprint checks.
    compiled_file_fingerprint_checks: usize,
    /// Number of template fragment cache hits.
    template_fragment_hits: usize,
    /// Number of template fragment cache misses.
    template_fragment_misses: usize,
    /// Number of template-fragment cache evictions.
    template_fragment_evictions: usize,
    /// Number of template fragments bypassed because of size or capacity limits.
    template_fragment_bypassed: usize,
}

/// Compiles a file and caches its value-returning bytecode using file metadata.
///
/// Web entries and runtime `include()` both use the final statement as the return value. Sharing this
/// path avoids rebuilding the AST and chunk per request. Template files remain restricted to callers
/// that set `allow_template`.
pub fn compile_cached_file(path: &Path, allow_template: bool) -> Result<Rc<Chunk>, String> {
    let cache_path = compiled_file_cache_path(path);
    let cache_key = CompiledFileCacheKey {
        path: cache_path.clone(),
        allow_template,
    };
    let metadata = fs::metadata(&cache_path).map_err(|err| {
        format!(
            "failed to read `{}` meta information: {}",
            path.display(),
            err
        )
    })?;
    let modified = metadata.modified().ok();
    let len = metadata.len();

    if let Some(chunk) = cached_compiled_chunk(&cache_key, &cache_path, modified, len) {
        return Ok(chunk);
    }

    let (chunk, source_mode, source_fingerprint) =
        compile_file_to_chunk(&cache_key.path, path, allow_template)?;
    let chunk = Rc::new(chunk);
    let estimated_bytes = compiled_file_entry_bytes(&cache_key, &chunk);
    store_compiled_chunk(
        cache_key,
        modified,
        len,
        source_mode,
        source_fingerprint,
        estimated_bytes,
        chunk.clone(),
    );
    Ok(chunk)
}

/// Compiles a single BT file without writing to any resident cache.
fn compile_file_to_chunk(
    cache_key: &Path,
    path: &Path,
    allow_template: bool,
) -> Result<(Chunk, SourceMode, u64), String> {
    let source = fs::read_to_string(&cache_key)
        .map_err(|err| format!("failed to read `{}`: {}", path.display(), err))?;
    let source_fingerprint = source_fingerprint(&source);
    let display_path = bt_path::path_text(&bt_path::normalize_path(path));
    let document = analyze_source(&display_path, &source)?;
    let source_mode = document.mode.clone();
    let statements = match document.mode {
        SourceMode::Script => {
            let tokens = tokenize(&document.body).collect::<Vec<_>>();
            let mut parser = Parser::new(display_path.clone(), &document.body, tokens);
            parser.parse().map_err(|err| err.to_string())?
        }
        SourceMode::Template if allow_template => vec![Statement::Print(PosExpr {
            expr: Expr::Strs(document.body),
            file: display_path.clone(),
            line: document.body_line,
            column: 1,
        })],
        SourceMode::Template => {
            return Err(format!(
                "{}:1:1: The web entry file must be a regular BT script",
                display_path
            ))
        }
    };
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let chunk = Compiler::with_source_file(display_path, base_dir)
        .compile_returning_value(&statements)
        .map_err(|err| err.to_string())?;
    Ok((chunk, source_mode, source_fingerprint))
}

/// Generates the file path used by the compilation cache.
fn compiled_file_cache_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reads still valid bytecode from the thread local cache.
fn cached_compiled_chunk(
    cache_key: &CompiledFileCacheKey,
    cache_path: &Path,
    modified: Option<SystemTime>,
    len: u64,
) -> Option<Rc<Chunk>> {
    COMPILED_FILE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let Some(cached) = cache.get(cache_key) else {
            update_cache_metrics(|metrics| metrics.compiled_file_misses += 1);
            return None;
        };
        if !compiled_file_metadata_matches(cached, modified, len) {
            cache.shift_remove(cache_key);
            update_cache_metrics(|metrics| {
                metrics.compiled_file_misses += 1;
                metrics.compiled_file_invalidations += 1;
            });
            return None;
        }
        if compiled_file_needs_fingerprint_check(modified) {
            update_cache_metrics(|metrics| metrics.compiled_file_fingerprint_checks += 1);
            if !compiled_file_source_still_valid(cache_path, cached) {
                cache.shift_remove(cache_key);
                update_cache_metrics(|metrics| {
                    metrics.compiled_file_misses += 1;
                    metrics.compiled_file_invalidations += 1;
                });
                return None;
            }
        }
        let cached = cache
            .shift_remove(cache_key)
            .expect("Compile cache entries confirmed to exist should be removable");
        let chunk = cached.chunk.clone();
        cache.insert(cache_key.clone(), cached);
        update_cache_metrics(|metrics| metrics.compiled_file_hits += 1);
        Some(chunk)
    })
}

/// Writes to the thread-local bytecode cache.
fn store_compiled_chunk(
    cache_key: CompiledFileCacheKey,
    modified: Option<SystemTime>,
    len: u64,
    source_mode: SourceMode,
    source_fingerprint: u64,
    estimated_bytes: usize,
    chunk: Rc<Chunk>,
) {
    if estimated_bytes > COMPILED_FILE_CACHE_BYTES_LIMIT {
        update_cache_metrics(|metrics| metrics.compiled_file_evictions += 1);
        return;
    }
    COMPILED_FILE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.shift_remove(&cache_key);
        evict_compiled_file_cache_until_fit(&mut cache, estimated_bytes);
        cache.insert(
            cache_key,
            CachedChunk {
                modified,
                len,
                source_mode,
                source_fingerprint,
                compile_config_version: COMPILE_CACHE_CONFIG_VERSION,
                bytecode_format_version: BYTECODE_FORMAT_VERSION,
                estimated_bytes,
                chunk,
            },
        );
    });
}

/// Read template fragment bytecode from thread local cache.
fn cached_template_fragment(key: &TemplateFragmentCacheKey) -> Option<Rc<Chunk>> {
    TEMPLATE_FRAGMENT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let Some(cached) = cache.shift_remove(key) else {
            update_cache_metrics(|metrics| metrics.template_fragment_misses += 1);
            return None;
        };
        let chunk = cached.chunk.clone();
        cache.insert(key.clone(), cached);
        update_cache_metrics(|metrics| metrics.template_fragment_hits += 1);
        Some(chunk)
    })
}

/// Writes to the thread-local template fragment cache.
fn store_template_fragment(key: TemplateFragmentCacheKey, chunk: Rc<Chunk>) {
    if key.code.len() > TEMPLATE_FRAGMENT_CACHE_MAX_CODE_BYTES {
        update_cache_metrics(|metrics| metrics.template_fragment_bypassed += 1);
        return;
    }
    let estimated_bytes = template_fragment_entry_bytes(&key, &chunk);
    if estimated_bytes > TEMPLATE_FRAGMENT_CACHE_BYTES_LIMIT {
        update_cache_metrics(|metrics| metrics.template_fragment_bypassed += 1);
        return;
    }
    TEMPLATE_FRAGMENT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.shift_remove(&key);
        evict_template_fragment_cache_until_fit(&mut cache, estimated_bytes);
        cache.insert(
            key,
            CachedTemplateFragment {
                estimated_bytes,
                chunk,
            },
        );
    });
}

/// Returns a snapshot of VM cache statistics for the current thread.
pub fn cache_stats() -> VmCacheStats {
    let metrics = VM_CACHE_METRICS.with(|metrics| metrics.borrow().clone());
    VmCacheStats {
        compiled_file_entries: COMPILED_FILE_CACHE.with(|cache| cache.borrow().len()),
        compiled_file_limit: COMPILED_FILE_CACHE_LIMIT,
        compiled_file_bytes: COMPILED_FILE_CACHE
            .with(|cache| compiled_file_cache_bytes(&cache.borrow())),
        compiled_file_bytes_limit: COMPILED_FILE_CACHE_BYTES_LIMIT,
        compiled_file_hits: metrics.compiled_file_hits,
        compiled_file_misses: metrics.compiled_file_misses,
        compiled_file_invalidations: metrics.compiled_file_invalidations,
        compiled_file_evictions: metrics.compiled_file_evictions,
        compiled_file_fingerprint_checks: metrics.compiled_file_fingerprint_checks,
        template_fragment_entries: TEMPLATE_FRAGMENT_CACHE.with(|cache| cache.borrow().len()),
        template_fragment_limit: TEMPLATE_FRAGMENT_CACHE_LIMIT,
        template_fragment_bytes: TEMPLATE_FRAGMENT_CACHE
            .with(|cache| template_fragment_cache_bytes(&cache.borrow())),
        template_fragment_bytes_limit: TEMPLATE_FRAGMENT_CACHE_BYTES_LIMIT,
        template_fragment_hits: metrics.template_fragment_hits,
        template_fragment_misses: metrics.template_fragment_misses,
        template_fragment_evictions: metrics.template_fragment_evictions,
        template_fragment_bypassed: metrics.template_fragment_bypassed,
        template_fragment_max_code_bytes: TEMPLATE_FRAGMENT_CACHE_MAX_CODE_BYTES,
    }
}

/// Generates source code content fingerprints.
fn source_fingerprint(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

/// Determines whether the cache entry's metainformation still matches the current file.
fn compiled_file_metadata_matches(
    cached: &CachedChunk,
    modified: Option<SystemTime>,
    len: u64,
) -> bool {
    cached.modified == modified
        && cached.len == len
        && cached.compile_config_version == COMPILE_CACHE_CONFIG_VERSION
        && cached.bytecode_format_version == BYTECODE_FORMAT_VERSION
}

/// Determines whether the current file needs to read the source code and review the content fingerprint.
fn compiled_file_needs_fingerprint_check(modified: Option<SystemTime>) -> bool {
    let Some(modified) = modified else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age <= COMPILED_FILE_RECENT_VERIFY_WINDOW)
        .unwrap_or(true)
}

/// Reads the source code and confirms that cache entries are not contaminated by rapid overwrites of the same length.
fn compiled_file_source_still_valid(cache_path: &Path, cached: &CachedChunk) -> bool {
    let Ok(source) = fs::read_to_string(cache_path) else {
        return false;
    };
    if source_fingerprint(&source) != cached.source_fingerprint {
        return false;
    }
    let display_path = bt_path::path_text(&bt_path::normalize_path(cache_path));
    analyze_source(&display_path, &source)
        .map(|document| document.mode == cached.source_mode)
        .unwrap_or(false)
}

/// Estimates the number of compiled file cache entries in bytes.
fn compiled_file_entry_bytes(cache_key: &CompiledFileCacheKey, chunk: &Chunk) -> usize {
    cache_key
        .path
        .to_string_lossy()
        .len()
        .saturating_add(chunk.estimated_heap_bytes())
}

/// Estimates the size of template fragment cache entries in bytes.
fn template_fragment_entry_bytes(key: &TemplateFragmentCacheKey, chunk: &Chunk) -> usize {
    key.file
        .len()
        .saturating_add(key.code.len())
        .saturating_add(chunk.estimated_heap_bytes())
}

/// Counts the current estimated number of bytes in the compiled file cache.
fn compiled_file_cache_bytes(cache: &IndexMap<CompiledFileCacheKey, CachedChunk>) -> usize {
    cache.values().fold(0usize, |total, cached| {
        total.saturating_add(cached.estimated_bytes)
    })
}

/// Statistics template fragment cache current estimated number of bytes.
fn template_fragment_cache_bytes(
    cache: &IndexMap<TemplateFragmentCacheKey, CachedTemplateFragment>,
) -> usize {
    cache.values().fold(0usize, |total, cached| {
        total.saturating_add(cached.estimated_bytes)
    })
}

/// Evicts the compiled file cache in oldest write order until new entries can be written.
fn evict_compiled_file_cache_until_fit(
    cache: &mut IndexMap<CompiledFileCacheKey, CachedChunk>,
    incoming_bytes: usize,
) {
    while cache.len() >= COMPILED_FILE_CACHE_LIMIT
        || compiled_file_cache_bytes(cache).saturating_add(incoming_bytes)
            > COMPILED_FILE_CACHE_BYTES_LIMIT
    {
        if cache.shift_remove_index(0).is_none() {
            break;
        }
        update_cache_metrics(|metrics| metrics.compiled_file_evictions += 1);
    }
}

/// Evicts the template fragment cache in oldest written order until new entries can be written.
fn evict_template_fragment_cache_until_fit(
    cache: &mut IndexMap<TemplateFragmentCacheKey, CachedTemplateFragment>,
    incoming_bytes: usize,
) {
    while cache.len() >= TEMPLATE_FRAGMENT_CACHE_LIMIT
        || template_fragment_cache_bytes(cache).saturating_add(incoming_bytes)
            > TEMPLATE_FRAGMENT_CACHE_BYTES_LIMIT
    {
        if cache.shift_remove_index(0).is_none() {
            break;
        }
        update_cache_metrics(|metrics| metrics.template_fragment_evictions += 1);
    }
}

/// Updates the current thread VM cache counter.
fn update_cache_metrics(update: impl FnOnce(&mut VmCacheMetrics)) {
    VM_CACHE_METRICS.with(|metrics| update(&mut metrics.borrow_mut()));
}

/// BT bytecode virtual machine.
pub struct Vm {
    /// Global variable environment.
    ///
    /// Bytecode instruction still uses the symbol pool id internally; it is mapped to text through the symbol pool of the current chunk during execution.
    /// In this way, when the main program, functions, and include files each have their own symbol pools, they can also share the same global variable space.
    globals: HashMap<String, Value>,
    /// Names of defined global constants.
    ///
    /// Only queried for constant definitions and global write protection, keeping local-variable
    /// reads and writes off this path.
    global_constants: HashSet<String>,
    /// Output buffer to facilitate testing and subsequent access to the Web resident process.
    output: String,
    /// The project root directory of the current VM.
    ///
    /// `@`, `@/...` and `cur_root()` are all read from here; normal script entrance, Web site entrance and desktop
    /// App VM explicitly writes to a different root directory when creating the VM.
    project_root: PathBuf,
    /// Source stack for the current execution chain.
    ///
    /// Includes, functions, template fragments, and network callbacks push the source frame of the
    /// chunk they enter. Relative paths therefore follow the definition site rather than the call
    /// site or process working directory.
    source_stack: Vec<SourceFrame>,
    /// The source code location currently being executed, used to map runtime errors back to the source code.
    current_span: Option<SourceSpan>,
    /// Name of the function currently being executed.
    current_function: Option<String>,
    /// Maps class instances to the bytecode chunks that define them.
    ///
    /// Method function IDs belong to the chunk that created the class. Calls such as
    /// `this.other_method()` must return to that owner chunk to resolve the ID correctly.
    instance_chunks: HashMap<usize, Rc<Chunk>>,
    /// Maps class values to the bytecode chunks that define them.
    ///
    /// Runtime `include()` writes class definitions to globals, but their method bytecode remains in
    /// the included chunk. Mapping the class member table to that owner prevents a later `DB::new()`
    /// from resolving method IDs against the main chunk.
    class_chunks: HashMap<usize, Rc<Chunk>>,
    /// Maps global function names to the bytecode chunks that define them.
    ///
    /// An included `fn msg(){}` is stored in the shared global table, but its function ID is valid
    /// only in the defining chunk. Retaining that chunk by global name lets reads produce lightweight
    /// bound values and keeps included functions and callbacks on the correct function table.
    global_function_chunks: HashMap<String, Rc<Chunk>>,
    /// The extension manager currently used by the VM.
    ///
    /// Present only with the `extensions` feature, so builds without extensions pay no cost on the
    /// bytecode hot path. Entry functions are injected into `globals` and dispatched through this manager.
    #[cfg(feature = "extensions")]
    extension_manager: Option<Arc<ExtensionManager>>,
    /// Extension object handles released by WASM `close()` or `dispose()` in this VM.
    ///
    /// Disposal belongs to one script execution and cannot live in the site-wide `ExtensionManager`.
    /// The parent and template child VMs share this set so a handle disposed inside a template stays
    /// invalid in its parent. A fixed limit prevents silent growth in resident VMs.
    #[cfg(feature = "extensions")]
    disposed_extension_objects: Rc<RefCell<HashSet<ExtObject>>>,
    /// Response control status within a web request.
    ///
    /// The ordinary command line script is empty; the web request is injected by the `web` module before execution. `header()`,
    /// `status_code()`, `redirect()`, `send_file()` will write to the same status object, and a unified response will be generated after execution.
    web_response: Option<Rc<RefCell<BtWebResponse>>>,
    /// Script level force exit value.
    ///
    /// `exit()` does not terminate the BT process; it stops the current script. Once set, the same
    /// exit signal propagates through the innermost function, included chunks, and the main chunk.
    exit_value: Option<Value>,
    /// Collection of include_once files that have been executed within the current execution context.
    ///
    /// Web requests, CLI entries, desktop `bt.call()`, and network callbacks create independent
    /// contexts. Template child VMs share the current set so templates and scripts within one entry
    /// follow the same once-only rules.
    include_once_files: Rc<RefCell<HashSet<PathBuf>>>,
    /// Current nested execution context depth.
    ///
    /// Only the outermost entry will replace the new include_once collection; internal reentrant functions, include, template fragments, etc. will only be reused
    /// Already has a collection to avoid repeated execution of the same include_once file in the same entry link.
    execution_context_depth: usize,
    /// Mapping of TCP service numbers to script callbacks.
    net_tcp_callbacks: HashMap<usize, VmTcpCallbacks>,
    /// TCP client number to service number mapping.
    net_tcp_clients: HashMap<usize, usize>,
    /// Mapping of UDP socket numbers to script callbacks.
    net_udp_callbacks: HashMap<usize, VmUdpCallbacks>,
    /// Mapping of WebSocket service numbers to script callbacks.
    net_ws_callbacks: HashMap<usize, VmWsCallbacks>,
    /// Mapping of WebSocket client connection numbers to script callbacks.
    net_ws_client_callbacks: HashMap<usize, VmWsCallbacks>,
    /// WebSocket connection number to service number mapping.
    net_ws_sockets: HashMap<usize, usize>,
    /// VM lazy initialized timer event channel.
    timer_inbox: Option<VmTimerInbox>,
    /// Active timer callback table held by the VM.
    timer_callbacks: HashMap<usize, VmTimerCallback>,
    /// VM lazy initialized task completion event channel.
    task_inbox: Option<VmTaskInbox>,
    /// Task completion callback table held by VM.
    task_callbacks: HashMap<usize, VmTaskCallback>,
    /// Next VM local task callback number.
    next_task_callback_id: usize,
}

/// The result of a single bytecode execution.
enum ExecSignal {
    /// Has been executed normally.
    Done,
    /// Function returns.
    Return(Value),
    /// Error value explicitly thrown by the script.
    Throw(Value),
    /// Script is forced to end via `exit()`.
    Exit(Value),
}

/// The control flow result after the execution of a single instruction.
enum ExecStep {
    /// Executes the next instruction sequentially.
    Next,
    /// Jumps to the specified instruction index.
    Jump(usize),
    /// The current bytecode block ends and the execution signal is returned.
    Signal(ExecSignal),
}

/// Try capture range within the current bytecode block.
struct TryHandler {
    /// Catch Code block starting instruction index.
    catch_target: usize,
    /// Catch variable symbol.
    error_symbol: SymbolId,
}

impl Vm {
    /// Creates a new VM.
    pub fn new() -> Self {
        BtRuntime::init_start_snapshot();
        Self {
            globals: HashMap::new(),
            global_constants: HashSet::new(),
            output: String::new(),
            project_root: PathBuf::from("."),
            source_stack: Vec::new(),
            current_span: None,
            current_function: None,
            instance_chunks: HashMap::new(),
            class_chunks: HashMap::new(),
            global_function_chunks: HashMap::new(),
            #[cfg(feature = "extensions")]
            extension_manager: None,
            #[cfg(feature = "extensions")]
            disposed_extension_objects: Rc::new(RefCell::new(HashSet::new())),
            web_response: None,
            exit_value: None,
            include_once_files: Rc::new(RefCell::new(HashSet::new())),
            execution_context_depth: 0,
            net_tcp_callbacks: HashMap::new(),
            net_tcp_clients: HashMap::new(),
            net_udp_callbacks: HashMap::new(),
            net_ws_callbacks: HashMap::new(),
            net_ws_client_callbacks: HashMap::new(),
            net_ws_sockets: HashMap::new(),
            timer_inbox: None,
            timer_callbacks: HashMap::new(),
            task_inbox: None,
            task_callbacks: HashMap::new(),
            next_task_callback_id: 1,
        }
    }

    /// Creates a VM with the project root directory.
    ///
    /// The caller must explicitly pass in the normal script entry directory, Web site root directory or desktop project root directory.
    /// Prevents path resolution from falling back to the process startup directory.
    pub fn with_project_root(project_root: impl Into<PathBuf>) -> Self {
        let mut vm = Self::new();
        vm.set_project_root(project_root);
        vm
    }

    /// Updates the project root directory of the current VM.
    pub fn set_project_root(&mut self, project_root: impl Into<PathBuf>) {
        let project_root = bt_path::normalize_path(project_root.into());
        self.project_root = PathBuf::from(bt_path::path_text(&project_root));
    }

    /// Loads extensions into the current project root directory and injects global entries.
    #[cfg(feature = "extensions")]
    pub fn load_project_extensions(&mut self) -> Result<(), String> {
        let manager = Self::project_extension_manager(&self.project_root)?;
        self.set_extension_manager(manager)
    }

    /// Check project extensions directory in builds without extension capabilities enabled.
    #[cfg(not(feature = "extensions"))]
    pub fn load_project_extensions(&mut self) -> Result<(), String> {
        Self::check_project_extensions_available(&self.project_root)
    }

    /// Loads the project extension manager.
    #[cfg(feature = "extensions")]
    pub fn project_extension_manager(
        project_root: &Path,
    ) -> Result<Option<Arc<ExtensionManager>>, String> {
        ExtensionManager::load_project(
            project_root,
            Self::system_environment_names().iter().copied(),
        )
        .map(|manager| manager.map(Arc::new))
    }

    /// Check whether the project extension directory exists when the extension capability is not enabled.
    #[cfg(not(feature = "extensions"))]
    pub fn check_project_extensions_available(project_root: &Path) -> Result<(), String> {
        let extension_dir = project_root.join("extensions");
        if extension_dir.exists() {
            return Err(format!(
                "The current BT lightweight build does not enable extension capabilities and cannot load `{}`; the default build has extension capabilities enabled. For lightweight builds, please remove --no-default-features and rebuild",
                extension_dir.display()
            ));
        }
        Ok(())
    }

    /// Sets the extension manager for the current VM and injects a global entry.
    #[cfg(feature = "extensions")]
    pub fn set_extension_manager(
        &mut self,
        manager: Option<Arc<ExtensionManager>>,
    ) -> Result<(), String> {
        self.extension_manager = manager;
        self.disposed_extension_objects.borrow_mut().clear();
        self.inject_extension_globals()
    }

    /// Injects the extension entry into a global variable and marks it as a global constant.
    #[cfg(feature = "extensions")]
    fn inject_extension_globals(&mut self) -> Result<(), String> {
        let Some(manager) = &self.extension_manager else {
            return Ok(());
        };
        for (name, function) in manager.function_values() {
            if self.globals.contains_key(&name)
                || self.global_constants.contains(&name)
                || Self::native_constant(&name).is_some()
                || Self::native_function(&name).is_some()
            {
                return Err(format!(
                    "extension entry `{}` conflicts with the current global name",
                    name
                ));
            }
            self.global_constants.insert(name.clone());
            self.globals
                .insert(name, Value::ExtensionFunction(function));
        }
        Ok(())
    }

    /// Parses the file system path passed in to the script.
    ///
    /// Absolute paths pass through unchanged, `@` resolves from the project root, and ordinary
    /// relative paths resolve from the active source file's directory.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        bt_path::resolve_path(path, &self.project_root, &self.current_source_dir_path())
    }

    /// Returns the script text of the current project root directory.
    fn current_root_text(&self) -> String {
        bt_path::path_text(&self.project_root)
    }

    /// Returns the script text of the current source directory.
    fn current_dir_text(&self, complete: bool) -> String {
        let dir = self.current_source_dir_path();
        if complete {
            bt_path::path_text(&dir)
        } else {
            bt_path::relative_path_text(&dir, &self.project_root)
        }
    }

    /// Returns the script text of the current source code file.
    fn current_file_text(&self, complete: bool) -> String {
        let file = self.current_source_file_path();
        if complete {
            bt_path::path_text(&file)
        } else {
            bt_path::relative_path_text(&file, &self.project_root)
        }
    }

    /// Reads the current source code directory; falls back to the project root when there is no source code frame.
    fn current_source_dir_path(&self) -> PathBuf {
        self.source_stack
            .last()
            .map(|frame| frame.dir.clone())
            .unwrap_or_else(|| self.project_root.clone())
    }

    /// Reads the current source code file; returns the project root placeholder when there is no source code frame.
    fn current_source_file_path(&self) -> PathBuf {
        self.source_stack
            .last()
            .map(|frame| frame.file.clone())
            .unwrap_or_else(|| self.project_root.clone())
    }

    /// Pushes the Chunk source code element information into the execution stack.
    fn push_source_frame(&mut self, chunk: &Chunk) -> bool {
        if chunk.source_file.is_empty() && chunk.source_dir.is_empty() {
            return false;
        }
        let file = if chunk.source_file.is_empty() {
            self.current_source_file_path()
        } else {
            bt_path::normalize_path(PathBuf::from(&chunk.source_file))
        };
        let dir = if chunk.source_dir.is_empty() {
            file.parent()
                .map(bt_path::normalize_path)
                .unwrap_or_else(|| self.project_root.clone())
        } else {
            bt_path::normalize_path(PathBuf::from(&chunk.source_dir))
        };
        self.source_stack.push(SourceFrame { file, dir });
        true
    }

    /// Determines whether the script parameter enables full path output.
    fn bool_arg(args: &[Value], index: usize) -> bool {
        args.get(index).map(Value::is_truthy).unwrap_or(false)
    }

    /// Writes to global variables.
    ///
    /// Web requests take this path when injecting the `web` context to avoid bypassing the VM's unified global tables.
    pub fn set_global(&mut self, name: impl Into<String>, value: Value) {
        self.globals.insert(name.into(), value);
    }

    /// Reads global variables.
    pub fn get_global(&self, name: &str) -> Option<&Value> {
        self.globals.get(name)
    }

    /// Bind Web response control status.
    pub fn set_web_response(&mut self, response: Rc<RefCell<BtWebResponse>>) {
        self.web_response = Some(response);
    }

    /// Reads the currently collected script output.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Clears the script output buffer.
    ///
    /// CLI and web entries consume the complete output. A resident desktop VM needs to retain only
    /// globals, so front-end calls clear this buffer to keep debug output from accumulating.
    #[allow(dead_code)]
    pub fn clear_output(&mut self) {
        self.output.clear();
    }

    /// Immediately writes the current script output buffer to standard output.
    ///
    /// `print` and `println` first write to the VM buffer for web responses and template execution.
    /// Before `pause()` waits for console input, the existing output must be flushed so the user can
    /// see everything printed before the pause.
    fn flush_output_to_stdout(&mut self) {
        if !self.output.is_empty() {
            print!("{}", self.output);
            self.output.clear();
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    /// Runs entry logic in an isolated execution context.
    ///
    /// The `include_once` set is created only for the outermost entry and shared by nested function,
    /// include, and template calls. Restoring the previous set afterward prevents a resident desktop
    /// VM or background callback from leaking one entry's include state into the next.
    fn with_execution_context<T, E>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        let previous = self.enter_execution_context();
        let result = action(self);
        self.leave_execution_context(previous);
        result
    }

    /// Enters the execution context and replaces the new include_once collection at the outermost entry.
    fn enter_execution_context(&mut self) -> Option<Rc<RefCell<HashSet<PathBuf>>>> {
        if self.execution_context_depth == 0 {
            self.execution_context_depth = 1;
            Some(std::mem::replace(
                &mut self.include_once_files,
                Rc::new(RefCell::new(HashSet::new())),
            ))
        } else {
            self.execution_context_depth += 1;
            None
        }
    }

    /// Leaves the execution context and releases this include_once collection at the end of the outermost entry.
    fn leave_execution_context(&mut self, previous: Option<Rc<RefCell<HashSet<PathBuf>>>>) {
        self.execution_context_depth = self.execution_context_depth.saturating_sub(1);
        if self.execution_context_depth == 0 {
            self.include_once_files =
                previous.unwrap_or_else(|| Rc::new(RefCell::new(HashSet::new())));
        }
    }

    /// Executes a block of bytecode and returns the output.
    pub fn run(&mut self, chunk: &Chunk) -> Result<String, VmError> {
        self.with_execution_context(|vm| {
            let signal = vm.execute_chunk(chunk, None, None, None)?;
            if let ExecSignal::Exit(value) = signal {
                if vm.output.is_empty() {
                    vm.output.push_str(&value.to_output_string());
                }
            } else if let ExecSignal::Throw(value) = signal {
                return Err(vm.throw_error(0, value));
            }
            Ok(vm.output.clone())
        })
    }

    /// Waits for network, timer and task background events and executes callbacks on the current VM main thread.
    ///
    /// Ordinary scripts will not enter here when there are no network events, timer and task callbacks; once there are background events, this loop will only
    /// Runs after the entry execution flow ends, so it does not add polling cost to the bytecode instruction hot path.
    pub fn wait_for_background_events(&mut self, chunk: &Chunk) -> Result<(), String> {
        loop {
            self.drain_timer_events();
            self.drain_task_events();
            if !net_runtime::has_event_tasks()
                && !self.has_active_timers()
                && !self.has_active_task_callbacks()
            {
                break;
            }

            let timeout = self
                .next_background_wait()
                .unwrap_or(TASK_CALLBACK_SCAN_WAIT);
            if net_runtime::has_event_tasks() {
                let Some(event) = net_runtime::recv_event(timeout) else {
                    continue;
                };
                if matches!(event, NetEvent::Wake) {
                    continue;
                }
                if let Err(err) =
                    self.with_execution_context(|vm| vm.dispatch_net_event(chunk, event))
                {
                    eprintln!("{}", err);
                }
                self.flush_output_to_stdout();
            } else if self.has_active_timers() {
                if let Some(event) = self.recv_timer_event_timeout(timeout) {
                    self.dispatch_timer_event(event);
                }
            } else if self.has_active_task_callbacks() {
                if let Some(event) = self.recv_task_event_timeout(timeout) {
                    self.dispatch_task_event(event);
                }
            }
        }
        net_runtime::wait_for_background_tasks()
    }

    /// Whether the current VM still has background events that need to be kept alive.
    pub fn has_background_events(&self) -> bool {
        self.has_active_timers() || self.has_active_task_callbacks()
    }

    /// Whether the current VM still has active timers.
    pub fn has_active_timers(&self) -> bool {
        !self.timer_callbacks.is_empty()
    }

    /// Handles all timer events that have arrived for the current VM.
    pub fn drain_timer_events(&mut self) {
        loop {
            match self.try_recv_timer_event() {
                Some(event) => self.dispatch_timer_event(event),
                None => break,
            }
        }
    }

    /// Calculates how long the current VM needs to wait before its most recent timer expires.
    pub fn next_timer_wait(&self) -> Option<Duration> {
        let due = self
            .timer_callbacks
            .values()
            .filter_map(|callback| callback.next_due)
            .min()?;
        let now = Instant::now();
        if due <= now {
            Some(Duration::from_millis(1))
        } else {
            Some(due.saturating_duration_since(now))
        }
    }

    /// Calculate the waiting time for the next background event of the current VM.
    pub fn next_background_wait(&self) -> Option<Duration> {
        match (self.next_timer_wait(), self.has_active_task_callbacks()) {
            (Some(timer_wait), true) => Some(timer_wait.min(TASK_CALLBACK_SCAN_WAIT)),
            (Some(timer_wait), false) => Some(timer_wait),
            (None, true) => Some(TASK_CALLBACK_SCAN_WAIT),
            (None, false) => None,
        }
    }

    /// Non-blocking read of a timer event.
    fn try_recv_timer_event(&mut self) -> Option<TimerEvent> {
        let inbox = self.timer_inbox.as_ref()?;
        match inbox.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Blocks reading a timer event within a limited time.
    fn recv_timer_event_timeout(&mut self, timeout: Duration) -> Option<TimerEvent> {
        self.timer_inbox
            .as_ref()
            .and_then(|inbox| inbox.receiver.recv_timeout(timeout).ok())
    }

    /// Dispatches a timer event.
    fn dispatch_timer_event(&mut self, event: TimerEvent) {
        let Some(kind) = self
            .timer_callbacks
            .get(&event.id)
            .map(|callback| callback.kind)
        else {
            return;
        };
        match kind {
            TimerKind::Timeout => self.dispatch_timeout_event(event.id),
            TimerKind::Interval => self.dispatch_interval_event(event.id),
        }
    }

    /// Dispatches a one-time timeout callback.
    fn dispatch_timeout_event(&mut self, id: usize) {
        let Some(callback) = self.timer_callbacks.remove(&id) else {
            return;
        };
        timer::finish(id);
        let fallback = Chunk::new();
        self.call_event_callback(&fallback, &callback.callback, Vec::new(), "timeout");
    }

    /// Dispatches interval callbacks and re-registers for the next round with a fixed delay after the callback completes.
    fn dispatch_interval_event(&mut self, id: usize) {
        let Some(entry) = self.timer_callbacks.get_mut(&id) else {
            return;
        };
        if entry.running {
            return;
        }
        entry.running = true;
        entry.next_due = None;
        let callback = entry.callback.clone();
        let timer = entry.timer.clone();
        let delay_ms = entry.delay_ms;
        let fallback = Chunk::new();

        self.call_event_callback(
            &fallback,
            &callback,
            vec![Value::Timer(timer.clone())],
            "interval",
        );

        let Some(sender) = self.timer_sender() else {
            self.timer_callbacks.remove(&id);
            timer::cancel(id);
            return;
        };
        let next_due = timer::schedule(&timer, TimerKind::Interval, delay_ms, sender);
        match next_due {
            Some(due) => {
                if let Some(entry) = self.timer_callbacks.get_mut(&id) {
                    entry.running = false;
                    entry.next_due = Some(due);
                }
            }
            None => {
                self.timer_callbacks.remove(&id);
            }
        }
    }

    /// Returns the initialized timer event sender.
    fn timer_sender(&self) -> Option<SyncSender<TimerEvent>> {
        self.timer_inbox.as_ref().map(|inbox| inbox.sender.clone())
    }

    /// Lazy initializes and returns the timer event sender of the current VM.
    fn ensure_timer_sender(&mut self) -> SyncSender<TimerEvent> {
        if self.timer_inbox.is_none() {
            let (sender, receiver) = mpsc::sync_channel(timer::event_queue_limit());
            self.timer_inbox = Some(VmTimerInbox { sender, receiver });
        }
        self.timer_inbox
            .as_ref()
            .expect("timer inbox should have been initialized")
            .sender
            .clone()
    }

    /// Whether the current VM still has active task completion callbacks.
    pub fn has_active_task_callbacks(&self) -> bool {
        !self.task_callbacks.is_empty()
    }

    /// Handles task callback events that the current VM has received or completed.
    pub fn drain_task_events(&mut self) {
        while let Some(event) = self.try_recv_task_event() {
            self.dispatch_task_event(event);
        }
        self.dispatch_ready_task_callbacks();
    }

    /// Non-blocking reading of a task completion event.
    fn try_recv_task_event(&mut self) -> Option<usize> {
        let inbox = self.task_inbox.as_ref()?;
        match inbox.receiver.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Blocks reading a task completion event within a limited time.
    fn recv_task_event_timeout(&mut self, timeout: Duration) -> Option<usize> {
        self.task_inbox
            .as_ref()
            .and_then(|inbox| inbox.receiver.recv_timeout(timeout).ok())
    }

    /// Lazy initialization and returns the task completion event sender of the current VM.
    fn ensure_task_sender(&mut self) -> SyncSender<usize> {
        if self.task_inbox.is_none() {
            let (sender, receiver) = mpsc::sync_channel(TASK_EVENT_QUEUE_LIMIT);
            self.task_inbox = Some(VmTaskInbox { sender, receiver });
        }
        self.task_inbox
            .as_ref()
            .expect("Task completion inbox should have been initialized")
            .sender
            .clone()
    }

    /// Task callback for which the scan has been completed but the wake-up event has not been successfully delivered.
    fn dispatch_ready_task_callbacks(&mut self) {
        let ready = self
            .task_callbacks
            .iter()
            .filter_map(|(id, callback)| callback.task.done().then_some(*id))
            .collect::<Vec<_>>();
        for id in ready {
            self.dispatch_task_event(id);
        }
    }

    /// Dispatches a task completion callback event.
    fn dispatch_task_event(&mut self, id: usize) {
        let Some(mut callback) = self.task_callbacks.remove(&id) else {
            return;
        };
        let Some(outcome) = callback.task.result() else {
            self.task_callbacks.insert(id, callback);
            return;
        };
        let _ = callback.subscription.take();
        let args = Self::task_callback_args(&outcome);
        let fallback = Chunk::new();
        self.call_event_callback(&fallback, &callback.callback, args, "task.on_done");
    }

    /// Converts task results into on_done callback parameters.
    fn task_callback_args(outcome: &TaskRunOutcome) -> Vec<Value> {
        match outcome {
            TaskRunOutcome::Success(value) => vec![
                value.to_value(),
                Value::Empty,
                Value::Str("success".to_string()),
            ],
            TaskRunOutcome::Thrown(value) => vec![
                Value::Empty,
                value.to_value(),
                Value::Str("throw".to_string()),
            ],
            TaskRunOutcome::Failed(message) => vec![
                Value::Empty,
                Value::Str(message.clone()),
                Value::Str("failed".to_string()),
            ],
        }
    }

    /// Executes a block of bytecode and returns both an output buffer and a script return value.
    ///
    /// Web handlers may produce a response through `print` or through their final expression, so both
    /// results are retained and the caller chooses which takes precedence.
    #[allow(dead_code)]
    pub fn run_with_value(&mut self, chunk: &Chunk) -> Result<(String, Value), VmError> {
        self.with_execution_context(|vm| {
            let signal = vm.execute_chunk(chunk, None, None, None)?;
            vm.value_from_signal(signal)
        })
    }

    /// Executes a cached bytecode chunk and reuses it as the owner of functions and classes.
    ///
    /// Web services repeatedly execute the same entry file. Passing an `Rc<Chunk>` lets functions,
    /// closures, and class instances reference the cached owner instead of cloning its bytecode per request.
    pub fn run_with_value_owned(&mut self, chunk: Rc<Chunk>) -> Result<(String, Value), VmError> {
        self.with_execution_context(|vm| {
            let signal = vm.execute_chunk(chunk.as_ref(), None, None, Some(chunk.clone()))?;
            vm.value_from_signal(signal)
        })
    }

    /// Converts bytecode execution signals into output and return values required by the web/caller.
    fn value_from_signal(&self, signal: ExecSignal) -> Result<(String, Value), VmError> {
        let value = match signal {
            ExecSignal::Return(value) => value,
            ExecSignal::Exit(value) => value,
            ExecSignal::Throw(value) => {
                return Err(self.throw_error(0, value));
            }
            ExecSignal::Done => Value::Empty,
        };
        Ok((self.output.clone(), value))
    }

    /// Calls the BT function that has been registered in the global environment.
    ///
    /// Desktop `bt.call()` resolves only functions left in globals after `main.bt` executes. It never
    /// falls back to built-ins by name, preventing the front end from bypassing explicit registration
    /// to call capabilities such as `fs()` or `process()`.
    #[allow(dead_code)]
    pub fn call_global(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        self.with_execution_context(|vm| vm.call_global_inner(name, args))
    }

    /// Lets the pure-BT extension runner call object methods inside its own VM.
    ///
    /// The runner VM owns the extension object while the main VM holds only a lightweight handle.
    /// Method calls must therefore return to the runner; the main VM cannot expand pure-BT objects directly.
    #[cfg(feature = "extensions")]
    pub fn call_value_method_for_extension(
        &mut self,
        receiver: &Value,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, VmError> {
        self.with_execution_context(|vm| {
            let chunk = Chunk::new();
            vm.call_native_method(&chunk, receiver, name, false, args, 0)
        })
    }

    /// Implements global function calls.
    ///
    /// Public entrance is responsible for creating the execution context; here only the function owner parsing and specific calls are processed to facilitate internal reuse.
    fn call_global_inner(&mut self, name: &str, args: Vec<Value>) -> Result<Value, VmError> {
        let callable = self.global_value_for_name(name).ok_or_else(|| VmError {
            ip: 0,
            message: format!("Callable BT function not found `{}`", name),
            span: self.current_span.clone(),
            function: self.current_function.clone(),
            throw_value: None,
        })?;

        match callable {
            Value::Function(function_id) => {
                let owner = self
                    .global_function_chunks
                    .get(name)
                    .cloned()
                    .ok_or_else(|| VmError {
                        ip: 0,
                        message: format!(
                            "BT function `{}` is missing the corresponding bytecode block",
                            name
                        ),
                        span: self.current_span.clone(),
                        function: self.current_function.clone(),
                        throw_value: None,
                    })?;
                self.call_user_function(&owner, function_id, args, 0)
            }
            Value::BoundFunction(function_id, owner) => {
                self.call_user_function(&owner, function_id, args, 0)
            }
            Value::Closure(function_id, owner, captures) => self.call_user_function_inner(
                &owner,
                function_id,
                args,
                None,
                Some(captures.as_ref().clone()),
                0,
            ),
            Value::NativeFunction(native_name) => {
                let chunk = Chunk::new();
                self.call_native_function(&chunk, &native_name, args, 0)
            }
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(function) => self.call_extension_function(&function, args, 0),
            other => Err(VmError {
                ip: 0,
                message: format!(
                    "`{}` is not a callable function, the current type is {}",
                    name,
                    other.type_name()
                ),
                span: self.current_span.clone(),
                function: self.current_function.clone(),
                throw_value: None,
            }),
        }
    }

    /// Executes a bytecode chunk.
    ///
    /// With no local scope this runs the main program against globals. With a local scope it runs a
    /// function, reading and writing locals first and falling back to globals when a name is absent.
    fn execute_chunk(
        &mut self,
        chunk: &Chunk,
        locals: Option<LocalScope>,
        function_name: Option<String>,
        owner_hint: Option<Rc<Chunk>>,
    ) -> Result<ExecSignal, VmError> {
        let pushed_source = self.push_source_frame(chunk);
        let previous_function = self.current_function.clone();
        self.current_function = function_name;
        let result = self.execute_chunk_inner(chunk, locals, owner_hint);
        self.current_function = previous_function;
        if pushed_source {
            self.source_stack.pop();
        }
        result
    }

    /// Executes the bytecode block body.
    fn execute_chunk_inner(
        &mut self,
        chunk: &Chunk,
        mut locals: Option<LocalScope>,
        owner_hint: Option<Rc<Chunk>>,
    ) -> Result<ExecSignal, VmError> {
        let mut ip = 0usize;
        let mut registers = vec![Value::Empty; chunk.register_count as usize + 1];
        let mut origins = vec![None; chunk.register_count as usize + 1];
        let mut chunk_owner = owner_hint;
        let mut try_stack: Vec<TryHandler> = Vec::new();

        while ip < chunk.code.len() {
            self.current_span = chunk.spans.get(ip).cloned().flatten();
            let step = (|| -> Result<ExecStep, VmError> {
                match &chunk.code[ip] {
                    Instruction::LoadConst { dst, constant } => {
                        let value = chunk
                            .constants
                            .get(*constant as usize)
                            .map(Value::clone_mutable_literal)
                            .ok_or_else(|| {
                                self.error(ip, "constant pool subscript out of bounds")
                            })?;
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::ExpandTemplate { dst, src } => {
                        let value = Self::read_register(&registers, *src, ip)?;
                        let Value::Str(template) = value else {
                            return Err(
                                self.error(ip, "template string source value must be a string")
                            );
                        };
                        let text = self.expand_template(chunk, locals.as_ref(), template, ip)?;
                        Self::write_register(&mut registers, *dst, Value::Str(text), ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::Move { dst, src } => {
                        let value = Self::read_register(&registers, *src, ip)?.clone();
                        let origin = Self::read_origin(&origins, *src).cloned();
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, origin, ip)?;
                    }
                    Instruction::LoadGlobal { dst, symbol } => {
                        let name = Self::symbol_name(chunk, *symbol, ip)?;
                        let local_value = locals.as_ref().and_then(|locals| {
                            locals
                                .get(*symbol as usize)
                                .and_then(Option::as_ref)
                                .map(|cell| cell.borrow().clone().unwrap_or(Value::Empty))
                                .or_else(|| chunk.is_local(*symbol).then_some(Value::Empty))
                        });
                        let global_value = match self.globals.get(name) {
                            Some(Value::Function(function_id)) => self
                                .global_function_chunks
                                .get(name)
                                .map(|owner| Value::BoundFunction(*function_id, owner.clone()))
                                .or_else(|| Some(Value::Function(*function_id))),
                            Some(value) => Some(value.clone()),
                            None => None,
                        };
                        let native_value =
                            Self::native_function(name).or_else(|| Self::native_constant(name));
                        let missing = local_value.is_none()
                            && global_value.is_none()
                            && native_value.is_none();
                        let value = local_value
                            .or(global_value)
                            .or(native_value)
                            .unwrap_or(Value::Empty);
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(
                            &mut origins,
                            *dst,
                            self.variable_origin(name.to_string(), missing),
                            ip,
                        )?;
                    }
                    Instruction::StoreGlobal { symbol, src } => {
                        let value = Self::read_register(&registers, *src, ip)?.clone();
                        self.store_assignment_symbol(
                            chunk,
                            &mut chunk_owner,
                            locals.as_mut(),
                            *symbol,
                            value,
                            ip,
                        )?;
                    }
                    Instruction::StoreConst { symbol, src } => {
                        let value = Self::read_register(&registers, *src, ip)?.clone();
                        self.store_constant_symbol(
                            chunk,
                            &mut chunk_owner,
                            locals.as_mut(),
                            *symbol,
                            value,
                            ip,
                        )?;
                    }
                    Instruction::DestructureAssign {
                        src,
                        symbols,
                        constants,
                    } => {
                        let value = Self::read_register(&registers, *src, ip)?;
                        self.destructure_assign(
                            chunk,
                            &mut chunk_owner,
                            locals.as_mut(),
                            value,
                            symbols,
                            constants,
                            ip,
                        )?;
                    }
                    Instruction::Binary { op, dst, lhs, rhs } => {
                        let left = Self::read_register(&registers, *lhs, ip)?;
                        let right = Self::read_register(&registers, *rhs, ip)?;
                        let value = Self::eval_binary(op, left, right).map_err(|message| {
                            self.binary_error(
                                ip,
                                op,
                                left,
                                right,
                                Self::read_origin(&origins, *lhs),
                                Self::read_origin(&origins, *rhs),
                                message,
                            )
                        })?;
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::Increment { dst, src, delta } => {
                        let current = Self::read_register(&registers, *src, ip)?;
                        let value = match current {
                            Value::Int(value) => Value::Int(
                                value.checked_add(i64::from(*delta)).ok_or_else(|| {
                                    self.error(
                                        ip,
                                        "increment or decrement causing integer overflow",
                                    )
                                })?,
                            ),
                            Value::Float(value) => Value::Float(*value + f64::from(*delta)),
                            other => {
                                return Err(self.error(
                                    ip,
                                    format!(
                                        "auto-increment or auto-decrement only supports Int or Float, and the current value type is {}",
                                        other.type_name()
                                    ),
                                ));
                            }
                        };
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::Not { dst, src } => {
                        let value = !Self::read_register(&registers, *src, ip)?.is_truthy();
                        Self::write_register(&mut registers, *dst, Value::Bool(value), ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::BitNot { dst, src } => {
                        let value = Self::read_register(&registers, *src, ip)?
                            .bitwise_not()
                            .map_err(|message| self.error(ip, message))?;
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::MakeArray { dst, items } => {
                        let mut values = Vec::with_capacity(items.len());
                        for item in items {
                            values.push(Self::read_register(&registers, *item, ip)?.clone());
                        }
                        Self::write_register(
                            &mut registers,
                            *dst,
                            Value::Array(Rc::new(RefCell::new(values))),
                            ip,
                        )?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::MakeObject { dst, entries } => {
                        let mut values = IndexMap::new();
                        for (symbol, register) in entries {
                            let key = chunk
                                .symbols
                                .name(*symbol)
                                .ok_or_else(|| {
                                    self.error(ip, "object attribute symbol is missing")
                                })?
                                .to_string();
                            values.insert(
                                key,
                                Self::read_register(&registers, *register, ip)?.clone(),
                            );
                        }
                        Self::write_register(
                            &mut registers,
                            *dst,
                            Value::Object(Rc::new(RefCell::new(values))),
                            ip,
                        )?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::GetProperty { dst, object, key } => {
                        let object_register = *object;
                        let object = Self::read_register(&registers, object_register, ip)?;
                        let key = Self::read_register(&registers, *key, ip)?;
                        let allow_private =
                            Self::is_this_origin(Self::read_origin(&origins, object_register));
                        let value = self
                            .get_property(object, key, allow_private)
                            .map_err(|message| self.error(ip, message))?;
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::SetProperty { object, key, value } => {
                        let object_register = *object;
                        let object = Self::read_register(&registers, object_register, ip)?;
                        let key = Self::read_register(&registers, *key, ip)?;
                        let value = Self::read_register(&registers, *value, ip)?.clone();
                        let allow_private =
                            Self::is_this_origin(Self::read_origin(&origins, object_register));
                        Self::set_property(object, key, value, allow_private)
                            .map_err(|message| self.error(ip, message))?;
                    }
                    Instruction::MakeFunction { dst, function } => {
                        let function_id = *function as usize;
                        let value = if let Some(parent_locals) = locals.as_ref() {
                            let owner = chunk_owner
                                .get_or_insert_with(|| Rc::new(chunk.clone()))
                                .clone();
                            let function = chunk
                                .functions
                                .get(function_id)
                                .ok_or_else(|| self.error(ip, "function number does not exist"))?;
                            let captures = Self::capture_function_locals(
                                chunk,
                                parent_locals,
                                function.chunk.as_ref(),
                            );
                            Value::Closure(function_id, owner, Rc::new(captures))
                        } else {
                            Value::Function(function_id)
                        };
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::MakeClass { dst, name, members } => {
                        let name = chunk
                            .symbols
                            .name(*name)
                            .unwrap_or("<anonymous>")
                            .to_string();
                        let mut values = IndexMap::new();
                        for (symbol, register, is_public) in members {
                            let key = chunk
                                .symbols
                                .name(*symbol)
                                .ok_or_else(|| self.error(ip, "class member symbol is missing"))?
                                .to_string();
                            values.insert(
                                key,
                                ClassMember {
                                    value: Self::read_register(&registers, *register, ip)?.clone(),
                                    is_public: *is_public,
                                },
                            );
                        }
                        Self::write_register(
                            &mut registers,
                            *dst,
                            Value::Class(name, Rc::new(values)),
                            ip,
                        )?;
                        if let Value::Class(_, class_members) =
                            Self::read_register(&registers, *dst, ip)?
                        {
                            let owner = chunk_owner
                                .get_or_insert_with(|| Rc::new(chunk.clone()))
                                .clone();
                            self.class_chunks
                                .insert(Rc::as_ptr(class_members) as usize, owner);
                        }
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::IterInit { dst, iterable } => {
                        let iterable = Self::read_register(&registers, *iterable, ip)?;
                        let state = Self::make_iterator(iterable)
                            .map_err(|message| self.error(ip, message))?;
                        Self::write_register(
                            &mut registers,
                            *dst,
                            Value::Iterator(Rc::new(RefCell::new(state))),
                            ip,
                        )?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::CountInit { dst, count, step } => {
                        let count = Self::read_register(&registers, *count, ip)?;
                        let step = step
                            .map(|register| Self::read_register(&registers, register, ip))
                            .transpose()?;
                        let state = Self::make_count_iterator(count, step)
                            .map_err(|message| self.error(ip, message))?;
                        Self::write_register(
                            &mut registers,
                            *dst,
                            Value::Iterator(Rc::new(RefCell::new(state))),
                            ip,
                        )?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::RangeInit {
                        dst,
                        start,
                        end,
                        step,
                    } => {
                        let start = Self::read_register(&registers, *start, ip)?;
                        let end = end
                            .map(|register| Self::read_register(&registers, register, ip))
                            .transpose()?;
                        let step = Self::read_register(&registers, *step, ip)?;
                        let state = Self::make_range_iterator(start, end, step)
                            .map_err(|message| self.error(ip, message))?;
                        Self::write_register(
                            &mut registers,
                            *dst,
                            Value::Iterator(Rc::new(RefCell::new(state))),
                            ip,
                        )?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::IterNext {
                        iterator,
                        key_symbol,
                        value_symbol,
                        jump_to_end,
                    } => {
                        let iterator = Self::read_register(&registers, *iterator, ip)?;
                        let Value::Iterator(state) = iterator else {
                            return Err(
                                self.error(ip, "for loop internal iterator status is invalid")
                            );
                        };
                        let next = {
                            let mut state = state.borrow_mut();
                            Self::next_iterator_item(&mut state)
                                .map_err(|message| self.error(ip, message))?
                        };
                        let Some((key, value)) = next else {
                            return Ok(ExecStep::Jump(*jump_to_end as usize));
                        };
                        if let Some(symbol) = key_symbol {
                            self.store_symbol(chunk, locals.as_mut(), *symbol, key, ip)?;
                        }
                        if let Some(symbol) = value_symbol {
                            self.store_symbol(chunk, locals.as_mut(), *symbol, value, ip)?;
                        }
                    }
                    Instruction::IterNextDestructure {
                        iterator,
                        symbols,
                        jump_to_end,
                    } => {
                        let iterator = Self::read_register(&registers, *iterator, ip)?;
                        let Value::Iterator(state) = iterator else {
                            return Err(
                                self.error(ip, "for loop internal iterator status is invalid")
                            );
                        };
                        let next = {
                            let mut state = state.borrow_mut();
                            Self::next_iterator_item(&mut state)
                                .map_err(|message| self.error(ip, message))?
                        };
                        let Some((_, value)) = next else {
                            return Ok(ExecStep::Jump(*jump_to_end as usize));
                        };
                        self.destructure_assign(
                            chunk,
                            &mut chunk_owner,
                            locals.as_mut(),
                            &value,
                            symbols,
                            &[],
                            ip,
                        )?;
                    }
                    Instruction::UseFields { object, fields } => {
                        let object = Self::read_register(&registers, *object, ip)?;
                        self.import_fields(chunk, locals.as_mut(), object, fields, ip)?;
                    }
                    Instruction::Call { dst, callee, args } => {
                        let callee = Self::read_register(&registers, *callee, ip)?.clone();
                        let mut values = Vec::with_capacity(args.len());
                        for arg in args {
                            values.push(Self::read_register(&registers, *arg, ip)?.clone());
                        }
                        let value = self.call_value(chunk, &callee, values, ip)?;
                        Self::write_register(&mut registers, *dst, value, ip)?;
                        Self::write_origin(&mut origins, *dst, self.span_origin(), ip)?;
                    }
                    Instruction::Jump { target } => {
                        return Ok(ExecStep::Jump(*target as usize));
                    }
                    Instruction::JumpIfFalse { condition, target } => {
                        let condition = Self::read_register(&registers, *condition, ip)?;
                        if !condition.is_truthy() {
                            return Ok(ExecStep::Jump(*target as usize));
                        }
                    }
                    Instruction::JumpIfTrue { condition, target } => {
                        let condition = Self::read_register(&registers, *condition, ip)?;
                        if condition.is_truthy() {
                            return Ok(ExecStep::Jump(*target as usize));
                        }
                    }
                    Instruction::JumpIfNullish { condition, target } => {
                        let condition = Self::read_register(&registers, *condition, ip)?;
                        if matches!(condition, Value::Null | Value::Empty) {
                            return Ok(ExecStep::Jump(*target as usize));
                        }
                    }
                    Instruction::EnterTry {
                        catch_target,
                        error_symbol,
                    } => {
                        try_stack.push(TryHandler {
                            catch_target: *catch_target as usize,
                            error_symbol: *error_symbol,
                        });
                    }
                    Instruction::LeaveTry => {
                        try_stack.pop();
                    }
                    Instruction::Print { src, newline } => {
                        let value = Self::read_register(&registers, *src, ip)?;
                        self.output.push_str(&value.to_output_string());
                        if *newline {
                            self.output.push('\n');
                        }
                    }
                    Instruction::Pop { src } => {
                        let _ = Self::read_register(&registers, *src, ip)?;
                    }
                    Instruction::Return { src } => {
                        let value = Self::read_register(&registers, *src, ip)?.clone();
                        return Ok(ExecStep::Signal(ExecSignal::Return(value)));
                    }
                    Instruction::Throw { src } => {
                        let value = Self::read_register(&registers, *src, ip)?.clone();
                        return Ok(ExecStep::Signal(ExecSignal::Throw(value)));
                    }
                    Instruction::Halt => return Ok(ExecStep::Signal(ExecSignal::Done)),
                }
                Ok(ExecStep::Next)
            })();
            let step = match step {
                Ok(ExecStep::Signal(ExecSignal::Throw(value))) => {
                    if let Some(handler) = try_stack.pop() {
                        self.store_symbol(chunk, locals.as_mut(), handler.error_symbol, value, ip)?;
                        ExecStep::Jump(handler.catch_target)
                    } else {
                        return Ok(ExecSignal::Throw(value));
                    }
                }
                Ok(step) => step,
                Err(err) if err.throw_value.is_some() => {
                    let value = err.throw_value.clone().unwrap_or(Value::Empty);
                    if let Some(handler) = try_stack.pop() {
                        self.store_symbol(chunk, locals.as_mut(), handler.error_symbol, value, ip)?;
                        ExecStep::Jump(handler.catch_target)
                    } else {
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            };
            if let Some(value) = self.exit_value.clone() {
                return Ok(ExecSignal::Exit(value));
            }
            match step {
                ExecStep::Next => ip += 1,
                ExecStep::Jump(target) => ip = target,
                ExecStep::Signal(signal) => return Ok(signal),
            }
        }

        Ok(ExecSignal::Done)
    }

    /// Expands backtick template strings.
    ///
    /// Scanning, fast paths, and script execution deliberately stay in one top-down pass. Plain text
    /// is copied directly, and simple snippets such as `${name}` and `${123}` skip the compiler.
    /// Only real expressions and `${...}$` script blocks are compiled temporarily, keeping common
    /// templates close to a linear scan.
    fn expand_template(
        &mut self,
        chunk: &Chunk,
        locals: Option<&LocalScope>,
        template: &str,
        ip: usize,
    ) -> Result<String, VmError> {
        if !template.as_bytes().windows(2).any(|item| item == b"${") {
            return Ok(template.to_string());
        }

        let mut output = String::with_capacity(template.len());
        let mut cursor = 0usize;
        while let Some(relative_start) = template[cursor..].find("${") {
            let tag_start = cursor + relative_start;
            output.push_str(&template[cursor..tag_start]);
            let body_start = tag_start + 2;
            let is_script = Self::is_template_script_tag(template, body_start);
            let Some(tag_end) = Self::find_template_tag_end(template, body_start, is_script) else {
                let span = self.template_span(template, body_start);
                let message = if is_script {
                    "The template script tag is missing the closing tag `}$`"
                } else {
                    "The template expression tag is missing the closing tag `}`"
                };
                return Err(self.error_at(ip, message, span));
            };
            let code = &template[body_start..tag_end];
            let value = if is_script {
                self.eval_template_script(chunk, locals, template, body_start, code, ip)?
            } else {
                self.eval_template_expr(chunk, locals, template, body_start, code, ip)?
            };
            output.push_str(&value);
            cursor = tag_end + if is_script { 2 } else { 1 };
        }
        output.push_str(&template[cursor..]);
        Ok(output)
    }

    /// Determines whether the current `${` belongs to the script template label.
    ///
    /// Simple expression tags cannot contain braces. Multiline content, or a `{` before the first
    /// `}`, is parsed as `${...}$` so constructs such as `for { ... }` are not cut off early.
    fn is_template_script_tag(template: &str, body_start: usize) -> bool {
        let bytes = template.as_bytes();
        let mut index = body_start;
        let mut multiline_prefix = false;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => {
                    multiline_prefix = true;
                    index += 1;
                }
                b' ' | b'\t' | b'\r' => index += 1,
                _ => break,
            }
        }
        if multiline_prefix && template[body_start..].contains("}$") {
            return true;
        }
        while index < bytes.len() {
            match bytes[index] {
                b'{' => return true,
                b'}' => return index + 1 < bytes.len() && bytes[index + 1] == b'$',
                _ => index += 1,
            }
        }
        false
    }

    /// Finds the end position of the template tag.
    fn find_template_tag_end(template: &str, body_start: usize, is_script: bool) -> Option<usize> {
        if is_script {
            template[body_start..]
                .find("}$")
                .map(|offset| body_start + offset)
        } else {
            template[body_start..]
                .find('}')
                .map(|offset| body_start + offset)
        }
    }

    /// Executes the `${...}` expression template.
    ///
    /// Variable names, numbers, Boolean, strings, null and empty are returned directly; other expressions are temporarily packed into
    /// `return expression`, reuses the existing parser/compiler/VM and maintains consistent language semantics.
    fn eval_template_expr(
        &mut self,
        parent_chunk: &Chunk,
        locals: Option<&LocalScope>,
        template: &str,
        body_start: usize,
        code: &str,
        ip: usize,
    ) -> Result<String, VmError> {
        let code = code.trim();
        if code.is_empty() {
            return Ok(String::new());
        }
        if let Some(value) = self.eval_template_literal_or_variable(parent_chunk, locals, code) {
            return Ok(value.to_string());
        }
        let span = self.template_span(template, body_start);
        let chunk = self.compile_template_fragment(&span, code, false, ip)?;
        let mut vm = self.template_child_vm();
        vm.globals = self.capture_template_globals_from(parent_chunk, locals);
        match vm.execute_chunk(chunk.as_ref(), None, None, None)? {
            ExecSignal::Return(value) => Ok(value.to_string()),
            ExecSignal::Exit(value) => Ok(value.to_string()),
            ExecSignal::Throw(value) => Err(vm.throw_error(ip, value)),
            ExecSignal::Done => Ok(std::mem::take(&mut vm.output)),
        }
    }

    /// Executes a `${...}$` script template.
    ///
    /// Script blocks return text written by `print` or `println`, unlike expression templates that
    /// return their final value, so this path reads the child VM's output buffer directly.
    fn eval_template_script(
        &mut self,
        chunk: &Chunk,
        locals: Option<&LocalScope>,
        template: &str,
        body_start: usize,
        code: &str,
        ip: usize,
    ) -> Result<String, VmError> {
        let span = self.template_span(template, body_start);
        let compiled = self.compile_template_fragment(&span, code, true, ip)?;
        let mut vm = self.template_child_vm();
        vm.globals = self.capture_template_globals_from(chunk, locals);
        match vm.execute_chunk(compiled.as_ref(), None, None, None)? {
            ExecSignal::Return(value) => Ok(value.to_string()),
            ExecSignal::Exit(value) => Ok(value.to_string()),
            ExecSignal::Throw(value) => Err(vm.throw_error(ip, value)),
            ExecSignal::Done => Ok(std::mem::take(&mut vm.output)),
        }
    }

    /// Compiles a template fragment and caches its bytecode.
    ///
    /// Caching only the template file would still reparse complex expressions and script blocks on
    /// every request. Keying compiled fragments by source location and content keeps those short-lived
    /// allocations off the request path.
    fn compile_template_fragment(
        &self,
        span: &SourceSpan,
        code: &str,
        is_script: bool,
        ip: usize,
    ) -> Result<Rc<Chunk>, VmError> {
        let key = TemplateFragmentCacheKey {
            file: span.file.clone(),
            line: span.line,
            column: span.column,
            is_script,
            code: code.to_string(),
        };
        if key.code.len() <= TEMPLATE_FRAGMENT_CACHE_MAX_CODE_BYTES {
            if let Some(chunk) = cached_template_fragment(&key) {
                return Ok(chunk);
            }
        }
        let chunk = Rc::new(self.compile_template_fragment_uncached(span, code, is_script, ip)?);
        store_template_fragment(key, chunk.clone());
        Ok(chunk)
    }

    /// Compiles the template fragment and does not write to the cache.
    fn compile_template_fragment_uncached(
        &self,
        span: &SourceSpan,
        code: &str,
        is_script: bool,
        ip: usize,
    ) -> Result<Chunk, VmError> {
        let source = Self::source_with_origin(code, span);
        let mut statements = self
            .parse_template_source(&span.file, &source, ip)
            .map_err(|message| self.error(ip, message))?;
        if let [Statement::Expr(expr)] = statements.as_slice() {
            statements = vec![Statement::Return(expr.clone())];
        }
        let kind = if is_script {
            "template script"
        } else {
            "template expression"
        };
        Compiler::with_source_file(span.file.clone(), Self::template_base_dir(&span.file))
            .compile(&statements)
            .map_err(|err| self.error(ip, format!("{} compilation failed: {}", kind, err)))
    }

    /// Parses template temporary code.
    fn parse_template_source(
        &self,
        file: &str,
        source: &str,
        _ip: usize,
    ) -> Result<Vec<Statement>, String> {
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file, source, tokens);
        parser.parse().map_err(|err| err.to_string())
    }

    /// Creates a child VM using currently visible variables.
    fn template_child_vm(&self) -> Vm {
        Vm {
            globals: HashMap::new(),
            global_constants: self.global_constants.clone(),
            output: String::new(),
            project_root: self.project_root.clone(),
            source_stack: self.source_stack.clone(),
            current_span: None,
            current_function: None,
            instance_chunks: self.instance_chunks.clone(),
            class_chunks: self.class_chunks.clone(),
            global_function_chunks: self.global_function_chunks.clone(),
            #[cfg(feature = "extensions")]
            extension_manager: self.extension_manager.clone(),
            #[cfg(feature = "extensions")]
            disposed_extension_objects: self.disposed_extension_objects.clone(),
            web_response: self.web_response.clone(),
            exit_value: None,
            include_once_files: self.include_once_files.clone(),
            execution_context_depth: self.execution_context_depth,
            net_tcp_callbacks: HashMap::new(),
            net_tcp_clients: HashMap::new(),
            net_udp_callbacks: HashMap::new(),
            net_ws_callbacks: HashMap::new(),
            net_ws_client_callbacks: HashMap::new(),
            net_ws_sockets: HashMap::new(),
            timer_inbox: None,
            timer_callbacks: HashMap::new(),
            task_inbox: None,
            task_callbacks: HashMap::new(),
            next_task_callback_id: 1,
        }
    }

    /// Collects global and local variables visible when the template is executed.
    fn capture_template_globals_from(
        &self,
        chunk: &Chunk,
        locals: Option<&LocalScope>,
    ) -> HashMap<String, Value> {
        let mut globals = self.globals.clone();
        if let Some(locals) = locals {
            for index in 0..locals.len() {
                let Some(cell) = locals.get(index).and_then(Option::as_ref) else {
                    continue;
                };
                let Some(value) = cell.borrow().clone() else {
                    continue;
                };
                if let Some(name) = chunk.symbols.name(index as SymbolId) {
                    globals.insert(name.to_string(), value);
                }
            }
        }
        globals
    }

    /// Directly evaluates constants or variables in the template.
    fn eval_template_literal_or_variable(
        &self,
        chunk: &Chunk,
        locals: Option<&LocalScope>,
        code: &str,
    ) -> Option<Value> {
        match code {
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            "null" => return Some(Value::Null),
            "empty" => return Some(Value::Empty),
            _ => {}
        }
        if let Ok(value) = code.parse::<i64>() {
            return Some(Value::Int(value));
        }
        if code.contains('.') {
            if let Ok(value) = code.parse::<f64>() {
                return Some(Value::Float(value));
            }
        }
        if let Some(value) = Self::template_quoted_literal(code) {
            return Some(Value::Str(value));
        }
        if !Self::is_template_identifier(code) {
            return None;
        }
        if let Some(locals) = locals {
            for index in 0..locals.len() {
                if chunk.symbols.name(index as SymbolId) == Some(code) {
                    if let Some(cell) = locals.get(index).and_then(Option::as_ref) {
                        return cell.borrow().clone().or(Some(Value::Empty));
                    }
                }
            }
        }
        self.globals.get(code).cloned().or(Some(Value::Empty))
    }

    /// Reads a simple quoted string constant.
    fn template_quoted_literal(code: &str) -> Option<String> {
        let mut chars = code.chars();
        let quote = chars.next()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        if !code.ends_with(quote) || code.len() < 2 {
            return None;
        }
        let body = &code[quote.len_utf8()..code.len() - quote.len_utf8()];
        let mut result = String::with_capacity(body.len());
        let mut escaped = false;
        for ch in body.chars() {
            if escaped {
                match ch {
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    '\\' => result.push('\\'),
                    '\'' => result.push('\''),
                    '"' => result.push('"'),
                    other => result.push(other),
                }
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else {
                result.push(ch);
            }
        }
        Some(result)
    }

    /// Determines whether the template fast path variable name conforms to the BT identifier specification.
    fn is_template_identifier(code: &str) -> bool {
        let mut chars = code.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
            return false;
        }
        chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
    }

    /// Converts the real source code position based on the template content offset.
    fn template_span(&self, template: &str, offset: usize) -> SourceSpan {
        let Some(base) = &self.current_span else {
            return SourceSpan {
                file: "<template>".to_string(),
                line: 1,
                column: 1,
            };
        };
        let mut line = base.line;
        let mut column = base.column + 1;
        for ch in template[..offset].chars() {
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        SourceSpan {
            file: base.file.clone(),
            line,
            column,
        }
    }

    /// Constructs temporary code with true row and column offsets.
    fn source_with_origin(code: &str, span: &SourceSpan) -> String {
        let mut source = String::with_capacity(code.len() + span.line + span.column);
        for _ in 1..span.line {
            source.push('\n');
        }
        for _ in 1..span.column {
            source.push(' ');
        }
        source.push_str(code);
        source
    }

    /// Reads the template's parent directory so its includes can keep using relative paths.
    fn template_base_dir(file: &str) -> std::path::PathBuf {
        Path::new(file)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    }

    /// Performs binary operations.
    fn eval_binary(op: &TokenKind, left: &Value, right: &Value) -> Result<Value, String> {
        match op {
            TokenKind::Plus => left.add(right),
            TokenKind::Minus => left.sub(right),
            TokenKind::Multiply => left.mul(right),
            TokenKind::Divide => left.div(right),
            TokenKind::Modulo => left.modulo(right),
            TokenKind::Equal => Ok(left.equal(right)),
            TokenKind::NotEqual => Ok(left.not_equal(right)),
            TokenKind::StrictEqual => Ok(left.strict_equal(right)),
            TokenKind::StrictNotEqual => Ok(left.strict_not_equal(right)),
            TokenKind::Less => left.compare_number(right, "<"),
            TokenKind::LessEqual => left.compare_number(right, "<="),
            TokenKind::Greater => left.compare_number(right, ">"),
            TokenKind::GreaterEqual => left.compare_number(right, ">="),
            TokenKind::BitAnd => left.bitwise(right, "&"),
            TokenKind::BitOr => left.bitwise(right, "|"),
            TokenKind::Xor => left.bitwise(right, "^"),
            TokenKind::ShiftLeft => left.bitwise(right, "<<"),
            TokenKind::ShiftRight => left.bitwise(right, ">>"),
            TokenKind::And => Ok(if left.is_truthy() {
                right.clone()
            } else {
                left.clone()
            }),
            TokenKind::Or => Ok(if left.is_truthy() {
                left.clone()
            } else {
                right.clone()
            }),
            TokenKind::Coalesce => Ok(if matches!(left, Value::Null | Value::Empty) {
                right.clone()
            } else {
                left.clone()
            }),
            _ => Err(format!(
                "The VM does not support the binary operator {:?}",
                op
            )),
        }
    }

    /// Reads an array, object, class, or string property.
    ///
    /// Class instances preserve member visibility: external access sees only `pub` members, while
    /// access through `this` is internal and may read private fields and methods.
    fn get_property(
        &self,
        object: &Value,
        key: &Value,
        allow_private: bool,
    ) -> Result<Value, String> {
        // Attribute key is converted only once during the entire read. Prototype method invocation is a popular path in scripting languages.
        // Repeating `to_string()` will create short-lived strings in each `obj.xxx()`, `arr.len()`.
        let key_text = match key {
            Value::Str(value) => value.clone(),
            Value::Int(index) if *index >= 0 => index.to_string(),
            other => other.to_string(),
        };
        match object {
            Value::Array(values) => Ok(if let Ok(index) = key_text.parse::<usize>() {
                values.borrow().get(index).cloned().unwrap_or(Value::Empty)
            } else if Self::is_array_method(&key_text) {
                Value::NativeMethod {
                    receiver: Box::new(object.clone()),
                    name: key_text,
                    allow_private,
                }
            } else {
                Value::Empty
            }),
            Value::Object(values) => Ok(values
                .borrow()
                .get(&key_text)
                .map(|value| {
                    if matches!(
                        value,
                        Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _)
                    ) {
                        Value::NativeMethod {
                            receiver: Box::new(object.clone()),
                            name: key_text.clone(),
                            allow_private,
                        }
                    } else {
                        value.clone()
                    }
                })
                .unwrap_or_else(|| {
                    if Self::is_object_method(&key_text) {
                        Value::NativeMethod {
                            receiver: Box::new(object.clone()),
                            name: key_text,
                            allow_private,
                        }
                    } else {
                        Value::Empty
                    }
                })),
            Value::Instance(instance) => {
                Self::get_instance_member(object, instance, &key_text, allow_private)
            }
            Value::Class(class_name, values) => {
                let Some(member) = values.get(&key_text) else {
                    if key_text == "new" {
                        return Ok(Value::NativeMethod {
                            receiver: Box::new(object.clone()),
                            name: key_text,
                            allow_private,
                        });
                    }
                    return Ok(Value::Empty);
                };
                if !member.is_public && key_text != "new" {
                    let kind = if matches!(
                        member.value,
                        Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _)
                    ) {
                        "method"
                    } else {
                        "member"
                    };
                    return Err(format!(
                        "{} `{}` is a private {} of class `{}` and can only be accessed within the class",
                        kind, key_text, class_name, kind
                    ));
                }
                Ok({
                    if matches!(
                        member.value,
                        Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _)
                    ) {
                        Value::NativeMethod {
                            receiver: Box::new(object.clone()),
                            name: key_text,
                            allow_private,
                        }
                    } else {
                        member.value.clone()
                    }
                })
            }
            Value::Str(_) => Ok(if Self::is_string_method(&key_text) {
                Value::NativeMethod {
                    receiver: Box::new(object.clone()),
                    name: key_text,
                    allow_private,
                }
            } else {
                Value::Empty
            }),
            Value::Bytes(value) => Ok(if let Ok(index) = key_text.parse::<usize>() {
                value
                    .byte_at(index)
                    .map(|byte| Value::Int(byte as i64))
                    .unwrap_or(Value::Empty)
            } else if BtBytes::is_method(&key_text) {
                Value::NativeMethod {
                    receiver: Box::new(object.clone()),
                    name: key_text,
                    allow_private,
                }
            } else {
                Value::Empty
            }),
            #[cfg(feature = "ffi")]
            Value::Ffi(value) => {
                if (value.is_library() || value.is_buffer()) && self.is_web_request() {
                    return Err(
                        "FFI resource property cannot be accessed in the context of a web request"
                            .to_string(),
                    );
                }
                Ok(if value.has_method(&key_text)? {
                    Value::NativeMethod {
                        receiver: Box::new(object.clone()),
                        name: key_text,
                        allow_private,
                    }
                } else {
                    Value::Empty
                })
            }
            Value::Int(_) | Value::Float(_) => Ok(if Self::is_number_method(&key_text) {
                Value::NativeMethod {
                    receiver: Box::new(object.clone()),
                    name: key_text,
                    allow_private,
                }
            } else {
                Value::Empty
            }),
            Value::Bt(value) => Ok(value.get_property(&key_text).unwrap_or_else(|| {
                if BtRuntime::is_method(&key_text) {
                    Value::NativeMethod {
                        receiver: Box::new(object.clone()),
                        name: key_text,
                        allow_private,
                    }
                } else {
                    Value::Empty
                }
            })),
            Value::Math(value) => Ok(value.get_property(&key_text).unwrap_or_else(|| {
                if Self::is_math_method(&key_text) {
                    Value::NativeMethod {
                        receiver: Box::new(object.clone()),
                        name: key_text,
                        allow_private,
                    }
                } else {
                    Value::Empty
                }
            })),
            #[cfg(feature = "extensions")]
            Value::ExtObject(object) => Ok(self
                .extension_manager
                .as_deref()
                .filter(|manager| manager.has_method(object, &key_text))
                .map(|_| Value::NativeMethod {
                    receiver: Box::new(Value::ExtObject(object.clone())),
                    name: key_text,
                    allow_private,
                })
                .unwrap_or(Value::Empty)),
            Value::NativeFunction(_)
            | Value::Regex(_, _, _)
            | Value::Date(_)
            | Value::Base64(_)
            | Value::Fs(_)
            | Value::Html(_)
            | Value::Crypto(_)
            | Value::Url(_)
            | Value::Path(_)
            | Value::Md5(_)
            | Value::Modbus(_)
            | Value::Mysql(_)
            | Value::MysqlTransaction(_)
            | Value::Net(_)
            | Value::Process(_)
            | Value::Reqwest(_)
            | Value::Device(_) => Ok(Value::NativeMethod {
                receiver: Box::new(object.clone()),
                name: key_text,
                allow_private,
            }),
            Value::Task(_) => Ok(
                if matches!(key_text.as_str(), "await" | "done" | "result" | "on_done") {
                    Value::NativeMethod {
                        receiver: Box::new(object.clone()),
                        name: key_text,
                        allow_private,
                    }
                } else {
                    Value::Empty
                },
            ),
            Value::Timer(_) => Ok(if key_text == "cancel" {
                Value::NativeMethod {
                    receiver: Box::new(object.clone()),
                    name: key_text,
                    allow_private,
                }
            } else {
                Value::Empty
            }),
            Value::NetWebServer(value) => Ok(Self::get_net_object_property(
                object,
                key_text,
                value.kind(),
                value.addr(),
                &["close"],
                allow_private,
            )),
            Value::NetTcpServer(value) => Ok(Self::get_net_object_property(
                object,
                key_text,
                value.kind(),
                value.addr(),
                &["close"],
                allow_private,
            )),
            Value::NetTcpClient(value) => Ok(Self::get_net_object_property(
                object,
                key_text,
                value.kind(),
                value.addr(),
                &["write", "send", "read", "read_bytes", "close"],
                allow_private,
            )),
            Value::NetUdpSocket(value) => Ok(Self::get_net_object_property(
                object,
                key_text,
                value.kind(),
                value.addr(),
                &["send", "close"],
                allow_private,
            )),
            Value::NetWsServer(value) => Ok(Self::get_net_object_property(
                object,
                key_text,
                value.kind(),
                value.addr(),
                &["close"],
                allow_private,
            )),
            Value::NetWsSocket(value) => Ok(Self::get_net_object_property(
                object,
                key_text,
                value.kind(),
                value.addr(),
                &[
                    "send",
                    "write",
                    "close",
                    "on_message",
                    "on_close",
                    "on_error",
                ],
                allow_private,
            )),
            _ => Ok(Value::Empty),
        }
    }

    /// Determines whether a name is an array prototype method.
    ///
    /// Array supports both numeric subscripts and prototype methods. Missing fields must return `empty`, otherwise `if !arr.xxx`
    /// would be polluted by placeholder values. Callable methods are created only for whitelisted names.
    fn is_array_method(name: &str) -> bool {
        matches!(
            name,
            "len"
                | "to_string"
                | "join"
                | "push"
                | "pop"
                | "first"
                | "last"
                | "at"
                | "shift"
                | "unshift"
                | "reverse"
                | "slice"
                | "concat"
                | "contains"
                | "index_of"
                | "last_index_of"
                | "keys"
                | "values"
                | "clone"
                | "entries"
                | "delete"
                | "clear"
                | "is_empty"
                | "insert"
                | "remove_at"
                | "unique"
                | "chunk"
                | "each"
                | "sort"
                | "splice"
                | "find"
                | "find_index"
                | "find_last"
                | "find_last_index"
                | "every"
                | "some"
                | "map"
                | "filter"
                | "reduce"
                | "reduce_right"
                | "fill"
                | "flat"
                | "flat_map"
        )
    }

    /// Determines whether the name is an object prototype method.
    ///
    /// Ordinary objects allow arbitrary string keys; therefore, missing fields cannot be turned into methods by default and must only be whitelisted method names.
    /// Returns `NativeMethod`, making `obj.missing` and `!obj.missing` consistent with JS style.
    fn is_object_method(name: &str) -> bool {
        matches!(
            name,
            "len"
                | "to_string"
                | "keys"
                | "values"
                | "entries"
                | "reverse"
                | "clone"
                | "concat"
                | "delete"
                | "has_key"
                | "get"
                | "is_empty"
                | "from_entries"
                | "filter"
                | "every"
                | "some"
                | "find"
                | "find_key"
                | "update"
                | "pick"
                | "omit"
                | "clear"
                | "each"
                | "map"
        )
    }

    /// Determines whether a name is a string prototype method.
    ///
    /// The string prototype uses a whitelist, as arrays and objects do, so `text.missing` is not
    /// mistaken for a callable method and property-existence checks remain consistent.
    fn is_string_method(name: &str) -> bool {
        matches!(
            name,
            "len"
                | "trim"
                | "trim_start"
                | "trim_end"
                | "char_at"
                | "char_code_at"
                | "parse_json"
                | "parse_radix_int"
                | "parse_radix_str"
                | "concat"
                | "ends_with"
                | "contains"
                | "index_of"
                | "last_index_of"
                | "repeat"
                | "replace"
                | "replace_all"
                | "search"
                | "match"
                | "slice"
                | "split"
                | "starts_with"
                | "substr"
                | "to_lowercase"
                | "to_uppercase"
                | "to_number"
                | "to_string"
                | "pad_start"
                | "pad_end"
        )
    }

    /// Determines whether a name is a number prototype method.
    ///
    /// The number prototype exposes only formatting and conversion functions. Unknown properties
    /// return `empty` instead of allocating a useless `NativeMethod` object.
    fn is_number_method(name: &str) -> bool {
        matches!(
            name,
            "len"
                | "to_number"
                | "to_string"
                | "to_fixed"
                | "to_radix"
                | "to_exponential"
                | "to_char"
                | "is_int"
                | "is_float"
                | "is_finite"
        )
    }

    /// Determines whether the name is a Math static method.
    fn is_math_method(name: &str) -> bool {
        matches!(
            name,
            "random"
                | "abs"
                | "pow"
                | "sqrt"
                | "cbrt"
                | "hypot"
                | "exp"
                | "exp2"
                | "expm1"
                | "ln"
                | "log"
                | "log10"
                | "log2"
                | "log1p"
                | "sin"
                | "cos"
                | "tan"
                | "asin"
                | "acos"
                | "atan"
                | "atan2"
                | "sinh"
                | "cosh"
                | "tanh"
                | "asinh"
                | "acosh"
                | "atanh"
                | "round"
                | "ceil"
                | "floor"
                | "trunc"
                | "rad"
                | "deg"
                | "sign"
                | "clamp"
                | "min"
                | "max"
        )
    }

    /// Reads network object properties.
    ///
    /// Network values are not ordinary `Object` values, but scripts still use JavaScript-style
    /// access such as `server.addr`, `server.type`, and `server.close()`. Centralizing the field and
    /// method whitelist avoids rebuilding `NativeMethod` values in the main property path.
    fn get_net_object_property(
        object: &Value,
        key: String,
        kind: &'static str,
        addr: String,
        methods: &[&str],
        allow_private: bool,
    ) -> Value {
        match key.as_str() {
            "addr" => Value::Str(addr),
            "type" => Value::Str(kind.to_string()),
            _ if methods.iter().any(|method| *method == key) => Value::NativeMethod {
                receiver: Box::new(object.clone()),
                name: key,
                allow_private,
            },
            _ => Value::Empty,
        }
    }

    /// Reads class instance members and verifies visibility.
    fn get_instance_member(
        object: &Value,
        instance: &Rc<RefCell<InstanceObject>>,
        name: &str,
        allow_private: bool,
    ) -> Result<Value, String> {
        let instance = instance.borrow();
        let Some(member) = instance.members.get(name) else {
            return Ok(Value::Empty);
        };
        if !member.is_public && !allow_private {
            let kind = if matches!(
                member.value,
                Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _)
            ) {
                "method"
            } else {
                "member"
            };
            return Err(format!(
                "{} `{}` is a private {} of class `{}` and can only be accessed via `this.{}` within the class",
                kind, name, instance.class_name, kind, name
            ));
        }
        if matches!(
            member.value,
            Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _)
        ) {
            Ok(Value::NativeMethod {
                receiver: Box::new(object.clone()),
                name: name.to_string(),
                allow_private,
            })
        } else {
            Ok(member.value.clone())
        }
    }

    /// Writes an array, object, or class instance property.
    fn set_property(
        object: &Value,
        key: &Value,
        value: Value,
        allow_private: bool,
    ) -> Result<(), String> {
        if value.contains_reference_to(object) {
            return Err("cannot write the object, array, class instance or function that captures it back to itself, otherwise it will form an Rc circular reference and cause the resident process to be unable to release the memory.".to_string());
        }
        // Also only performs key normalization once when writing attributes; the array integer subscript takes a direct path to avoid common problems.
        // `arr[i] = value` repeatedly allocates strings and parses back numbers in a loop.
        let key_text;
        match object {
            Value::Array(values) => {
                let index = match key {
                    Value::Int(index) if *index >= 0 => *index as usize,
                    Value::Str(text) => text
                        .parse::<usize>()
                        .map_err(|_| "array subscript must be an integer".to_string())?,
                    other => other
                        .to_string()
                        .parse::<usize>()
                        .map_err(|_| "array subscript must be an integer".to_string())?,
                };
                let mut values = values.borrow_mut();
                if index >= values.len() {
                    values.resize(index + 1, Value::Null);
                }
                values[index] = value;
                Ok(())
            }
            Value::Object(values) => {
                key_text = match key {
                    Value::Str(text) => text.clone(),
                    other => other.to_string(),
                };
                values.borrow_mut().insert(key_text, value);
                Ok(())
            }
            Value::Instance(instance) => {
                let key = match key {
                    Value::Str(text) => text.clone(),
                    other => other.to_string(),
                };
                let mut instance = instance.borrow_mut();
                match instance.members.get_mut(&key) {
                    Some(member) if member.is_public || allow_private => {
                        member.value = value;
                        Ok(())
                    }
                    Some(_) => Err(format!(
                        "member `{}` is a private member of class `{}` and can only be accessed within the class through `this.{}`",
                        key, instance.class_name, key
                    )),
                    None => {
                        instance.members.insert(
                            key,
                            ClassMember {
                                value,
                                is_public: !allow_private,
                            },
                        );
                        Ok(())
                    }
                }
            }
            Value::Bt(_) | Value::Math(_) => {
                Err("Global static object properties are read-only".to_string())
            }
            #[cfg(feature = "extensions")]
            Value::ExtObject(_) => Err("extension object properties are read-only".to_string()),
            _ => Err("Only arrays and objects can write properties".to_string()),
        }
    }

    /// Calls the function value or built-in placeholder method.
    fn call_value(
        &mut self,
        chunk: &Chunk,
        callee: &Value,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match callee {
            Value::Function(function_id) => self.call_user_function(chunk, *function_id, args, ip),
            Value::BoundFunction(function_id, owner) => {
                self.call_user_function(owner, *function_id, args, ip)
            }
            Value::Closure(function_id, owner, captures) => self.call_user_function_inner(
                owner,
                *function_id,
                args,
                None,
                Some(captures.as_ref().clone()),
                ip,
            ),
            Value::NativeFunction(name) => self.call_native_function(chunk, name, args, ip),
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(function) => self.call_extension_function(function, args, ip),
            Value::Str(name) if name == "include" => {
                Ok(args.first().cloned().unwrap_or(Value::Empty))
            }
            Value::NativeMethod {
                receiver,
                name,
                allow_private,
            } => {
                #[cfg(feature = "extensions")]
                if let Value::ExtObject(object) = receiver.as_ref() {
                    return self.call_extension_method(object, name, args, ip);
                }
                self.call_native_method(chunk, receiver, name, *allow_private, args, ip)
            }
            _ => Ok(Value::Empty),
        }
    }

    /// Calls the extended entry function.
    #[cfg(feature = "extensions")]
    fn call_extension_function(
        &self,
        function: &crate::extensions::manager::ExtensionFunctionRef,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        let manager = self
            .extension_manager
            .as_deref()
            .ok_or_else(|| self.error(ip, "extension manager not initialized"))?;
        let source_dir = self.current_source_dir_path();
        manager
            .call_function(function, args, &source_dir)
            .map_err(|message| self.error(ip, message))
    }

    /// Calls an extension object method.
    #[cfg(feature = "extensions")]
    fn call_extension_method(
        &self,
        object: &ExtObject,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        if self.is_extension_object_disposed(object) {
            return Err(self.error(
                ip,
                format!(
                    "extension object `{}` handle {} has expired",
                    object.type_name, object.object_id
                ),
            ));
        }
        let manager = self
            .extension_manager
            .as_deref()
            .ok_or_else(|| self.error(ip, "extension manager not initialized"))?;
        let source_dir = self.current_source_dir_path();
        let tracks_dispose = manager.method_needs_vm_dispose_tracking(object, name);
        if tracks_dispose {
            self.ensure_extension_dispose_capacity(object, ip)?;
        }
        let value = manager
            .call_method(object, name, args, &source_dir)
            .map_err(|message| self.error(ip, message))?;
        if tracks_dispose {
            self.mark_extension_object_disposed(object);
        }
        Ok(value)
    }

    /// Confirms that the current VM can also record a new WASM extended invalidation handle.
    #[cfg(feature = "extensions")]
    fn ensure_extension_dispose_capacity(
        &self,
        object: &ExtObject,
        ip: usize,
    ) -> Result<(), VmError> {
        let disposed = self.disposed_extension_objects.borrow();
        if disposed.len() >= DISPOSED_EXTENSION_OBJECT_LIMIT && !disposed.contains(object) {
            return Err(self.error(
                ip,
                format!(
                    "The number of disposed WASM extension object handles exceeds {}",
                    DISPOSED_EXTENSION_OBJECT_LIMIT
                ),
            ));
        }
        Ok(())
    }

    /// Determines whether an extension object handle has expired in the current VM.
    #[cfg(feature = "extensions")]
    fn is_extension_object_disposed(&self, object: &ExtObject) -> bool {
        self.disposed_extension_objects.borrow().contains(object)
    }

    /// Marks an extension object handle as invalid in the current VM.
    #[cfg(feature = "extensions")]
    fn mark_extension_object_disposed(&self, object: &ExtObject) {
        self.disposed_extension_objects
            .borrow_mut()
            .insert(object.clone());
    }

    /// Calls user functions.
    fn call_user_function(
        &mut self,
        chunk: &Chunk,
        function_id: usize,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.call_user_function_inner(chunk, function_id, args, None, None, ip)
    }

    /// Calls the user function and injects `this`.
    ///
    /// Both instance methods and class construction methods reuse ordinary function bytecodes, but write the receiver before entering the function block.
    /// Symbol in the local scope of the function, so that semantics such as `this.title` and `return this` take effect naturally.
    fn call_user_function_with_this(
        &mut self,
        chunk: &Chunk,
        function_id: usize,
        args: Vec<Value>,
        this_value: Value,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.call_user_function_inner(chunk, function_id, args, Some(this_value), None, ip)
    }

    /// Captures the outer local variable with the same name according to the sub-function symbol slot.
    fn capture_function_locals(
        parent_chunk: &Chunk,
        parent_locals: &LocalScope,
        function_chunk: &Chunk,
    ) -> LocalScope {
        let mut captures = vec![None; function_chunk.symbols.len()];
        for slot in 0..captures.len() {
            if function_chunk.is_local(slot as SymbolId) {
                continue;
            }
            let Some(name) = function_chunk.symbols.name(slot as SymbolId) else {
                continue;
            };
            let Some(parent_slot) = parent_chunk.symbols.id(name).map(|id| id as usize) else {
                continue;
            };
            if let Some(cell) = parent_locals.get(parent_slot).and_then(Option::as_ref) {
                captures[slot] = Some(cell.clone());
            }
        }
        captures
    }

    /// Reads the shared variable slot in the current scope.
    fn local_cell(locals: &LocalScope, symbol: SymbolId) -> Option<LocalCell> {
        locals
            .get(symbol as usize)
            .and_then(Option::as_ref)
            .cloned()
    }

    /// Determines whether the current local slot has been written with a value.
    fn local_cell_has_value(locals: &LocalScope, symbol: SymbolId) -> bool {
        locals
            .get(symbol as usize)
            .and_then(Option::as_ref)
            .is_some_and(|cell| cell.borrow().is_some())
    }

    /// Writes a slot in the current function's local scope.
    fn write_local_cell(locals: &mut LocalScope, symbol: SymbolId, value: Value) {
        let index = symbol as usize;
        if index >= locals.len() {
            locals.resize(index + 1, None);
        }
        let cell = locals[index]
            .get_or_insert_with(|| Rc::new(RefCell::new(None)))
            .clone();
        *cell.borrow_mut() = Some(value);
    }

    /// User function calls.
    fn call_user_function_inner(
        &mut self,
        chunk: &Chunk,
        function_id: usize,
        args: Vec<Value>,
        this_value: Option<Value>,
        captures: Option<LocalScope>,
        ip: usize,
    ) -> Result<Value, VmError> {
        let function = chunk.functions.get(function_id).ok_or_else(|| VmError {
            ip,
            message: format!("function number {} does not exist", function_id),
            span: self.current_span.clone(),
            function: self.current_function.clone(),
            throw_value: None,
        })?;
        // Function locals use a dense slot table addressed directly by symbol ID.
        //
        // Symbol IDs are dense integers starting at zero, so `Vec<Option<Value>>` turns each local
        // lookup into a bounds check plus an array access. `None` means the current local
        // scope has no value, preserving the "fall back to globals" semantics.
        let mut locals = captures.unwrap_or_else(|| vec![None; function.chunk.symbols.len()]);
        if locals.len() < function.chunk.symbols.len() {
            locals.resize(function.chunk.symbols.len(), None);
        }
        for index in 0..function.chunk.local_symbols.len().min(locals.len()) {
            if function.chunk.local_symbols[index] {
                locals[index] = None;
            }
        }
        for (index, param) in function.params.iter().enumerate() {
            let value = args
                .get(index)
                .cloned()
                .or_else(|| param.default.as_ref().map(Value::clone_mutable_literal))
                .unwrap_or(Value::Empty);
            Self::write_local_cell(&mut locals, param.symbol, value);
        }
        if let Some(this_value) = this_value {
            if let Some(symbol) = function.chunk.symbols.id("this") {
                Self::write_local_cell(&mut locals, symbol, this_value);
            }
        }
        let function_name = if function.name.is_empty() {
            None
        } else {
            Some(function.name.clone())
        };
        match self.execute_chunk(&function.chunk, Some(locals), function_name, None)? {
            ExecSignal::Return(value) => Ok(value),
            ExecSignal::Exit(value) => Ok(value),
            ExecSignal::Throw(value) => Err(self.throw_error(ip, value)),
            ExecSignal::Done => Ok(Value::Empty),
        }
    }

    /// Calls the class constructor and returns the instance.
    ///
    /// Any class method in BT can become a construction entry through `Class::method()`; the instance will first copy the class fields and methods,
    /// Then executes the corresponding method with the instance as `this`. The return value is used when the constructor explicitly returns an object, otherwise a new instance is returned.
    fn call_class_constructor(
        &mut self,
        chunk: &Chunk,
        owner: Option<Rc<Chunk>>,
        class_name: &str,
        members: &Rc<IndexMap<String, ClassMember>>,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        let mut instance = IndexMap::new();
        for (key, member) in members.iter() {
            instance.insert(key.clone(), member.clone());
        }
        let instance_ref = Rc::new(RefCell::new(InstanceObject {
            class_name: class_name.to_string(),
            members: instance,
        }));
        let instance_id = Rc::as_ptr(&instance_ref) as usize;
        let owner = owner.unwrap_or_else(|| Rc::new(chunk.clone()));
        self.instance_chunks.insert(instance_id, owner);
        let instance = Value::Instance(instance_ref);
        let Some(member) = members.get(name) else {
            return Ok(instance);
        };
        let function_id = match &member.value {
            Value::Function(function_id) => *function_id,
            Value::BoundFunction(function_id, _) => *function_id,
            Value::Closure(function_id, _, _) => *function_id,
            _ => return Ok(instance),
        };
        let returned =
            self.call_user_function_with_this(chunk, function_id, args, instance.clone(), ip)?;
        if matches!(returned, Value::Empty | Value::Null) {
            Ok(instance)
        } else {
            Ok(returned)
        }
    }

    /// Converts a traversable value to a unified iteration state.
    ///
    /// Arrays use numeric indexes, objects use string keys, and strings are traversed by character.
    /// Integers produce lazy count iterators. Other values produce an empty iterator, preserving the
    /// language's permissive collection traversal semantics.
    fn make_iterator(value: &Value) -> Result<IterState, String> {
        match value {
            Value::Int(count) => Self::count_iterator_from_i64(*count),
            Value::Array(values) => Ok(IterState::Items {
                items: values
                    .borrow()
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (Value::Int(index as i64), value.clone()))
                    .collect(),
                index: 0,
            }),
            Value::Object(values) => Ok(IterState::Items {
                items: values
                    .borrow()
                    .iter()
                    .map(|(key, value)| (Value::Str(key.clone()), value.clone()))
                    .collect(),
                index: 0,
            }),
            Value::Str(value) => Ok(IterState::Items {
                items: value
                    .chars()
                    .enumerate()
                    .map(|(index, ch)| (Value::Int(index as i64), Value::Str(ch.to_string())))
                    .collect(),
                index: 0,
            }),
            Value::Bytes(value) => Ok(IterState::Bytes {
                data: value.clone(),
                index: 0,
            }),
            _ => Ok(IterState::Items {
                items: Vec::new(),
                index: 0,
            }),
        }
    }

    /// Creates count iterators according to the strict rules of `for count {}` and `for i in count step n {}`.
    fn make_count_iterator(value: &Value, step: Option<&Value>) -> Result<IterState, String> {
        let count = Self::expect_for_integer(value, "for count")?;
        let step = step
            .map(|value| Self::expect_for_integer(value, "for count step"))
            .transpose()?
            .unwrap_or(1);
        Self::count_iterator_from_i64_with_step(count, step)
    }

    /// Creates a left-closed, right-open count iterator from a non-negative integer.
    fn count_iterator_from_i64(count: i64) -> Result<IterState, String> {
        Self::count_iterator_from_i64_with_step(count, 1)
    }

    /// Creates a count iterator from a non-negative count and positive step.
    fn count_iterator_from_i64_with_step(count: i64, step: i64) -> Result<IterState, String> {
        if count < 0 {
            return Err("for count must be a non-negative integer".to_string());
        }
        if step <= 0 {
            return Err("for count step must be an integer greater than 0".to_string());
        }
        Ok(IterState::Count {
            index: 0,
            count,
            current: 0,
            step,
        })
    }

    /// Creates a lazy range iterator.
    fn make_range_iterator(
        start: &Value,
        end: Option<&Value>,
        step: &Value,
    ) -> Result<IterState, String> {
        let start = Self::expect_for_integer(start, "for range start")?;
        let end = end
            .map(|value| Self::expect_for_integer(value, "for range end"))
            .transpose()?;
        let step = Self::expect_for_integer(step, "for range step")?;
        if step <= 0 {
            return Err("for range step must be an integer greater than 0".to_string());
        }
        let direction = if end.is_some_and(|end| start > end) {
            -1
        } else {
            1
        };
        Ok(IterState::Range(RangeState {
            current: start,
            end,
            step,
            direction,
            finished: false,
            index: 0,
        }))
    }

    /// Reads integer parameters according to the for loop rules.
    fn expect_for_integer(value: &Value, context: &str) -> Result<i64, String> {
        match value {
            Value::Int(value) => Ok(*value),
            other => Err(format!(
                "{} must be an integer, got {}",
                context,
                other.type_name()
            )),
        }
    }

    /// Gets the next set of key/values from the iterator.
    fn next_iterator_item(state: &mut IterState) -> Result<Option<(Value, Value)>, String> {
        match state {
            IterState::Items { items, index } => {
                if *index >= items.len() {
                    Ok(None)
                } else {
                    let item = items[*index].clone();
                    *index += 1;
                    Ok(Some(item))
                }
            }
            IterState::Count {
                index,
                count,
                current,
                step,
            } => {
                if *index >= *count {
                    Ok(None)
                } else {
                    let value = *current;
                    *index += 1;
                    if *index < *count {
                        *current = current
                            .checked_add(*step)
                            .ok_or_else(|| "for count step overflow".to_string())?;
                    }
                    Ok(Some((Value::Int(value), Value::Int(value))))
                }
            }
            IterState::Range(range) => Self::next_range_item(range),
            IterState::Bytes { data, index } => {
                let Some(byte) = data.byte_at(*index) else {
                    return Ok(None);
                };
                let key = *index as i64;
                *index = index.saturating_add(1);
                Ok(Some((Value::Int(key), Value::Int(byte as i64))))
            }
        }
    }

    /// Computes the next item from a lazy range.
    fn next_range_item(range: &mut RangeState) -> Result<Option<(Value, Value)>, String> {
        if range.finished {
            return Ok(None);
        }
        let value = range.current;
        if let Some(end) = range.end {
            if range.direction > 0 {
                if value > end {
                    range.finished = true;
                    return Ok(None);
                }
                let remaining = (end as i128) - (value as i128);
                if value >= end || (range.step as i128) > remaining {
                    range.finished = true;
                } else {
                    range.current += range.step;
                }
            } else {
                if value < end {
                    range.finished = true;
                    return Ok(None);
                }
                let remaining = (value as i128) - (end as i128);
                if value <= end || (range.step as i128) > remaining {
                    range.finished = true;
                } else {
                    range.current -= range.step;
                }
            }
        } else {
            range.current = range
                .current
                .checked_add(range.step)
                .ok_or_else(|| "for range step overflow".to_string())?;
        }
        let key = range.index;
        range.index = range
            .index
            .checked_add(1)
            .ok_or_else(|| "for range index overflow".to_string())?;
        Ok(Some((Value::Int(key), Value::Int(value))))
    }

    /// Writes a variable symbol according to ordinary assignment rules.
    ///
    /// Destructuring uses the same scope path as `name = value`: functions write to local slots first,
    /// while top-level assignments write to globals and record the owner chunk for function values.
    fn store_assignment_symbol(
        &mut self,
        chunk: &Chunk,
        chunk_owner: &mut Option<Rc<Chunk>>,
        locals: Option<&mut LocalScope>,
        symbol: SymbolId,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        if let Some(locals) = locals {
            if chunk.is_local(symbol) {
                Self::write_local_cell(locals, symbol, value);
            } else if let Some(cell) = Self::local_cell(locals, symbol) {
                *cell.borrow_mut() = Some(value);
            } else {
                let name = Self::symbol_name(chunk, symbol, ip)?.to_string();
                if self.global_constants.contains(&name) || Self::is_native_constant_name(&name) {
                    return Err(self.error(ip, format!("constant `{}` cannot be reassigned", name)));
                }
                self.globals.insert(name, value);
            }
        } else {
            self.store_global_assignment_symbol(chunk, chunk_owner, symbol, value, ip)?;
        }
        Ok(())
    }

    /// Writes variable symbols according to constant definition rules.
    ///
    /// Top-level constants are written into the global constant collection; constants within functions are written into the current function's own local slot and checked before writing.
    /// Whether there is a global variable or constant with the same name. This function only serves `StoreConst`, ordinary variable assignment will not enter here.
    fn store_constant_symbol(
        &mut self,
        chunk: &Chunk,
        chunk_owner: &mut Option<Rc<Chunk>>,
        locals: Option<&mut LocalScope>,
        symbol: SymbolId,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        let name = Self::symbol_name(chunk, symbol, ip)?.to_string();
        if let Some(locals) = locals {
            if self.global_constants.contains(&name)
                || Self::is_native_constant_name(&name)
                || self.globals.contains_key(&name)
            {
                return Err(self.error(
                    ip,
                    format!("constant `{}` has been defined in the global scope", name),
                ));
            }
            if chunk.is_local(symbol) {
                if Self::local_cell_has_value(locals, symbol) {
                    return Err(self.error(
                        ip,
                        format!("constant `{}` cannot be defined repeatedly", name),
                    ));
                }
                Self::write_local_cell(locals, symbol, value);
                return Ok(());
            }
            if let Some(cell) = Self::local_cell(locals, symbol) {
                if cell.borrow().is_some() {
                    return Err(self.error(
                        ip,
                        format!("constant `{}` cannot be defined repeatedly", name),
                    ));
                }
                *cell.borrow_mut() = Some(value);
                return Ok(());
            }
            self.store_global_constant_symbol(chunk, chunk_owner, symbol, value, ip)?;
        } else {
            self.store_global_constant_symbol(chunk, chunk_owner, symbol, value, ip)?;
        }
        Ok(())
    }

    /// Writes global variables according to top-level assignment rules.
    ///
    /// A top-level function value stores only its function ID, so its owner chunk must be recorded as
    /// well. Otherwise functions from includes or cached chunks may later resolve against the wrong chunk.
    fn store_global_assignment_symbol(
        &mut self,
        chunk: &Chunk,
        chunk_owner: &mut Option<Rc<Chunk>>,
        symbol: SymbolId,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        let name = Self::symbol_name(chunk, symbol, ip)?.to_string();
        if self.global_constants.contains(&name) || Self::is_native_constant_name(&name) {
            return Err(self.error(ip, format!("constant `{}` cannot be reassigned", name)));
        }
        if matches!(value, Value::Function(_)) {
            let owner = chunk_owner
                .get_or_insert_with(|| Rc::new(chunk.clone()))
                .clone();
            self.global_function_chunks.insert(name.clone(), owner);
        } else {
            self.global_function_chunks.remove(&name);
        }
        self.globals.insert(name, value);
        Ok(())
    }

    /// Writes global variables according to top-level constant definition rules.
    fn store_global_constant_symbol(
        &mut self,
        chunk: &Chunk,
        chunk_owner: &mut Option<Rc<Chunk>>,
        symbol: SymbolId,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        let name = Self::symbol_name(chunk, symbol, ip)?.to_string();
        if self.global_constants.contains(&name)
            || Self::is_native_constant_name(&name)
            || self.globals.contains_key(&name)
        {
            return Err(self.error(
                ip,
                format!("constant `{}` cannot be defined repeatedly", name),
            ));
        }
        self.global_constants.insert(name.clone());
        if matches!(value, Value::Function(_)) {
            let owner = chunk_owner
                .get_or_insert_with(|| Rc::new(chunk.clone()))
                .clone();
            self.global_function_chunks.insert(name.clone(), owner);
        } else {
            self.global_function_chunks.remove(&name);
        }
        self.globals.insert(name, value);
        Ok(())
    }

    /// Performs destructuring assignment.
    ///
    /// Arrays are assigned in index order and yield `empty` past their end. Objects read fields that
    /// match the names on the left and also yield `empty` for missing fields. No implicit conversion
    /// from strings, instances, or other values is performed.
    fn destructure_assign(
        &mut self,
        chunk: &Chunk,
        chunk_owner: &mut Option<Rc<Chunk>>,
        mut locals: Option<&mut LocalScope>,
        value: &Value,
        symbols: &[SymbolId],
        constants: &[bool],
        ip: usize,
    ) -> Result<(), VmError> {
        match value {
            Value::Array(values) => {
                let values = values.borrow();
                for (index, symbol) in symbols.iter().enumerate() {
                    let item = values.get(index).cloned().unwrap_or(Value::Empty);
                    let locals = locals.as_mut().map(|scope| &mut **scope);
                    if constants.get(index).copied().unwrap_or(false) {
                        self.store_constant_symbol(chunk, chunk_owner, locals, *symbol, item, ip)?;
                    } else {
                        self.store_assignment_symbol(
                            chunk,
                            chunk_owner,
                            locals,
                            *symbol,
                            item,
                            ip,
                        )?;
                    }
                }
                Ok(())
            }
            Value::Object(values) => {
                let values = values.borrow();
                for (index, symbol) in symbols.iter().enumerate() {
                    let name = Self::symbol_name(chunk, *symbol, ip)?;
                    let item = values.get(name).cloned().unwrap_or(Value::Empty);
                    let locals = locals.as_mut().map(|scope| &mut **scope);
                    if constants.get(index).copied().unwrap_or(false) {
                        self.store_constant_symbol(chunk, chunk_owner, locals, *symbol, item, ip)?;
                    } else {
                        self.store_assignment_symbol(
                            chunk,
                            chunk_owner,
                            locals,
                            *symbol,
                            item,
                            ip,
                        )?;
                    }
                }
                Ok(())
            }
            _ => Err(self.error(
                ip,
                "The right side of the destructuring assignment must be array or object",
            )),
        }
    }

    /// Writes symbols to the current scope.
    ///
    /// The main program writes to globals. When a function or loop has a local scope, that scope is
    /// updated first. Resolving the name through the current chunk's symbol pool keeps symbol IDs
    /// from different function chunks independent.
    fn store_symbol(
        &mut self,
        chunk: &Chunk,
        locals: Option<&mut LocalScope>,
        symbol: SymbolId,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        if let Some(locals) = locals {
            if chunk.is_local(symbol) {
                Self::write_local_cell(locals, symbol, value);
            } else if let Some(cell) = Self::local_cell(locals, symbol) {
                *cell.borrow_mut() = Some(value);
            } else {
                let name = Self::symbol_name(chunk, symbol, ip)?.to_string();
                if self.global_constants.contains(&name) || Self::is_native_constant_name(&name) {
                    return Err(self.error(ip, format!("constant `{}` cannot be reassigned", name)));
                }
                self.globals.insert(name, value);
            }
        } else {
            let name = Self::symbol_name(chunk, symbol, ip)?.to_string();
            if self.global_constants.contains(&name) || Self::is_native_constant_name(&name) {
                return Err(self.error(ip, format!("constant `{}` cannot be reassigned", name)));
            }
            self.globals.insert(name, value);
        }
        Ok(())
    }

    /// Writes a value to the current scope by field name.
    ///
    /// Fields imported by `use obj` come from a runtime object. If a field already has a symbol in
    /// the current function chunk, it is written to the matching local slot; otherwise it falls back
    /// to the global environment.
    fn store_symbol_name(
        &mut self,
        chunk: &Chunk,
        locals: &mut Option<&mut LocalScope>,
        name: &str,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        Self::validate_import_variable_name(name).map_err(|message| {
            self.error(ip, format!("use import variable `{}` {}", name, message))
        })?;
        if let Some(locals) = locals.as_mut() {
            if let Some(symbol) = chunk.symbols.id(name) {
                if chunk.is_local(symbol) {
                    Self::write_local_cell(locals, symbol, value);
                    return Ok(());
                }
                if let Some(cell) = Self::local_cell(locals, symbol) {
                    *cell.borrow_mut() = Some(value);
                    return Ok(());
                }
            }
        }
        if self.global_constants.contains(name) || Self::is_native_constant_name(name) {
            return Err(self.error(ip, format!("constant `{}` cannot be reassigned", name)));
        }
        self.globals.insert(name.to_string(), value);
        Ok(())
    }

    /// Verifies that an imported field is a valid runtime variable name.
    ///
    /// `use obj` does not pass through the lexer, so this lightweight ASCII validation runs before
    /// writing to the scope. It prevents a full import from bypassing the uppercase-name rule.
    fn validate_import_variable_name(name: &str) -> Result<(), &'static str> {
        let Some(first) = name.as_bytes().first().copied() else {
            return Err("cannot be empty");
        };
        if first.is_ascii_uppercase() {
            return Err("cannot start with an uppercase letter");
        }
        if !(first.is_ascii_lowercase() || first == b'_' || first == b'$') {
            return Err("must be a lowercase English letter, _ or $");
        }
        if name
            .as_bytes()
            .get(1..)
            .unwrap_or_default()
            .iter()
            .any(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$'))
        {
            return Err("can only contain English letters, numbers, _ or $");
        }
        Ok(())
    }

    /// Performs `use` field import.
    ///
    /// Listed fields use direct symbol IDs. A full import borrows the object once and clones its fields
    /// in a batch instead of constructing a property-access chain for each one. Importing a non-object
    /// remains a no-op, preserving the language's permissive behavior.
    fn import_fields(
        &mut self,
        chunk: &Chunk,
        mut locals: Option<&mut LocalScope>,
        object: &Value,
        fields: &[SymbolId],
        ip: usize,
    ) -> Result<(), VmError> {
        let Value::Object(values) = object else {
            return Ok(());
        };
        if fields.is_empty() {
            let items = values.borrow().clone();
            for (name, value) in items {
                self.store_symbol_name(chunk, &mut locals, &name, value, ip)?;
            }
            return Ok(());
        }
        let values = values.borrow();
        for symbol in fields {
            let name = Self::symbol_name(chunk, *symbol, ip)?;
            let value = values.get(name).cloned().unwrap_or(Value::Empty);
            self.store_symbol_name(chunk, &mut locals, name, value, ip)?;
        }
        Ok(())
    }

    /// Determines whether to request JSON beautified output.
    fn is_json_pretty_arg(value: Option<&Value>) -> bool {
        matches!(value, Some(Value::Str(text)) if text == "JSON_PRETTY")
    }

    /// Format an array or object into indented JSON-style text.
    fn pretty_json(value: &Value) -> String {
        let mut output = String::new();
        Self::write_pretty_json(value, 0, &mut output);
        output
    }

    /// Writes JSON style text recursively.
    ///
    /// Serde_json is not used here in order to maintain the runtime value semantics of BT: functions, class instances, library objects, etc. are not JSON
    /// Native types can still participate in debug output as script-visible strings, while arrays and objects retain stable indentation structures.
    fn write_pretty_json(value: &Value, depth: usize, output: &mut String) {
        match value {
            Value::Null | Value::Empty => output.push_str("null"),
            Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Value::Int(value) => output.push_str(&value.to_string()),
            Value::Float(value) => output.push_str(&value.to_string()),
            Value::Str(value) => output
                .push_str(&serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())),
            Value::Array(values) => {
                let values = values.borrow();
                if values.is_empty() {
                    output.push_str("[]");
                    return;
                }
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push('\n');
                    Self::write_json_indent(depth + 1, output);
                    Self::write_pretty_json(value, depth + 1, output);
                }
                output.push('\n');
                Self::write_json_indent(depth, output);
                output.push(']');
            }
            Value::Object(values) => {
                let values = values.borrow();
                if values.is_empty() {
                    output.push_str("{}");
                    return;
                }
                output.push('{');
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push('\n');
                    Self::write_json_indent(depth + 1, output);
                    output.push_str(
                        &serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                    );
                    output.push_str(": ");
                    Self::write_pretty_json(value, depth + 1, output);
                }
                output.push('\n');
                Self::write_json_indent(depth, output);
                output.push('}');
            }
            other => output.push_str(
                &serde_json::to_string(&other.to_string()).unwrap_or_else(|_| "\"\"".to_string()),
            ),
        }
    }

    /// Writes JSON with indentation.
    fn write_json_indent(depth: usize, output: &mut String) {
        for _ in 0..depth {
            output.push_str("  ");
        }
    }

    /// Calls the built-in method of the bound receiver.
    fn call_native_method(
        &mut self,
        chunk: &Chunk,
        receiver: &Value,
        name: &str,
        allow_private: bool,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match receiver {
            Value::Str(value) => {
                if name == "replace" {
                    self.call_string_replace_method(chunk, value, args, ip)
                } else {
                    Ok(self.call_string_method(value, name, args))
                }
            }
            Value::Bytes(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Int(_) | Value::Float(_) => Ok(self.call_number_method(receiver, name, args)),
            Value::Array(values) => self.call_array_method(chunk, values, name, args, ip),
            Value::Object(values) => {
                let callable = {
                    let values = values.borrow();
                    values.get(name).cloned().filter(|value| {
                        matches!(
                            value,
                            Value::Function(_)
                                | Value::BoundFunction(_, _)
                                | Value::Closure(_, _, _)
                        )
                    })
                };
                if let Some(callable) = callable {
                    match callable {
                        Value::Function(function_id) => self.call_user_function_with_this(
                            chunk,
                            function_id,
                            args,
                            receiver.clone(),
                            ip,
                        ),
                        Value::BoundFunction(function_id, owner) => self
                            .call_user_function_with_this(
                                &owner,
                                function_id,
                                args,
                                receiver.clone(),
                                ip,
                            ),
                        Value::Closure(function_id, owner, captures) => self
                            .call_user_function_inner(
                                &owner,
                                function_id,
                                args,
                                Some(receiver.clone()),
                                Some(captures.as_ref().clone()),
                                ip,
                            ),
                        _ => Ok(Value::Null),
                    }
                } else {
                    self.call_object_method(chunk, values, name, args, ip)
                }
            }
            Value::Instance(instance) => {
                let method_chunk_owner = self.instance_owner_chunk(instance);
                let method_chunk = method_chunk_owner.as_deref().unwrap_or(chunk);
                let function_id = {
                    let instance = instance.borrow();
                    match instance.members.get(name) {
                        Some(member) if member.is_public || allow_private => match &member.value {
                            Value::Function(function_id) => Some(*function_id),
                            Value::BoundFunction(function_id, _) => Some(*function_id),
                            Value::Closure(function_id, _, _) => Some(*function_id),
                            _ => None,
                        },
                        Some(member)
                            if matches!(
                                member.value,
                                Value::Function(_)
                                    | Value::BoundFunction(_, _)
                                    | Value::Closure(_, _, _)
                            ) =>
                        {
                            return Err(self.error(
                                ip,
                                format!(
                                    "method `{}` is a private method of class `{}` and can only be called through `this.{}` within the class",
                                    name, instance.class_name, name
                                ),
                            ));
                        }
                        _ => None,
                    }
                };
                if let Some(function_id) = function_id {
                    self.call_user_function_with_this(
                        method_chunk,
                        function_id,
                        args,
                        receiver.clone(),
                        ip,
                    )
                } else {
                    Ok(Value::Null)
                }
            }
            Value::Class(class_name, members) => {
                let class_chunk_owner = self.class_owner_chunk(members);
                let class_chunk = class_chunk_owner.as_deref().unwrap_or(chunk);
                self.call_class_constructor(
                    class_chunk,
                    class_chunk_owner.clone(),
                    class_name,
                    members,
                    name,
                    args,
                    ip,
                )
            }
            Value::Regex(regex, pattern, flags) => {
                Ok(self.call_regex_method(regex, pattern, flags, name, args))
            }
            Value::NativeFunction(library) if Self::is_library_constructor(library) => {
                let receiver = self.call_native_function(chunk, library, Vec::new(), ip)?;
                self.call_native_method(chunk, &receiver, name, allow_private, args, ip)
            }
            Value::Date(date) => date
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Base64(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Fs(value) => self.call_fs_method(value, name, args, ip),
            Value::Html(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Crypto(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Url(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Path(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Bt(value) => self.call_bt_method(value, name, args, ip),
            #[cfg(feature = "ffi")]
            Value::Ffi(value) => self.call_ffi_method(value, name, args, ip),
            Value::Math(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Md5(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Modbus(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::Mysql(value) => {
                self.check_permission(Capability::Mysql, ip)?;
                value
                    .call_method(name, args)
                    .map_err(|message| self.error(ip, message))
            }
            Value::MysqlTransaction(value) => {
                self.check_permission(Capability::Mysql, ip)?;
                value
                    .call_method(name, args)
                    .map_err(|message| self.error(ip, message))
            }
            Value::Net(value) => self.call_net_method(chunk, value, name, args, ip),
            Value::NetWebServer(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::NetTcpServer(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::NetTcpClient(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::NetUdpSocket(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::NetWsServer(value) => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
            Value::NetWsSocket(value) => self.call_ws_socket_method(chunk, value, name, args, ip),
            Value::Process(value) => self.call_process_method(value, name, args, ip),
            Value::Reqwest(value) => {
                self.check_permission(Capability::Http, ip)?;
                value
                    .call_method(name, args)
                    .map_err(|message| self.error(ip, message))
            }
            Value::Device(value) => {
                self.check_permission(Capability::Device, ip)?;
                value
                    .call_method(name, args)
                    .map_err(|message| self.error(ip, message))
            }
            Value::Task(value) => self.call_task_method(chunk, value, name, args, ip),
            Value::Timer(value) => self.call_timer_method(value, name),
            _ => Ok(Value::Empty),
        }
    }

    /// Calls the timer object method.
    fn call_timer_method(&mut self, value: &BtTimer, name: &str) -> Result<Value, VmError> {
        match name {
            "cancel" => {
                let local_removed = self.timer_callbacks.remove(&value.id()).is_some();
                let runtime_removed = timer::cancel(value.id());
                Ok(Value::Bool(local_removed || runtime_removed))
            }
            _ => Ok(Value::Empty),
        }
    }

    /// Calls the background task object method.
    fn call_task_method(
        &mut self,
        chunk: &Chunk,
        value: &crate::task::BtTask,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match name {
            "done" => Ok(Value::Bool(value.done())),
            "await" => {
                if self.is_web_request() {
                    return Err(self.error(ip, "Task.await() cannot wait for a background task in the context of a web request"));
                }
                let outcome = value.wait();
                self.task_outcome_to_value(&outcome, ip)
            }
            "result" => match value.result() {
                Some(outcome) => self.task_outcome_to_value(&outcome, ip),
                None => Ok(Value::Empty),
            },
            "on_done" => self.register_task_on_done(chunk, value, args, ip),
            _ => Ok(Value::Empty),
        }
    }

    /// Register task completion callback.
    fn register_task_on_done(
        &mut self,
        chunk: &Chunk,
        value: &crate::task::BtTask,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        if self.web_response.is_some() {
            return Err(self.error(
                ip,
                "Task.on_done() cannot register a callback in the web request context",
            ));
        }
        if self.task_callbacks.len() >= TASK_CALLBACK_LIMIT {
            return Err(self.error(
                ip,
                format!(
                    "The number of task completion callbacks exceeds {}",
                    TASK_CALLBACK_LIMIT
                ),
            ));
        }
        let callback_value = args
            .first()
            .cloned()
            .ok_or_else(|| self.error(ip, "Task.on_done() requires a function parameter"))?;
        let callback = self
            .bind_callback_value(chunk, callback_value.clone())
            .ok_or_else(|| {
                self.error(
                    ip,
                    format!(
                        "Task.on_done() argument must be a BT function, got {}",
                        callback_value.type_name()
                    ),
                )
            })?;
        let id = self.allocate_task_callback_id(ip)?;
        let sender = self.ensure_task_sender();
        let subscription = value
            .subscribe(id, sender.clone())
            .map_err(|message| self.error(ip, message))?;
        self.task_callbacks.insert(
            id,
            VmTaskCallback {
                task: value.clone(),
                callback,
                subscription,
            },
        );
        if value.done() {
            let _ = sender.try_send(id);
        }
        Ok(Value::Task(value.clone()))
    }

    /// Assigns a VM local task callback number.
    fn allocate_task_callback_id(&mut self, ip: usize) -> Result<usize, VmError> {
        for _ in 0..=TASK_CALLBACK_LIMIT {
            let id = self.next_task_callback_id.max(1);
            self.next_task_callback_id = id.checked_add(1).unwrap_or(1);
            if !self.task_callbacks.contains_key(&id) {
                return Ok(id);
            }
        }
        Err(self.error(ip, "Task completion callback number has been exhausted"))
    }

    /// Calls the fs object method and parses the method parameters that receive the new path.
    fn call_fs_method(
        &self,
        value: &BtFs,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.check_permission(Capability::Fs, ip)?;
        let args = match name {
            "rename" | "move" | "copy" => {
                self.with_resolved_path_arg(args, 0, &format!("fs.{}()", name), ip)?
            }
            _ => args,
        };
        value
            .call_method(name, args)
            .map_err(|message| self.error(ip, message))
    }

    /// Calls the process object method and parses the working directory parameter.
    fn call_process_method(
        &self,
        value: &BtProcess,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.check_permission(Capability::Process, ip)?;
        if self.is_web_request() && Self::is_web_rejected_process_method(name) {
            return Err(self.error(
                ip,
                format!(
                    "process.{}() cannot perform blocking or forking process operations in the context of a web request",
                    name
                ),
            ));
        }
        let args = match name {
            "current_dir" => self.with_resolved_path_arg(args, 0, "process.current_dir()", ip)?,
            _ => args,
        };
        value
            .call_method(name, args)
            .map_err(|message| self.error(ip, message))
    }

    /// Calls FFI static object or dynamic library method.
    #[cfg(feature = "ffi")]
    fn call_ffi_method(
        &self,
        value: &BtFfiValue,
        name: &str,
        mut args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        if value.is_static() {
            return match name {
                "load" => {
                    if self.is_web_request() {
                        return Err(
                            self.error(ip, "ffi.load() cannot load native dynamic libraries in the web request context")
                        );
                    }
                    if let Some(Value::Str(path)) = args.first_mut() {
                        if bt_path::has_path_semantics(path) {
                            let resolved = bt_path::resolve_path(
                                path,
                                &self.project_root,
                                &self.current_source_dir_path(),
                            );
                            *path = bt_path::path_text(&resolved);
                        }
                    }
                    self.check_permission(Capability::Ffi, ip)?;
                    BtFfiValue::load(args).map_err(|message| self.error(ip, message))
                }
                "buffer" => {
                    if self.is_web_request() {
                        return Err(self.error(
                            ip,
                            "ffi.buffer() cannot allocate native memory in the web request context",
                        ));
                    }
                    self.check_permission(Capability::Ffi, ip)?;
                    BtFfiValue::buffer(args).map_err(|message| self.error(ip, message))
                }
                "close" => BtFfiValue::close(args).map_err(|message| self.error(ip, message)),
                _ => Ok(Value::Empty),
            };
        }
        if value.is_library() || value.is_buffer() {
            if self.is_web_request() {
                return Err(self.error(
                    ip,
                    format!("FFI dynamic library function `{}` cannot be called in the web request context", name),
                ));
            }
            self.check_permission(Capability::Ffi, ip)?;
            return value
                .call(name, args)
                .map_err(|message| self.error(ip, message));
        }
        Err(self.error(
            ip,
            format!("{} does not support calling `{}`", value.type_name(), name),
        ))
    }

    /// Calls the BT static object method and passes in the source code directory and project root required for path resolution.
    fn call_bt_method(
        &self,
        value: &BtRuntime,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        if Self::bt_method_needs_env_permission(name) {
            self.check_permission(Capability::Env, ip)?;
        }
        let source_dir = self.current_source_dir_path();
        value
            .call_method_with_paths(name, args, &source_dir, &self.project_root)
            .map_err(|message| self.error(ip, message))
    }

    /// Determines whether the BT static object method reaches the environment variable overlay.
    fn bt_method_needs_env_permission(name: &str) -> bool {
        matches!(
            name,
            "env"
                | "set_env"
                | "remove_env"
                | "has_env"
                | "envs"
                | "path_entries"
                | "add_path"
                | "remove_path"
                | "has_path"
        )
    }

    /// Calls net standard library methods and records event callbacks on the VM side.
    fn call_net_method(
        &mut self,
        chunk: &Chunk,
        value: &BtNet,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.check_permission(Capability::Net, ip)?;
        let callbacks = if name == "listen" {
            self.net_listen_callbacks(chunk, args.first())
        } else {
            None
        };
        let source_dir = self.current_source_dir_path();
        let result = value
            .call_method_with_paths(name, args, &source_dir, &self.project_root)
            .map_err(|message| self.error(ip, message))?;
        match (callbacks, &result) {
            (Some(VmNetListenCallbacks::Tcp(callbacks)), Value::NetTcpServer(server)) => {
                self.net_tcp_callbacks.insert(server.id(), callbacks);
            }
            (Some(VmNetListenCallbacks::Udp(callbacks)), Value::NetUdpSocket(socket)) => {
                self.net_udp_callbacks.insert(socket.id(), callbacks);
            }
            (Some(VmNetListenCallbacks::Ws(callbacks)), Value::NetWsServer(server)) => {
                self.net_ws_callbacks.insert(server.id(), callbacks);
            }
            _ => {}
        }
        Ok(result)
    }

    /// Parses the callback field in the `net.listen()` configuration.
    fn net_listen_callbacks(
        &self,
        chunk: &Chunk,
        config: Option<&Value>,
    ) -> Option<VmNetListenCallbacks> {
        let config = config?;
        let config_type = Self::net_object_field(config, "type")?.to_string();
        match config_type.as_str() {
            "tcp" => Some(VmNetListenCallbacks::Tcp(VmTcpCallbacks {
                binary: Self::net_object_bool(config, "binary"),
                on_connect: self.bind_net_callback(chunk, config, "on_connect"),
                on_message: self.bind_net_callback(chunk, config, "on_message"),
                on_close: self.bind_net_callback(chunk, config, "on_close"),
                on_error: self.bind_net_callback(chunk, config, "on_error"),
            })),
            "udp" => Some(VmNetListenCallbacks::Udp(VmUdpCallbacks {
                binary: Self::net_object_bool(config, "binary"),
                on_message: self.bind_net_callback(chunk, config, "on_message"),
                on_error: self.bind_net_callback(chunk, config, "on_error"),
            })),
            "ws" => Some(VmNetListenCallbacks::Ws(VmWsCallbacks {
                binary: Self::net_object_bool(config, "binary"),
                on_connect: self.bind_net_callback(chunk, config, "on_connect"),
                on_message: self.bind_net_callback(chunk, config, "on_message"),
                on_close: self.bind_net_callback(chunk, config, "on_close"),
                on_error: self.bind_net_callback(chunk, config, "on_error"),
            })),
            _ => None,
        }
    }

    /// Reads object fields and binds bare functions to the current bytecode block.
    fn bind_net_callback(&self, chunk: &Chunk, config: &Value, key: &str) -> Option<Value> {
        let value = Self::net_object_field(config, key)?;
        self.bind_callback_value(chunk, value)
    }

    /// Binds the script function value to the current bytecode block so that background events can be called later.
    fn bind_callback_value(&self, chunk: &Chunk, value: Value) -> Option<Value> {
        match value {
            Value::Function(function_id) => {
                Some(Value::BoundFunction(function_id, Rc::new(chunk.clone())))
            }
            Value::BoundFunction(_, _) | Value::Closure(_, _, _) => Some(value),
            _ => None,
        }
    }

    /// Calls the WebSocket connection method and handles client event callback registration.
    fn call_ws_socket_method(
        &mut self,
        chunk: &Chunk,
        value: &crate::net::ws::WsSocketHandle,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match name {
            "on_message" | "on_close" | "on_error" => {
                let callback = args
                    .first()
                    .cloned()
                    .and_then(|value| self.bind_callback_value(chunk, value))
                    .ok_or_else(|| {
                        self.error(ip, format!("ws.{}() requires a function argument", name))
                    })?;
                let callbacks = self.net_ws_client_callbacks.entry(value.id()).or_default();
                match name {
                    "on_message" => {
                        callbacks.binary = args.get(1).map(Value::is_truthy).unwrap_or(false);
                        callbacks.on_message = Some(callback);
                    }
                    "on_close" => callbacks.on_close = Some(callback),
                    "on_error" => callbacks.on_error = Some(callback),
                    _ => {}
                }
                Ok(Value::NetWsSocket(value.clone()))
            }
            _ => value
                .call_method(name, args)
                .map_err(|message| self.error(ip, message)),
        }
    }

    /// Reads net configuration object fields.
    fn net_object_field(value: &Value, key: &str) -> Option<Value> {
        let Value::Object(values) = value else {
            return None;
        };
        values.borrow().get(key).cloned()
    }

    /// Reads the boolean field in the net configuration object.
    fn net_object_bool(value: &Value, key: &str) -> bool {
        Self::net_object_field(value, key)
            .map(|value| value.is_truthy())
            .unwrap_or(false)
    }

    /// Converts network message parameters according to callback configuration.
    fn net_message_value(&self, data: Vec<u8>, binary: bool) -> Result<Value, VmError> {
        if binary {
            crate::libs::bytes::from_vec(data).map_err(|message| self.error(0, message))
        } else {
            Ok(Value::Str(String::from_utf8_lossy(&data).to_string()))
        }
    }

    /// Distributes network background events.
    fn dispatch_net_event(&mut self, chunk: &Chunk, event: NetEvent) -> Result<(), VmError> {
        match event {
            NetEvent::TcpConnect {
                server_id,
                client_id,
                addr,
            } => {
                self.net_tcp_clients.insert(client_id, server_id);
                let callback = self
                    .net_tcp_callbacks
                    .get(&server_id)
                    .and_then(|callbacks| callbacks.on_connect.clone());
                if let Some(callback) = callback {
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![Value::NetTcpClient(crate::net::tcp::TcpClientHandle::new(
                            client_id, addr,
                        ))],
                        0,
                    )?;
                }
            }
            NetEvent::TcpMessage {
                client_id,
                addr,
                data,
            } => {
                let callbacks = self
                    .net_tcp_clients
                    .get(&client_id)
                    .and_then(|server_id| self.net_tcp_callbacks.get(server_id))
                    .cloned();
                if let Some(callbacks) = callbacks {
                    let Some(callback) = callbacks.on_message else {
                        return Ok(());
                    };
                    let message = self.net_message_value(data, callbacks.binary)?;
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            Value::NetTcpClient(crate::net::tcp::TcpClientHandle::new(
                                client_id, addr,
                            )),
                            message,
                        ],
                        0,
                    )?;
                }
            }
            NetEvent::TcpClose { client_id, addr } => {
                let server_id = self.net_tcp_clients.remove(&client_id);
                let callback = server_id
                    .and_then(|server_id| self.net_tcp_callbacks.get(&server_id))
                    .and_then(|callbacks| callbacks.on_close.clone());
                if let Some(callback) = callback {
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![Value::NetTcpClient(crate::net::tcp::TcpClientHandle::new(
                            client_id, addr,
                        ))],
                        0,
                    )?;
                }
            }
            NetEvent::TcpError {
                server_id,
                client_id,
                message,
            } => {
                let callback = client_id
                    .and_then(|client_id| self.net_tcp_clients.get(&client_id).copied())
                    .or(server_id)
                    .and_then(|server_id| self.net_tcp_callbacks.get(&server_id))
                    .and_then(|callbacks| callbacks.on_error.clone());
                self.call_or_log_net_error(chunk, callback, message)?;
            }
            NetEvent::UdpMessage {
                socket_id,
                addr,
                data,
            } => {
                let callbacks = self.net_udp_callbacks.get(&socket_id).cloned();
                if let Some(callbacks) = callbacks {
                    let Some(callback) = callbacks.on_message else {
                        return Ok(());
                    };
                    let message = self.net_message_value(data, callbacks.binary)?;
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![message, Self::net_addr_value(&addr)],
                        0,
                    )?;
                }
            }
            NetEvent::UdpError { socket_id, message } => {
                let callback = self
                    .net_udp_callbacks
                    .get(&socket_id)
                    .and_then(|callbacks| callbacks.on_error.clone());
                self.call_or_log_net_error(chunk, callback, message)?;
            }
            NetEvent::WsConnect {
                server_id,
                socket_id,
                addr,
            } => {
                self.net_ws_sockets.insert(socket_id, server_id);
                let callback = self
                    .net_ws_callbacks
                    .get(&server_id)
                    .and_then(|callbacks| callbacks.on_connect.clone());
                if let Some(callback) = callback {
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![Value::NetWsSocket(crate::net::ws::WsSocketHandle::new(
                            socket_id, addr,
                        ))],
                        0,
                    )?;
                }
            }
            NetEvent::WsMessage {
                socket_id,
                addr,
                data,
            } => {
                let client_callbacks = self.net_ws_client_callbacks.get(&socket_id).cloned();
                if let Some(callbacks) = client_callbacks {
                    if let Some(callback) = callbacks.on_message {
                        let message = self.net_message_value(data, callbacks.binary)?;
                        self.call_callback(chunk, &callback, vec![message], 0)?;
                        return Ok(());
                    }
                }
                let callbacks = self
                    .net_ws_sockets
                    .get(&socket_id)
                    .and_then(|server_id| self.net_ws_callbacks.get(server_id))
                    .cloned();
                if let Some(callbacks) = callbacks {
                    let Some(callback) = callbacks.on_message else {
                        return Ok(());
                    };
                    let message = self.net_message_value(data, callbacks.binary)?;
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            Value::NetWsSocket(crate::net::ws::WsSocketHandle::new(
                                socket_id, addr,
                            )),
                            message,
                        ],
                        0,
                    )?;
                }
            }
            NetEvent::WsClose { socket_id, addr } => {
                if let Some(callbacks) = self.net_ws_client_callbacks.remove(&socket_id) {
                    if let Some(callback) = callbacks.on_close {
                        self.call_callback(chunk, &callback, Vec::new(), 0)?;
                    }
                    return Ok(());
                }
                let server_id = self.net_ws_sockets.remove(&socket_id);
                let callback = server_id
                    .and_then(|server_id| self.net_ws_callbacks.get(&server_id))
                    .and_then(|callbacks| callbacks.on_close.clone());
                if let Some(callback) = callback {
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![Value::NetWsSocket(crate::net::ws::WsSocketHandle::new(
                            socket_id, addr,
                        ))],
                        0,
                    )?;
                }
            }
            NetEvent::WsError {
                server_id,
                socket_id,
                message,
            } => {
                let client_callback = socket_id
                    .and_then(|socket_id| self.net_ws_client_callbacks.get(&socket_id))
                    .and_then(|callbacks| callbacks.on_error.clone());
                if client_callback.is_some() {
                    self.call_or_log_net_error(chunk, client_callback, message)?;
                    return Ok(());
                }
                let callback = socket_id
                    .and_then(|socket_id| self.net_ws_sockets.get(&socket_id).copied())
                    .or(server_id)
                    .and_then(|server_id| self.net_ws_callbacks.get(&server_id))
                    .and_then(|callbacks| callbacks.on_error.clone());
                self.call_or_log_net_error(chunk, callback, message)?;
            }
            NetEvent::Wake => {}
        }
        Ok(())
    }

    /// Converts the `host:port` address into a script-side object, and retains the original text when parsing fails.
    fn net_addr_value(addr: &str) -> Value {
        let mut values = IndexMap::new();
        match addr.parse::<std::net::SocketAddr>() {
            Ok(addr) => {
                values.insert("ip".to_string(), Value::Str(addr.ip().to_string()));
                values.insert("port".to_string(), Value::Int(addr.port() as i64));
            }
            Err(_) => {
                values.insert("ip".to_string(), Value::Str(addr.to_string()));
                values.insert("port".to_string(), Value::Null);
            }
        }
        values.insert("addr".to_string(), Value::Str(addr.to_string()));
        Value::Object(Rc::new(RefCell::new(values)))
    }

    /// Calls the network error callback; when there is no callback, it writes directly to the standard error.
    fn call_or_log_net_error(
        &mut self,
        chunk: &Chunk,
        callback: Option<Value>,
        message: String,
    ) -> Result<(), VmError> {
        if let Some(callback) = callback {
            self.call_callback(chunk, &callback, vec![Value::Str(message)], 0)?;
        } else {
            eprintln!("{}", message);
        }
        Ok(())
    }

    /// Reads the defining bytecode block to which the class instance belongs.
    fn instance_owner_chunk(&self, instance: &Rc<RefCell<InstanceObject>>) -> Option<Rc<Chunk>> {
        let instance_id = Rc::as_ptr(instance) as usize;
        self.instance_chunks.get(&instance_id).cloned()
    }

    /// Reads the defined bytecode block to which the class value belongs.
    fn class_owner_chunk(&self, members: &Rc<IndexMap<String, ClassMember>>) -> Option<Rc<Chunk>> {
        let class_id = Rc::as_ptr(members) as usize;
        self.class_chunks.get(&class_id).cloned()
    }

    /// Calls the string `replace` method.
    ///
    /// String-template replacement delegates expansion to `regex`. When the replacement is a user
    /// function, this path assembles the output and invokes the VM callback with ECMAScript v3-style
    /// arguments: full match, capture groups, match offset, and original string.
    fn call_string_replace_method(
        &mut self,
        chunk: &Chunk,
        receiver: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        let callback = args.get(1);
        match (args.first(), callback) {
            (
                Some(Value::Regex(regex, _, flags)),
                Some(
                    callback @ (Value::Function(_)
                    | Value::BoundFunction(_, _)
                    | Value::Closure(_, _, _)),
                ),
            ) => {
                let mut output = String::with_capacity(receiver.len());
                let source = Value::Str(receiver.to_string());
                let mut last_end = 0usize;
                let mut replaced = false;
                for captures in regex.captures_iter(receiver) {
                    let Some(matched) = captures.get(0) else {
                        continue;
                    };
                    output.push_str(&receiver[last_end..matched.start()]);
                    let mut callback_args = Vec::with_capacity(captures.len() + 2);
                    for index in 0..captures.len() {
                        callback_args.push(
                            captures
                                .get(index)
                                .map(|item| Value::Str(item.as_str().to_string()))
                                .unwrap_or(Value::Null),
                        );
                    }
                    callback_args.push(Value::Int(matched.start() as i64));
                    callback_args.push(source.clone());
                    let replacement = self.call_callback(chunk, callback, callback_args, ip)?;
                    output.push_str(&replacement.to_string());
                    last_end = matched.end();
                    replaced = true;
                    if !flags.contains('g') {
                        break;
                    }
                }
                if !replaced {
                    return Ok(Value::Str(receiver.to_string()));
                }
                output.push_str(&receiver[last_end..]);
                Ok(Value::Str(output))
            }
            (Some(Value::Regex(regex, _, flags)), _) => {
                let to = args.get(1).map(Value::to_string).unwrap_or_default();
                if flags.contains('g') {
                    Ok(Value::Str(
                        regex.replace_all(receiver, to.as_str()).to_string(),
                    ))
                } else {
                    Ok(Value::Str(regex.replace(receiver, to.as_str()).to_string()))
                }
            }
            (from, _) => {
                let from = from.map(Value::to_string).unwrap_or_default();
                let to = args.get(1).map(Value::to_string).unwrap_or_default();
                Ok(Value::Str(receiver.replacen(&from, &to, 1)))
            }
        }
    }

    /// Calls the string method.
    fn call_string_method(&self, receiver: &str, name: &str, args: Vec<Value>) -> Value {
        match name {
            "len" => Value::Int(if receiver.is_ascii() {
                receiver.len() as i64
            } else {
                receiver.chars().count() as i64
            }),
            "trim" => Value::Str(receiver.trim().to_string()),
            "trim_start" => Value::Str(receiver.trim_start().to_string()),
            "trim_end" => Value::Str(receiver.trim_end().to_string()),
            "char_at" => {
                let index = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as usize;
                if receiver.is_ascii() {
                    receiver
                        .as_bytes()
                        .get(index)
                        .map(|byte| Value::Str((*byte as char).to_string()))
                        .unwrap_or(Value::Empty)
                } else {
                    receiver
                        .chars()
                        .nth(index)
                        .map(|ch| Value::Str(ch.to_string()))
                        .unwrap_or(Value::Empty)
                }
            }
            "char_code_at" => {
                let index = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as usize;
                if receiver.is_ascii() {
                    receiver
                        .as_bytes()
                        .get(index)
                        .map(|byte| Value::Int(*byte as i64))
                        .unwrap_or(Value::Empty)
                } else {
                    receiver
                        .chars()
                        .nth(index)
                        .map(|ch| Value::Int(ch as i64))
                        .unwrap_or(Value::Empty)
                }
            }
            "parse_json" => system::parse_json_text(receiver),
            "parse_radix_int" => {
                let radix = args.first().map(Value::to_i64_lossy).unwrap_or(10);
                system::parse_radix_int_text(receiver, radix)
            }
            "parse_radix_str" => {
                let radix = args.first().map(Value::to_i64_lossy).unwrap_or(10);
                system::parse_radix_str_text(receiver, radix)
            }
            "concat" => {
                let mut text = String::with_capacity(receiver.len() + args.len() * 8);
                text.push_str(receiver);
                for arg in args {
                    text.push_str(&arg.to_string());
                }
                Value::Str(text)
            }
            "ends_with" => Value::Bool(
                args.first()
                    .map(|arg| receiver.ends_with(&arg.to_string()))
                    .unwrap_or(false),
            ),
            "contains" => Value::Bool(
                args.first()
                    .map(|arg| receiver.contains(&arg.to_string()))
                    .unwrap_or(false),
            ),
            "index_of" => {
                let needle = args.first().map(Value::to_string).unwrap_or_default();
                Value::Int(
                    receiver
                        .find(&needle)
                        .map(|index| index as i64)
                        .unwrap_or(-1),
                )
            }
            "last_index_of" => {
                let needle = args.first().map(Value::to_string).unwrap_or_default();
                Value::Int(
                    receiver
                        .rfind(&needle)
                        .map(|index| index as i64)
                        .unwrap_or(-1),
                )
            }
            "repeat" => {
                let count = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as usize;
                Value::Str(receiver.repeat(count))
            }
            "to_lowercase" => Value::Str(receiver.to_lowercase()),
            "to_uppercase" => Value::Str(receiver.to_uppercase()),
            "to_number" => Value::Str(receiver.to_string()).to_number_value(),
            "to_string" => Value::Str(receiver.to_string()),
            "starts_with" => Value::Bool(
                args.first()
                    .map(|arg| receiver.starts_with(&arg.to_string()))
                    .unwrap_or(false),
            ),
            "slice" | "substr" => {
                // String slicing cannot directly truncate UTF-8 by bytes, but there is no need to collect all characters into Vec.
                // ASCII uses O(1) byte slicing; Unicode only scans to the target boundary to avoid high-frequency slice/substr from generating a whole string of temporary arrays.
                let char_len = if receiver.is_ascii() {
                    receiver.len()
                } else {
                    receiver.chars().count()
                };
                let raw_start = args.first().map(Value::to_i64_lossy).unwrap_or(0);
                let raw_second = args.get(1).map(Value::to_i64_lossy);
                let start = if raw_start < 0 {
                    char_len.saturating_sub((-raw_start) as usize)
                } else {
                    (raw_start as usize).min(char_len)
                };
                let end = if name == "substr" {
                    let len = raw_second.unwrap_or(char_len as i64).max(0) as usize;
                    start.saturating_add(len).min(char_len)
                } else {
                    let raw_end = raw_second.unwrap_or(char_len as i64);
                    if raw_end < 0 {
                        char_len.saturating_sub((-raw_end) as usize)
                    } else {
                        (raw_end as usize).min(char_len)
                    }
                };
                if start >= end || start >= char_len {
                    return Value::Str(String::new());
                }
                if receiver.is_ascii() {
                    Value::Str(receiver[start..end].to_string())
                } else {
                    let mut start_byte = receiver.len();
                    let mut end_byte = receiver.len();
                    for (char_index, (byte_index, _)) in receiver.char_indices().enumerate() {
                        if char_index == start {
                            start_byte = byte_index;
                        }
                        if char_index == end {
                            end_byte = byte_index;
                            break;
                        }
                    }
                    Value::Str(receiver[start_byte..end_byte].to_string())
                }
            }
            "replace" => {
                let to = args.get(1).map(Value::to_string).unwrap_or_default();
                match args.first() {
                    Some(Value::Regex(regex, _, flags)) => {
                        if flags.contains('g') {
                            Value::Str(regex.replace_all(receiver, to.as_str()).to_string())
                        } else {
                            Value::Str(regex.replace(receiver, to.as_str()).to_string())
                        }
                    }
                    from => {
                        let from = from.map(Value::to_string).unwrap_or_default();
                        Value::Str(receiver.replacen(&from, &to, 1))
                    }
                }
            }
            "replace_all" => {
                let to = args.get(1).map(Value::to_string).unwrap_or_default();
                match args.first() {
                    Some(Value::Regex(regex, _, _)) => {
                        Value::Str(regex.replace_all(receiver, to.as_str()).to_string())
                    }
                    from => {
                        let from = from.map(Value::to_string).unwrap_or_default();
                        Value::Str(receiver.replace(&from, &to))
                    }
                }
            }
            "search" => match args.first() {
                Some(Value::Regex(regex, _, _)) => Value::Int(
                    regex
                        .find(receiver)
                        .map(|item| item.start() as i64)
                        .unwrap_or(-1),
                ),
                needle => {
                    let needle = needle.map(Value::to_string).unwrap_or_default();
                    Value::Int(
                        receiver
                            .find(&needle)
                            .map(|index| index as i64)
                            .unwrap_or(-1),
                    )
                }
            },
            "match" => match args.first() {
                Some(Value::Regex(regex, _, flags)) => {
                    if flags.contains('g') {
                        return Value::Array(Rc::new(RefCell::new(
                            regex
                                .find_iter(receiver)
                                .map(|item| Value::Str(item.as_str().to_string()))
                                .collect(),
                        )));
                    }
                    let Some(captures) = regex.captures(receiver) else {
                        return Value::Empty;
                    };
                    let mut items = Vec::with_capacity(captures.len());
                    for index in 0..captures.len() {
                        items.push(
                            captures
                                .get(index)
                                .map(|item| Value::Str(item.as_str().to_string()))
                                .unwrap_or(Value::Null),
                        );
                    }
                    Value::Array(Rc::new(RefCell::new(items)))
                }
                needle => {
                    let needle = needle.map(Value::to_string).unwrap_or_default();
                    receiver
                        .find(&needle)
                        .map(|_| Value::Array(Rc::new(RefCell::new(vec![Value::Str(needle)]))))
                        .unwrap_or(Value::Empty)
                }
            },
            "split" => {
                let sep = args.first().map(Value::to_string).unwrap_or_default();
                let items = if sep.is_empty() {
                    receiver
                        .chars()
                        .map(|ch| Value::Str(ch.to_string()))
                        .collect()
                } else {
                    receiver
                        .split(&sep)
                        .map(|item| Value::Str(item.to_string()))
                        .collect()
                };
                Value::Array(Rc::new(RefCell::new(items)))
            }
            "pad_start" => Value::Str(Self::pad_string(receiver, &args, true)),
            "pad_end" => Value::Str(Self::pad_string(receiver, &args, false)),
            _ => Value::Empty,
        }
    }

    /// Pads a string by character length.
    ///
    /// `pad_start()` and `pad_end()` are shared here; ASCII is in byte length, non-ASCII is in character count.
    /// Maintains the same semantics as the existing `String.len()`.
    fn pad_string(receiver: &str, args: &[Value], left: bool) -> String {
        let target_len = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as usize;
        let current_len = if receiver.is_ascii() {
            receiver.len()
        } else {
            receiver.chars().count()
        };
        if current_len >= target_len {
            return receiver.to_string();
        }
        let pad = args
            .get(1)
            .map(Value::to_string)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| " ".to_string());
        let fill_len = target_len - current_len;
        let mut filler = String::with_capacity(fill_len.saturating_mul(pad.len()));
        while filler.chars().count() < fill_len {
            filler.push_str(&pad);
        }
        let filler = filler.chars().take(fill_len).collect::<String>();
        let mut output = String::with_capacity(receiver.len() + filler.len());
        if left {
            output.push_str(&filler);
            output.push_str(receiver);
        } else {
            output.push_str(receiver);
            output.push_str(&filler);
        }
        output
    }

    /// Calls a number prototype method.
    ///
    /// The documented number methods are mostly formatting and conversion helpers. Integers and
    /// floats share this implementation to preserve the previous interpreter's permissive behavior.
    fn call_number_method(&self, receiver: &Value, name: &str, args: Vec<Value>) -> Value {
        match name {
            "len" => Value::Int(receiver.to_string().chars().count() as i64),
            "to_number" => receiver.to_number_value(),
            "to_string" => Value::Str(receiver.to_string()),
            "to_fixed" => {
                let digits = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as usize;
                Value::Str(format!("{:.*}", digits, receiver.to_f64_lossy()))
            }
            "to_exponential" => {
                let digits = args.first().map(Value::to_i64_lossy).unwrap_or(6).max(0) as usize;
                Value::Str(format!("{:.*e}", digits, receiver.to_f64_lossy()))
            }
            "to_radix" => {
                let radix = args.first().map(Value::to_i64_lossy).unwrap_or(10);
                Value::Str(system::format_radix(receiver.to_i64_lossy(), radix))
            }
            "to_char" => system::char_from_code(receiver.to_i64_lossy()),
            "is_int" => Value::Bool(match receiver {
                Value::Int(_) => true,
                Value::Float(value) => value.is_finite() && value.fract() == 0.0,
                _ => false,
            }),
            "is_float" => Value::Bool(matches!(receiver, Value::Float(_))),
            "is_finite" => Value::Bool(match receiver {
                Value::Int(_) => true,
                Value::Float(value) => value.is_finite(),
                _ => false,
            }),
            _ => Value::Empty,
        }
    }

    /// Calls the array method.
    fn call_array_method(
        &mut self,
        chunk: &Chunk,
        receiver: &Rc<RefCell<Vec<Value>>>,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match name {
            "len" => Ok(Value::Int(receiver.borrow().len() as i64)),
            "to_string" => {
                if Self::is_json_pretty_arg(args.first()) {
                    return Ok(Value::Str(Self::pretty_json(&Value::Array(
                        receiver.clone(),
                    ))));
                }
                Ok(Value::Str(Value::Array(receiver.clone()).to_json_string()))
            }
            "join" => {
                let sep = args
                    .first()
                    .map(Value::to_string)
                    .unwrap_or_else(|| ",".to_string());
                let values = receiver.borrow();
                let mut text = String::new();
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        text.push_str(&sep);
                    }
                    text.push_str(&value.to_string());
                }
                Ok(Value::Str(text))
            }
            "push" => {
                receiver.borrow_mut().extend(args);
                Ok(Value::Array(receiver.clone()))
            }
            "pop" => Ok(receiver.borrow_mut().pop().unwrap_or(Value::Empty)),
            "first" => Ok(receiver.borrow().first().cloned().unwrap_or(Value::Empty)),
            "last" => Ok(receiver.borrow().last().cloned().unwrap_or(Value::Empty)),
            "at" => {
                let values = receiver.borrow();
                let index = args.first().map(Value::to_i64_lossy).unwrap_or(0);
                Ok(Self::normalize_array_existing_index(index, values.len())
                    .and_then(|index| values.get(index).cloned())
                    .unwrap_or(Value::Empty))
            }
            "shift" => {
                let mut values = receiver.borrow_mut();
                if values.is_empty() {
                    Ok(Value::Empty)
                } else {
                    Ok(values.remove(0))
                }
            }
            "unshift" => {
                let mut values = receiver.borrow_mut();
                if !args.is_empty() {
                    values.splice(0..0, args);
                }
                Ok(Value::Array(receiver.clone()))
            }
            "insert" => {
                let mut args = args.into_iter();
                let raw_start = args.next().map(|value| value.to_i64_lossy()).unwrap_or(0);
                let mut values = receiver.borrow_mut();
                let start = Self::normalize_array_insert_index(raw_start, values.len());
                values.splice(start..start, args);
                Ok(Value::Array(receiver.clone()))
            }
            "reverse" => {
                receiver.borrow_mut().reverse();
                Ok(Value::Array(receiver.clone()))
            }
            "sort" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let mut values = receiver.borrow().clone();
                if matches!(
                    callback,
                    Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _)
                ) {
                    values = self.sort_array_with_callback(chunk, values, &callback, ip)?;
                } else {
                    values.sort_by_key(Value::to_string);
                }
                *receiver.borrow_mut() = values;
                Ok(Value::Array(receiver.clone()))
            }
            "slice" => {
                let values = receiver.borrow();
                let len = values.len();
                let raw_start = args.first().map(Value::to_i64_lossy).unwrap_or(0);
                let raw_end = args.get(1).map(Value::to_i64_lossy).unwrap_or(len as i64);
                let start = Self::normalize_array_bound(raw_start, len);
                let end = Self::normalize_array_bound(raw_end, len);
                Ok(Value::Array(Rc::new(RefCell::new(if start >= end {
                    Vec::new()
                } else {
                    values[start..end].to_vec()
                }))))
            }
            "splice" => {
                let mut values = receiver.borrow_mut();
                let len = values.len();
                let raw_start = args.first().map(Value::to_i64_lossy).unwrap_or(0);
                let start = if raw_start < 0 {
                    len.saturating_sub((-raw_start) as usize)
                } else {
                    (raw_start as usize).min(len)
                };
                let delete_count = args
                    .get(1)
                    .map(Value::to_i64_lossy)
                    .unwrap_or((len - start) as i64)
                    .max(0) as usize;
                let delete_end = start.saturating_add(delete_count).min(len);
                let removed = values[start..delete_end].to_vec();
                let inserts = if args.len() > 2 {
                    args[2..].to_vec()
                } else {
                    Vec::new()
                };
                values.splice(start..delete_end, inserts);
                Ok(Value::Array(Rc::new(RefCell::new(removed))))
            }
            "concat" => {
                let values = receiver.borrow();
                let mut output = Vec::with_capacity(values.len() + args.len());
                output.extend(values.iter().cloned());
                drop(values);
                for arg in args {
                    if let Value::Array(items) = arg {
                        output.extend(items.borrow().iter().cloned());
                    } else {
                        output.push(arg);
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "contains" => Ok(Value::Bool(args.first().map_or(false, |arg| {
                let values = receiver.borrow();
                values.contains(arg)
            }))),
            "index_of" => Ok(Value::Int(
                args.first()
                    .and_then(|arg| {
                        let values = receiver.borrow();
                        values.iter().position(|item| item == arg)
                    })
                    .map(|i| i as i64)
                    .unwrap_or(-1),
            )),
            "last_index_of" => Ok(Value::Int(
                args.first()
                    .and_then(|arg| {
                        let values = receiver.borrow();
                        values.iter().rposition(|item| item == arg)
                    })
                    .map(|i| i as i64)
                    .unwrap_or(-1),
            )),
            "keys" => Ok(Value::Array(Rc::new(RefCell::new({
                let len = receiver.borrow().len();
                let mut keys = Vec::with_capacity(len);
                for index in 0..len {
                    keys.push(Value::Int(index as i64));
                }
                keys
            })))),
            "values" => Ok(Value::Array(Rc::new(RefCell::new(
                receiver.borrow().clone(),
            )))),
            "clone" => Ok(Value::Array(receiver.clone()).clone_mutable_literal()),
            "entries" => {
                let items = receiver
                    .borrow()
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        Value::Array(Rc::new(RefCell::new(vec![
                            Value::Int(index as i64),
                            value.clone(),
                        ])))
                    })
                    .collect();
                Ok(Value::Array(Rc::new(RefCell::new(items))))
            }
            "delete" => {
                let index = args.first().map(Value::to_i64_lossy).unwrap_or(-1);
                let mut values = receiver.borrow_mut();
                if index >= 0 && (index as usize) < values.len() {
                    values.remove(index as usize);
                    Ok(Value::Bool(true))
                } else {
                    Ok(Value::Bool(false))
                }
            }
            "remove_at" => {
                let index = args.first().map(Value::to_i64_lossy).unwrap_or(-1);
                let mut values = receiver.borrow_mut();
                Ok(Self::normalize_array_existing_index(index, values.len())
                    .map(|index| values.remove(index))
                    .unwrap_or(Value::Empty))
            }
            "clear" => {
                receiver.borrow_mut().clear();
                Ok(Value::Array(receiver.clone()))
            }
            "is_empty" => Ok(Value::Bool(receiver.borrow().is_empty())),
            "unique" => {
                let values = receiver.borrow();
                let mut output = Vec::with_capacity(values.len());
                for value in values.iter() {
                    if !output.contains(value) {
                        output.push(value.clone());
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "chunk" => {
                let size = args.first().map(Value::to_i64_lossy).unwrap_or(1);
                if size <= 0 {
                    return Ok(Value::Array(Rc::new(RefCell::new(Vec::new()))));
                }
                let size = size as usize;
                let values = receiver.borrow();
                let mut output = Vec::with_capacity(values.len().div_ceil(size));
                for item in values.chunks(size) {
                    output.push(Value::Array(Rc::new(RefCell::new(item.to_vec()))));
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "each" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let current = Value::Array(receiver.clone());
                let source = receiver.borrow().clone();
                for (index, item) in source.into_iter().enumerate() {
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![item, Value::Int(index as i64), current.clone()],
                        ip,
                    )?;
                }
                Ok(Value::Array(receiver.clone()))
            }
            "find" | "find_index" | "find_last" | "find_last_index" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let current = Value::Array(receiver.clone());
                let source = receiver.borrow().clone();
                let reverse = matches!(name, "find_last" | "find_last_index");
                let mut found_index = None;
                if reverse {
                    for index in (0..source.len()).rev() {
                        let keep = self.call_callback(
                            chunk,
                            &callback,
                            vec![
                                source[index].clone(),
                                Value::Int(index as i64),
                                current.clone(),
                            ],
                            ip,
                        )?;
                        if keep.is_truthy() {
                            found_index = Some(index);
                            break;
                        }
                    }
                } else {
                    for (index, item) in source.iter().enumerate() {
                        let keep = self.call_callback(
                            chunk,
                            &callback,
                            vec![item.clone(), Value::Int(index as i64), current.clone()],
                            ip,
                        )?;
                        if keep.is_truthy() {
                            found_index = Some(index);
                            break;
                        }
                    }
                }
                Ok(match (name, found_index) {
                    ("find" | "find_last", Some(index)) => source[index].clone(),
                    ("find" | "find_last", None) => Value::Empty,
                    (_, Some(index)) => Value::Int(index as i64),
                    (_, None) => Value::Int(-1),
                })
            }
            "every" | "some" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let current = Value::Array(receiver.clone());
                let source = receiver.borrow().clone();
                if name == "every" {
                    for (index, item) in source.into_iter().enumerate() {
                        let keep = self.call_callback(
                            chunk,
                            &callback,
                            vec![item, Value::Int(index as i64), current.clone()],
                            ip,
                        )?;
                        if !keep.is_truthy() {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Ok(Value::Bool(true))
                } else {
                    for (index, item) in source.into_iter().enumerate() {
                        let keep = self.call_callback(
                            chunk,
                            &callback,
                            vec![item, Value::Int(index as i64), current.clone()],
                            ip,
                        )?;
                        if keep.is_truthy() {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Ok(Value::Bool(false))
                }
            }
            "map" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let current = Value::Array(receiver.clone());
                let source = receiver.borrow().clone();
                let mut output = Vec::with_capacity(source.len());
                for (index, item) in source.into_iter().enumerate() {
                    output.push(self.call_callback(
                        chunk,
                        &callback,
                        vec![item, Value::Int(index as i64), current.clone()],
                        ip,
                    )?);
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "filter" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let current = Value::Array(receiver.clone());
                let source = receiver.borrow().clone();
                let mut output = Vec::new();
                for (index, item) in source.into_iter().enumerate() {
                    let keep = self.call_callback(
                        chunk,
                        &callback,
                        vec![item.clone(), Value::Int(index as i64), current.clone()],
                        ip,
                    )?;
                    if keep.is_truthy() {
                        output.push(item);
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "reduce" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = receiver.borrow().clone();
                if source.is_empty() && args.len() <= 1 {
                    return Err(
                        self.error(ip, "empty array reduce() must provide an initial value")
                    );
                }
                let mut iter = source.into_iter();
                let mut acc = if args.len() > 1 {
                    args[1].clone()
                } else {
                    iter.next().unwrap_or(Value::Empty)
                };
                let start_index = if args.len() > 1 { 0 } else { 1 };
                for (offset, item) in iter.enumerate() {
                    acc = self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            acc,
                            item,
                            Value::Int((start_index + offset) as i64),
                            Value::Array(receiver.clone()),
                        ],
                        ip,
                    )?;
                }
                Ok(acc)
            }
            "reduce_right" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = receiver.borrow().clone();
                if source.is_empty() && args.len() <= 1 {
                    return Err(self.error(
                        ip,
                        "empty array reduce_right() must provide an initial value",
                    ));
                }
                let mut index = source.len();
                let mut acc = if args.len() > 1 {
                    args[1].clone()
                } else if let Some(value) = source.last() {
                    index = index.saturating_sub(1);
                    value.clone()
                } else {
                    Value::Empty
                };
                while index > 0 {
                    index -= 1;
                    acc = self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            acc,
                            source[index].clone(),
                            Value::Int(index as i64),
                            Value::Array(receiver.clone()),
                        ],
                        ip,
                    )?;
                }
                Ok(acc)
            }
            "fill" => {
                let value = args.first().cloned().unwrap_or(Value::Empty);
                let mut values = receiver.borrow_mut();
                let len = values.len();
                let start = Self::normalize_array_bound(
                    args.get(1).map(Value::to_i64_lossy).unwrap_or(0),
                    len,
                );
                let end = Self::normalize_array_bound(
                    args.get(2).map(Value::to_i64_lossy).unwrap_or(len as i64),
                    len,
                );
                for index in start.min(end)..end {
                    values[index] = value.clone();
                }
                Ok(Value::Array(receiver.clone()))
            }
            "flat" => {
                let depth = args.first().map(Value::to_i64_lossy).unwrap_or(1).max(0) as usize;
                let source = receiver.borrow().clone();
                let mut output = Vec::with_capacity(source.len());
                for item in source {
                    Self::push_flattened_array_item(item, depth, &mut output);
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "flat_map" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let current = Value::Array(receiver.clone());
                let source = receiver.borrow().clone();
                let mut output = Vec::with_capacity(source.len());
                for (index, item) in source.into_iter().enumerate() {
                    let mapped = self.call_callback(
                        chunk,
                        &callback,
                        vec![item, Value::Int(index as i64), current.clone()],
                        ip,
                    )?;
                    Self::push_flattened_array_item(mapped, 1, &mut output);
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            _ => Ok(Value::Empty),
        }
    }

    /// Appends flattened elements to the array output.
    ///
    /// `flat()` and `flat_map()` share this recursive path so their semantics cannot drift apart.
    /// Arrays expand only while depth remains; all other values pass through unchanged.
    fn push_flattened_array_item(value: Value, depth: usize, output: &mut Vec<Value>) {
        match value {
            Value::Array(values) if depth > 0 => {
                for item in values.borrow().iter().cloned() {
                    Self::push_flattened_array_item(item, depth - 1, output);
                }
            }
            value => output.push(value),
        }
    }

    /// Normalizes the read subscript to the actual element position according to the array length.
    ///
    /// `at()` and `remove_at()` accept negative indices relative to the end. Out-of-range indices
    /// return `None`, which callers convert to `null`, without risking a panic.
    fn normalize_array_existing_index(index: i64, len: usize) -> Option<usize> {
        if index < 0 {
            let offset = (-index) as usize;
            (offset <= len).then(|| len - offset)
        } else {
            let index = index as usize;
            (index < len).then_some(index)
        }
    }

    /// Clamps an insertion index to a valid position for the array length.
    ///
    /// `insert()` follows the same start-index rules as `splice()`: negative values count from the end,
    /// and out-of-range values clamp to the beginning or end.
    fn normalize_array_insert_index(index: i64, len: usize) -> usize {
        if index < 0 {
            len.saturating_sub((-index) as usize)
        } else {
            (index as usize).min(len)
        }
    }

    /// Normalizes an array boundary to an index in `[0, len]` using JavaScript-style rules.
    ///
    /// Methods such as `slice()` and `fill()` accept negative indices relative to the end. Sharing this
    /// helper keeps boundary behavior consistent and safely clamps out-of-range values.
    fn normalize_array_bound(index: i64, len: usize) -> usize {
        if index < 0 {
            len.saturating_sub((-index) as usize)
        } else {
            (index as usize).min(len)
        }
    }

    /// Sorts an array using a user comparator.
    ///
    /// The comparator mutably calls back into the VM, so it cannot be passed directly to `sort_by`.
    /// This bottom-up stable merge sort stays O(n log n) and follows JavaScript comparator semantics:
    /// negative keeps left first, positive swaps the order, and zero preserves it.
    fn sort_array_with_callback(
        &mut self,
        chunk: &Chunk,
        mut values: Vec<Value>,
        callback: &Value,
        ip: usize,
    ) -> Result<Vec<Value>, VmError> {
        let len = values.len();
        if len < 2 {
            return Ok(values);
        }
        let mut buffer = values.clone();
        let mut width = 1usize;
        while width < len {
            let mut start = 0usize;
            while start < len {
                let mid = start.saturating_add(width).min(len);
                let end = start.saturating_add(width * 2).min(len);
                if mid >= end {
                    for index in start..end {
                        buffer[index] = values[index].clone();
                    }
                    start = end;
                    continue;
                }
                let mut left = start;
                let mut right = mid;
                let mut write = start;
                while left < mid && right < end {
                    let order = self.call_callback(
                        chunk,
                        callback,
                        vec![values[left].clone(), values[right].clone()],
                        ip,
                    )?;
                    if order.to_f64_lossy() <= 0.0 {
                        buffer[write] = values[left].clone();
                        left += 1;
                    } else {
                        buffer[write] = values[right].clone();
                        right += 1;
                    }
                    write += 1;
                }
                while left < mid {
                    buffer[write] = values[left].clone();
                    left += 1;
                    write += 1;
                }
                while right < end {
                    buffer[write] = values[right].clone();
                    right += 1;
                    write += 1;
                }
                start = end;
            }
            std::mem::swap(&mut values, &mut buffer);
            width = width.saturating_mul(2);
        }
        Ok(values)
    }

    /// Calls an object prototype method.
    ///
    /// Object prototype function mainly enumerates key/value; the method that modifies the object returns the object itself, which facilitates the script to continue chain calls.
    fn call_object_method(
        &mut self,
        chunk: &Chunk,
        receiver: &Rc<RefCell<IndexMap<String, Value>>>,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match name {
            "len" => Ok(Value::Int(receiver.borrow().len() as i64)),
            "to_string" => {
                if Self::is_json_pretty_arg(args.first()) {
                    Ok(Value::Str(Self::pretty_json(&Value::Object(
                        receiver.clone(),
                    ))))
                } else {
                    Ok(Value::Str(Value::Object(receiver.clone()).to_json_string()))
                }
            }
            "keys" => Ok(Value::Array(Rc::new(RefCell::new(
                receiver
                    .borrow()
                    .keys()
                    .map(|key| Value::Str(key.clone()))
                    .collect(),
            )))),
            "values" => Ok(Value::Array(Rc::new(RefCell::new(
                receiver.borrow().values().cloned().collect(),
            )))),
            "entries" => {
                let values = receiver.borrow();
                let mut output = Vec::with_capacity(values.len());
                for (key, value) in values.iter() {
                    output.push(Value::Array(Rc::new(RefCell::new(vec![
                        Value::Str(key.clone()),
                        value.clone(),
                    ]))));
                }
                Ok(Value::Array(Rc::new(RefCell::new(output))))
            }
            "reverse" => {
                let values = receiver.borrow();
                let mut output = IndexMap::with_capacity(values.len());
                for (key, value) in values.iter().rev() {
                    output.insert(key.clone(), value.clone());
                }
                Ok(Value::Object(Rc::new(RefCell::new(output))))
            }
            "clone" => Ok(Value::Object(receiver.clone()).clone_mutable_literal()),
            "concat" => {
                let mut output = receiver.borrow().clone();
                for arg in args {
                    if let Value::Object(values) = arg {
                        for (key, value) in values.borrow().iter() {
                            output.insert(key.clone(), value.clone());
                        }
                    }
                }
                Ok(Value::Object(Rc::new(RefCell::new(output))))
            }
            "delete" => {
                let key = args.first().map(Value::to_string).unwrap_or_default();
                Ok(Value::Bool(
                    receiver.borrow_mut().shift_remove(&key).is_some(),
                ))
            }
            "has_key" => {
                let key = args.first().map(Value::to_string).unwrap_or_default();
                Ok(Value::Bool(receiver.borrow().contains_key(&key)))
            }
            "get" => {
                let key = args.first().map(Value::to_string).unwrap_or_default();
                Ok(receiver
                    .borrow()
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::Empty)))
            }
            "is_empty" => Ok(Value::Bool(receiver.borrow().is_empty())),
            "from_entries" => {
                let entries = args.first().cloned().unwrap_or_else(|| {
                    let values = receiver.borrow();
                    let mut entries = Vec::with_capacity(values.len());
                    for (key, value) in values.iter() {
                        entries.push(Value::Array(Rc::new(RefCell::new(vec![
                            Value::Str(key.clone()),
                            value.clone(),
                        ]))));
                    }
                    Value::Array(Rc::new(RefCell::new(entries)))
                });
                Ok(Self::object_from_entries(entries))
            }
            "clear" => {
                receiver.borrow_mut().clear();
                Ok(Value::Object(receiver.clone()))
            }
            "update" => {
                let current = Value::Object(receiver.clone());
                let mut entries = Vec::new();
                for arg in args {
                    if let Value::Object(values) = arg {
                        let values = values.borrow();
                        entries.reserve(values.len());
                        for (key, value) in values.iter() {
                            if value.contains_reference_to(&current) {
                                return Err(
                                    self.error(ip, "object update() cannot write values that would form a circular reference")
                                );
                            }
                            entries.push((key.clone(), value.clone()));
                        }
                    }
                }
                let mut receiver = receiver.borrow_mut();
                for (key, value) in entries {
                    receiver.insert(key, value);
                }
                Ok(Value::Object(match current {
                    Value::Object(values) => values,
                    _ => unreachable!(),
                }))
            }
            "pick" => {
                let keys = Self::object_key_args(&args);
                let values = receiver.borrow();
                let mut output = IndexMap::with_capacity(keys.len());
                for key in keys {
                    if let Some(value) = values.get(&key) {
                        output.insert(key, value.clone());
                    }
                }
                Ok(Value::Object(Rc::new(RefCell::new(output))))
            }
            "omit" => {
                let keys = Self::object_key_set(&args);
                let values = receiver.borrow();
                let mut output = IndexMap::with_capacity(values.len().saturating_sub(keys.len()));
                for (key, value) in values.iter() {
                    if !keys.contains(key) {
                        output.insert(key.clone(), value.clone());
                    }
                }
                Ok(Value::Object(Rc::new(RefCell::new(output))))
            }
            "each" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = Self::snapshot_object_entries(receiver);
                for (key, value) in source {
                    // Object and array iteration share the callback order: value, key, then source.
                    // This keeps `obj.map(fn(v){ v.name = 1 })` and `obj.each((v,k)->{})` intuitive and
                    // prevents callers from mistaking the string key for the value object.
                    self.call_callback(
                        chunk,
                        &callback,
                        vec![value, Value::Str(key), Value::Object(receiver.clone())],
                        ip,
                    )?;
                }
                Ok(Value::Object(receiver.clone()))
            }
            "map" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = Self::snapshot_object_entries(receiver);
                let mut output = IndexMap::with_capacity(source.len());
                for (key, value) in source {
                    // Match `each` and array `map`: value, key, then source. Store each callback result
                    // under the original key so the new object keeps the same shape.
                    let mapped = self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            value,
                            Value::Str(key.clone()),
                            Value::Object(receiver.clone()),
                        ],
                        ip,
                    )?;
                    output.insert(key, mapped);
                }
                Ok(Value::Object(Rc::new(RefCell::new(output))))
            }
            "filter" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = Self::snapshot_object_entries(receiver);
                let mut output = IndexMap::new();
                for (key, value) in source {
                    let keep = self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            value.clone(),
                            Value::Str(key.clone()),
                            Value::Object(receiver.clone()),
                        ],
                        ip,
                    )?;
                    if keep.is_truthy() {
                        output.insert(key, value);
                    }
                }
                Ok(Value::Object(Rc::new(RefCell::new(output))))
            }
            "every" | "some" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = Self::snapshot_object_entries(receiver);
                if name == "every" {
                    for (key, value) in source {
                        let keep = self.call_callback(
                            chunk,
                            &callback,
                            vec![value, Value::Str(key), Value::Object(receiver.clone())],
                            ip,
                        )?;
                        if !keep.is_truthy() {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Ok(Value::Bool(true))
                } else {
                    for (key, value) in source {
                        let keep = self.call_callback(
                            chunk,
                            &callback,
                            vec![value, Value::Str(key), Value::Object(receiver.clone())],
                            ip,
                        )?;
                        if keep.is_truthy() {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Ok(Value::Bool(false))
                }
            }
            "find" | "find_key" => {
                let callback = args.first().cloned().unwrap_or(Value::Empty);
                let source = Self::snapshot_object_entries(receiver);
                for (key, value) in source {
                    let keep = self.call_callback(
                        chunk,
                        &callback,
                        vec![
                            value.clone(),
                            Value::Str(key.clone()),
                            Value::Object(receiver.clone()),
                        ],
                        ip,
                    )?;
                    if keep.is_truthy() {
                        return Ok(if name == "find" {
                            value
                        } else {
                            Value::Str(key)
                        });
                    }
                }
                Ok(Value::Empty)
            }
            _ => Ok(Value::Empty),
        }
    }

    /// Snapshots entries before traversing an object.
    ///
    /// Callbacks may mutate the original object. Cloning the current entries avoids a `RefCell` borrow
    /// conflict and gives the traversal a stable view of the object as it existed at the start.
    fn snapshot_object_entries(
        receiver: &Rc<RefCell<IndexMap<String, Value>>>,
    ) -> Vec<(String, Value)> {
        let values = receiver.borrow();
        let mut output = Vec::with_capacity(values.len());
        for (key, value) in values.iter() {
            output.push((key.clone(), value.clone()));
        }
        output
    }

    /// Expands the object key parameter into a list of strings.
    ///
    /// `pick()` accepts either `obj.pick(['a', 'b'])` or `obj.pick('a', 'b')`. Normalizing both forms
    /// here leaves the caller responsible only for lookup and result-object construction.
    fn object_key_args(args: &[Value]) -> Vec<String> {
        if let Some(Value::Array(values)) = args.first() {
            let values = values.borrow();
            let mut keys = Vec::with_capacity(values.len());
            for value in values.iter() {
                keys.push(value.to_string());
            }
            keys
        } else {
            let mut keys = Vec::with_capacity(args.len());
            for value in args {
                keys.push(value.to_string());
            }
            keys
        }
    }

    /// Expands the object key parameter into a hash set.
    ///
    /// `omit()` must scan the source object once. Building a `HashSet` first keeps each exclusion check
    /// at amortized O(1), avoiding nested loops for large objects or many keys.
    fn object_key_set(args: &[Value]) -> HashSet<String> {
        let keys = Self::object_key_args(args);
        let mut set = HashSet::with_capacity(keys.len());
        for key in keys {
            set.insert(key);
        }
        set
    }

    /// Object from an array of `[key, value]` entries.
    ///
    /// This function serves `Object.from_entries()`; invalid entries will be skipped to maintain fault tolerance during batch conversion on the script side.
    /// A later entry with the same key overwrites the earlier value.
    fn object_from_entries(entries: Value) -> Value {
        let Value::Array(items) = entries else {
            return Value::Object(Rc::new(RefCell::new(IndexMap::new())));
        };
        let items = items.borrow();
        let mut output = IndexMap::with_capacity(items.len());
        for item in items.iter() {
            let Value::Array(entry) = item else {
                continue;
            };
            let entry = entry.borrow();
            if entry.len() < 2 {
                continue;
            }
            output.insert(entry[0].to_string(), entry[1].clone());
        }
        Value::Object(Rc::new(RefCell::new(output)))
    }

    /// Calls the regular object method.
    ///
    /// The current regular object provides the most basic `test`, `match`, and `replace` capabilities. The string prototype can also receive regular values.
    fn call_regex_method(
        &self,
        regex: &Rc<regex::Regex>,
        pattern: &str,
        flags: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Value {
        match name {
            "test" => Value::Bool(
                args.first()
                    .map(|value| regex.is_match(&value.to_string()))
                    .unwrap_or(false),
            ),
            "match" => {
                let text = args.first().map(Value::to_string).unwrap_or_default();
                Value::Array(Rc::new(RefCell::new(
                    regex
                        .find_iter(&text)
                        .map(|item| Value::Str(item.as_str().to_string()))
                        .collect(),
                )))
            }
            "replace" => {
                let text = args.first().map(Value::to_string).unwrap_or_default();
                let to = args.get(1).map(Value::to_string).unwrap_or_default();
                if flags.contains('g') {
                    Value::Str(regex.replace_all(&text, to.as_str()).to_string())
                } else {
                    Value::Str(regex.replace(&text, to.as_str()).to_string())
                }
            }
            "to_string" => Value::Str(format!("/{}/{}", pattern, flags)),
            _ => Value::Empty,
        }
    }

    /// Returns the system function value visible to the script.
    fn native_function(name: &str) -> Option<Value> {
        (system::is_system_function(name)
            || Self::is_library_constructor(name)
            || matches!(
                name,
                "task"
                    | "task_all"
                    | "task_race"
                    | "set_timeout"
                    | "set_interval"
                    | "header"
                    | "status_code"
                    | "redirect"
                    | "send_file"
            ))
        .then(|| Value::NativeFunction(name.to_string()))
    }

    /// Returns the system constant value visible to the script.
    fn native_constant(name: &str) -> Option<Value> {
        match name {
            "JSON_PRETTY" => Some(Value::Str("JSON_PRETTY".to_string())),
            "BT" => Some(Value::Bt(BtRuntime)),
            "Math" => Some(Value::Math(BtMath)),
            #[cfg(feature = "ffi")]
            "ffi" => Some(Value::Ffi(BtFfiValue::static_value())),
            _ => None,
        }
    }

    /// Determines whether the name is a read-only system constant.
    fn is_native_constant_name(name: &str) -> bool {
        if matches!(name, "JSON_PRETTY" | "BT" | "Math") {
            return true;
        }
        #[cfg(feature = "ffi")]
        if name == "ffi" {
            return true;
        }
        false
    }

    /// Reads global variables and binds the chunk to which the function belongs when needed.
    ///
    /// `env('fn_name')` should maintain the same semantics as ordinary variable reading: the included function cannot lose the owner chunk.
    /// Without the owner, a dynamic call could resolve the function ID against the wrong chunk.
    fn global_value_for_name(&self, name: &str) -> Option<Value> {
        match self.globals.get(name) {
            Some(Value::Function(function_id)) => self
                .global_function_chunks
                .get(name)
                .map(|owner| Value::BoundFunction(*function_id, owner.clone()))
                .or_else(|| Some(Value::Function(*function_id))),
            Some(value) => Some(value.clone()),
            None => None,
        }
    }

    /// Builds the user global environment object.
    ///
    /// This snapshots only the VM's global table; it neither scans locals nor reflects the compiler's
    /// symbol pool. `env()` therefore provides controlled dynamic/debug access without taxing normal
    /// variable reads and writes.
    fn user_environment_object(&self) -> Value {
        let mut values = IndexMap::with_capacity(self.globals.len());
        for name in self.globals.keys() {
            if let Some(value) = self.global_value_for_name(name) {
                values.insert(name.clone(), value);
            }
        }
        Value::Object(Rc::new(RefCell::new(values)))
    }

    /// Builds the system global environment object.
    ///
    /// Exposes system functions, constants, and standard-library constructors to debuggers, REPLs,
    /// and script introspection. The object is built on demand rather than retained in globals.
    fn system_environment_object() -> Value {
        let names = Self::system_environment_names();
        let mut values = IndexMap::with_capacity(names.len());
        for name in names {
            let value = Self::native_constant(name)
                .or_else(|| Self::native_function(name))
                .unwrap_or(Value::Null);
            values.insert((*name).to_string(), value);
        }
        Value::Object(Rc::new(RefCell::new(values)))
    }

    /// Returns the list of system environment names visible to the script.
    pub(crate) fn system_environment_names() -> &'static [&'static str] {
        &[
            "envs",
            "env",
            "has_envs",
            "has_env",
            "pause",
            "assert",
            "echo",
            "eval",
            "exit",
            "include",
            "include_once",
            "cur_dir",
            "cur_file",
            "cur_root",
            "bool",
            "type",
            "call",
            "task",
            "task_all",
            "task_race",
            "set_timeout",
            "set_interval",
            "number",
            "string",
            "float",
            "int",
            "array",
            "object",
            "json",
            "regex",
            "is_empty",
            "is_null",
            "sleep",
            "rand",
            "date",
            "base64",
            "bytes",
            "fs",
            "html",
            "crypto",
            "url",
            "path",
            "BT",
            "Math",
            #[cfg(feature = "ffi")]
            "ffi",
            "md5",
            "modbus",
            "mysql",
            "net",
            "process",
            "reqwest",
            "device",
            "JSON_PRETTY",
        ]
    }

    /// Determines whether the name is an object standard library constructor.
    fn is_library_constructor(name: &str) -> bool {
        matches!(
            name,
            "date"
                | "base64"
                | "bytes"
                | "fs"
                | "html"
                | "crypto"
                | "url"
                | "path"
                | "md5"
                | "modbus"
                | "mysql"
                | "net"
                | "process"
                | "reqwest"
                | "device"
        )
    }

    /// Calls native functions.
    ///
    /// System functions live in `libs::system`; this method also performs lightweight routing for
    /// standard-library object constructors such as `date()`.
    fn call_native_function(
        &mut self,
        chunk: &Chunk,
        name: &str,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        let result = match name {
            "envs" => {
                if args.is_empty() {
                    return Ok(Self::system_environment_object());
                }
                return Ok(args
                    .first()
                    .map(Value::to_string)
                    .filter(|name| !name.is_empty())
                    .and_then(|name| {
                        Self::native_constant(&name).or_else(|| Self::native_function(&name))
                    })
                    .unwrap_or(Value::Empty));
            }
            "env" => {
                return Ok(args
                    .first()
                    .map(Value::to_string)
                    .filter(|name| !name.is_empty())
                    .and_then(|name| self.global_value_for_name(&name))
                    .unwrap_or_else(|| {
                        if args.is_empty() {
                            self.user_environment_object()
                        } else {
                            Value::Empty
                        }
                    }))
            }
            "has_envs" => {
                let name = args.first().map(Value::to_string).unwrap_or_default();
                return Ok(Value::Bool(
                    Self::native_constant(&name).is_some()
                        || Self::native_function(&name).is_some(),
                ));
            }
            "has_env" => {
                let name = args.first().map(Value::to_string).unwrap_or_default();
                return Ok(Value::Bool(self.globals.contains_key(&name)));
            }
            "assert" => return self.call_assert(args, ip),
            "echo" => {
                let mut text = String::new();
                for (index, value) in args.iter().enumerate() {
                    if index > 0 {
                        text.push(' ');
                    }
                    text.push_str(&value.to_output_string());
                }
                println!("{}", text);
                return Ok(Value::Empty);
            }
            "pause" => {
                if self.is_web_request() {
                    return Err(self.error(
                        ip,
                        "pause() cannot wait for terminal input in the context of a web request",
                    ));
                }
                self.flush_output_to_stdout();
                if let Some(message) = args.first() {
                    print!("{}", message.to_output_string());
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                return Ok(Value::Empty);
            }
            "eval" => return self.call_eval(args, ip),
            "exit" => {
                let value = args.first().cloned().unwrap_or(Value::Empty);
                self.output.push_str(&value.to_output_string());
                self.exit_value = Some(value.clone());
                return Ok(value);
            }
            "task" => return self.call_task(chunk, args, ip),
            "task_all" => {
                if self.is_web_request() {
                    return Err(self.error(ip, "task_all() cannot wait for background tasks in the context of a web request"));
                }
                return self.call_task_all(args, ip);
            }
            "task_race" => {
                if self.is_web_request() {
                    return Err(self.error(ip, "task_race() cannot wait for a background task in the context of a web request"));
                }
                return self.call_task_race(args, ip);
            }
            "set_timeout" => return self.call_set_timeout(chunk, args, ip),
            "set_interval" => return self.call_set_interval(chunk, args, ip),
            "call" => return self.call_dynamic(chunk, args, ip),
            "include" => return self.call_include(args, ip, false),
            "include_once" => return self.call_include(args, ip, true),
            "cur_dir" => return Ok(Value::Str(self.current_dir_text(Self::bool_arg(&args, 0)))),
            "cur_file" => return Ok(Value::Str(self.current_file_text(Self::bool_arg(&args, 0)))),
            "cur_root" => return Ok(Value::Str(self.current_root_text())),
            "date" => BtDate::new(args),
            "base64" => BtBase64::new(args),
            "bytes" => BtBytes::new(args),
            "fs" => return self.create_fs(args, ip),
            "html" => BtHtml::new(args),
            "crypto" => BtCrypto::new(args),
            "url" => BtUrl::new(args),
            "path" => BtPath::new(args),
            "header" | "status_code" | "redirect" | "send_file" => {
                let Some(response) = &self.web_response else {
                    return Err(self.error(
                        ip,
                        format!("{}() can only be called in a web request", name),
                    ));
                };
                let args = if name == "send_file" {
                    self.check_permission(Capability::Fs, ip)?;
                    self.with_resolved_path_arg(args, 0, "send_file()", ip)?
                } else {
                    args
                };
                return response
                    .borrow_mut()
                    .call_method(name, args)
                    .map_err(|message| self.error(ip, message));
            }
            "md5" => BtMd5::new(args),
            "modbus" => BtModbus::new(args),
            "mysql" => {
                self.check_permission(Capability::Mysql, ip)?;
                BtMysql::new(args)
            }
            "net" => {
                self.check_permission(Capability::Net, ip)?;
                BtNet::new(args)
            }
            "process" => return self.create_process(args, ip),
            "reqwest" => {
                self.check_permission(Capability::Http, ip)?;
                BtReqwest::new(args)
            }
            "device" => {
                self.check_permission(Capability::Device, ip)?;
                BtDevice::new(args)
            }
            "sleep" => return self.call_sleep(args, ip),
            _ => system::call(name, args),
        };
        result.map_err(|message| self.error(ip, message))
    }

    /// Determines whether the current VM is executing a single web request.
    fn is_web_request(&self) -> bool {
        self.web_response.is_some()
    }

    /// Checks the standard library capability permissions.
    fn check_permission(&self, capability: Capability, ip: usize) -> Result<(), VmError> {
        permission::check(capability).map_err(|message| self.error(ip, message))
    }

    /// Direct execution of process methods is not allowed in the web request context.
    fn is_web_rejected_process_method(name: &str) -> bool {
        matches!(name, "status" | "output" | "child" | "wait")
    }

    /// Executes sleep() and applies the default I/O timeout limit on web requests.
    fn call_sleep(&self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        let millis = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as u64;
        if self.is_web_request() {
            let max_millis = crate::io::default_timeout()
                .as_millis()
                .min(u64::MAX as u128) as u64;
            if millis > max_millis {
                return Err(self.error(
                    ip,
                    format!(
                        "sleep() allows up to {} milliseconds in a web request context, currently {} milliseconds",
                        max_millis, millis
                    ),
                ));
            }
        }
        std::thread::sleep(Duration::from_millis(millis));
        Ok(Value::Empty)
    }

    /// Executes the `assert()` system function.
    ///
    /// Reuses `Value::is_truthy()` so assertion conditions match `if` and `while`. The source statement
    /// is read only on failure, keeping file I/O and caching work off the successful hot path.
    fn call_assert(&self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        let Some(condition) = args.first() else {
            return Err(self.error(ip, "assert requires at least 1 argument"));
        };
        if condition.is_truthy() {
            return Ok(Value::Bool(true));
        }

        let statement = self.current_assert_statement();
        let message = if let Some(custom_message) = args.get(1) {
            if let Some(statement) = statement {
                format!(
                    "Assertion failed: {}\n Statement: {}",
                    custom_message.to_string(),
                    statement
                )
            } else {
                format!("Assertion failed: {}", custom_message.to_string())
            }
        } else if let Some(statement) = statement {
            format!("Assertion failed: {}", statement)
        } else {
            "Assertion failed".to_string()
        };
        Err(self.error(ip, message))
    }

    /// Reads the source code statement where the current assertion call is located.
    ///
    /// The compiler points the call expression at the opening parenthesis. Scan left for `assert`, then
    /// track nesting to the matching closing parenthesis. Returning `None` lets diagnostics fall back cleanly.
    fn current_assert_statement(&self) -> Option<String> {
        let span = self.current_span.as_ref()?;
        let source = std::fs::read_to_string(&span.file).ok()?;
        let line = source.lines().nth(span.line.saturating_sub(1))?;
        Self::extract_assert_statement(line, span.column)
    }

    /// Intercepts the `assert(...)` statement from a single line of source code according to the left bracket column number.
    fn extract_assert_statement(line: &str, column: usize) -> Option<String> {
        let chars = line.chars().collect::<Vec<_>>();
        let mut open = column.saturating_sub(1).min(chars.len().saturating_sub(1));
        if chars.get(open) != Some(&'(') {
            open = chars
                .iter()
                .enumerate()
                .skip(open)
                .find_map(|(index, ch)| (*ch == '(').then_some(index))?;
        }

        let mut name_end = open;
        while name_end > 0 && chars[name_end - 1].is_whitespace() {
            name_end -= 1;
        }
        let mut name_start = name_end;
        while name_start > 0 && Self::is_identifier_char(chars[name_start - 1]) {
            name_start -= 1;
        }
        if chars.get(name_start..name_end)?.iter().collect::<String>() != "assert" {
            return None;
        }

        let mut depth = 0usize;
        let mut quote = None;
        let mut escaped = false;
        for (index, ch) in chars.iter().enumerate().skip(open) {
            if let Some(quote_ch) = quote {
                if escaped {
                    escaped = false;
                } else if *ch == '\\' {
                    escaped = true;
                } else if *ch == quote_ch {
                    quote = None;
                }
                continue;
            }
            match *ch {
                '\'' | '"' | '`' => quote = Some(*ch),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(chars[name_start..=index].iter().collect());
                    }
                }
                _ => {}
            }
        }

        Some(
            chars[name_start..]
                .iter()
                .collect::<String>()
                .trim()
                .to_string(),
        )
    }

    /// Determines whether a character is valid within a BT identifier.
    fn is_identifier_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
    }

    /// Creates a file-system object after resolving its path.
    fn create_fs(&self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        self.check_permission(Capability::Fs, ip)?;
        let path = self.required_path_arg(&args, 0, "fs() requires a path argument", ip)?;
        Ok(Value::Fs(BtFs::from_path(path)))
    }

    /// Creates a process object and resolves explicit program paths.
    fn create_process(&self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        self.check_permission(Capability::Process, ip)?;
        let program = args
            .first()
            .map(Value::to_string)
            .filter(|program| !program.is_empty())
            .ok_or_else(|| self.error(ip, "process() requires a program argument"))?;
        let program = if bt_path::is_process_program_path(&program) {
            bt_path::path_text(&self.resolve_path(&program))
        } else {
            program
        };
        BtProcess::new(vec![Value::Str(program)]).map_err(|message| self.error(ip, message))
    }

    /// Reads and resolves a required path argument.
    fn required_path_arg(
        &self,
        args: &[Value],
        index: usize,
        message: &str,
        ip: usize,
    ) -> Result<PathBuf, VmError> {
        let path = args
            .get(index)
            .map(Value::to_string)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| self.error(ip, message))?;
        Ok(self.resolve_path(&path))
    }

    /// Returns the arguments after resolving one path argument.
    fn with_resolved_path_arg(
        &self,
        mut args: Vec<Value>,
        index: usize,
        method: &str,
        ip: usize,
    ) -> Result<Vec<Value>, VmError> {
        let path = self.required_path_arg(
            &args,
            index,
            &format!("{} requires a path argument", method),
            ip,
        )?;
        args[index] = Value::Str(bt_path::path_text(&path));
        Ok(args)
    }

    /// Executes a string script and returns the script result.
    ///
    /// `eval()` shares the current VM's globals and function-owner mapping, so functions defined
    /// dynamically remain available to later `call()` and `env()` expressions. This path serves
    /// REPLs, debugging, and dynamic scripts; normal hot paths do not use it.
    fn call_eval(&mut self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        let source = args.first().map(Value::to_string).unwrap_or_default();
        if source.trim().is_empty() {
            return Ok(Value::Empty);
        }
        let file = "<eval>".to_string();
        let tokens: Vec<_> = tokenize(&source).collect();
        let mut parser = Parser::new(file.clone(), &source, tokens);
        let statements = parser
            .parse()
            .map_err(|err| self.error(ip, format!("eval script parsing failed: {}", err)))?;
        let chunk = Compiler::with_source_file(file.clone(), self.current_source_dir_path())
            .compile_returning_value(&statements)
            .map_err(|err| self.error(ip, format!("eval script compilation failed: {}", err)))?;
        match self.execute_chunk(&chunk, None, Some(file), None)? {
            ExecSignal::Return(value) | ExecSignal::Exit(value) => Ok(value),
            ExecSignal::Throw(value) => Err(self.throw_error(ip, value)),
            ExecSignal::Done => Ok(Value::Empty),
        }
    }

    /// Calls a function dynamically.
    ///
    /// Scripts should prefer `call('name', arg1, arg2)` over `env('name')(...)`: it keeps environment
    /// lookup separate from invocation. The first argument may also be a function value, which keeps
    /// higher-order calls straightforward.
    fn call_dynamic(
        &mut self,
        chunk: &Chunk,
        mut args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        if args.is_empty() {
            return Err(self.error(ip, "call() requires function name or function value"));
        }
        let callee = args.remove(0);
        let callable = match callee {
            Value::Str(name) => self
                .global_value_for_name(&name)
                .or_else(|| Self::native_function(&name))
                .ok_or_else(|| self.error(ip, format!("call() cannot find function `{}`", name)))?,
            Value::Function(_)
            | Value::BoundFunction(_, _)
            | Value::Closure(_, _, _)
            | Value::NativeFunction(_) => callee,
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(_) => callee,
            other => {
                return Err(self.error(
                    ip,
                    format!(
                        "call() first argument must be a function name or function value, got {}",
                        other.type_name()
                    ),
                ))
            }
        };
        self.call_value(chunk, &callable, args, ip)
    }

    /// Registers a one-time delayed callback.
    fn call_set_timeout(
        &mut self,
        chunk: &Chunk,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.call_set_timer("set_timeout", TimerKind::Timeout, chunk, args, ip)
    }

    /// Registers a fixed-delay repeating callback.
    fn call_set_interval(
        &mut self,
        chunk: &Chunk,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        self.call_set_timer("set_interval", TimerKind::Interval, chunk, args, ip)
    }

    /// Registers a shared implementation of timer callbacks.
    fn call_set_timer(
        &mut self,
        function_name: &str,
        kind: TimerKind,
        chunk: &Chunk,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        if self.web_response.is_some() {
            return Err(self.error(
                ip,
                format!(
                    "{}() cannot create a timer in the context of a web request",
                    function_name
                ),
            ));
        }
        let callback_value = args.first().cloned().ok_or_else(|| {
            self.error(
                ip,
                format!("{}() requires a function parameter", function_name),
            )
        })?;
        let callback = self
            .bind_callback_value(chunk, callback_value.clone())
            .ok_or_else(|| {
                self.error(
                    ip,
                    format!(
                        "{}() first argument must be a BT function, got {}",
                        function_name,
                        callback_value.type_name()
                    ),
                )
            })?;
        let delay_ms = Self::timer_delay_ms(kind, args.get(1));
        let sender = self.ensure_timer_sender();
        let (timer, due) =
            timer::register(kind, delay_ms, sender).map_err(|message| self.error(ip, message))?;
        self.timer_callbacks.insert(
            timer.id(),
            VmTimerCallback {
                kind,
                callback,
                timer: timer.clone(),
                delay_ms,
                next_due: Some(due),
                running: false,
            },
        );
        Ok(Value::Timer(timer))
    }

    /// Reads the timer delay parameters and applies default values and bounds.
    fn timer_delay_ms(kind: TimerKind, value: Option<&Value>) -> u64 {
        match kind {
            TimerKind::Timeout => value.map(Value::to_i64_lossy).unwrap_or(0).max(0) as u64,
            TimerKind::Interval => value.map(Value::to_i64_lossy).unwrap_or(1).max(1) as u64,
        }
    }

    /// Creates a background task.
    ///
    /// Here, all cross-thread boundary costs are concentrated at the `task(fn, ...args)` call point: the bytecode to which the function belongs, closure capture,
    /// Explicit parameters, required global values, and the project root are converted into shippable snapshots; no additional locks or atomic checks are required for the normal sync execution path.
    fn call_task(&mut self, chunk: &Chunk, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        let callee = args
            .first()
            .ok_or_else(|| self.error(ip, "task() requires a function parameter"))?;
        let task_args = self.snapshot_task_args(args.get(1..).unwrap_or_default(), ip)?;
        let snapshot = self.snapshot_task_function(chunk, callee, task_args, ip)?;
        let task = task::submit(move || Self::run_task_snapshot(snapshot))
            .map_err(|message| self.error(ip, message))?;
        Ok(Value::Task(task))
    }

    /// Waits for a set of tasks to complete and returns an array of results in the order entered.
    fn call_task_all(&self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        let tasks = self.task_array_arg(&args, "task_all()", ip)?;
        let mut outcomes = Vec::with_capacity(tasks.len());
        for task in &tasks {
            outcomes.push(task.wait());
        }
        for outcome in &outcomes {
            match outcome.as_ref() {
                TaskRunOutcome::Success(_) => {}
                TaskRunOutcome::Thrown(value) => return Err(self.throw_error(ip, value.to_value())),
                TaskRunOutcome::Failed(message) => {
                    return Err(self.error(ip, message.clone()));
                }
            }
        }
        let mut values = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            if let TaskRunOutcome::Success(value) = outcome.as_ref() {
                values.push(value.to_value());
            }
        }
        Ok(Value::Array(Rc::new(RefCell::new(values))))
    }

    /// Waits for the first task in a set of tasks to complete and returns its result.
    fn call_task_race(&self, args: Vec<Value>, ip: usize) -> Result<Value, VmError> {
        let tasks = self.task_array_arg(&args, "task_race()", ip)?;
        if tasks.is_empty() {
            return Ok(Value::Empty);
        }
        if let Some(outcome) = Self::first_completed_task_outcome(&tasks) {
            return self.task_outcome_to_value(&outcome, ip);
        }

        let (sender, receiver) = mpsc::sync_channel(tasks.len());
        let mut subscriptions = Vec::with_capacity(tasks.len());
        for (index, task) in tasks.iter().enumerate() {
            match task
                .subscribe(index, sender.clone())
                .map_err(|message| self.error(ip, message))?
            {
                Some(subscription) => subscriptions.push(subscription),
                None => {
                    for subscription in &subscriptions {
                        subscription.cancel();
                    }
                    return Self::first_completed_task_outcome(&tasks)
                        .map(|outcome| self.task_outcome_to_value(&outcome, ip))
                        .unwrap_or_else(|| Ok(Value::Empty));
                }
            }
        }
        drop(sender);

        if let Some(outcome) = Self::first_completed_task_outcome(&tasks) {
            for subscription in &subscriptions {
                subscription.cancel();
            }
            return self.task_outcome_to_value(&outcome, ip);
        }

        loop {
            let index = receiver.recv().map_err(|_| {
                self.error(ip, "task_race() failed to wait for task completion event")
            })?;
            if let Some(outcome) = tasks.get(index).and_then(BtTask::result) {
                for subscription in &subscriptions {
                    subscription.cancel();
                }
                return self.task_outcome_to_value(&outcome, ip);
            }
            if let Some(outcome) = Self::first_completed_task_outcome(&tasks) {
                for subscription in &subscriptions {
                    subscription.cancel();
                }
                return self.task_outcome_to_value(&outcome, ip);
            }
        }
    }

    /// Reads Task array parameters.
    fn task_array_arg(
        &self,
        args: &[Value],
        function_name: &str,
        ip: usize,
    ) -> Result<Vec<BtTask>, VmError> {
        let value = args.first().ok_or_else(|| {
            self.error(
                ip,
                format!("{} requires a Task array parameter", function_name),
            )
        })?;
        let Value::Array(values) = value else {
            return Err(self.error(
                ip,
                format!(
                    "{} argument must be an array, got {}",
                    function_name,
                    value.type_name()
                ),
            ));
        };
        let values = values.borrow();
        let mut tasks = Vec::with_capacity(values.len());
        for (index, value) in values.iter().enumerate() {
            match value {
                Value::Task(task) => tasks.push(task.clone()),
                other => {
                    return Err(self.error(
                        ip,
                        format!(
                            "{} item at index {} must be a Task, got {}",
                            function_name,
                            index,
                            other.type_name()
                        ),
                    ))
                }
            }
        }
        Ok(tasks)
    }

    /// Reads the first completed task result in the order entered.
    fn first_completed_task_outcome(tasks: &[BtTask]) -> Option<std::sync::Arc<TaskRunOutcome>> {
        tasks.iter().find_map(BtTask::result)
    }

    /// Snapshots explicit arguments passed to a task entry function.
    fn snapshot_task_args(
        &self,
        args: &[Value],
        ip: usize,
    ) -> Result<Vec<TaskValueSnapshot>, VmError> {
        let mut output = Vec::with_capacity(args.len());
        for (index, value) in args.iter().enumerate() {
            output.push(TaskValueSnapshot::from_value(value).map_err(|message| {
                self.error(
                    ip,
                    format!(
                        "Task argument {} cannot be snapshotted: {}",
                        index + 1,
                        message
                    ),
                )
            })?);
        }
        Ok(output)
    }

    /// Creates a background task entry snapshot.
    fn snapshot_task_function(
        &self,
        chunk: &Chunk,
        callee: &Value,
        args: Vec<TaskValueSnapshot>,
        ip: usize,
    ) -> Result<TaskFunctionSnapshot, VmError> {
        let (function_id, owner, captures) = match callee {
            Value::Function(function_id) => (*function_id, chunk, None),
            Value::BoundFunction(function_id, owner) => (*function_id, owner.as_ref(), None),
            Value::Closure(function_id, owner, captures) => {
                (*function_id, owner.as_ref(), Some(captures.as_ref()))
            }
            other => {
                return Err(self.error(
                    ip,
                    format!(
                        "task() first argument must be a BT function, got {}",
                        other.type_name()
                    ),
                ))
            }
        };
        if owner.functions.get(function_id).is_none() {
            return Err(self.error(
                ip,
                format!("task() function number {} does not exist", function_id),
            ));
        }

        let owner_snapshot =
            TaskChunkSnapshot::from_chunk(owner).map_err(|message| self.error(ip, message))?;
        let captures = captures
            .map(|captures| Self::snapshot_task_captures(captures.as_slice()))
            .transpose()
            .map_err(|message| self.error(ip, message))?;
        let (globals, global_constants) = self
            .snapshot_task_globals(owner, function_id)
            .map_err(|message| self.error(ip, message))?;

        Ok(TaskFunctionSnapshot {
            function_id,
            owner: owner_snapshot,
            captures,
            args,
            globals,
            global_constants,
            project_root: self.project_root.clone(),
        })
    }

    /// Snapshot closure capture slot.
    fn snapshot_task_captures(
        captures: &[Option<LocalCell>],
    ) -> Result<TaskCaptureScopeSnapshot, String> {
        let mut slots = Vec::with_capacity(captures.len());
        for cell in captures {
            let slot = match cell {
                Some(cell) => match cell.borrow().as_ref() {
                    Some(value) => Some(TaskCaptureSnapshot::Value(
                        TaskValueSnapshot::from_value(value).map_err(|message| {
                            format!(
                                "Task closure capture value cannot be snapshotted: {}",
                                message
                            )
                        })?,
                    )),
                    None => Some(TaskCaptureSnapshot::Uninitialized),
                },
                None => None,
            };
            slots.push(slot);
        }
        Ok(TaskCaptureScopeSnapshot { slots })
    }

    /// Global variables that may be accessed by the snapshot task function.
    fn snapshot_task_globals(
        &self,
        owner: &Chunk,
        function_id: usize,
    ) -> Result<(Vec<(String, TaskValueSnapshot)>, Vec<String>), String> {
        let function = owner
            .functions
            .get(function_id)
            .ok_or_else(|| format!("task function number {} does not exist", function_id))?;
        let mut names = HashSet::new();
        Self::collect_task_global_symbols(&function.chunk, &mut names);

        let mut globals = Vec::new();
        let mut global_constants = Vec::new();
        for name in names {
            if self.global_constants.contains(&name) {
                global_constants.push(name.clone());
            }
            let Some(value) = self.global_value_for_name(&name) else {
                continue;
            };
            globals.push((
                name.clone(),
                TaskValueSnapshot::from_value(&value).map_err(|message| {
                    format!(
                        "task global variable `{}` cannot be snapshotted: {}",
                        name, message
                    )
                })?,
            ));
        }
        Ok((globals, global_constants))
    }

    /// Recursively collects symbol names in function blocks that may fall back to the global environment.
    fn collect_task_global_symbols(chunk: &Chunk, names: &mut HashSet<String>) {
        for index in 0..chunk.symbols.len() {
            let symbol = index as SymbolId;
            if chunk.is_local(symbol) {
                continue;
            }
            if let Some(name) = chunk.symbols.name(symbol) {
                names.insert(name.to_string());
            }
        }
        for function in &chunk.functions {
            Self::collect_task_global_symbols(&function.chunk, names);
        }
    }

    /// Performs task snapshots in a background thread.
    fn run_task_snapshot(snapshot: TaskFunctionSnapshot) -> TaskRunOutcome {
        let owner = Rc::new(snapshot.owner.to_chunk());
        let mut vm = Vm::with_project_root(snapshot.project_root);
        for (name, value) in snapshot.globals {
            vm.globals.insert(name, value.to_value());
        }
        for name in snapshot.global_constants {
            vm.global_constants.insert(name);
        }
        let captures = snapshot.captures.map(Self::restore_task_captures);
        let args = snapshot
            .args
            .into_iter()
            .map(|value| value.to_value())
            .collect();
        let result =
            vm.call_user_function_inner(&owner, snapshot.function_id, args, None, captures, 0);
        vm.flush_output_to_stdout();
        match result {
            Ok(value) => TaskValueSnapshot::from_value(&value)
                .map(TaskRunOutcome::Success)
                .unwrap_or_else(|message| {
                    TaskRunOutcome::Failed(format!(
                        "The task return value cannot be snapshotted: {}",
                        message
                    ))
                }),
            Err(err) => {
                if let Some(value) = err.throw_value {
                    TaskValueSnapshot::from_value(&value)
                        .map(TaskRunOutcome::Thrown)
                        .unwrap_or_else(|message| {
                            TaskRunOutcome::Failed(format!(
                                "task throw value cannot be snapshotted: {}",
                                message
                            ))
                        })
                } else {
                    TaskRunOutcome::Failed(err.to_string())
                }
            }
        }
    }

    /// Restores the closure capture slot to local scope within the current background VM.
    fn restore_task_captures(snapshot: TaskCaptureScopeSnapshot) -> LocalScope {
        snapshot
            .slots
            .into_iter()
            .map(|slot| {
                slot.map(|slot| {
                    Rc::new(RefCell::new(match slot {
                        TaskCaptureSnapshot::Uninitialized => None,
                        TaskCaptureSnapshot::Value(value) => Some(value.to_value()),
                    }))
                })
            })
            .collect()
    }

    /// Restores background task results to return values or errors visible to the current VM.
    fn task_outcome_to_value(&self, outcome: &TaskRunOutcome, ip: usize) -> Result<Value, VmError> {
        match outcome {
            TaskRunOutcome::Success(value) => Ok(value.to_value()),
            TaskRunOutcome::Thrown(value) => Err(self.throw_error(ip, value.to_value())),
            TaskRunOutcome::Failed(message) => Err(self.error(ip, message.clone())),
        }
    }

    /// Introduces and executes BT files during runtime.
    ///
    /// Compilation can expand only literal include paths. Web projects often construct paths at
    /// runtime, so this path resolves the argument, compiles the file, and executes it in the current globals.
    fn call_include(&mut self, args: Vec<Value>, ip: usize, once: bool) -> Result<Value, VmError> {
        let function_name = if once { "include_once" } else { "include" };
        let path = args
            .first()
            .map(Value::to_string)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                self.error(
                    ip,
                    format!("{}() requires a file path argument", function_name),
                )
            })?;
        let resolved = self.resolve_path(&path);
        if once {
            let once_key = fs::canonicalize(&resolved).map_err(|err| {
                self.error(
                    ip,
                    format!("Failed to resolve include_once file `{}`: {}", path, err),
                )
            })?;
            if !self.include_once_files.borrow_mut().insert(once_key) {
                return Ok(Value::Empty);
            }
        }
        let display_path = bt_path::path_text(&resolved);
        let chunk = compile_cached_file(&resolved, true).map_err(|err| {
            self.error(
                ip,
                format!(
                    "{} failed to compile file `{}`: {}",
                    function_name, path, err
                ),
            )
        })?;
        match self.execute_chunk(
            chunk.as_ref(),
            None,
            Some(format!("{}:{}", function_name, display_path)),
            Some(chunk.clone()),
        )? {
            ExecSignal::Return(value) => Ok(value),
            ExecSignal::Exit(value) => Ok(value),
            ExecSignal::Throw(value) => Err(self.throw_error(ip, value)),
            ExecSignal::Done => Ok(Value::Empty),
        }
    }

    /// Calls the callback in the array higher-order function.
    fn call_callback(
        &mut self,
        chunk: &Chunk,
        callback: &Value,
        args: Vec<Value>,
        ip: usize,
    ) -> Result<Value, VmError> {
        match callback {
            Value::Function(function_id) => self.call_user_function(chunk, *function_id, args, ip),
            Value::BoundFunction(function_id, owner) => {
                self.call_user_function(owner, *function_id, args, ip)
            }
            Value::Closure(function_id, owner, captures) => self.call_user_function_inner(
                owner,
                *function_id,
                args,
                None,
                Some(captures.as_ref().clone()),
                ip,
            ),
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(function) => self.call_extension_function(function, args, ip),
            _ => Ok(Value::Empty),
        }
    }

    /// Calls the background event callback and isolates the impact of `exit()` on subsequent events.
    fn call_event_callback(
        &mut self,
        chunk: &Chunk,
        callback: &Value,
        args: Vec<Value>,
        label: &str,
    ) {
        let previous_exit = self.exit_value.take();
        let result = self.with_execution_context(|vm| vm.call_callback(chunk, callback, args, 0));
        self.exit_value = previous_exit;
        if let Err(err) = result {
            eprintln!("{} Callback execution failed: {}", label, err);
        }
        self.flush_output_to_stdout();
    }

    /// Reads the register.
    fn read_register(
        registers: &[Value],
        register: Register,
        ip: usize,
    ) -> Result<&Value, VmError> {
        registers.get(register as usize).ok_or_else(|| VmError {
            ip,
            message: format!("read register r{} out of bounds", register),
            span: None,
            function: None,
            throw_value: None,
        })
    }

    /// Reads the symbol name according to the symbol pool of the current chunk.
    fn symbol_name(chunk: &Chunk, symbol: SymbolId, ip: usize) -> Result<&str, VmError> {
        chunk.symbols.name(symbol).ok_or_else(|| VmError {
            ip,
            message: format!("symbol number {} does not exist", symbol),
            span: None,
            function: None,
            throw_value: None,
        })
    }

    /// Write register.
    fn write_register(
        registers: &mut [Value],
        register: Register,
        value: Value,
        ip: usize,
    ) -> Result<(), VmError> {
        let slot = registers
            .get_mut(register as usize)
            .ok_or_else(|| VmError {
                ip,
                message: format!("writes to register r{} out of bounds", register),
                span: None,
                function: None,
                throw_value: None,
            })?;
        *slot = value;
        Ok(())
    }

    /// Reads the register value source.
    fn read_origin(origins: &[Option<ValueOrigin>], register: Register) -> Option<&ValueOrigin> {
        origins.get(register as usize).and_then(Option::as_ref)
    }

    /// Write register value source.
    fn write_origin(
        origins: &mut [Option<ValueOrigin>],
        register: Register,
        origin: Option<ValueOrigin>,
        ip: usize,
    ) -> Result<(), VmError> {
        let slot = origins.get_mut(register as usize).ok_or_else(|| VmError {
            ip,
            message: format!("write register source r{} out of bounds", register),
            span: None,
            function: None,
            throw_value: None,
        })?;
        *slot = origin;
        Ok(())
    }

    /// Determines whether the register value comes from `this` of the current function.
    fn is_this_origin(origin: Option<&ValueOrigin>) -> bool {
        matches!(
            origin.and_then(|origin| origin.variable.as_deref()),
            Some("this")
        )
    }

    /// Constructs an ordinary expression source using the current instruction position.
    fn span_origin(&self) -> Option<ValueOrigin> {
        self.current_span.clone().map(|span| ValueOrigin {
            span,
            variable: None,
            missing: false,
        })
    }

    /// Constructs a variable read source using the current instruction position.
    fn variable_origin(&self, variable: String, missing: bool) -> Option<ValueOrigin> {
        self.current_span.clone().map(|span| ValueOrigin {
            span,
            variable: Some(variable),
            missing,
        })
    }

    /// Constructs a binary arithmetic error and tries to point the location to the actual problematic operand.
    ///
    /// `Value` only knows that "a certain value cannot participate in the operation", but does not know which source code token this value comes from;
    /// VM fills in this part of the context through the register source, giving priority to prompting the variable name and the column where the variable is located.
    fn binary_error(
        &self,
        ip: usize,
        op: &TokenKind,
        left: &Value,
        right: &Value,
        left_origin: Option<&ValueOrigin>,
        right_origin: Option<&ValueOrigin>,
        fallback: String,
    ) -> VmError {
        let invalid_operand =
            Self::invalid_numeric_operand(op, left, right, left_origin, right_origin);
        if let Some((value, origin)) = invalid_operand {
            if let Some(variable) = &origin.variable {
                let value = value.to_string();
                let requirement = if fallback.contains("integer") {
                    "integer"
                } else {
                    "Number"
                };
                let message = if origin.missing {
                    format!(
                        "variable `{}` is undefined and reads as `empty`, which cannot be used as {}; define and assign the variable first",
                        variable, requirement
                    )
                } else {
                    format!(
                        "variable `{}` is `{}` and cannot be used as {}; ensure it contains a {} value",
                        variable, value, requirement, requirement
                    )
                };
                return self.error_at(ip, message, origin.span.clone());
            }
            return self.error_at(ip, fallback, origin.span.clone());
        }
        self.error(ip, fallback)
    }

    /// Find the first operand and value source in a numerical operation that cannot be used as a number.
    fn invalid_numeric_operand<'a>(
        op: &TokenKind,
        left: &'a Value,
        right: &'a Value,
        left_origin: Option<&'a ValueOrigin>,
        right_origin: Option<&'a ValueOrigin>,
    ) -> Option<(&'a Value, &'a ValueOrigin)> {
        if matches!(op, TokenKind::Plus)
            && (matches!(left, Value::Str(_)) || matches!(right, Value::Str(_)))
        {
            return None;
        }
        if !Self::is_numeric_operand(left) {
            return left_origin.map(|origin| (left, origin));
        }
        if !Self::is_numeric_operand(right) {
            return right_origin.map(|origin| (right, origin));
        }
        None
    }

    /// Determines whether the value can directly participate in the numerical operation of the current VM.
    fn is_numeric_operand(value: &Value) -> bool {
        matches!(value, Value::Int(_) | Value::Float(_) | Value::Bool(_))
    }

    /// Construct run error.
    fn error(&self, ip: usize, message: impl Into<String>) -> VmError {
        VmError {
            ip,
            message: message.into(),
            span: self.current_span.clone(),
            function: self.current_function.clone(),
            throw_value: None,
        }
    }

    /// Uncaught throw error in construction script.
    fn throw_error(&self, ip: usize, value: Value) -> VmError {
        VmError {
            ip,
            message: format!("Uncaught exception: {}", value.to_output_string()),
            span: self.current_span.clone(),
            function: self.current_function.clone(),
            throw_value: Some(value),
        }
    }

    /// Constructs a runtime error at the specified source code location.
    fn error_at(&self, ip: usize, message: impl Into<String>, span: SourceSpan) -> VmError {
        VmError {
            ip,
            message: message.into(),
            span: Some(span),
            function: self.current_function.clone(),
            throw_value: None,
        }
    }
}

impl Drop for Vm {
    /// Cancel active timer and task subscriptions still held by the current VM when the VM is released.
    fn drop(&mut self) {
        for id in self.timer_callbacks.keys().copied().collect::<Vec<_>>() {
            timer::cancel(id);
        }
        self.timer_callbacks.clear();
        self.task_callbacks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::CompileError;
    use crate::parser::ParseError;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Temporary BT project directory for testing.
    struct TempProject {
        /// Project root directory.
        root: PathBuf,
    }

    impl Drop for TempProject {
        /// Clean up the temporary project directory after the test.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Creates a unique test project directory.
    fn fresh_temp_project(name: &str) -> TempProject {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("bt-path-{}-{}-{}", name, std::process::id(), stamp));
        fs::create_dir_all(&root).expect("test project directory should be created successfully");
        TempProject { root }
    }

    /// Writes UTF-8 test files.
    fn write_text(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .expect("test parent directory should be created successfully");
        }
        fs::write(path, text).expect("test file should be written successfully");
    }

    /// Is written to `calc.bts` for pure BT extended access testing.
    #[cfg(feature = "extensions")]
    fn write_calc_extension(project_root: &Path) {
        use std::io::Write;

        let extension_dir = project_root.join("extensions");
        fs::create_dir_all(&extension_dir)
            .expect("test extension directory should be created successfully");
        let file = fs::File::create(extension_dir.join("calc.bts")).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("manifest.json", options).unwrap();
        writer
            .write_all(
                br#"{
                    "format": "bts",
                    "format_version": 1,
                    "name": "calc_pkg",
                    "version": "1.0.0",
                    "kind": "bt",
                    "abi": "bts-bt-1",
                    "bt_min_version": "1.1.0",
                    "api_version": 1,
                    "entry": "src/lib.bt",
                    "bindings": "bindings.json",
                    "permissions": []
                }"#,
            )
            .unwrap();
        writer.start_file("bindings.json", options).unwrap();
        writer
            .write_all(
                br#"{
                    "api_version": 1,
                    "functions": [
                        {
                            "name": "calc",
                            "id": 1,
                            "params": [{ "name": "value", "type": "int" }],
                            "returns": "Calc"
                        }
                    ],
                    "objects": [
                        {
                            "name": "Calc",
                            "type_id": 1,
                            "methods": [
                                {
                                    "name": "add",
                                    "id": 2,
                                    "params": [{ "name": "value", "type": "int" }],
                                    "returns": "Calc"
                                },
                                {
                                    "name": "value",
                                    "id": 3,
                                    "params": [],
                                    "returns": "int"
                                },
                                {
                                    "name": "close",
                                    "id": 4,
                                    "params": [],
                                    "returns": "bool",
                                    "lifecycle": "dispose"
                                }
                            ]
                        }
                    ]
                }"#,
            )
            .unwrap();
        writer.start_file("src/lib.bt", options).unwrap();
        writer
            .write_all(
                br#"
                class Calc {
                    value_num: 0

                    new(value) {
                        this.value_num = value
                        this
                    }

                    pub add(value) {
                        this.value_num += value
                        this
                    }

                    pub value() {
                        this.value_num
                    }

                    pub close() {
                        true
                    }
                }

                fn calc(value) {
                    Calc::new(value)
                }
                "#,
            )
            .unwrap();
        writer.finish().unwrap();
    }

    /// Writes to `file_demo.bts` for WASM file access testing.
    #[cfg(feature = "extensions")]
    fn write_file_demo_extension(project_root: &Path) {
        use std::io::Write;

        let extension_dir = project_root.join("extensions");
        fs::create_dir_all(&extension_dir)
            .expect("test extension directory should be created successfully");
        let file = fs::File::create(extension_dir.join("file_demo.bts")).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("manifest.json", options).unwrap();
        writer
            .write_all(
                br#"{
                    "format": "bts",
                    "format_version": 1,
                    "name": "file_demo_pkg",
                    "version": "1.0.0",
                    "kind": "wasm",
                    "abi": "bts-wasi-1",
                    "bt_min_version": "1.1.0",
                    "api_version": 1,
                    "entry": "module.wasm",
                    "bindings": "bindings.json",
                    "permissions": ["fs_read", "fs_write"],
                    "limits": {
                        "max_args_bytes": 4096,
                        "max_result_bytes": 4096
                    }
                }"#,
            )
            .unwrap();
        writer.start_file("bindings.json", options).unwrap();
        writer
            .write_all(
                br#"{
                    "api_version": 1,
                    "functions": [
                        {
                            "name": "file_demo",
                            "id": 1,
                            "params": [
                                { "name": "input", "type": "string", "role": "path_read" }
                            ],
                            "returns": "FileDemo"
                        }
                    ],
                    "objects": [
                        {
                            "name": "FileDemo",
                            "type_id": 1,
                            "methods": [
                                {
                                    "name": "copy_to",
                                    "id": 2,
                                    "params": [
                                        { "name": "output", "type": "string", "role": "path_write" }
                                    ],
                                    "returns": "bool"
                                }
                            ]
                        }
                    ]
                }"#,
            )
            .unwrap();
        writer.start_file("module.wasm", options).unwrap();
        let wasm = wat::parse_str(
            r#"
            (module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open
                        (param i32 i32 i32 i32 i32 i64 i64 i32 i32)
                        (result i32)
                    )
                )
                (import "wasi_snapshot_preview1" "fd_read"
                    (func $fd_read (param i32 i32 i32 i32) (result i32))
                )
                (import "wasi_snapshot_preview1" "fd_write"
                    (func $fd_write (param i32 i32 i32 i32) (result i32))
                )
                (import "wasi_snapshot_preview1" "fd_close"
                    (func $fd_close (param i32) (result i32))
                )
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 16384))
                (global $input_len (mut i32) (i32.const 0))
                (data (i32.const 16) "\00\09\00\00\00\00\00\00\00\00\01\00\00\00\01\00\00\00\00\00\00\00\08\00\00\00FileDemo")
                (data (i32.const 80) "\00\02\01")
                (data (i32.const 84) "\00\02\00")
                (func (export "bts_alloc") (param $len i32) (result i32)
                    (local $ptr i32)
                    global.get $heap
                    local.set $ptr
                    global.get $heap
                    local.get $len
                    i32.add
                    global.set $heap
                    local.get $ptr
                )
                (func (export "bts_free") (param i32) (param i32))
                (func $store_input_path (param $args_ptr i32)
                    local.get $args_ptr
                    i32.const 6
                    i32.add
                    i32.load
                    global.set $input_len
                    i32.const 4096
                    local.get $args_ptr
                    i32.const 10
                    i32.add
                    global.get $input_len
                    memory.copy
                )
                (func $copy_file (param $out_ptr i32) (param $out_len i32) (result i32)
                    (local $in_fd i32)
                    (local $out_fd i32)
                    (local $nread i32)
                    i32.const 3
                    i32.const 0
                    i32.const 4096
                    global.get $input_len
                    i32.const 0
                    i64.const 2
                    i64.const 0
                    i32.const 0
                    i32.const 512
                    call $path_open
                    i32.const 0
                    i32.ne
                    if
                        i32.const 0
                        return
                    end
                    i32.const 512
                    i32.load
                    local.set $in_fd
                    i32.const 3
                    i32.const 0
                    local.get $out_ptr
                    local.get $out_len
                    i32.const 9
                    i64.const 64
                    i64.const 0
                    i32.const 0
                    i32.const 516
                    call $path_open
                    i32.const 0
                    i32.ne
                    if
                        local.get $in_fd
                        call $fd_close
                        drop
                        i32.const 0
                        return
                    end
                    i32.const 516
                    i32.load
                    local.set $out_fd
                    i32.const 600
                    i32.const 8192
                    i32.store
                    i32.const 604
                    i32.const 4096
                    i32.store
                    local.get $in_fd
                    i32.const 600
                    i32.const 1
                    i32.const 608
                    call $fd_read
                    i32.const 0
                    i32.ne
                    if
                        local.get $in_fd
                        call $fd_close
                        drop
                        local.get $out_fd
                        call $fd_close
                        drop
                        i32.const 0
                        return
                    end
                    i32.const 608
                    i32.load
                    local.set $nread
                    i32.const 612
                    i32.const 8192
                    i32.store
                    i32.const 616
                    local.get $nread
                    i32.store
                    local.get $out_fd
                    i32.const 612
                    i32.const 1
                    i32.const 620
                    call $fd_write
                    i32.const 0
                    i32.ne
                    if
                        local.get $in_fd
                        call $fd_close
                        drop
                        local.get $out_fd
                        call $fd_close
                        drop
                        i32.const 0
                        return
                    end
                    local.get $in_fd
                    call $fd_close
                    drop
                    local.get $out_fd
                    call $fd_close
                    drop
                    i32.const 1
                )
                (func (export "bts_call") (param $id i32) (param $args_ptr i32) (param $args_len i32) (result i64)
                    (local $name_len i32)
                    (local $out_tag i32)
                    (local $out_len i32)
                    local.get $id
                    i32.const 1
                    i32.eq
                    if (result i64)
                        local.get $args_ptr
                        call $store_input_path
                        i64.const 68719476770
                    else
                        local.get $id
                        i32.const 2
                        i32.eq
                        if (result i64)
                            local.get $args_ptr
                            i32.const 26
                            i32.add
                            i32.load
                            local.set $name_len
                            local.get $args_ptr
                            i32.const 30
                            i32.add
                            local.get $name_len
                            i32.add
                            local.set $out_tag
                            local.get $out_tag
                            i32.const 1
                            i32.add
                            i32.load
                            local.set $out_len
                            local.get $out_tag
                            i32.const 5
                            i32.add
                            local.get $out_len
                            call $copy_file
                            if (result i64)
                                i64.const 343597383683
                            else
                                i64.const 360777252867
                            end
                        else
                            i64.const 360777252867
                        end
                    end
                )
            )
            "#,
        )
        .expect("test WASM WAT should compile successfully");
        writer.write_all(&wasm).unwrap();
        writer.finish().unwrap();
    }

    /// Writes a WASM extension used to verify VM host-level release interception.
    #[cfg(feature = "extensions")]
    fn write_wasm_dispose_extension(project_root: &Path) {
        use std::io::Write;

        let extension_dir = project_root.join("extensions");
        fs::create_dir_all(&extension_dir)
            .expect("test extension directory should be created successfully");
        let file = fs::File::create(extension_dir.join("wasm_dispose.bts")).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("manifest.json", options).unwrap();
        writer
            .write_all(
                br#"{
                    "format": "bts",
                    "format_version": 1,
                    "name": "wasm_dispose",
                    "version": "1.0.0",
                    "kind": "wasm",
                    "abi": "bts-wasi-1",
                    "bt_min_version": "1.1.0",
                    "api_version": 1,
                    "entry": "module.wasm",
                    "bindings": "bindings.json",
                    "permissions": []
                }"#,
            )
            .unwrap();
        writer.start_file("bindings.json", options).unwrap();
        writer
            .write_all(
                br#"{
                    "api_version": 1,
                    "functions": [
                        {
                            "name": "wasm_dispose",
                            "id": 1,
                            "params": [],
                            "returns": "Calc"
                        }
                    ],
                    "objects": [
                        {
                            "name": "Calc",
                            "type_id": 1,
                            "methods": [
                                {
                                    "name": "value",
                                    "id": 2,
                                    "params": [],
                                    "returns": "int"
                                },
                                {
                                    "name": "close",
                                    "id": 3,
                                    "params": [],
                                    "returns": "bool",
                                    "lifecycle": "dispose"
                                }
                            ]
                        }
                    ]
                }"#,
            )
            .unwrap();
        writer.start_file("module.wasm", options).unwrap();
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 1024))
                (func (export "bts_alloc") (param $len i32) (result i32)
                    (local $ptr i32)
                    global.get $heap
                    local.set $ptr
                    global.get $heap
                    local.get $len
                    i32.add
                    global.set $heap
                    local.get $ptr
                )
                (func (export "bts_free") (param i32) (param i32))
                (data (i32.const 16) "\00\09\00\00\00\00\00\00\00\00\01\00\00\00\0a\00\00\00\00\00\00\00\04\00\00\00Calc")
                (data (i32.const 64) "\00\03\07\00\00\00\00\00\00\00")
                (data (i32.const 96) "\00\02\01")
                (func (export "bts_call") (param $id i32) (param i32) (param i32) (result i64)
                    local.get $id
                    i32.const 2
                    i32.eq
                    if (result i64)
                        i64.const 274877906954
                    else
                        local.get $id
                        i32.const 3
                        i32.eq
                        if (result i64)
                            i64.const 412316860419
                        else
                            i64.const 68719476766
                        end
                    end
                )
            )
            "#,
        )
        .expect("release test WASM WAT should compile successfully");
        writer.write_all(&wasm).unwrap();
        writer.finish().unwrap();
    }

    /// Compiles and executes the extended test source code in a temporary project.
    #[cfg(feature = "extensions")]
    fn run_extension_project_source(
        project_root: &Path,
        source: &str,
    ) -> Result<(String, Value), VmError> {
        let source_file = project_root.join("main.bt");
        write_text(&source_file, source);
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(bt_path::path_text(&source_file), source, tokens);
        let statements = parser
            .parse()
            .expect("extended test script should parse successfully");
        let chunk = Compiler::with_source_file(bt_path::path_text(&source_file), project_root)
            .compile_returning_value(&statements)
            .expect("extended test script should compile successfully");
        let mut vm = Vm::with_project_root(project_root);
        vm.load_project_extensions()
            .expect("test extension should load successfully");
        vm.run_with_value(&chunk)
    }

    /// Loads the calc extension in the temporary project and executes the test source code.
    #[cfg(feature = "extensions")]
    fn run_extension_project(name: &str, source: &str) -> Result<(String, Value), VmError> {
        let project = fresh_temp_project(name);
        write_calc_extension(&project.root);
        run_extension_project_source(&project.root, source)
    }

    /// Compiles and executes a test script and returns the value of the last expression.
    fn run_test_source(source: &str) -> Value {
        run_test_source_with_output(source).1
    }

    /// Compiles and executes a test script, returning the output buffer and the value of the last expression.
    fn run_test_source_with_output(source: &str) -> (String, Value) {
        let file = "test.bt".to_string();
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        let chunk = Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect("test script should compile successfully");
        let mut vm = Vm::new();
        vm.run_with_value(&chunk)
            .expect("test script should execute successfully")
    }

    /// Compiles a test script into ordinary entry bytecode.
    fn compile_test_entry(source: &str) -> Chunk {
        let file = "test.bt".to_string();
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        Compiler::with_source_file(file, Path::new("."))
            .compile(&statements)
            .expect("test script should compile successfully")
    }

    /// Executes the test entry script but does not wait for background events.
    fn run_test_entry(source: &str) -> (Vm, Chunk) {
        let chunk = compile_test_entry(source);
        let mut vm = Vm::new();
        vm.run(&chunk)
            .expect("test script should execute successfully");
        vm.clear_output();
        (vm, chunk)
    }

    /// Executes the test entry script and waits for the background event to complete.
    fn run_test_entry_with_background(source: &str) -> Vm {
        let (mut vm, chunk) = run_test_entry(source);
        vm.wait_for_background_events(&chunk)
            .expect("background event should be executed successfully");
        vm
    }

    /// Compiles and executes a test script and returns a running error.
    fn run_test_source_error(source: &str) -> VmError {
        let file = "test.bt".to_string();
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        let chunk = Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect("test script should compile successfully");
        let mut vm = Vm::new();
        vm.run_with_value(&chunk)
            .expect_err("test script should fail to execute")
    }

    /// Executes the entry script in the context of a web request and returns a runtime error.
    fn run_web_entry_error(source: &str) -> VmError {
        let chunk = compile_test_entry(source);
        let mut vm = Vm::new();
        vm.set_web_response(Rc::new(RefCell::new(BtWebResponse::new())));
        vm.run(&chunk)
            .expect_err("Web request script should fail to execute")
    }

    /// Compiles a test script and returns a compilation error.
    fn compile_test_source_error(source: &str) -> CompileError {
        let file = "test.bt".to_string();
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect_err("test script should fail to compile")
    }

    /// Parses a test script and returns a syntax error.
    fn parse_test_source_error(source: &str) -> ParseError {
        let file = "test.bt".to_string();
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file, source, tokens);
        parser
            .parse()
            .expect_err("test script should fail to parse")
    }

    /// Compiles and executes the temporary project entry and returns a running error.
    fn run_temp_project_error(name: &str, source: &str) -> VmError {
        let project = fresh_temp_project(name);
        let entry = project.root.join("main.bt");
        write_text(&entry, source);
        let chunk = compile_cached_file(&entry, false)
            .expect("entry script should be compiled successfully");
        let mut vm = Vm::with_project_root(&project.root);
        vm.run_with_value_owned(chunk)
            .expect_err("test script should fail to execute")
    }

    /// Constant naming, duplicate definitions and compound assignments should be rejected at compile time.
    #[test]
    fn constant_rules_reject_invalid_or_repeated_bindings() {
        let repeated = compile_test_source_error("Name = 'A'\nName = 'B'");
        assert!(repeated
            .message
            .contains("Constant `Name` is already defined"));

        let valid = run_test_source("Name_1 = 1\nName_1 + 1");
        assert_eq!(valid, Value::Int(2));

        let invalid = compile_test_source_error("User$ = 1");
        assert!(invalid.message.contains("[A-Z][A-Za-z0-9_]*"));

        let let_constant = compile_test_source_error("let Name = 1");
        assert!(let_constant
            .message
            .contains("cannot start with an uppercase letter"));

        let declare_constant = compile_test_source_error("Name");
        assert!(declare_constant
            .message
            .contains("cannot start with an uppercase letter"));

        let use_import = compile_test_source_error("obj = {Name: 1}\nuse obj{Name}");
        assert!(use_import
            .message
            .contains("cannot start with an uppercase letter"));

        let param = compile_test_source_error("fn read(Name) { Name }");
        assert!(param
            .message
            .contains("cannot start with an uppercase letter"));

        let compound = compile_test_source_error("Total += 1");
        assert!(compound
            .message
            .contains("Constant `Total` cannot use Compound assignment"));
    }

    /// All ten compound assignments reuse binary-operation semantics on writable targets, returning and storing the new value.
    #[test]
    fn compound_assignments_cover_all_operators_and_writable_targets() {
        let operators = [
            ("+=", "10", "3", "13"),
            ("+=", "1.5", "0.25", "1.75"),
            ("+=", "'BT'", "' Lang'", "'BT Lang'"),
            ("-=", "10", "3", "7"),
            ("*=", "10", "3", "30"),
            ("/=", "10", "2", "5.0"),
            ("%=", "10", "3", "1"),
            ("<<=", "8", "2", "32"),
            (">>=", "8", "2", "2"),
            ("&=", "14", "11", "10"),
            ("^=", "14", "11", "5"),
            ("|=", "10", "5", "15"),
        ];
        let targets = [
            (
                "global",
                r#"
value = $INITIAL
result = (value $OP $RHS)
result === value && value === $EXPECTED
"#,
            ),
            (
                "local",
                r#"
fn run() {
    let value = $INITIAL
    let result = (value $OP $RHS)
    result === value && value === $EXPECTED
}
run()
"#,
            ),
            (
                "parameter",
                r#"
fn run(value) {
    let result = (value $OP $RHS)
    result === value && value === $EXPECTED
}
run($INITIAL)
"#,
            ),
            (
                "closure",
                r#"
fn run() {
    let value = $INITIAL
    fn mutate() {
        value $OP $RHS
    }
    let result = mutate()
    result === value && value === $EXPECTED
}
run()
"#,
            ),
            (
                "object-field",
                r#"
item = {value: $INITIAL}
result = (item.value $OP $RHS)
result === item.value && item.value === $EXPECTED
"#,
            ),
            (
                "dynamic-field",
                r#"
key = 'value'
item = {value: $INITIAL}
result = (item[key] $OP $RHS)
result === item.value && item.value === $EXPECTED
"#,
            ),
            (
                "array-index",
                r#"
items = [$INITIAL]
result = (items[0] $OP $RHS)
result === items[0] && items[0] === $EXPECTED
"#,
            ),
            (
                "nested-field",
                r#"
root = {child: {value: $INITIAL}}
result = (root.child.value $OP $RHS)
result === root.child.value && root.child.value === $EXPECTED
"#,
            ),
            (
                "public-instance-field",
                r#"
class Counter {
    pub value: $INITIAL
    new() { this }
}
item = Counter::new()
result = (item.value $OP $RHS)
result === item.value && item.value === $EXPECTED
"#,
            ),
            (
                "private-this-field",
                r#"
class Counter {
    value: $INITIAL
    new() { this }
    pub mutate() { this.value $OP $RHS }
    pub read() { this.value }
}
item = Counter::new()
result = item.mutate()
result === item.read() && item.read() === $EXPECTED
"#,
            ),
        ];

        for (operator, initial, rhs, expected) in operators {
            for (target_name, template) in targets {
                let source = template
                    .replace("$INITIAL", initial)
                    .replace("$OP", operator)
                    .replace("$RHS", rhs)
                    .replace("$EXPECTED", expected);
                assert_eq!(
                    run_test_source(&source),
                    Value::Bool(true),
                    "composite assignment matrix failed: operator={operator}, target={target_name}"
                );
            }
        }
    }

    /// The pre- and post-increment and self-decrement should cover all writable targets and strictly distinguish between old value and new value return semantics.
    #[test]
    fn increment_and_decrement_cover_writable_targets_and_return_values() {
        let targets = [
            (
                "global",
                r#"
value = 10
$ASSERTIONS
"#,
            ),
            (
                "local",
                r#"
fn run() {
    let value = 10
    $ASSERTIONS
}
run()
"#,
            ),
            (
                "parameter",
                r#"
fn run(value) {
    $ASSERTIONS
}
run(10)
"#,
            ),
            (
                "closure",
                r#"
fn run() {
    let value = 10
    fn post_inc() { value++ }
    fn pre_inc() { ++value }
    fn post_dec() { value-- }
    fn pre_dec() { --value }
    let a = post_inc()
    let b = pre_inc()
    let c = post_dec()
    let d = pre_dec()
    a === 10 && b === 12 && c === 12 && d === 10 && value === 10
}
run()
"#,
            ),
            (
                "object-field",
                r#"
item = {value: 10}
$OBJECT_ASSERTIONS
"#,
            ),
            (
                "dynamic-field",
                r#"
key = 'value'
item = {value: 10}
$DYNAMIC_ASSERTIONS
"#,
            ),
            (
                "array-index",
                r#"
items = [10]
$ARRAY_ASSERTIONS
"#,
            ),
            (
                "nested-field",
                r#"
root = {child: {value: 10}}
$NESTED_ASSERTIONS
"#,
            ),
            (
                "public-instance-field",
                r#"
class Counter {
    pub value: 10
    new() { this }
}
item = Counter::new()
$OBJECT_ASSERTIONS
"#,
            ),
            (
                "private-this-field",
                r#"
class Counter {
    value: 10
    new() { this }
    pub verify() {
        let a = this.value++
        let b = ++this.value
        let c = this.value--
        let d = --this.value
        a === 10 && b === 12 && c === 12 && d === 10 && this.value === 10
    }
}
Counter::new().verify()
"#,
            ),
        ];
        let variable_assertions = r#"
let a = value++
let b = ++value
let c = value--
let d = --value
a === 10 && b === 12 && c === 12 && d === 10 && value === 10
"#;
        let object_assertions = r#"
a = item.value++
b = ++item.value
c = item.value--
d = --item.value
a === 10 && b === 12 && c === 12 && d === 10 && item.value === 10
"#;
        let dynamic_assertions = r#"
a = item[key]++
b = ++item[key]
c = item[key]--
d = --item[key]
a === 10 && b === 12 && c === 12 && d === 10 && item.value === 10
"#;
        let array_assertions = r#"
a = items[0]++
b = ++items[0]
c = items[0]--
d = --items[0]
a === 10 && b === 12 && c === 12 && d === 10 && items[0] === 10
"#;
        let nested_assertions = r#"
a = root.child.value++
b = ++root.child.value
c = root.child.value--
d = --root.child.value
a === 10 && b === 12 && c === 12 && d === 10 && root.child.value === 10
"#;

        for (target_name, template) in targets {
            let source = template
                .replace("$ASSERTIONS", variable_assertions)
                .replace("$OBJECT_ASSERTIONS", object_assertions)
                .replace("$DYNAMIC_ASSERTIONS", dynamic_assertions)
                .replace("$ARRAY_ASSERTIONS", array_assertions)
                .replace("$NESTED_ASSERTIONS", nested_assertions);
            assert_eq!(
                run_test_source(&source),
                Value::Bool(true),
                "failed to increase and decrease the target: target={target_name}"
            );
        }

        assert_eq!(
            run_test_source("value = 1.5\nold = value++\njson([old, value])"),
            Value::Str("[1.5,2.5]".to_string())
        );
    }

    /// The object, dynamic subscript and rvalue in the assignment target must be executed once each in order from left to right.
    #[test]
    fn assignments_resolve_side_effectful_targets_once_in_source_order() {
        let value = run_test_source(
            r#"
order = ''
calls = {object: 0, key: 0, rhs: 0}
item = {value: 1}
fn pick() {
    calls.object++
    order += 'o'
    item
}
fn index() {
    calls.key++
    order += 'k'
    'value'
}
fn next() {
    calls.rhs++
    order += 'r'
    9
}
plain = (pick()[index()] = next())
compound = (pick()[index()] += next())
postfix = pick()[index()]++
json([order, calls.object, calls.key, calls.rhs, plain, compound, postfix, item.value])
"#,
        );

        assert_eq!(
            value,
            Value::Str("[\"okrokrok\",3,3,2,9,18,18,19]".to_string())
        );
    }

    /// Non-lvalue and constant increment and decrement must fail at compile time, and the prefix syntax cannot swallow subsequent binary expressions.
    #[test]
    fn assignment_mutations_validate_lvalues_and_prefix_precedence() {
        for source in [
            "1 += 2",
            "fn foo() { 1 }\nfoo()++",
            "a = 1\nb = 2\n(a + b) += 1",
        ] {
            let err = compile_test_source_error(source);
            assert!(err
                .message
                .contains("The assignment target must be a writable variable"));
        }

        let constant = compile_test_source_error("Total = 1\nTotal++");
        assert!(constant
            .message
            .contains("cannot use Increment or decrement"));

        assert_eq!(
            run_test_source("value = 1\nresult = ++value + 2\njson([result, value])"),
            Value::Str("[4,2]".to_string())
        );
    }

    /// Increment and decrement only accepts integers and floating point numbers, and retains the read-only target error of the property setter.
    #[test]
    fn increment_types_and_readonly_targets_fail_explicitly() {
        for source in [
            "value = '1'\nvalue++",
            "value = true\nvalue++",
            "value = null\nvalue--",
            "value = []\n++value",
        ] {
            let err = run_test_source_error(source);
            assert!(err.message.contains("only supports Int or Float"));
        }

        let overflow = run_test_source_error("value = 9223372036854775807\nvalue++");
        assert!(overflow
            .message
            .contains("increment or decrement causing integer overflow"));

        for (source, expected) in [
            (
                "text = 'BT'\ntext[0] += 'x'",
                "Only arrays and objects can write properties",
            ),
            (
                "data = bytes('00', 'hex')\ndata[0] += 1",
                "Only arrays and objects can write properties",
            ),
            (
                "class Counter { pub value: 1 }\nCounter.value += 1",
                "Only arrays and objects can write properties",
            ),
            (
                "BT.VERSION += ''",
                "Global static object properties are read-only",
            ),
            ("Math.PI++", "Global static object properties are read-only"),
        ] {
            let err = run_test_source_error(source);
            assert!(
                err.message.contains(expected),
                "Read-only target error is unclear: {source}"
            );
        }
    }

    /// Function local constants are destroyed after the function call is completed, and constants with the same name in different functions do not conflict.
    #[test]
    fn function_constants_are_scoped_per_function_call() {
        let value = run_test_source(
            r#"
fn first() {
    Name = 'A'
    Name
}
fn second() {
    Name = 'B'
    Name
}
json([first(), second(), first(), is_empty(Name)])
"#,
        );

        assert_eq!(value, Value::Str("[\"A\",\"B\",\"A\",true]".to_string()));
    }

    /// Constants within a function cannot obscure global constants. Even if global constants are written after the function declaration, they should be discovered by the compiler.
    #[test]
    fn function_constant_cannot_shadow_global_constant() {
        let err = compile_test_source_error(
            r#"
fn demo() {
    Name = 'local'
}
Name = 'global'
"#,
        );

        assert!(err.message.contains("is already defined in global scope"));
    }

    /// A function-local constant cannot shadow an existing global variable at runtime.
    #[test]
    fn function_constant_rejects_existing_global_variable_at_runtime() {
        let source = r#"
fn demo() {
    Name = 'local'
}
demo()
"#;
        let file = "test.bt".to_string();
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        let chunk = Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect("test script should compile successfully");
        let mut vm = Vm::new();
        vm.globals.insert("Name".to_string(), Value::Int(1));
        let err = vm
            .run_with_value(&chunk)
            .expect_err("test script should fail to execute");

        assert!(err.message.contains("has been defined in the global scope"));
    }

    /// Constant definition is executed repeatedly in a loop, the second write should be blocked at runtime.
    #[test]
    fn function_constant_rejects_second_runtime_definition() {
        let err = run_temp_project_error(
            "constant-loop",
            r#"
fn demo() {
    for item in [1, 2] {
        Name = item
    }
}
demo()
"#,
        );

        assert!(err
            .message
            .contains("constant `Name` cannot be defined repeatedly"));
    }

    /// `set_timeout()` returns a Timer immediately and runs its callback asynchronously.
    #[test]
    fn set_timeout_returns_without_running_callback() {
        let (vm, _chunk) = run_test_entry(
            r#"
fired = false
timer = set_timeout(fn() {
    fired = true
}, 100)
"#,
        );

        assert_eq!(vm.get_global("fired"), Some(&Value::Bool(false)));
        assert!(matches!(vm.get_global("timer"), Some(Value::Timer(_))));
    }

    /// Timeout expires, a callback should be executed on the corresponding VM thread.
    #[test]
    fn set_timeout_runs_callback_once() {
        let vm = run_test_entry_with_background(
            r#"
value = 0
set_timeout(fn() {
    value = 7
}, 5)
"#,
        );

        assert_eq!(vm.get_global("value"), Some(&Value::Int(7)));
        assert!(!vm.has_active_timers());
    }

    /// Timer.cancel() should block a timeout that has not yet fired.
    #[test]
    fn timer_cancel_prevents_timeout_callback() {
        let vm = run_test_entry_with_background(
            r#"
value = 0
timer = set_timeout(fn() {
    value = 1
}, 20)
ok = timer.cancel()
"#,
        );

        assert_eq!(vm.get_global("ok"), Some(&Value::Bool(true)));
        assert_eq!(vm.get_global("value"), Some(&Value::Int(0)));
        assert!(!vm.has_active_timers());
    }

    /// Interval callbacks should be able to self-cancel via self.cancel().
    #[test]
    fn set_interval_can_cancel_itself() {
        let vm = run_test_entry_with_background(
            r#"
count = 0
set_interval(fn(self) {
    count = count + 1
    if count >= 3 {
        self.cancel()
    }
}, 1)
"#,
        );

        assert_eq!(vm.get_global("count"), Some(&Value::Int(3)));
        assert!(!vm.has_active_timers());
    }

    /// The slow interval callback should not re-enter the same timer.
    #[test]
    fn set_interval_does_not_reenter_slow_callback() {
        let vm = run_test_entry_with_background(
            r#"
running = false
overlap = false
count = 0

set_interval(fn(self) {
    if running {
        overlap = true
    }
    running = true
    sleep(10)
    running = false
    count = count + 1
    if count >= 3 {
        self.cancel()
    }
}, 1)
"#,
        );

        assert_eq!(vm.get_global("count"), Some(&Value::Int(3)));
        assert_eq!(vm.get_global("overlap"), Some(&Value::Bool(false)));
    }

    /// An expired timer must not preempt the current synchronous execution flow.
    #[test]
    fn timer_callback_does_not_preempt_current_vm_execution() {
        let vm = run_test_entry_with_background(
            r#"
value = 0
set_timeout(fn() {
    value = 1
}, 1)
sleep(20)
seen = value
"#,
        );

        assert_eq!(vm.get_global("seen"), Some(&Value::Int(0)));
        assert_eq!(vm.get_global("value"), Some(&Value::Int(1)));
    }

    /// Interval callback throws an error, and subsequent rounds can still be executed.
    #[test]
    fn timer_callback_error_does_not_stop_later_ticks() {
        let vm = run_test_entry_with_background(
            r#"
count = 0
set_interval(fn(self) {
    count = count + 1
    if count == 1 {
        throw 'fail'
    }
    if count >= 2 {
        self.cancel()
    }
}, 1)
"#,
        );

        assert_eq!(vm.get_global("count"), Some(&Value::Int(2)));
        assert!(!vm.has_active_timers());
    }

    /// Creation of long-term timers is not allowed in the Web Request VM.
    #[test]
    fn timer_is_rejected_in_web_request_context() {
        let chunk = compile_test_entry(
            r#"
set_timeout(fn() {
    println('no')
}, 1)
"#,
        );
        let mut vm = Vm::new();
        vm.set_web_response(Rc::new(RefCell::new(BtWebResponse::new())));
        let err = vm
            .run(&chunk)
            .expect_err("Creating timer in web request should fail");

        assert!(err
            .message
            .contains("set_timeout() cannot create a timer in the context of a web request"));
    }

    /// Sleep() in web requests cannot exceed the I/O default timeout limit.
    #[test]
    fn web_request_sleep_is_capped_by_io_timeout() {
        let err = run_web_entry_error("sleep(999999999)");

        assert!(err.message.contains("sleep() allows up to"));
        assert!(err.message.contains("in a web request context"));
    }

    /// Pause() is not allowed to wait for terminal input in web requests.
    #[test]
    fn web_request_pause_is_rejected() {
        let err = run_web_entry_error("pause('wait')");

        assert!(err
            .message
            .contains("pause() cannot wait for terminal input in the context of a web request"));
    }

    /// Web requests.
    #[test]
    fn web_request_process_output_is_rejected() {
        let program = if cfg!(windows) { "cmd" } else { "sh" };
        let source = format!("process('{}').output()", program);
        let err = run_web_entry_error(&source);

        assert!(err
            .message
            .contains("process.output() cannot perform blocking or forking process operations in the context of a web request"));
    }

    /// Permission configuration should deny disabled capabilities in the VM standard library constructor entry.
    #[test]
    fn permission_denies_stdlib_constructors_at_vm_boundary() {
        permission::with_test_config(None, Some("process,http"), || {
            let process_err = run_test_source_error("process('cmd')");
            assert!(process_err.message.contains("Permission denied"));
            assert!(process_err.message.contains("`process`"));

            let http_err = run_test_source_error("reqwest('https://example.com')");
            assert!(http_err.message.contains("Permission denied"));
            assert!(http_err.message.contains("`http`"));
        });
    }

    /// FfiBuffer's script methods, statistics, and post-closure invalidation semantics must be fully integrated into the VM.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_buffer_methods_and_stats_work_in_vm() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let value = run_test_source(
            r#"
before = BT.stats().ffi
buffer = ffi.buffer(16)
written = buffer.write(bytes([66, 84, 0]))
during = BT.stats().ffi
text = buffer.to_string()
copy = buffer.to_bytes(0, written).to_hex()
pointer_type = type(buffer.ptr())
closed = ffi.close(buffer)
after = BT.stats().ffi
[written, text, copy, pointer_type, closed, during.buffers - before.buffers, during.buffer_bytes - before.buffer_bytes, after.buffers - before.buffers]
"#,
        );
        assert_eq!(
            value.to_json_string(),
            "[3,\"BT\",\"425400\",\"FfiPointer\",true,1,16,0]"
        );
        let err = run_test_source_error("buffer = ffi.buffer(1)\nffi.close(buffer)\nbuffer.len()");
        assert!(
            err.message.contains("Buffer has been closed"),
            "{}",
            err.message
        );
    }

    /// FFI Buffer allocation must respect both permissions and web request boundaries.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_buffer_respects_permission_and_web_boundaries() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        permission::with_test_config(None, Some("ffi"), || {
            let err = run_test_source_error("ffi.buffer(16)");
            assert!(err.message.contains("Permission denied"), "{}", err.message);
        });
        let err = run_web_entry_error("ffi.buffer(16)");
        assert!(
            err.message
                .contains("cannot allocate native memory in the web request context"),
            "{}",
            err.message
        );
    }

    /// FFI static values should be consistent in type, string, JSON, truth value, and compile-time capability flags.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_static_value_semantics_and_feature_flags_are_stable() {
        let value = run_test_source(
            "[type(ffi), string(ffi), bool(ffi), json(ffi), BT.has('ffi'), BT.features().ffi]",
        );

        assert_eq!(
            value.to_string(),
            "[\"Ffi\",\"ffi\",true,\"null\",true,true]"
        );
        let err = run_test_source_error("ffi = 1");
        assert!(
            err.message.contains("constant `ffi` cannot be reassigned"),
            "{}",
            err.message
        );
    }

    /// FFI load must pass permission boundaries before touching the system loader.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_load_respects_permission_boundary() {
        permission::with_test_config(None, Some("ffi"), || {
            let err = run_test_source_error("ffi.load('unused-library', {})");

            assert!(err.message.contains("Permission denied"));
            assert!(err.message.contains("`ffi`"));
        });
    }

    /// Web request VM must deny FFI before system loads.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_load_is_rejected_in_web_request_context() {
        let err = run_web_entry_error("ffi.load('unused-library', {})");

        assert!(err.message.contains(
            "ffi.load() cannot load native dynamic libraries in the web request context"
        ));
    }

    /// Background task parameter snapshot must reject thread-local values by FFI exact type name.
    #[cfg(feature = "ffi")]
    #[test]
    fn ffi_value_is_rejected_by_task_snapshot() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let err = run_test_source_error("task(fn(value) { value }, ffi)");

        assert!(
            err.message
                .contains("Task argument 1 cannot be snapshotted"),
            "{}",
            err.message
        );
        assert!(
            err.message.contains("do not support values of type `Ffi`"),
            "{}",
            err.message
        );
        let err = run_test_source_error("task(fn(value) { value }, ffi.buffer(16))");
        assert!(
            err.message
                .contains("Task argument 1 cannot be snapshotted"),
            "{}",
            err.message
        );
        assert!(
            err.message
                .contains("do not support values of type `FfiBuffer`"),
            "{}",
            err.message
        );
    }

    /// Windows user32 complete signature and true declaration-free call should complete the true closed loop.
    #[cfg(all(feature = "ffi", windows))]
    #[test]
    fn ffi_calls_user32_with_complete_signature() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let value = run_test_source(
            r#"
implicit_user32 = ffi.load('user32.dll')
implicit_width = implicit_user32.GetSystemMetrics(0)
implicit_closed = ffi.close(implicit_user32)

declared_user32 = ffi.load('user32.dll', {
    GetSystemMetrics: 'i32(i32)'
})
declared_width = declared_user32.GetSystemMetrics(0)
declared_closed = ffi.close(declared_user32)
[implicit_width > 0, implicit_closed, declared_width > 0, declared_closed]
"#,
        );
        let Value::Array(values) = value else {
            panic!("FFI user32 test should return array");
        };

        assert_eq!(
            values.borrow().as_slice(),
            &[
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true),
                Value::Bool(true)
            ]
        );
    }

    /// Windows return hints and A/W parameter assist must be in effect and not overwrite return types or symbol names.
    #[cfg(all(feature = "ffi", windows))]
    #[test]
    fn ffi_return_hints_and_windows_aw_inference_work() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let value = run_test_source(
            r#"
user32 = ffi.load('user32.dll', {
    GetDesktopWindow: 'ptr',
    FindWindowW: 'ptr',
    FindWindowA: 'ptr'
})
desktop = user32.GetDesktopWindow()
wide = user32.FindWindowW(null, '__bt_missing_window_w__')
ansi = user32.FindWindowA(null, '__bt_missing_window_a__')
[type(desktop), wide, ansi, ffi.close(user32)]
"#,
        );
        assert_eq!(value.to_json_string(), "[\"FfiPointer\",null,null,true]");

        let non_ascii = run_test_source_error(
            "lib = ffi.load('user32.dll', {FindWindowA: 'ptr'})\nlib.FindWindowA(null, 'café')",
        );
        assert!(
            non_ascii.message.contains("only allows ASCII"),
            "{}",
            non_ascii.message
        );

        let locked_null = run_test_source_error(
            "lib = ffi.load('user32.dll', {FindWindowW: 'ptr'})\nlib.FindWindowW(null, null)\nlib.FindWindowW(null, 'BT')",
        );
        assert!(
            locked_null.message.contains("locked as ptr"),
            "{}",
            locked_null.message
        );
    }

    /// Missing items, missing symbols, number of parameters, and parameter types of strict schema must be converted to VmError.
    #[cfg(all(feature = "ffi", windows))]
    #[test]
    fn ffi_detectable_call_failures_are_vm_errors() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let undeclared = run_test_source_error(
            "lib = ffi.load('user32.dll', {GetSystemMetrics: 'i32(i32)'})\nlib.NotDeclared(0)",
        );
        assert!(undeclared
            .message
            .contains("Strict schema does not declare"));

        let missing = run_test_source_error(
            "lib = ffi.load('user32.dll', {BtMissingSymbol: 'i32()'})\nlib.BtMissingSymbol()",
        );
        assert!(missing
            .message
            .contains("cannot be found in the dynamic library"));

        let count = run_test_source_error(
            "lib = ffi.load('user32.dll', {GetSystemMetrics: 'i32(i32)'})\nlib.GetSystemMetrics()",
        );
        assert!(count.message.contains("expects 1 arguments"));

        let kind = run_test_source_error(
            "lib = ffi.load('user32.dll', {GetSystemMetrics: 'i32(i32)'})\nlib.GetSystemMetrics('bad')",
        );
        assert!(kind
            .message
            .contains("parameter 1 requires i32, but received String"));
    }

    /// The saved NativeMethod must also be invalidated immediately after the dynamic library is closed.
    #[cfg(all(feature = "ffi", windows))]
    #[test]
    fn ffi_saved_method_is_invalid_after_close() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let err = run_test_source_error(
            r#"
lib = ffi.load('user32.dll', {GetSystemMetrics: 'i32(i32)'})
saved = lib.GetSystemMetrics
ffi.close(lib)
saved(0)
"#,
        );

        assert!(err.message.contains("FFI dynamic library has been closed"));
    }

    /// Permission configuration should deny env capability at the BT environment variable overlay method entry.
    #[test]
    fn permission_denies_bt_env_methods_at_vm_boundary() {
        permission::with_test_config(None, Some("env"), || {
            let err = run_test_source_error("BT.env('PATH')");

            assert!(err.message.contains("Permission denied"));
            assert!(err.message.contains("`env`"));
        });
    }

    /// Task() should return the Task object immediately and wait asynchronously for the completion of task function execution.
    #[test]
    fn task_returns_without_waiting_for_body() {
        let value = run_test_source(
            r#"
start = date().timestamp_millis()
t = task(fn() {
    sleep(500)
    return 1
})
date().timestamp_millis() - start < 300
"#,
        );

        assert_eq!(value, Value::Bool(true));
    }

    /// Await() should wait for the task to complete and allow repeated reading of the same result snapshot.
    #[test]
    fn task_await_returns_saved_result_repeatedly() {
        let value = run_test_source(
            r#"
t = task(fn() {
    sleep(20)
    return 7
})
json([type(t), t.await(), t.await()])
"#,
        );

        assert_eq!(value, Value::Str("[\"Task\",7,7]".to_string()));
    }

    /// `done()` and `result()` remain non-blocking while a task is pending.
    #[test]
    fn task_done_and_result_are_non_blocking() {
        let value = run_test_source(
            r#"
t = task(fn() {
    sleep(120)
    return 9
})
pending = t.result()
first_done = t.done()
sleep(220)
json([pending, first_done, t.done(), t.result()])
"#,
        );

        assert_eq!(value, Value::Str("[null,false,true,9]".to_string()));
    }

    /// Tasks read globals from their creation-time snapshot; internal changes do not flow back to the main VM.
    #[test]
    fn task_globals_are_snapshot_values() {
        let value = run_test_source(
            r#"
x = 1
t = task(fn() {
    x = 2
    return x
})
main_before = x
result = t.await()
json([main_before, x, result])
"#,
        );

        assert_eq!(value, Value::Str("[1,1,2]".to_string()));
    }

    /// Closure should be snapshotted at task() creation time.
    #[test]
    fn task_closure_captures_are_snapshotted() {
        let value = run_test_source(
            r#"
fn make_task() {
    let x = 10
    return task(fn() {
        return x + 1
    })
}
t = make_task()
t.await()
"#,
        );

        assert_eq!(value, Value::Int(11));
    }

    /// Arrays and objects should be recursively deep-snapshotted when entering the task, and background modifications will not affect the main VM.
    #[test]
    fn task_arrays_and_objects_are_deep_snapshots() {
        let value = run_test_source(
            r#"
data = {count: 1, items: [1, 2]}
t = task(fn() {
    data.count = 9
    data.items.push(3)
    return data
})
data.count = 2
data.items.push(8)
result = t.await()
json([data.count, data.items.len(), result.count, result.items.len()])
"#,
        );

        assert_eq!(value, Value::Str("[2,3,9,3]".to_string()));
    }

    /// A task's thrown value is deferred until `await()`, then rethrown for an outer `catch` to handle.
    #[test]
    fn task_throw_is_rethrown_on_await() {
        let (output, _) = run_test_source_with_output(
            r#"
t = task(fn() {
    throw 'fatal'
})
value = try {
    t.await()
    'none'
} catch e {
    e
}
println(value)
"#,
        );

        assert_eq!(output, "fatal\n");
    }

    /// Task should return a normal runtime error when await()ing a read when it returns a non-snapshotable value.
    #[test]
    fn task_returning_function_fails_when_read() {
        let err = run_test_source_error(
            r#"
t = task(fn() {
    return fn() {
        return 1
    }
})
t.await()
"#,
        );

        assert!(err
            .message
            .contains("task return value cannot be snapshotted"));
    }

    /// `task(fn, ...args)` invokes the background function with arguments snapshotted at creation.
    #[test]
    fn task_explicit_args_are_snapshotted() {
        let value = run_test_source(
            r#"
data = {count: 1}
t = task(fn(value, item, fallback = 'default') {
    item.count = 9
    return [value + 1, item.count, fallback]
}, 6, data)
data.count = 2
json([data.count, t.await()])
"#,
        );

        assert_eq!(value, Value::Str("[2,[7,9,\"default\"]]".to_string()));
    }

    /// Creating a task fails immediately when an explicit argument cannot be snapshotted.
    #[test]
    fn task_explicit_args_reject_unsnapshotable_values() {
        let err = run_test_source_error(
            r#"
fn helper() {
    return 1
}
task(fn(value) {
    return value
}, helper)
"#,
        );

        assert!(err
            .message
            .contains("Task argument 1 cannot be snapshotted"));
    }

    /// `task_all()` waits for every task and returns results in input order.
    #[test]
    fn task_all_returns_results_in_input_order() {
        let value = run_test_source(
            r#"
slow = task(fn() {
    sleep(30)
    return 'slow'
})
fast = task(fn() {
    return 'fast'
})
json(task_all([slow, fast]))
"#,
        );

        assert_eq!(value, Value::Str("[\"slow\",\"fast\"]".to_string()));
    }

    /// Task_all() encounters multiple failures, it should wait for all tasks and then select the first throw in the input order.
    #[test]
    fn task_all_reports_first_failure_by_input_order() {
        let value = run_test_source(
            r#"
first = task(fn() {
    sleep(30)
    throw 'first'
})
second = task(fn() {
    throw 'second'
})
try {
    task_all([first, second])
    'none'
} catch e {
    e
}
"#,
        );

        assert_eq!(value, Value::Str("first".to_string()));
    }

    /// Task_race([]) should return empty, indicating that there are no results to compete with.
    #[test]
    fn task_race_empty_returns_empty() {
        let value = run_test_source("task_race([])");

        assert_eq!(value, Value::Empty);
    }

    /// `task_race()` returns the first completed task's result without waiting for slower tasks.
    #[test]
    fn task_race_returns_first_completed_result() {
        let value = run_test_source(
            r#"
slow = task(fn() {
    sleep(80)
    return 'slow'
})
fast = task(fn() {
    return 'fast'
})
task_race([slow, fast])
"#,
        );

        assert_eq!(value, Value::Str("fast".to_string()));
    }

    /// Task_race() throws, it should be re-thrown at the call site.
    #[test]
    fn task_race_rethrows_winning_throw() {
        let value = run_test_source(
            r#"
slow = task(fn() {
    sleep(80)
    return 'slow'
})
fast = task(fn() {
    throw 'boom'
})
try {
    task_race([slow, fast])
    'none'
} catch e {
    e
}
"#,
        );

        assert_eq!(value, Value::Str("boom".to_string()));
    }

    /// Task.on_done() should execute the callback in the VM event loop after the entry script ends.
    #[test]
    fn task_on_done_runs_in_background_event_loop() {
        let vm = run_test_entry_with_background(
            r#"
value = 0
status_text = ''
t = task(fn() {
    sleep(20)
    return 123
})
t.on_done(fn(result, err, status) {
    value = result
    status_text = status
})
"#,
        );

        assert_eq!(vm.get_global("value"), Some(&Value::Int(123)));
        assert_eq!(
            vm.get_global("status_text"),
            Some(&Value::Str("success".to_string()))
        );
    }

    /// Registers on_done() after completing the task and should also execute it once on subsequent event boundaries.
    #[test]
    fn task_on_done_after_completion_runs_once() {
        let vm = run_test_entry_with_background(
            r#"
count = 0
t = task(fn() {
    return 7
})
t.await()
t.on_done(fn(result, err, status) {
    count = count + result
})
"#,
        );

        assert_eq!(vm.get_global("count"), Some(&Value::Int(7)));
        assert!(!vm.has_active_task_callbacks());
    }

    /// On_done() requires status to distinguish between successful return empty and throw empty.
    #[test]
    fn task_on_done_status_distinguishes_throw_empty() {
        let vm = run_test_entry_with_background(
            r#"
status_text = ''
err_is_empty = false
result_is_empty = false
t = task(fn() {
    throw empty
})
t.on_done(fn(result, err, status) {
    status_text = status
    err_is_empty = is_empty(err)
    result_is_empty = is_empty(result)
})
"#,
        );

        assert_eq!(
            vm.get_global("status_text"),
            Some(&Value::Str("throw".to_string()))
        );
        assert_eq!(vm.get_global("err_is_empty"), Some(&Value::Bool(true)));
        assert_eq!(vm.get_global("result_is_empty"), Some(&Value::Bool(true)));
    }

    /// Registration of Task.on_done() long-lived callback is not allowed in the Web Request VM.
    #[test]
    fn task_on_done_is_rejected_in_web_request_context() {
        let chunk = compile_test_entry(
            r#"
t = task(fn() {
    return 1
})
t.on_done(fn(result, err, status) {
    return empty
})
"#,
        );
        let mut vm = Vm::new();
        vm.set_web_response(Rc::new(RefCell::new(BtWebResponse::new())));
        let err = vm
            .run(&chunk)
            .expect_err("Registration on_done should fail in web request");

        assert!(err
            .message
            .contains("Task.on_done() cannot register a callback in the web request context"));
    }

    /// Synchronous waiting for Task completion is not allowed in the Web Request VM.
    #[test]
    fn task_await_is_rejected_in_web_request_context() {
        let err = run_web_entry_error(
            r#"
t = task(fn() {
    return 1
})
t.await()
"#,
        );

        assert!(err.message.contains(
            "Task.await() cannot wait for a background task in the context of a web request"
        ));
    }

    /// `~` inverts the integer bit pattern using the same integer conversion rules as other bitwise operators.
    #[test]
    fn bitwise_not_operator_uses_integer_bits() {
        let value = run_test_source("json([~0, ~1, ~true])");

        assert_eq!(value, Value::Str("[-1,-2,-2]".to_string()));
    }

    /// Full use come from the runtime object and must also comply with the variable naming rules.
    #[test]
    fn full_use_imports_follow_variable_name_rules() {
        let err = run_temp_project_error("use-full-name", "obj = {Name: 1}\nuse obj");

        assert!(err
            .message
            .contains("use import variable `Name` cannot start with an uppercase letter"));
    }

    /// `use obj` has been imported in full, and `use obj{*}` no longer retains duplicate syntax.
    #[test]
    fn use_import_rejects_removed_wildcard_syntax() {
        let err = parse_test_source_error("obj = {name: 1}\nuse obj{*}");

        assert!(err.message.contains("use import list requires identifier"));
    }

    /// Assert should return true when successful, and the truth value judgment rules are consistent with the if condition.
    #[test]
    fn assert_returns_true_when_condition_is_truthy() {
        let value = run_test_source("assert('BT')");

        assert_eq!(value, Value::Bool(true));
    }

    /// Assert should return a clear argument error when a condition argument is missing.
    #[test]
    fn assert_requires_condition_argument() {
        let err = run_temp_project_error("assert-empty", "assert()");

        assert!(err.message.contains("assert requires at least 1 argument"));
    }

    /// Assert fails and there is no custom message, you should try to bring the current assertion source code statement.
    #[test]
    fn assert_failure_includes_statement_source() {
        let err = run_temp_project_error("assert-source", "value = 1\nassert(value == 2)\n");

        assert!(err.message.contains("Assertion failed: assert(value == 2)"));
    }

    /// Assert custom message should be used as the main error message, while retaining the source code statement for easy location.
    #[test]
    fn assert_failure_uses_custom_message() {
        let err = run_temp_project_error(
            "assert-message",
            "name = 'bt'\nassert(name == 'BT', 'name should be BT')\n",
        );

        assert!(err.message.contains("Assertion failed: name should be BT"));
        assert!(err
            .message
            .contains("Statement: assert(name == 'BT', 'name should be BT')"));
    }

    /// Destructuring assignment should support array subscripts, object fields with the same name, missing values, and mixed comma/space separation.
    #[test]
    fn destructuring_assignment_reads_array_and_object_values() {
        let source = r#"
arr = [1 2 3]
(a, b c d) = arr
obj = {
    name: 'BT'
    sex: 1
}
(name sex age) = obj
json([a, b, c, is_empty(d), name, sex, is_empty(age)])
"#;
        let value = run_test_source(source);

        assert_eq!(value, Value::Str("[1,2,3,true,\"BT\",1,true]".to_string()));
        assert_eq!(
            run_test_source(&source.replace('\n', "\r\n")),
            Value::Str("[1,2,3,true,\"BT\",1,true]".to_string())
        );
    }

    /// Single element destructuring should be read as object fields and maintain array/object reference semantics.
    #[test]
    fn destructuring_assignment_keeps_single_object_field_reference() {
        let value = run_test_source(
            r#"
obj = {
    data: {
        name: 'old'
    }
}
(data) = obj
data.name = 'new'
obj.data.name
"#,
        );

        assert_eq!(value, Value::Str("new".to_string()));
    }

    /// Returns the right-hand side value when destructuring assignment as an expression, maintaining ordinary assignment expression semantics.
    #[test]
    fn destructuring_assignment_returns_right_value() {
        let value = run_test_source(
            r#"
arr = [1 2]
result = ((a b) = arr)
json([result[0], result[1], a, b])
"#,
        );

        assert_eq!(value, Value::Str("[1,2,1,2]".to_string()));
    }

    /// Rvalues that are not arrays or objects should return clear runtime errors.
    #[test]
    fn destructuring_assignment_rejects_non_array_or_object() {
        let err = run_temp_project_error("destructure-scalar", "(a b) = 123\n");

        assert!(err
            .message
            .contains("The right side of the destructuring assignment must be array or object"));
    }

    /// A destructuring loop reads object fields directly on each iteration, avoiding repeated `value.name` access.
    #[test]
    fn for_destructuring_reads_object_fields_from_each_item() {
        let value = run_test_source(
            r#"
users = [
    {name: 'Ada', age: 18}
    {name: 'Grace', age: 20}
]
out = []
for (name age missing) in users {
    out.push([name age is_empty(missing)])
}
json(out)
"#,
        );

        assert_eq!(
            value,
            Value::Str("[[\"Ada\",18,true],[\"Grace\",20,true]]".to_string())
        );
    }

    /// A destructuring loop also accepts arrays and fills missing positions with `empty`.
    #[test]
    fn for_destructuring_reads_array_items_from_each_item() {
        let value = run_test_source(
            r#"
rows = [
    [1 2]
    [3]
]
out = []
for (a b) in rows {
    out.push([a is_empty(b)])
}
json(out)
"#,
        );

        assert_eq!(value, Value::Str("[[1,false],[3,true]]".to_string()));
    }

    /// For destructuring loop only changes the bracket form; ordinary key, value loop still maintains the original iteration semantics.
    #[test]
    fn for_destructuring_keeps_plain_for_semantics_unchanged() {
        let value = run_test_source(
            r#"
users = [
    {name: 'Ada'}
    {name: 'Grace'}
]
plain = ''
for key,value in users {
    plain = plain + string(key) + ':' + value.name + ';'
}
picked = ''
for (name) in users {
    picked = picked + name + ';'
}
json([plain picked])
"#,
        );

        assert_eq!(
            value,
            Value::Str("[\"0:Ada;1:Grace;\",\"Ada;Grace;\"]".to_string())
        );
    }

    /// The for destructuring loop should return the same error as ordinary destructuring when it encounters the current value that cannot be deconstructed.
    #[test]
    fn for_destructuring_rejects_scalar_items() {
        let err = run_temp_project_error(
            "for-destructure-scalar",
            r#"
items = [1]
for (name) in items {
    echo name
}
"#,
        );

        assert!(err
            .message
            .contains("The right side of the destructuring assignment must be array or object"));
    }

    /// Counted loops support variable-free repetition and lazy integer iteration with `for i in n`.
    #[test]
    fn for_count_loops_repeat_without_prebuilt_items() {
        let value = run_test_source(
            r#"
out = []
for 3 {
    out.push('x')
}
for i in 3 {
    out.push(i)
}
for i in 3 step 2 {
    out.push(i)
}
for key, value in 3 step 2 {
    out.push([key value])
}
json(out)
"#,
        );

        assert_eq!(
            value,
            Value::Str("[\"x\",\"x\",\"x\",0,1,2,0,2,4,[0,0],[2,2],[4,4]]".to_string())
        );
    }

    /// Count-loop steps accept only integers greater than zero.
    #[test]
    fn for_count_rejects_zero_or_negative_step() {
        let zero = run_temp_project_error(
            "for-count-step-zero",
            r#"
for i in 3 step 0 {
    echo i
}
"#,
        );
        assert!(zero
            .message
            .contains("for count step must be an integer greater than 0"));

        let negative = run_temp_project_error(
            "for-count-step-negative",
            r#"
for i in 3 step -2 {
    echo i
}
"#,
        );
        assert!(negative
            .message
            .contains("for count step must be an integer greater than 0"));
    }

    /// Range loops support inclusive bounds, `..end`, reverse ranges, and positive integer steps.
    #[test]
    fn for_range_loops_cover_bounds_direction_and_step() {
        let value = run_test_source(
            r#"
out = []
for i in 1..3 {
    out.push(i)
}
for i in ..2 {
    out.push(i)
}
for i in 3..1 {
    out.push(i)
}
for i in 0..6 step 2 {
    out.push(i)
}
for i in 5..1 step 2 {
    out.push(i)
}
json(out)
"#,
        );

        assert_eq!(
            value,
            Value::Str("[1,2,3,0,1,2,3,2,1,0,2,4,6,5,3,1]".to_string())
        );
    }

    /// Ranges without a bound variable and open-ended ranges keep only iterator state and may exit normally through `break`.
    #[test]
    fn for_range_supports_discarded_value_and_open_end() {
        let value = run_test_source(
            r#"
count = 0
for 1..3 {
    count += 1
}
last = 0
for i in 100000000.. {
    last = i
    break
}
json([count, last])
"#,
        );

        assert_eq!(value, Value::Str("[3,100000000]".to_string()));
    }

    /// `for` bindings accept commas or spaces, matching destructuring and function-parameter syntax.
    #[test]
    fn for_key_value_binding_allows_optional_comma() {
        let value = run_test_source(
            r#"
users = {name: 'BT', age: 1}
out = []
for key value in users {
    out.push(key + ':' + string(value))
}
for index value in ['A', 'B'] {
    out.push(string(index) + ':' + value)
}
for order value in 2..3 {
    out.push(string(order) + ':' + string(value))
}
json(out)
"#,
        );

        assert_eq!(
            value,
            Value::Str("[\"name:BT\",\"age:1\",\"0:A\",\"1:B\",\"0:2\",\"1:3\"]".to_string())
        );
    }

    /// In the binding for `_` should be treated as a discarded value and no variable of the same name will be created or overwritten.
    #[test]
    fn for_discard_binding_skips_key_or_value_storage() {
        let value = run_test_source(
            r#"
_ = 'keep'
obj = {a: 1, b: 2}
out = []
for _, value in obj {
    out.push(value)
}
for key, _ in obj {
    out.push(key)
}
for _, _ in obj {
    out.push('x')
}
for _ in [7, 8] {
    out.push(_)
}
json(out)
"#,
        );

        assert_eq!(
            value,
            Value::Str("[1,2,\"a\",\"b\",\"x\",\"x\",\"keep\",\"keep\"]".to_string())
        );
    }

    /// Range steps must be positive integers.
    #[test]
    fn for_range_rejects_zero_or_negative_step() {
        let zero = run_temp_project_error(
            "for-range-step-zero",
            r#"
for i in 0..10 step 0 {
    echo i
}
"#,
        );
        assert!(zero
            .message
            .contains("step must be an integer greater than 0"));

        let negative = run_temp_project_error(
            "for-range-step-negative",
            r#"
for i in 0..10 step -2 {
    echo i
}
"#,
        );
        assert!(negative
            .message
            .contains("step must be an integer greater than 0"));
    }

    /// Array cannot save itself as elements, otherwise the `Rc` reference count will form a loop that cannot be released.
    #[test]
    fn set_property_rejects_array_self_cycle() {
        let array = Value::Array(Rc::new(RefCell::new(Vec::new())));
        let err = Vm::set_property(&array, &Value::Int(0), array.clone(), false)
            .expect_err("Array self-references must be rejected");

        assert!(err.contains("circular reference"));
    }

    /// Object cannot save native methods bound to itself to avoid the strong reference loop of `obj -> method -> obj`.
    #[test]
    fn set_property_rejects_native_method_receiver_cycle() {
        let vm = Vm::new();
        let object = Value::Object(Rc::new(RefCell::new(IndexMap::new())));
        let method = vm
            .get_property(&object, &Value::Str("keys".to_string()), false)
            .expect("object prototype method should be able to read");
        let err = Vm::set_property(&object, &Value::Str("keys_ref".to_string()), method, false)
            .expect_err("native methods binding to itself must be rejected");

        assert!(err.contains("circular reference"));
    }

    /// Ordinary scalar writes should not be accidentally damaged by circular reference checks.
    #[test]
    fn set_property_keeps_scalar_assignment_fast_path() {
        let object = Value::Object(Rc::new(RefCell::new(IndexMap::new())));

        Vm::set_property(
            &object,
            &Value::Str("name".to_string()),
            Value::Str("bt".to_string()),
            false,
        )
        .expect("Scalar property write should succeed");

        let Value::Object(values) = object else {
            panic!("test object has an unexpected type");
        };
        assert_eq!(
            values.borrow().get("name"),
            Some(&Value::Str("bt".to_string()))
        );
    }

    /// Extension entry should be injected into the user's global environment and run through the pure BT runner chain call.
    #[cfg(feature = "extensions")]
    #[test]
    fn extensions_inject_global_and_call_bt_runner() {
        let (_, value) = run_extension_project(
            "extension-calc-chain",
            r#"
            num = calc(1)
            [
                type(calc),
                type(num),
                has_env('calc'),
                has_envs('calc'),
                env().has_key('calc'),
                num.add(2).value(),
                call('calc', 4).add(5).value()
            ]
            "#,
        )
        .expect("extension chain call should succeed");

        assert_eq!(value.to_string(), "[\"Fn\",\"Calc\",true,false,true,3,9]");
    }

    /// Extended object close, the old handle should be invalidated to avoid uncontrolled growth of the object table in long-term operation.
    #[cfg(feature = "extensions")]
    #[test]
    fn extension_object_close_invalidates_handle() {
        let err = run_extension_project(
            "extension-calc-close",
            r#"
            num = calc(1)
            num.close()
            num.value()
            "#,
        )
        .expect_err("the extension object handle should be invalid after closing");

        assert!(err.message.contains("is no longer valid"));
    }

    /// After a WASM release method succeeds, the VM rejects the old handle regardless of the extension's internal cleanup.
    #[cfg(feature = "extensions")]
    #[test]
    fn wasm_dispose_method_invalidates_handle_in_vm() {
        let project = fresh_temp_project("extension-wasm-dispose");
        write_wasm_dispose_extension(&project.root);

        let err = run_extension_project_source(
            &project.root,
            r#"
            num = wasm_dispose()
            num.close()
            num.value()
            "#,
        )
        .expect_err("the old WASM handle should be rejected after disposal");

        assert!(err.message.contains("has expired"));
    }

    /// Extension entry cannot be reassigned by the script after being injected as a global constant.
    #[cfg(feature = "extensions")]
    #[test]
    fn extension_global_is_readonly() {
        let err = run_extension_project(
            "extension-calc-readonly",
            r#"
            calc = 1
            "#,
        )
        .expect_err("extension entry reassignment should fail");

        assert!(err.message.contains("constant `calc` cannot be reassigned"));
    }

    /// WASM extension should support path_read/path_write transformations and WASI project root pre-opening.
    #[cfg(feature = "extensions")]
    #[test]
    fn wasm_extension_path_roles_copy_file_with_wasi_preopen() {
        let project = fresh_temp_project("extension-file-demo");
        write_file_demo_extension(&project.root);
        write_text(&project.root.join("in.txt"), "hello wasi");
        fs::create_dir_all(project.root.join("nested")).unwrap();

        let (_, value) = run_extension_project_source(
            &project.root,
            r#"
            file_demo('@/in.txt').copy_to('@/nested/out.txt')
            "#,
        )
        .expect("WASM file-access extension should succeed");

        assert_eq!(value, Value::Bool(true));
        assert_eq!(
            fs::read_to_string(project.root.join("nested/out.txt")).unwrap(),
            "hello wasi"
        );
    }

    /// WASM path role must deny escape from the project root via `..`.
    #[cfg(feature = "extensions")]
    #[test]
    fn wasm_extension_path_role_rejects_project_escape() {
        let project = fresh_temp_project("extension-file-escape");
        write_file_demo_extension(&project.root);
        let outside_name = format!(
            "{}-outside.txt",
            project.root.file_name().unwrap().to_string_lossy()
        );
        let outside_path = project.root.parent().unwrap().join(&outside_name);
        write_text(&outside_path, "outside");
        let source = format!("file_demo('../{}')", outside_name);

        let err = run_extension_project_source(&project.root, &source)
            .expect_err("WASM extension reading path outside project root should fail");
        let _ = fs::remove_file(outside_path);

        assert!(err.message.contains("escapes project root"));
    }

    /// When the extension directory exists in the default build, it should be clearly prompted that the current build does not enable extension capabilities.
    #[cfg(not(feature = "extensions"))]
    #[test]
    fn disabled_extension_feature_rejects_project_extensions_dir() {
        let project = fresh_temp_project("extension-disabled");
        fs::create_dir_all(project.root.join("extensions")).unwrap();
        let mut vm = Vm::with_project_root(&project.root);
        let err = vm.load_project_extensions().unwrap_err();

        assert!(err.contains("expansion capability not enabled"));
    }

    /// Array/Object clone should copy the mutable container recursively to avoid nested objects continuing to share references.
    #[test]
    fn collection_clone_deep_copies_nested_mutable_values() {
        let value = run_test_source(
            "
            user = {profile: {name: 'A'}, tags: [{name: 'x'}]}
            copy = user.clone()
            copy.profile.name = 'B'
            copy.tags[0].name = 'y'
            [user.profile.name, user.tags[0].name, copy.profile.name, copy.tags[0].name]
            ",
        );

        assert_eq!(value.to_string(), "[\"A\",\"x\",\"B\",\"y\"]");
    }

    /// Object.reverse should reverse keys in object insertion order and is no longer equivalent to a shallow copy.
    #[test]
    fn object_reverse_returns_reversed_key_order() {
        let value = run_test_source("{a: 1, b: 2, c: 3}.reverse().keys()");

        assert_eq!(value.to_string(), "[\"c\",\"b\",\"a\"]");
    }

    /// String and number parsing capabilities should be linked to prototype methods, and old system functions no longer appear in the system environment.
    #[test]
    fn string_and_number_parse_methods_replace_global_helpers() {
        let value = run_test_source(
            r#"
            data = '{"name":"BT"}'.parse_json()
            code = 65
            [
                data.name,
                'ff'.parse_radix_int(16),
                '42 54'.parse_radix_str(16),
                code.to_char(),
                has_envs('parse_json'),
                has_envs('radix'),
                has_envs('to_char')
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            "[\"BT\",255,\"BT\",\"A\",false,false,false]"
        );
    }

    /// Math should be exposed as a global static object and the old math() built-in entry removed.
    #[test]
    fn math_static_object_replaces_old_constructor() {
        let value = run_test_source(
            r#"
            [
                Math.pow(2, 3),
                Math.sqrt(9),
                Math.TAU > Math.PI,
                type(Math.random()),
                has_envs('Math'),
                has_envs('math')
            ]
            "#,
        );

        assert_eq!(value.to_string(), "[8,3,true,\"Float\",true,false]");
    }

    /// BT should expose system information, environment overlays, PATH operations and capability detection as a global static object.
    #[test]
    fn bt_static_object_exposes_runtime_system_info() {
        let value = run_test_source(
            r#"
            BT.set_env('BT_TEST_RUNTIME_ENV', 'one')
            old = BT.set_env('BT_TEST_RUNTIME_ENV', 'two')
            removed = BT.remove_env('BT_TEST_RUNTIME_ENV')
            path_added = BT.add_path('@/bt-runtime-bin')
            path_has = BT.has_path('@/bt-runtime-bin')
            path_removed = BT.remove_path('@/bt-runtime-bin')
            info = BT.info()
            system = BT.system()
            runtime = BT.runtime()
            features = BT.features()
            [
                type(BT),
                has_envs('BT'),
                has_envs('bt'),
                BT.VERSION,
                BT.NAME == 'bt' || BT.NAME == 'bt_app',
                BT.OS == system.os,
                BT.ARCH == system.arch,
                BT.THREADS == system.threads,
                info.version == BT.VERSION,
                info.name == BT.NAME,
                info.cwd.len() > 0,
                runtime.threads == BT.THREADS,
                type(runtime.start_time),
                type(BT.runtime_id()),
                BT.has('process'),
                features.process,
                old,
                BT.env('BT_TEST_RUNTIME_ENV'),
                removed,
                BT.has_env('BT_TEST_RUNTIME_ENV'),
                path_added,
                path_has,
                path_removed
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            format!(
                "[\"BT\",true,false,\"{}\",true,true,true,true,true,true,true,true,\"Int\",\"String\",true,true,\"one\",null,\"two\",false,true,true,true]",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    /// BT runtime environment variable overlays should be merged into subsequent process subprocesses. Global static objects such as
    #[test]
    fn bt_env_overlay_is_applied_to_child_process() {
        let value = run_test_source(
            r#"
            BT.set_env('BT_TEST_PROCESS_ENV', 'from_bt')
            if BT.OS == 'windows' {
                process('cmd').args(['/C', 'echo %BT_TEST_PROCESS_ENV%']).output().stdout.trim()
            } else {
                process('sh').args(['-c', 'printf %s "$BT_TEST_PROCESS_ENV"']).output().stdout.trim()
            }
            "#,
        );

        assert_eq!(value, Value::Str("from_bt".to_string()));
    }

    /// BT and Math cannot be overwritten by scripts or have properties written to them.
    #[test]
    fn native_static_objects_are_readonly() {
        let err = run_temp_project_error("bt-readonly-name", "BT = 1");
        assert!(err
            .message
            .contains("constant `BT` cannot be defined repeatedly"));

        let err = run_temp_project_error("bt-readonly-property", "BT.VERSION = 'x'");
        assert!(err
            .message
            .contains("Global static object properties are read-only"));
    }

    /// The standardized prototype method name should replace the old name, and unknown old attributes will directly return empty.
    #[test]
    fn normalized_core_prototype_method_names_are_enforced() {
        let value = run_test_source(
            r#"
            text = 'abca'
            nums = [1, 2, 2]
            user = {name: 'BT'}
            [
                text.contains('bc'),
                text.contains(),
                text.index_of('a'),
                text.last_index_of('a'),
                text.char_at(1),
                text.char_code_at(0),
                nums.contains(2),
                nums.index_of(2),
                nums.last_index_of(2),
                user.has_key('name'),
                is_empty(text.contain),
                is_empty(nums.contain),
                is_empty(user.key_exists),
                is_empty((65).from_char)
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            "[true,false,0,3,\"b\",97,true,1,2,true,true,true,true,true]"
        );
    }

    /// HTML helpers live in the `html` standard library; String no longer exposes the legacy HTML methods.
    #[test]
    fn html_library_replaces_string_html_methods() {
        let value = run_test_source(
            r#"
            [
                html('<p>BT & "Web"</p>').escape(),
                html('&lt;b&gt;BT&lt;/b&gt;').unescape(),
                html('<p>BT</p>').strip(),
                type(html('x')),
                has_envs('html'),
                is_empty('x'.escape_html)
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            "[\"&lt;p&gt;BT &amp; &quot;Web&quot;&lt;/p&gt;\",\"<b>BT</b>\",\"BT\",\"Html\",true,true]"
        );
    }

    /// Low-risk basic prototype supplement should cover common string, array, object, and numeric judgment abilities.
    #[test]
    fn extra_core_prototype_helpers_cover_common_project_usage() {
        let value = run_test_source(
            r#"
            items = [1, 2, 2, 3]
            empty_obj = {}
            obj = empty_obj.from_entries([['a', 1], ['b', 2]])
            [
                '  bt  '.trim_start(),
                '  bt  '.trim_end(),
                'a-a-a'.replace_all('a', 'b'),
                '7'.pad_start(3, '0'),
                'x'.pad_end(4, 'ab'),
                items.first(),
                items.last(),
                items.is_empty(),
                items.unique(),
                items.chunk(2),
                {a: 1}.get('a'),
                {a: 1}.get('b', 'fallback'),
                empty_obj.is_empty(),
                obj.b,
                (1).is_int(),
                (1.5).is_float(),
                (1.5).is_finite()
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            "[\"bt  \",\"  bt\",\"b-b-b\",\"007\",\"xaba\",1,3,false,[1,2,3],[[1,2],[2,3]],1,\"fallback\",true,2,true,true,true]"
        );
    }

    /// The last batch of project-common standard libraries should be available for summary, URL, path and date operations directly from the scripting layer.
    #[test]
    fn project_utility_libraries_cover_crypto_url_path_and_date_math() {
        let value = run_test_source(
            r#"
            built = url({scheme: 'https', host: 'btlang.org', path: '/docs', query: {q: 'BT Lang'}}).build()
            parsed = url(built).parse()
            query = url(built).query()
            start = date('2026-01-01 10:11:12')
            next = start.add(1, 'days')
            [
                crypto('BT').sha256(),
                crypto('BT').hmac_sha256('key').len(),
                type(crypto().uuid()),
                built,
                parsed.host,
                query.q,
                path('root/a/../b.txt').normalize(),
                path('root/a/b.txt').dirname(),
                next.diff(start, 'days'),
                start.start_of_day().format('%Y-%m-%d %H:%M:%S'),
                has_envs('crypto'),
                has_envs('url'),
                has_envs('path')
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            "[\"4ea3d68e3581fa27f86acaa247b297686a8e1a8fecd5523cd8314f14b6a28c31\",64,\"String\",\"https://btlang.org/docs?q=BT%20Lang\",\"btlang.org\",\"BT Lang\",\"root/b.txt\",\"root/a\",1,\"2026-01-01 00:00:00\",true,true,true]"
        );
    }

    /// Empty represents a missing value, null represents an explicit null value or a failed conversion.
    #[test]
    fn empty_and_null_keep_separate_runtime_roles() {
        let value = run_test_source(
            r#"
            user = {name: null}
            nums = [1, 2]
            fn missing_arg(value) { value }
            [
                is_null(user.name),
                is_empty(user.age),
                is_empty(nums[9]),
                is_empty(missing_arg()),
                is_null(number('abc')),
                is_null(number(empty)),
                json([empty, null])
            ]
            "#,
        );

        assert_eq!(
            value.to_string(),
            "[true,true,true,true,true,true,\"[null,null]\"]"
        );
    }

    /// `&&`, `||`, and `??` short-circuit without evaluating the right side once the result is known.
    #[test]
    fn logical_operators_short_circuit_rhs_evaluation() {
        let value = run_test_source(
            r#"
            count = 0
            fn touch(value) {
                count += 1
                value
            }
            a = missing && missing.len() > 0
            b = false && touch('bad')
            c = 'ok' || touch('bad')
            d = false || touch('fallback')
            e = 0 ?? touch('zero')
            f = empty ?? touch('empty')
            g = null ?? touch('null')
            json([is_empty(a), b, c, d, e, f, g, count])
            "#,
        );

        assert_eq!(
            value,
            Value::Str("[true,false,\"ok\",\"fallback\",0,\"empty\",\"null\",3]".to_string())
        );
    }

    /// Actively outputs empty, and JSON serialization still outputs null according to standard JSON.
    #[test]
    fn explicit_output_shows_empty_but_json_uses_null() {
        let (output, value) = run_test_source_with_output(
            r#"
            print empty
            println null
            json({missing: empty, value: null})
            "#,
        );

        assert_eq!(output, "emptynull\n");
        assert_eq!(value.to_string(), "{\"missing\":null,\"value\":null}");
        assert_eq!(Value::Empty.to_output_string(), "empty");
    }

    /// Try/catch should catch the throw, and the sibling code after the throw will no longer be executed.
    #[test]
    fn try_catch_captures_throw_and_stops_current_flow() {
        let value = run_test_source(
            r#"
            log = []
            value = try {
                log.push('before')
                throw 'fatal'
                log.push('after')
                'ok'
            } catch e {
                log.push(e)
                'caught:' + e
            }
            json([value, log.join('|')])
            "#,
        );

        assert_eq!(
            value,
            Value::Str("[\"caught:fatal\",\"before|fatal\"]".to_string())
        );
    }

    /// Throw should propagate outward through the function call until caught by the outer catch.
    #[test]
    fn throw_propagates_through_function_call() {
        let value = run_test_source(
            r#"
            fn boom() {
                throw 'deep'
                'after'
            }
            try {
                boom()
            } catch e {
                e
            }
            "#,
        );

        assert_eq!(value, Value::Str("deep".to_string()));
    }

    /// An uncaught throw should terminate the program and return a clear runtime error.
    #[test]
    fn uncaught_throw_returns_runtime_error() {
        let file = "test.bt".to_string();
        let source = "throw 'fatal'";
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        let chunk = Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect("test script should compile successfully");
        let mut vm = Vm::new();
        let err = vm
            .run_with_value(&chunk)
            .expect_err("uncaught throw should fail");

        assert!(err.message.contains("Uncaught exception: fatal"));
    }

    /// Break must leave the capture scope synchronously when jumping out of try to avoid subsequent throws being captured by the old catch.
    #[test]
    fn break_leaves_try_handler_before_following_throw() {
        let file = "test.bt".to_string();
        let source = r#"
        loop {
            try {
                break
            } catch e {
                'bad'
            }
        }
        throw 'fatal'
        "#;
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        let chunk = Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect("test script should compile successfully");
        let mut vm = Vm::new();
        let err = vm
            .run_with_value(&chunk)
            .expect_err("uncaught throw should fail");

        assert!(err.message.contains("Uncaught exception: fatal"));
    }

    /// Allows the use of keywords to ensure that prototype methods such as `match` do not conflict with language keywords.
    #[test]
    fn keyword_named_method_can_be_called_after_dot() {
        let value = run_test_source(
            r#"
            text = 'hello BT'
            result = text.match('BT')
            json(result)
            "#,
        );

        assert_eq!(value, Value::Str("[\"BT\"]".to_string()));
    }

    /// Match expressions should be matched in order, and default branches are supported; empty is returned when there is no match and no default branch.
    #[test]
    fn match_expression_returns_selected_value_or_empty() {
        let value = run_test_source(
            r#"
            x = 1
            a = match x + 1 {
                2 => 'a',
                3 => 'b',
                _ => 'c'
            }
            b = match 9 {
                1 => 'x'
            }
            json([a, is_empty(b)])
            "#,
        );

        assert_eq!(value, Value::Str("[\"a\",true]".to_string()));
    }

    /// A match arm may use a multiline block whose value comes from its final statement.
    #[test]
    fn match_arm_block_returns_last_statement_value() {
        let value = run_test_source(
            r#"
            x = 1
            result = match x + 1 {
                2 => {
                    if x == 1 {
                        'ok'
                    } else {
                        'bad'
                    }
                },
                _ => 'c'
            }
            json([result])
            "#,
        );

        assert_eq!(value, Value::Str("[\"ok\"]".to_string()));
    }

    /// Match branch code block directly exits the current function, not just returns the match branch.
    #[test]
    fn return_inside_match_arm_block_returns_current_function() {
        let value = run_test_source(
            r#"
            fn demo() {
                x = 1
                result = match x + 1 {
                    2 => {
                        return 'ok'
                    },
                    _ => 'c'
                }
                'after:' + result
            }
            demo()
            "#,
        );

        assert_eq!(value, Value::Str("ok".to_string()));
    }

    /// The new Array prototype method should support negative subscripts, in-place insertion, bitwise removal and reserved capacity clearing.
    #[test]
    fn array_extra_methods_cover_indexing_mutation_and_reuse() {
        let value = run_test_source(
            "
            items = [1, 2, 3]
            last = items.at(-1)
            items.insert(1, 9, 8)
            removed = items.remove_at(-2)
            items.clear()
            [last, removed, items.len()]
            ",
        );

        assert_eq!(value.to_string(), "[3,2,0]");
    }

    /// The new Object prototype method should cover entry, filtering, search, cropping, in-place update and clearing.
    #[test]
    fn object_extra_methods_cover_entries_filter_find_update_and_shape() {
        let value = run_test_source(
            "
            obj = {a: 1, b: 2, c: 3}
            entries = obj.entries()
            filtered = obj.filter(fn(value, key) { value > 1 })
            picked = obj.pick(['c', 'a'])
            omitted = obj.omit('b')
            target = {x: 1}
            target.update({y: 2}, {z: 3})
            target.update(target)
            found = obj.find(fn(value) { value > 1 })
            found_key = obj.find_key(fn(value) { value > 1 })
            checks = [obj.some(fn(value) { value == 2 }), obj.every(fn(value) { value > 0 })]
            target.clear()
            [entries[0][0], filtered.keys(), picked.keys(), omitted.keys(), found, found_key, checks, target.len()]
            ",
        );

        assert_eq!(
            value.to_string(),
            "[\"a\",[\"b\",\"c\"],[\"c\",\"a\"],[\"a\",\"c\"],2,\"b\",[true,true],0]"
        );
    }

    /// Object.update must reuse circular reference detection before writing to prevent NativeMethod from strongly referencing the receiver back to the object.
    #[test]
    fn object_update_rejects_values_referencing_receiver() {
        let file = "test.bt".to_string();
        let source = "
            obj = {}
            holder = {method: obj.keys}
            obj.update(holder)
        ";
        let tokens = tokenize(source).collect::<Vec<_>>();
        let mut parser = Parser::new(file.clone(), source, tokens);
        let statements = parser
            .parse()
            .expect("test script should parse successfully");
        let chunk = Compiler::with_source_file(file, Path::new("."))
            .compile_returning_value(&statements)
            .expect("test script should compile successfully");
        let mut vm = Vm::new();
        let err = vm
            .run_with_value(&chunk)
            .expect_err("update writing self-referential value must fail");

        assert!(err.message.contains("circular reference"));
    }

    /// Template fragments reuse compiled chunks to avoid parser/compiler allocation on hot request paths.
    #[test]
    fn template_fragment_cache_reuses_compiled_chunk() {
        TEMPLATE_FRAGMENT_CACHE.with(|cache| cache.borrow_mut().clear());
        VM_CACHE_METRICS.with(|metrics| *metrics.borrow_mut() = VmCacheMetrics::default());
        let vm = Vm::new();
        let span = SourceSpan {
            file: "web/tpl/test.bt".to_string(),
            line: 1,
            column: 1,
        };

        let first = vm
            .compile_template_fragment(&span, "1 + 2", false, 0)
            .expect("template expression should compile successfully");
        let second = vm
            .compile_template_fragment(&span, "1 + 2", false, 0)
            .expect("template expression should hit cache");

        assert!(Rc::ptr_eq(&first, &second));
        let stats = cache_stats();
        assert!(stats.template_fragment_hits >= 1);
        assert!(stats.template_fragment_misses >= 1);
        assert!(stats.template_fragment_bytes > 0);
    }

    /// The file-compilation cache includes `allow_template` in its key so template includes cannot bypass regular web-entry validation.
    #[test]
    fn compiled_file_cache_separates_template_permission() {
        COMPILED_FILE_CACHE.with(|cache| cache.borrow_mut().clear());
        VM_CACHE_METRICS.with(|metrics| *metrics.borrow_mut() = VmCacheMetrics::default());
        let project = fresh_temp_project("cache-template-mode");
        let template = project.root.join("page.bt");
        write_text(&template, "# TPL\nhello");

        let cached_template = compile_cached_file(&template, true)
            .expect("template include should allow compilation");
        let err = compile_cached_file(&template, false)
            .expect_err("regular web entries must not reuse the template-include cache");

        assert!(err.contains("web entry file must be a regular BT script"));
        assert!(!cached_template.source_file.is_empty());
    }

    /// Recent files must be content fingerprinted to avoid reusing old bytecode even if the mtime and length appear to be unchanged.
    #[test]
    fn compiled_file_cache_rechecks_recent_same_length_rewrite() {
        COMPILED_FILE_CACHE.with(|cache| cache.borrow_mut().clear());
        VM_CACHE_METRICS.with(|metrics| *metrics.borrow_mut() = VmCacheMetrics::default());
        let project = fresh_temp_project("cache-same-length-rewrite");
        let main = project.root.join("main.bt");
        write_text(&main, "return 1");

        let cache_path = compiled_file_cache_path(&main);
        let cache_key = CompiledFileCacheKey {
            path: cache_path.clone(),
            allow_template: false,
        };
        let (old_chunk, source_mode, source_fingerprint) =
            compile_file_to_chunk(&cache_path, &main, false)
                .expect("old source code should be able to compile");
        let old_chunk = Rc::new(old_chunk);
        let estimated_bytes = compiled_file_entry_bytes(&cache_key, &old_chunk);

        write_text(&main, "return 2");
        let metadata =
            fs::metadata(&cache_path).expect("test file meta information should be readable");
        COMPILED_FILE_CACHE.with(|cache| {
            cache.borrow_mut().insert(
                cache_key,
                CachedChunk {
                    modified: metadata.modified().ok(),
                    len: metadata.len(),
                    source_mode,
                    source_fingerprint,
                    compile_config_version: COMPILE_CACHE_CONFIG_VERSION,
                    bytecode_format_version: BYTECODE_FORMAT_VERSION,
                    estimated_bytes,
                    chunk: old_chunk,
                },
            );
        });

        let chunk =
            compile_cached_file(&main, false).expect("new source code should be recompiled");
        let mut vm = Vm::with_project_root(&project.root);
        let (_, value) = vm
            .run_with_value_owned(chunk)
            .expect("new bytecode should execute successfully");
        let stats = cache_stats();

        assert_eq!(value.to_string(), "2");
        assert!(stats.compiled_file_fingerprint_checks >= 1);
        assert!(stats.compiled_file_invalidations >= 1);
    }

    /// File compilation cache statistics should expose hits, misses, bytes and caps for observation by BT.stats().
    #[test]
    fn compiled_file_cache_stats_count_hits_and_bytes() {
        COMPILED_FILE_CACHE.with(|cache| cache.borrow_mut().clear());
        VM_CACHE_METRICS.with(|metrics| *metrics.borrow_mut() = VmCacheMetrics::default());
        let project = fresh_temp_project("cache-stats");
        let main = project.root.join("main.bt");
        write_text(&main, "1 + 2");

        let first =
            compile_cached_file(&main, false).expect("The first compilation should be successful");
        let second = compile_cached_file(&main, false).expect("hits the cache for the second time");
        let stats = cache_stats();

        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(stats.compiled_file_entries, 1);
        assert!(stats.compiled_file_bytes > 0);
        assert_eq!(
            stats.compiled_file_bytes_limit,
            COMPILED_FILE_CACHE_BYTES_LIMIT
        );
        assert!(stats.compiled_file_hits >= 1);
        assert!(stats.compiled_file_misses >= 1);
    }

    /// Echo, include, and include_once should support directed open unbracketed statement forms.
    #[test]
    fn bare_system_calls_parse_and_execute() {
        let project = fresh_temp_project("bare-system-call");
        write_text(&project.root.join("lib.bt"), "value = value + 1");
        write_text(
            &project.root.join("main.bt"),
            "
            value = 0
            echo 'ready'
            include 'lib.bt'
            include_once 'lib.bt'
            include_once './lib.bt'
            value + 0
            ",
        );

        let chunk = compile_cached_file(&project.root.join("main.bt"), false)
            .expect("entry script should be compiled successfully");
        let mut vm = Vm::with_project_root(&project.root);
        let (_, value) = vm
            .run_with_value_owned(chunk)
            .expect("entry script should be executed successfully");

        assert_eq!(value.to_string(), "2");
    }

    /// Include must execute the target file every time and cannot skip running because it hits the compilation cache.
    #[test]
    fn include_runs_every_time_even_when_compiled_cached() {
        let project = fresh_temp_project("include-repeat");
        write_text(&project.root.join("inc.bt"), "counter = counter + 1");
        write_text(
            &project.root.join("main.bt"),
            "
            counter = 0
            include 'inc.bt'
            include './inc.bt'
            counter + 0
            ",
        );

        let chunk = compile_cached_file(&project.root.join("main.bt"), false)
            .expect("entry script should be compiled successfully");
        let mut vm = Vm::with_project_root(&project.root);
        let (_, value) = vm
            .run_with_value_owned(chunk)
            .expect("entry script should be executed successfully");

        assert_eq!(value.to_string(), "2");
    }

    /// Include_once Skip duplicate paths only within the current execution context until execution is re-allowed on the next long-lived VM call.
    #[test]
    fn include_once_state_is_scoped_to_execution_context() {
        let project = fresh_temp_project("include-once-context");
        write_text(&project.root.join("inc.bt"), "counter = counter + 1");
        write_text(
            &project.root.join("main.bt"),
            "
            counter = 0
            fn click() {
                include_once 'inc.bt'
                include_once './inc.bt'
                return counter
            }
            ",
        );

        let chunk = compile_cached_file(&project.root.join("main.bt"), false)
            .expect("entry script should be compiled successfully");
        let mut vm = Vm::with_project_root(&project.root);
        vm.run_with_value_owned(chunk)
            .expect("entry script should be executed successfully");

        let first = vm
            .call_global("click", Vec::new())
            .expect("The first call should execute include_once");
        let second = vm
            .call_global("click", Vec::new())
            .expect("the second call should allow include_once again");

        assert_eq!(first.to_string(), "1");
        assert_eq!(second.to_string(), "2");
    }

    /// Runtime paths follow the active source file and support `@` plus the `cur_*` system functions.
    #[test]
    fn runtime_paths_follow_source_stack_and_project_root() {
        let project = fresh_temp_project("source-stack");
        write_text(&project.root.join("config/root.txt"), "root");
        write_text(&project.root.join("common/local.txt"), "local");
        write_text(&project.root.join("common/nested.txt"), "nested");
        write_text(
            &project.root.join("common/nested.bt"),
            "fs('nested.txt').read()",
        );
        write_text(
            &project.root.join("common/util.bt"),
            "fn read_local() { fs('local.txt').read() }\nnested_value = include('nested.bt')",
        );
        write_text(
            &project.root.join("main.bt"),
            "include('common/util.bt')\nread_local() + '|' + nested_value + '|' + fs('@/config/root.txt').read() + '|' + cur_dir() + '|' + cur_file() + '|' + cur_dir(true) + '|' + cur_file(true) + '|' + cur_root()",
        );

        let chunk = compile_cached_file(&project.root.join("main.bt"), false)
            .expect("entry script should be compiled successfully");
        let mut vm = Vm::with_project_root(&project.root);
        let (_, value) = vm
            .run_with_value_owned(chunk)
            .expect("entry script should be executed successfully");
        let root = bt_path::path_text(&project.root);

        assert_eq!(
            value.to_string(),
            format!(
                "local|nested|root|.|main.bt|{}|{}/main.bt|{}",
                root, root, root
            )
        );
    }

    /// Web file direct outgoing path must reuse VM unified path rules.
    #[test]
    fn web_send_file_resolves_relative_path_from_source_file() {
        let project = fresh_temp_project("web-send-file-path");
        write_text(&project.root.join("common/file.txt"), "download");
        write_text(
            &project.root.join("common/main.bt"),
            "send_file('file.txt')",
        );

        let chunk = compile_cached_file(&project.root.join("common/main.bt"), false)
            .expect("entry script should be compiled successfully");
        let response = std::rc::Rc::new(std::cell::RefCell::new(
            crate::libs::web::BtWebResponse::new(),
        ));
        let mut vm = Vm::with_project_root(&project.root);
        vm.set_web_response(response.clone());
        let (_, value) = vm
            .run_with_value_owned(chunk)
            .expect("entry script should be executed successfully");

        assert_eq!(value, Value::Bool(true));
        assert_eq!(
            response.borrow().file.as_deref(),
            Some(bt_path::path_text(&project.root.join("common/file.txt")).as_str())
        );
    }
}
