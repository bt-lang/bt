use crate::app::resource::ResourceSource;
use crate::compiler::Compiler;
use crate::error::BtError;
use crate::lexer::{tokenize, Token};
use crate::parser::{Parser, Statement};
use crate::path as bt_path;
use crate::source::{analyze_source, SourceMode};
use crate::value::Value;
use crate::vm::Vm;
use indexmap::IndexMap;
use serde_json::{Number as JsonNumber, Value as JsonValue};
use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum capacity of the App VM call queue.
const APP_VM_QUEUE_LIMIT: usize = 128;
/// Fixed stack size for the long-lived desktop VM worker, sized for worst-case large-object serialization and deeply nested script calls.
const APP_VM_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Thread handle for the desktop app's long-lived VM.
///
/// The VM uses `Rc<RefCell<_>>` internally to preserve script object reference semantics, so it
/// cannot be stored directly in Tauri `State`. Pinning it to one worker thread lets frontend calls
/// enter serially through a channel, preserving persistent state without adding thread-safe
/// containers to the VM's hot path.
#[derive(Clone)]
pub struct AppVmHandle {
    /// Shared handle to the long-lived VM worker.
    worker: Arc<AppVmWorker>,
    /// Temporary project directory materialized for the VM in Bundle mode.
    #[allow(dead_code)]
    temp_dir: Option<Arc<AppVmTempDir>>,
}

impl std::fmt::Debug for AppVmHandle {
    /// Emits concise debug output without exposing channel internals.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppVmHandle").finish_non_exhaustive()
    }
}

/// State for the long-lived VM worker.
struct AppVmWorker {
    /// Call channel to the long-lived VM worker; cleared on shutdown to wake the VM thread.
    calls: Mutex<Option<SyncSender<AppVmRequest>>>,
    /// VM worker handle; joined on shutdown so VM resources are released first.
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl AppVmWorker {
    /// Closes the VM call channel and waits for the worker to release VM resources.
    fn shutdown(&self) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.take();
        }
        if let Ok(mut thread) = self.thread.lock() {
            if let Some(handle) = thread.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for AppVmWorker {
    /// Shuts down the worker synchronously when the last VM handle is dropped.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A BT function call requested by the frontend.
struct AppVmRequest {
    /// Name of the BT global function to call.
    name: String,
    /// JSON arguments supplied by the frontend.
    args: JsonValue,
    /// One-shot response channel for the call result.
    reply: Sender<Result<JsonValue, String>>,
}

/// Keep-alive handle for the App VM's temporary project directory.
struct AppVmTempDir {
    /// Temporary directory containing materialized Bundle resources.
    path: PathBuf,
}

impl Drop for AppVmTempDir {
    /// Removes the temporary directory when the VM handle is dropped.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl AppVmHandle {
    /// Calls a global function registered by `main.bt` in the long-lived VM.
    pub fn call(&self, name: String, args: JsonValue) -> Result<JsonValue, String> {
        let (reply, result) = mpsc::channel();
        let request = AppVmRequest { name, args, reply };
        let calls = {
            let calls = self
                .worker
                .calls
                .lock()
                .map_err(|_| "BT VM call channel is poisoned".to_string())?;
            calls
                .as_ref()
                .cloned()
                .ok_or_else(|| "BT VM worker has exited".to_string())?
        };
        calls.try_send(request).map_err(|err| match err {
            TrySendError::Full(_) => "BT VM call queue is full; please try again later".to_string(),
            TrySendError::Disconnected(_) => "BT VM worker has exited".to_string(),
        })?;
        result
            .recv()
            .map_err(|_| "BT VM did not return a call result".to_string())?
    }

    /// Shuts down the long-lived VM and waits for managed processes, timers, and temporary directories to be dropped.
    pub fn shutdown(&self) {
        self.worker.shutdown();
    }
}

/// Starts a long-lived BT VM when `app.main` is configured in app.json.
pub fn start_app_vm(
    resource: &ResourceSource,
    project_dir: &Path,
    main: Option<&str>,
) -> Result<Option<AppVmHandle>, BtError> {
    let Some(main) = main.map(str::trim).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };

    let mut prepared = prepare_app_vm_project(resource, project_dir)?;
    let source_path = prepared.root.join(main);
    let source = fs::read_to_string(&source_path).map_err(BtError::Io)?;
    let base_dir = main_base_dir(&prepared.root, main);
    let project_root = prepared.root.clone();
    let source_name = bt_path::path_text(&source_path);
    let temp_dir = prepared
        .temp_dir
        .take()
        .map(|path| Arc::new(AppVmTempDir { path }));
    let (calls, requests) = mpsc::sync_channel(APP_VM_QUEUE_LIMIT);
    let (ready, initialized) = mpsc::channel();

    let worker_thread = thread::Builder::new()
        .name("bt-app-vm".to_string())
        .stack_size(APP_VM_THREAD_STACK_BYTES)
        .spawn(move || {
            let mut vm = match init_vm_from_main(&source_name, &source, base_dir, project_root) {
                Ok(vm) => {
                    let _ = ready.send(Ok(()));
                    vm
                }
                Err(err) => {
                    let _ = ready.send(Err(err.to_string()));
                    return;
                }
            };
            run_vm_loop(&mut vm, requests);
        })
        .map_err(|err| BtError::Runtime(format!("Failed to create BT VM worker: {}", err)))?;

    match initialized.recv() {
        Ok(Ok(())) => Ok(Some(AppVmHandle {
            worker: Arc::new(AppVmWorker {
                calls: Mutex::new(Some(calls)),
                thread: Mutex::new(Some(worker_thread)),
            }),
            temp_dir,
        })),
        Ok(Err(message)) => {
            let _ = worker_thread.join();
            Err(BtError::Runtime(message))
        }
        Err(_) => {
            let _ = worker_thread.join();
            Err(BtError::Runtime(
                "BT VM worker did not return an initialization result after startup".to_string(),
            ))
        }
    }
}

/// Prepared App VM project directory.
struct PreparedAppVmProject {
    /// Project root used by the App VM.
    root: PathBuf,
    /// Temporary directory materialized in Bundle mode.
    temp_dir: Option<PathBuf>,
}

impl Drop for PreparedAppVmProject {
    /// Cleans up a temporary directory not yet transferred to the VM handle if preparation fails.
    fn drop(&mut self) {
        if let Some(path) = &self.temp_dir {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Prepares a filesystem-accessible project directory for the long-lived App VM.
fn prepare_app_vm_project(
    resource: &ResourceSource,
    project_dir: &Path,
) -> Result<PreparedAppVmProject, BtError> {
    match resource {
        ResourceSource::Directory(path) => Ok(PreparedAppVmProject {
            root: path
                .canonicalize()
                .unwrap_or_else(|_| project_dir.to_path_buf()),
            temp_dir: None,
        }),
        ResourceSource::Bundle(_) | ResourceSource::Btr(_) => {
            let temp_dir = create_app_vm_temp_dir()?;
            materialize_app_vm_resource(resource, &temp_dir)?;
            Ok(PreparedAppVmProject {
                root: temp_dir.clone(),
                temp_dir: Some(temp_dir),
            })
        }
        ResourceSource::Embedded(_) => Err(BtError::Config(
            "the built-in setup page does not support app.main".to_string(),
        )),
    }
}

/// Materializes app resources in a temporary project directory.
fn materialize_app_vm_resource(resource: &ResourceSource, root: &Path) -> Result<(), BtError> {
    for name in resource.list() {
        validate_materialized_path(&name)?;
        let output = root.join(Path::new(&name));
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(output, resource.read(&name)?)?;
    }
    Ok(())
}

/// Creates a dedicated temporary project directory for the App VM.
fn create_app_vm_temp_dir() -> Result<PathBuf, BtError> {
    let base = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    for index in 0..100u32 {
        let dir = base.join(format!(
            "bt-app-vm-{}-{}",
            std::process::id(),
            stamp + index as u128
        ));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(BtError::Io(err)),
        }
    }
    Err(BtError::Runtime(
        "failed to create App VM temporary directory".to_string(),
    ))
}

/// Ensures a materialization path remains a safe relative path within the Bundle.
fn validate_materialized_path(path: &str) -> Result<(), BtError> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(BtError::Bundle(format!(
            "Bundle materialization rejects absolute path: {}",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(BtError::Bundle(format!(
                    "Bundle materialization rejects unsafe path: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

/// Creates a VM from main.bt source and runs it once to register functions.
fn init_vm_from_main(
    source_name: &str,
    source: &str,
    base_dir: PathBuf,
    project_root: PathBuf,
) -> Result<Vm, BtError> {
    let statements = parse_app_main(source_name, source)?;
    let chunk = Compiler::with_source_file(source_name.to_string(), base_dir)
        .compile(&statements)
        .map_err(|err| BtError::Compile(err.to_string()))?;
    let mut vm = Vm::with_project_root(project_root);
    vm.load_project_extensions()
        .map_err(|err| BtError::Runtime(err.to_string()))?;
    vm.run(&chunk)
        .map_err(|err| BtError::Runtime(err.to_string()))?;
    vm.clear_output();
    Ok(vm)
}

/// Parses the regular BT script referenced by app.main.
fn parse_app_main(source_name: &str, source: &str) -> Result<Vec<Statement>, BtError> {
    let document =
        analyze_source(source_name, source).map_err(|err| BtError::Compile(err.to_string()))?;
    if document.mode != SourceMode::Script {
        return Err(BtError::Compile(format!(
            "`{}` is not a regular BT script and cannot be run as app.main",
            source_name
        )));
    }
    let tokens: Vec<Token> = tokenize(&document.body).collect();
    let mut parser = Parser::new(source_name, &document.body, tokens);
    parser
        .parse()
        .map_err(|err| BtError::Compile(err.to_string()))
}

/// Continuously processes frontend call requests and background events in the long-lived VM.
fn run_vm_loop(vm: &mut Vm, requests: Receiver<AppVmRequest>) {
    loop {
        vm.drain_timer_events();
        vm.drain_task_events();
        let request = if let Some(timeout) = vm.next_background_wait() {
            match requests.recv_timeout(timeout) {
                Ok(request) => Some(request),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match requests.recv() {
                Ok(request) => Some(request),
                Err(_) => break,
            }
        };
        if let Some(request) = request {
            let result = call_vm(vm, &request.name, request.args);
            let _ = request.reply.send(result);
            vm.drain_timer_events();
            vm.drain_task_events();
        }
    }
}

/// Calls the specified BT function in the long-lived VM and converts its return value to JSON.
fn call_vm(vm: &mut Vm, name: &str, args: JsonValue) -> Result<JsonValue, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("bt.call() function name cannot be empty".to_string());
    }

    vm.clear_output();
    let args = json_to_bt_args(args);
    let result = vm
        .call_global(name, args)
        .map_err(|err| err.to_string())
        .and_then(|value| bt_value_to_json_value(&value));
    vm.clear_output();
    result
}

/// Converts the frontend's JSON argument payload into BT function arguments.
///
/// The current bridge always encodes the remaining `window.bt.call(name, ...args)` arguments as a
/// JSON array. The non-array branch supports legacy callers so a direct Tauri command call does not
/// split a single object argument.
fn json_to_bt_args(args: JsonValue) -> Vec<Value> {
    match args {
        JsonValue::Array(values) => values.into_iter().map(json_to_bt_value).collect(),
        value => vec![json_to_bt_value(value)],
    }
}

/// Resolves the app.main file's directory for relative includes at compile time.
fn main_base_dir(project_dir: &Path, main: &str) -> PathBuf {
    let parent = Path::new(main)
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    project_dir.join(parent)
}

/// Converts a frontend JSON value to a BT VM value.
fn json_to_bt_value(value: JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(value) => Value::Bool(value),
        JsonValue::Number(value) => json_number_to_bt_value(value),
        JsonValue::String(value) => Value::Str(value),
        JsonValue::Array(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(json_to_bt_value(value));
            }
            Value::Array(Rc::new(RefCell::new(output)))
        }
        JsonValue::Object(values) => {
            let mut output = IndexMap::with_capacity(values.len());
            for (key, value) in values {
                output.insert(key, json_to_bt_value(value));
            }
            Value::Object(Rc::new(RefCell::new(output)))
        }
    }
}

/// Converts a JSON number to a BT numeric value, preferring lossless representations.
fn json_number_to_bt_value(value: JsonNumber) -> Value {
    if let Some(value) = value.as_i64() {
        return Value::Int(value);
    }
    if let Some(value) = value.as_u64() {
        if value <= i64::MAX as u64 {
            return Value::Int(value as i64);
        }
        return Value::Float(value as f64);
    }
    value.as_f64().map(Value::Float).unwrap_or(Value::Null)
}

/// Uses Value's standard JSON output and converts it to a JSON value suitable for a Tauri command response.
fn bt_value_to_json_value(value: &Value) -> Result<JsonValue, String> {
    serde_json::from_str(&value.to_json_string())
        .map_err(|err| format!("BT return value cannot be converted to JSON: {}", err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The JSON array from `window.bt.call(name, ...args)` is expanded into positional BT arguments.
    #[test]
    fn call_vm_expands_json_array_args() {
        let mut vm = init_vm_from_main(
            "main.bt",
            "fn add(a, b) { return { error:false, message:'ok', data:a + b } }",
            PathBuf::from("."),
            PathBuf::from("."),
        )
        .expect("test VM should initialize successfully");

        let value =
            call_vm(&mut vm, "add", json!([20, 22])).expect("multi-argument call should succeed");

        assert_eq!(
            value,
            json!({
                "error": false,
                "message": "ok",
                "data": 42
            })
        );
    }

    /// The preferred form passes a single object argument through unchanged so fields can be added easily.
    #[test]
    fn call_vm_keeps_object_arg_for_single_parameter() {
        let mut vm = init_vm_from_main(
            "main.bt",
            "fn add(args) { return { error:false, message:'ok', data:args.a + args.b } }",
            PathBuf::from("."),
            PathBuf::from("."),
        )
        .expect("test VM should initialize successfully");

        let value = call_vm(&mut vm, "add", json!([{ "a": 20, "b": 22 }]))
            .expect("object-argument call should succeed");

        assert_eq!(
            value,
            json!({
                "error": false,
                "message": "ok",
                "data": 42
            })
        );
    }

    /// A single JSON value passed through the legacy entry point remains one BT argument.
    #[test]
    fn json_to_bt_args_keeps_single_value() {
        let args = json_to_bt_args(json!({ "name": "BT" }));

        assert_eq!(args.len(), 1);
        assert!(matches!(args.first(), Some(Value::Object(_))));
    }

    /// Shutting down the long-lived App VM waits for the worker and releases its FfiBuffer allocation.
    #[cfg(feature = "ffi")]
    #[test]
    fn app_vm_shutdown_releases_ffi_buffers() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let baseline = crate::libs::ffi::stats();
        let (calls, requests) = mpsc::sync_channel(APP_VM_QUEUE_LIMIT);
        let (ready, initialized) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("bt-app-vm-ffi-test".to_string())
            .spawn(move || {
                let mut vm = init_vm_from_main(
                    "main.bt",
                    "buffer = ffi.buffer(17)\nfn buffer_len() { buffer.len() }",
                    PathBuf::from("."),
                    PathBuf::from("."),
                )
                .expect("test App VM should initialize successfully");
                ready
                    .send(())
                    .expect("App VM initialization should be reported");
                run_vm_loop(&mut vm, requests);
            })
            .expect("App VM test thread should be created");
        initialized
            .recv()
            .expect("should wait for App VM initialization");
        let handle = AppVmHandle {
            worker: Arc::new(AppVmWorker {
                calls: Mutex::new(Some(calls)),
                thread: Mutex::new(Some(thread)),
            }),
            temp_dir: None,
        };
        assert_eq!(
            handle.call("buffer_len".to_string(), json!([])).unwrap(),
            json!(17)
        );
        assert_eq!(crate::libs::ffi::stats().buffers, baseline.buffers + 1);
        handle.shutdown();
        assert_eq!(crate::libs::ffi::stats(), baseline);
    }
}
