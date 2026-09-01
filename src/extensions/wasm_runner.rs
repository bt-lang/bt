//! WASM/WASI extension runner.
//!
//! `kind=wasm` extensions expose `bts_alloc`, `bts_call`, and `bts_free` through the
//! `bts-wasi-1` ABI. Newer SDKs may also expose `bts_set_module_id` to receive the
//! host-assigned module ID. The runner caches instantiated Wasmtime runtimes only on
//! the current thread; the main `ExtensionManager` stores only shareable compiled
//! modules and binding metadata.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wasmparser::{Parser, Payload};
use wasmtime::{
    Cache, CacheConfig, Config, Engine, ExternType, Instance, Linker, Memory, Module, Store,
    TypedFunc, UpdateDeadline, ValType,
};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

use crate::extensions::bindings::{BindingParam, BindingParamRole, BindingValueType};
use crate::extensions::manager::ExtObject;
use crate::extensions::manifest::{ExtensionPermissions, ExtensionRuntime};
use crate::extensions::package::ExtensionPackage;
use crate::extensions::registry::{ExtensionModuleId, RegisteredFunction, RegisteredMethod};
use crate::extensions::value_codec::{
    decode_call_output, encode_value, ExtensionCallOutput, ValueCodecLimits,
};
use crate::path as bt_path;
use crate::value::Value;
use indexmap::IndexMap;

/// Name of the WASM entry-point memory export.
const WASM_EXPORT_MEMORY: &str = "memory";
/// Name of the WASM argument allocator export.
const WASM_EXPORT_ALLOC: &str = "bts_alloc";
/// Name of the WASM call dispatcher export.
const WASM_EXPORT_CALL: &str = "bts_call";
/// Name of the WASM memory deallocator export.
const WASM_EXPORT_FREE: &str = "bts_free";
/// Name of the optional WASM module ID initializer export.
const WASM_EXPORT_SET_MODULE_ID: &str = "bts_set_module_id";
/// Name of the optional WASM worker initializer export.
const WASM_EXPORT_INIT: &str = "bts_init";
/// Name of the optional WASM worker shutdown export.
const WASM_EXPORT_SHUTDOWN: &str = "bts_shutdown";
/// Name of the optional WASM worker statistics export.
const WASM_EXPORT_STATS: &str = "bts_stats";
/// Default advancement interval for shared-worker epoch checks.
const SHARED_EPOCH_DEADLINE_TICKS: u64 = 1;
/// Maximum number of WASM extension instances retained per thread.
const MAX_WASM_RUNTIME_CACHE_ENTRIES: usize = 32;

thread_local! {
    /// WASM extension runtime cache for the current thread.
    static WASM_RUNNER_RUNTIMES: RefCell<HashMap<WasmRunnerCacheKey, WasmRunnerRuntime>> =
        RefCell::new(HashMap::new());
}

/// Shareable metadata for a WASM extension module.
#[derive(Clone)]
pub struct WasmRunnerModule {
    /// Owning extension module ID.
    module_id: ExtensionModuleId,
    /// Extension package name.
    module_name: String,
    /// Display text for the extension package's local path.
    package_path: String,
    /// Project root normalized according to BT path rules.
    project_root: PathBuf,
    /// Canonical project root preopened for WASI when filesystem access is required.
    canonical_project_root: Option<PathBuf>,
    /// Normalized in-package path to the WASM entry point.
    entry_path: String,
    /// Permissions declared by the extension manifest.
    permissions: ExtensionPermissions,
    /// Runtime configuration validated from the manifest.
    runtime: ExtensionRuntime,
    /// Wasmtime compilation engine.
    engine: Engine,
    /// Whether shared-worker timeout interrupts are enabled for this module.
    timeout_interrupt: bool,
    /// Validated and compiled WASM module.
    module: Module,
    /// Entry-point function ID-to-name map declared by the bindings.
    functions: HashMap<u32, String>,
    /// Object type name index declared by the bindings.
    objects_by_name: HashMap<String, WasmRunnerObject>,
    /// Object type ID index declared by the bindings.
    objects_by_type_id: HashMap<u32, WasmRunnerObject>,
    /// Encoding limits for arguments to a single call.
    args_limits: ValueCodecLimits,
    /// Encoding limits for the result of a single call.
    result_limits: ValueCodecLimits,
    /// Runtime cache key for the current thread.
    cache_key: WasmRunnerCacheKey,
}

impl fmt::Debug for WasmRunnerModule {
    /// Emits debug information without Wasmtime's internal state.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmRunnerModule")
            .field("module_id", &self.module_id)
            .field("module_name", &self.module_name)
            .field("package_path", &self.package_path)
            .field("project_root", &self.project_root)
            .field("canonical_project_root", &self.canonical_project_root)
            .field("entry_path", &self.entry_path)
            .field("permissions", &self.permissions)
            .field("runtime", &self.runtime)
            .field("timeout_interrupt", &self.timeout_interrupt)
            .field("functions", &self.functions)
            .field("objects_by_name", &self.objects_by_name)
            .field("objects_by_type_id", &self.objects_by_type_id)
            .field("args_limits", &self.args_limits)
            .field("result_limits", &self.result_limits)
            .field("cache_key", &self.cache_key)
            .finish()
    }
}

/// WASM extension object type metadata.
#[derive(Debug, Clone)]
struct WasmRunnerObject {
    /// Extension object type ID.
    type_id: u32,
    /// Extension object type name.
    name: String,
}

/// WASM runtime cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WasmRunnerCacheKey {
    /// Owning extension module ID.
    module_id: ExtensionModuleId,
    /// Display text for the extension package's local path.
    package_path: String,
    /// WASI preopened project-root path, or an empty string without file permissions.
    project_root_key: String,
    /// Normalized in-package path to the WASM entry point.
    entry_path: String,
    /// Permission bits that affect WASI runtime capabilities.
    permissions_key: u8,
    /// Fingerprint of the WASM binary contents.
    wasm_hash: u64,
}

/// WASM extension runtime for the current thread.
pub(crate) struct WasmRunnerRuntime {
    /// Wasmtime store containing the WASI P1 context.
    store: Store<WasiP1Ctx>,
    /// Instantiated WASM module.
    _instance: Instance,
    /// Linear memory exported by the extension.
    memory: Memory,
    /// Argument-memory allocator exported by the extension.
    bts_alloc: TypedFunc<u32, u32>,
    /// Call dispatcher exported by the extension.
    bts_call: TypedFunc<(u32, u32, u32), u64>,
    /// Memory deallocator exported by the extension.
    bts_free: TypedFunc<(u32, u32), ()>,
    /// Optional worker initializer exported by the extension.
    bts_init: Option<TypedFunc<(u32, u32), u64>>,
    /// Optional worker shutdown function exported by the extension.
    bts_shutdown: Option<TypedFunc<(), u64>>,
    /// Optional worker statistics function exported by the extension.
    bts_stats: Option<TypedFunc<(), u64>>,
    /// Timeout-interrupt flag for the shared worker.
    timeout_abort: Option<Arc<AtomicBool>>,
}

/// WASM runner call error retaining whether a timeout interrupt triggered it.
pub(crate) struct WasmRunnerCallError {
    /// Error message shown to the script or caller.
    pub(crate) message: String,
    /// Whether a shared-worker timeout interrupt caused the error.
    pub(crate) timed_out: bool,
}

impl WasmRunnerCallError {
    /// Creates a regular WASM runner call error.
    fn new(message: String) -> Self {
        Self {
            message,
            timed_out: false,
        }
    }

    /// Creates a shared-worker timeout-interrupt error.
    fn timeout(message: String) -> Self {
        Self {
            message,
            timed_out: true,
        }
    }
}

impl WasmRunnerModule {
    /// Creates a shareable WASM runner module from an extension package.
    pub fn from_package(
        module_id: ExtensionModuleId,
        project_root: &Path,
        package: &ExtensionPackage,
    ) -> Result<Self, String> {
        if package.manifest.runtime.mode.is_shared() {
            return Err(format!(
                "Extension `{}` declares manifest.runtime.mode=shared and cannot run in the thread-local WASM runner",
                package.manifest.name
            ));
        }
        Self::from_package_inner(module_id, project_root, package, false)
    }

    /// Creates a service-specific WASM module from a shared-manifest extension package.
    pub(crate) fn from_shared_package(
        module_id: ExtensionModuleId,
        project_root: &Path,
        package: &ExtensionPackage,
    ) -> Result<Self, String> {
        if !package.manifest.runtime.mode.is_shared() {
            return Err(format!(
                "Extension `{}` does not declare manifest.runtime.mode=shared; cannot create a shared extension service",
                package.manifest.name
            ));
        }
        Self::from_package_inner(module_id, project_root, package, true)
    }

    /// Creates WASM module metadata independent of a specific runtime mode.
    fn from_package_inner(
        module_id: ExtensionModuleId,
        project_root: &Path,
        package: &ExtensionPackage,
        timeout_interrupt: bool,
    ) -> Result<Self, String> {
        validate_wasm_permissions(&package.manifest.name, package.manifest.permissions)?;
        let project_root = bt_path::normalize_path(project_root);
        let canonical_project_root = if package.manifest.permissions.uses_fs() {
            Some(canonicalize_project_root(
                &package.manifest.name,
                &project_root,
            )?)
        } else {
            None
        };
        let wasm = package.entry_wasm.as_ref().ok_or_else(|| {
            format!(
                "Extension `{}` is missing WASM entry point `{}`",
                package.manifest.name, package.manifest.entry
            )
        })?;
        reject_start_section(&package.manifest.name, wasm)?;

        let engine = create_wasm_engine(&package.manifest.name, timeout_interrupt)?;
        let module = Module::new(&engine, wasm).map_err(|err| {
            format!(
                "Extension `{}` failed to compile WASM entry point `{}`: {}",
                package.manifest.name, package.manifest.entry, err
            )
        })?;
        validate_module_exports(&package.manifest.name, &module)?;

        let mut functions = HashMap::with_capacity(package.bindings.functions.len());
        for function in &package.bindings.functions {
            functions.insert(function.id, function.name.clone());
        }

        let mut objects_by_name = HashMap::with_capacity(package.bindings.objects.len());
        let mut objects_by_type_id = HashMap::with_capacity(package.bindings.objects.len());
        for object in &package.bindings.objects {
            let runner_object = WasmRunnerObject {
                type_id: object.type_id,
                name: object.name.clone(),
            };
            objects_by_name.insert(object.name.clone(), runner_object.clone());
            objects_by_type_id.insert(object.type_id, runner_object);
        }

        let wasm_hash = wasm_fingerprint(wasm);
        let project_root_key = canonical_project_root
            .as_ref()
            .map(|path| bt_path::path_text(path))
            .unwrap_or_default();
        Ok(Self {
            module_id,
            module_name: package.manifest.name.clone(),
            package_path: package.path.display().to_string(),
            project_root,
            canonical_project_root,
            entry_path: package.manifest.entry.clone(),
            permissions: package.manifest.permissions,
            runtime: package.manifest.runtime,
            engine,
            timeout_interrupt,
            module,
            functions,
            objects_by_name,
            objects_by_type_id,
            args_limits: codec_limits_from_manifest(package.manifest.limits.max_args_bytes)?,
            result_limits: codec_limits_from_manifest(package.manifest.limits.max_result_bytes)?,
            cache_key: WasmRunnerCacheKey {
                module_id,
                package_path: package.path.display().to_string(),
                project_root_key,
                entry_path: package.manifest.entry.clone(),
                permissions_key: permissions_cache_key(package.manifest.permissions),
                wasm_hash,
            },
        })
    }

    /// Calls an extension entry-point function.
    pub fn call_function(
        &self,
        function: &RegisteredFunction,
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Value, String> {
        if !self.functions.contains_key(&function.function_id) {
            return Err(format!(
                "Extension `{}` has no WASM entry-point function with ID `{}`",
                self.module_name, function.function_id
            ));
        }
        let args = self.prepare_call_args(&function.name, &function.params, args, source_dir)?;
        self.call_export(
            function.function_id,
            &function.name,
            &function.returns,
            args,
        )
    }

    /// Calls an extension object method.
    pub fn call_method(
        &self,
        object: &ExtObject,
        method: &RegisteredMethod,
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Value, String> {
        self.validate_receiver(object, &method.name)?;
        let call_label = format!("{}.{}", object.type_name, method.name);
        let args = self.prepare_call_args(&call_label, &method.params, args, source_dir)?;
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(Value::ExtObject(object.clone()));
        call_args.extend(args);
        self.call_export(method.method_id, &call_label, &method.returns, call_args)
    }

    /// Retrieves or creates the runner runtime on the current thread, then invokes the action.
    fn with_runtime<T>(
        &self,
        action: impl FnOnce(&mut WasmRunnerRuntime) -> Result<T, String>,
    ) -> Result<T, String> {
        WASM_RUNNER_RUNTIMES.with(|runtimes| {
            let mut runtimes = runtimes.borrow_mut();
            if !runtimes.contains_key(&self.cache_key) {
                let runtime = WasmRunnerRuntime::new(self)?;
                evict_runtime_cache_if_needed(&mut runtimes);
                runtimes.insert(self.cache_key.clone(), runtime);
            }
            let runtime = runtimes
                .get_mut(&self.cache_key)
                .expect("the WASM runner runtime just inserted must exist");
            action(runtime)
        })
    }

    /// Calls the WASM ABI dispatch entry point.
    fn call_export(
        &self,
        call_id: u32,
        call_label: &str,
        returns: &str,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        let encoded_args = self.encode_call_args(call_label, args)?;
        let result = self.with_runtime(|runtime| {
            runtime.call_export_bytes(self, call_id, call_label, returns, &encoded_args)
        })?;
        self.decode_call_result(call_label, returns, &result)
    }

    /// Returns whether this WASM module declares the specified entry-point ID.
    pub(crate) fn contains_function_id(&self, function_id: u32) -> bool {
        self.functions.contains_key(&function_id)
    }

    /// Returns the extension package name.
    pub(crate) fn module_name(&self) -> &str {
        &self.module_name
    }

    /// Triggers a shared-worker epoch check so the target worker sees the timeout flag promptly.
    pub(crate) fn interrupt_epoch(&self) {
        if self.timeout_interrupt {
            self.engine.increment_epoch();
        }
    }

    /// Builds the host-normalized configuration object passed to optional `bts_init`.
    fn lifecycle_config_value(&self) -> Result<Value, String> {
        let module_id = i64::try_from(self.module_id).map_err(|_| {
            format!(
                "Extension `{}` module ID exceeds the i64 limit",
                self.module_name
            )
        })?;
        let runtime = object_value(vec![
            ("mode", Value::Str(self.runtime.mode.name().to_string())),
            ("workers", Value::Int(i64::from(self.runtime.workers))),
            (
                "queue_limit",
                Value::Int(i64::from(self.runtime.queue_limit)),
            ),
            (
                "call_timeout_ms",
                value_int_from_u64(self.runtime.call_timeout_ms, "call_timeout_ms")?,
            ),
            (
                "idle_ttl_ms",
                value_int_from_u64(self.runtime.idle_ttl_ms, "idle_ttl_ms")?,
            ),
            (
                "max_objects",
                Value::Int(i64::from(self.runtime.max_objects)),
            ),
            (
                "max_worker_objects",
                Value::Int(i64::from(self.runtime.max_worker_objects)),
            ),
            (
                "max_inflight_calls",
                Value::Int(i64::from(self.runtime.max_inflight_calls)),
            ),
        ]);
        Ok(object_value(vec![
            ("module_id", Value::Int(module_id)),
            ("module_name", Value::Str(self.module_name.clone())),
            ("runtime", runtime),
        ]))
    }

    /// Encodes call arguments as a BtValueBinary byte array.
    pub(crate) fn encode_call_args(
        &self,
        call_label: &str,
        args: Vec<Value>,
    ) -> Result<Vec<u8>, String> {
        let args_value = Value::Array(Rc::new(RefCell::new(args)));
        encode_value(&args_value, self.args_limits).map_err(|err| {
            format!(
                "Extension `{}` failed to encode arguments for call `{}`: {}",
                self.module_name, call_label, err
            )
        })
    }

    /// Decodes and validates the BtValueBinary result envelope returned by a WASM call.
    pub(crate) fn decode_call_result(
        &self,
        call_label: &str,
        returns: &str,
        bytes: &[u8],
    ) -> Result<Value, String> {
        decode_call_output(bytes, self.result_limits)
            .map_err(|err| {
                format!(
                    "Extension `{}` failed to decode the result of call `{}`: {}",
                    self.module_name, call_label, err
                )
            })
            .and_then(|output| match output {
                ExtensionCallOutput::Value(value) => {
                    self.convert_return(call_label, returns, value)
                }
                ExtensionCallOutput::Error(message) => Err(format!(
                    "Extension `{}` call `{}` returned an error: {}",
                    self.module_name, call_label, message
                )),
            })
    }

    /// Rewrites BT paths as virtual WASI paths according to binding parameter roles.
    pub(crate) fn prepare_call_args(
        &self,
        call_label: &str,
        params: &[BindingParam],
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Vec<Value>, String> {
        if !params.iter().any(|param| param.role.is_path()) {
            return Ok(args);
        }
        let mut prepared = Vec::with_capacity(args.len());
        for (index, (param, arg)) in params.iter().zip(args).enumerate() {
            if !param.role.is_path() {
                prepared.push(arg);
                continue;
            }
            let Value::Str(path) = arg else {
                return Err(format!(
                    "WASM extension `{}` call `{}` parameter `{}` must be a string for the {} role, but got {}",
                    self.module_name,
                    call_label,
                    param.name,
                    param.role.name(),
                    arg.type_name()
                ));
            };
            let wasi_path = self.resolve_wasi_path(
                call_label,
                &param.name,
                index,
                param.role,
                &path,
                source_dir,
            )?;
            prepared.push(Value::Str(wasi_path));
        }
        Ok(prepared)
    }

    /// Resolves one script path and converts it to a path relative to the WASI preopen.
    fn resolve_wasi_path(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        role: BindingParamRole,
        path: &str,
        source_dir: &Path,
    ) -> Result<String, String> {
        if path.is_empty() {
            return Err(format!(
                "WASM extension `{}` call `{}` parameter `{}` cannot be an empty path",
                self.module_name, call_label, param_name
            ));
        }
        if self.canonical_project_root.is_none() {
            return Err(format!(
                "WASM extension `{}` call `{}` parameter `{}` uses the {} role, but the extension declares no filesystem permission",
                self.module_name,
                call_label,
                param_name,
                role.name()
            ));
        }
        let resolved = bt_path::resolve_path(path, &self.project_root, source_dir);
        match role {
            BindingParamRole::Value => Ok(path.to_string()),
            BindingParamRole::PathRead => {
                self.resolve_existing_file_path(call_label, param_name, param_index, &resolved)
            }
            BindingParamRole::PathWrite => {
                self.resolve_write_file_path(call_label, param_name, param_index, &resolved)
            }
            BindingParamRole::PathDir => {
                self.resolve_existing_dir_path(call_label, param_name, param_index, &resolved)
            }
        }
    }

    /// Resolves the path of an existing file to read.
    fn resolve_existing_file_path(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        resolved: &Path,
    ) -> Result<String, String> {
        let canonical = self.canonicalize_existing_path(
            call_label,
            param_name,
            param_index,
            resolved,
            "read file",
        )?;
        let metadata = fs::metadata(&canonical).map_err(|err| {
            format!(
                "WASM extension `{}` call `{}` parameter `{}` failed to read metadata for file `{}`: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(resolved),
                err
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "WASM extension `{}` call `{}` parameter `{}` must point to a file for the path_read role: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(resolved)
            ));
        }
        self.wasi_path_from_canonical(call_label, param_name, param_index, &canonical)
    }

    /// Resolves a writable file path, validating the file if present or its parent otherwise.
    fn resolve_write_file_path(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        resolved: &Path,
    ) -> Result<String, String> {
        match fs::metadata(resolved) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    return Err(format!(
                        "WASM extension `{}` call `{}` parameter `{}` cannot point to a directory for the path_write role: {}",
                        self.module_name,
                        call_label,
                        param_name,
                        bt_path::path_text(resolved)
                    ));
                }
                let canonical = self.canonicalize_existing_path(
                    call_label,
                    param_name,
                    param_index,
                    resolved,
                    "write file",
                )?;
                self.wasi_path_from_canonical(call_label, param_name, param_index, &canonical)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.resolve_new_write_file_path(call_label, param_name, param_index, resolved)
            }
            Err(err) => Err(format!(
                "WASM extension `{}` call `{}` parameter `{}` failed to read metadata for output file `{}`: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(resolved),
                err
            )),
        }
    }

    /// Resolves the path of an existing directory.
    fn resolve_existing_dir_path(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        resolved: &Path,
    ) -> Result<String, String> {
        let canonical = self.canonicalize_existing_path(
            call_label,
            param_name,
            param_index,
            resolved,
            "directory",
        )?;
        let metadata = fs::metadata(&canonical).map_err(|err| {
            format!(
                "WASM extension `{}` call `{}` parameter `{}` failed to read metadata for directory `{}`: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(resolved),
                err
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "WASM extension `{}` call `{}` parameter `{}` must point to a directory for the path_dir role: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(resolved)
            ));
        }
        self.wasi_path_from_canonical(call_label, param_name, param_index, &canonical)
    }

    /// Canonicalizes an existing path while preserving useful error context.
    fn canonicalize_existing_path(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        resolved: &Path,
        label: &str,
    ) -> Result<PathBuf, String> {
        let canonical = fs::canonicalize(resolved).map_err(|err| {
            format!(
                "WASM extension `{}` call `{}` parameter {} (`{}`) failed to resolve the {} path `{}`: {}",
                self.module_name,
                call_label,
                param_index + 1,
                param_name,
                label,
                bt_path::path_text(resolved),
                err
            )
        })?;
        self.ensure_canonical_path_in_project(call_label, param_name, param_index, &canonical)?;
        Ok(canonical)
    }

    /// Resolves a nonexistent output path whose parent must exist within the project root.
    fn resolve_new_write_file_path(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        resolved: &Path,
    ) -> Result<String, String> {
        let parent = resolved.parent().ok_or_else(|| {
            format!(
                "WASM extension `{}` call `{}` parameter `{}` output path has no parent directory: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(resolved)
            )
        })?;
        let file_name = resolved
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                format!(
                    "WASM extension `{}` call `{}` parameter `{}` output filename is not UTF-8: {}",
                    self.module_name,
                    call_label,
                    param_name,
                    bt_path::path_text(resolved)
                )
            })?;
        validate_wasi_path_component(file_name).map_err(|message| {
            format!(
                "WASM extension `{}` call `{}` parameter `{}` has an invalid output filename: {}",
                self.module_name, call_label, param_name, message
            )
        })?;
        let canonical_parent = self.canonicalize_existing_path(
            call_label,
            param_name,
            param_index,
            parent,
            "output parent directory",
        )?;
        let metadata = fs::metadata(&canonical_parent).map_err(|err| {
            format!(
                "WASM extension `{}` call `{}` parameter `{}` failed to read metadata for output parent directory `{}`: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(parent),
                err
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "WASM extension `{}` call `{}` parameter `{}` output parent path is not a directory: {}",
                self.module_name,
                call_label,
                param_name,
                bt_path::path_text(parent)
            ));
        }
        let parent_wasi =
            self.wasi_path_from_canonical(call_label, param_name, param_index, &canonical_parent)?;
        if parent_wasi == "." {
            Ok(file_name.to_string())
        } else {
            Ok(format!("{}/{}", parent_wasi, file_name))
        }
    }

    /// Ensures the canonicalized path remains within the project root.
    fn ensure_canonical_path_in_project(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        canonical: &Path,
    ) -> Result<(), String> {
        let project_root = self
            .canonical_project_root
            .as_ref()
            .expect("path roles require the WASM runner to have a canonical_project_root");
        if canonical.strip_prefix(project_root).is_ok() {
            return Ok(());
        }
        Err(format!(
            "WASM extension `{}` call `{}` parameter {} (`{}`) path `{}` escapes project root `{}`",
            self.module_name,
            call_label,
            param_index + 1,
            param_name,
            bt_path::path_text(canonical),
            bt_path::path_text(project_root)
        ))
    }

    /// Converts a real path within the project root to a path relative to the WASI preopen.
    fn wasi_path_from_canonical(
        &self,
        call_label: &str,
        param_name: &str,
        param_index: usize,
        canonical: &Path,
    ) -> Result<String, String> {
        let project_root = self
            .canonical_project_root
            .as_ref()
            .expect("path roles require the WASM runner to have a canonical_project_root");
        let relative = canonical.strip_prefix(project_root).map_err(|_| {
            format!(
                "WASM extension `{}` call `{}` parameter {} (`{}`) path `{}` escapes project root `{}`",
                self.module_name,
                call_label,
                param_index + 1,
                param_name,
                bt_path::path_text(canonical),
                bt_path::path_text(project_root)
            )
        })?;
        if relative.as_os_str().is_empty() {
            return Ok(".".to_string());
        }
        let path = bt_path::path_text(relative);
        validate_wasi_relative_path(&path).map_err(|message| {
            format!(
                "WASM extension `{}` call `{}` failed to convert parameter {} (`{}`) to a WASI path: {}",
                self.module_name,
                call_label,
                param_index + 1,
                param_name,
                message
            )
        })?;
        Ok(path)
    }

    /// Validates that an extension object handle belongs to this WASM module and a declared type.
    fn validate_receiver(&self, object: &ExtObject, method_name: &str) -> Result<(), String> {
        if object.module_id != self.module_id {
            return Err(format!(
                "Extension `{}` method `{}` received an object handle from another module",
                self.module_name, method_name
            ));
        }
        let Some(object_type) = self.objects_by_type_id.get(&object.type_id) else {
            return Err(format!(
                "Extension `{}` method `{}` received undeclared object type ID `{}`",
                self.module_name, method_name, object.type_id
            ));
        };
        if object.type_name != object_type.name {
            return Err(format!(
                "Extension `{}` method `{}` received object type name `{}`, but type ID `{}` should be `{}`",
                self.module_name, method_name, object.type_name, object.type_id, object_type.name
            ));
        }
        Ok(())
    }

    /// Validates and returns a WASM result according to the binding return type.
    fn convert_return(
        &self,
        call_label: &str,
        returns: &str,
        value: Value,
    ) -> Result<Value, String> {
        if BindingValueType::is_primitive_return_name(returns) {
            validate_primitive_return(&self.module_name, call_label, returns, &value)?;
            return Ok(value);
        }

        let object_type = self.objects_by_name.get(returns).ok_or_else(|| {
            format!(
                "Extension `{}` return type `{}` has no corresponding object declaration",
                self.module_name, returns
            )
        })?;
        let Value::ExtObject(object) = value else {
            return Err(format!(
                "WASM extension `{}` call `{}` must return an extension object handle for `{}`, but got {}",
                self.module_name,
                call_label,
                returns,
                value.type_name()
            ));
        };
        if object.module_id != self.module_id {
            return Err(format!(
                "WASM extension `{}` call `{}` returned an object handle from another module",
                self.module_name, call_label
            ));
        }
        if object.type_id != object_type.type_id || object.type_name != object_type.name {
            return Err(format!(
                "WASM extension `{}` call `{}` returned the wrong object type: bindings require `{}`, but got `{}`",
                self.module_name, call_label, returns, object.type_name
            ));
        }
        Ok(Value::ExtObject(object))
    }
}

impl WasmRunnerRuntime {
    /// Creates and validates the WASM extension runtime for the current thread.
    pub(crate) fn new(module: &WasmRunnerModule) -> Result<Self, String> {
        Self::new_inner(module, None)
    }

    /// Creates a WASM extension runtime dedicated to a shared worker.
    pub(crate) fn new_shared_worker(
        module: &WasmRunnerModule,
        timeout_abort: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Self::new_inner(module, Some(timeout_abort))
    }

    /// Creates a WASM extension runtime and calls its optional initializer export.
    fn new_inner(
        module: &WasmRunnerModule,
        timeout_abort: Option<Arc<AtomicBool>>,
    ) -> Result<Self, String> {
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&module.engine);
        p1::add_to_linker_sync(&mut linker, |ctx| ctx).map_err(|err| {
            format!(
                "Extension `{}` failed to initialize the WASI P1 linker: {}",
                module.module_name, err
            )
        })?;
        let mut wasi = WasiCtxBuilder::new();
        configure_wasi_preopens(module, &mut wasi)?;
        let mut store = Store::new(&module.engine, wasi.build_p1());
        configure_epoch_timeout_callback(module, &mut store, timeout_abort.clone());
        let instance = linker
            .instantiate(&mut store, &module.module)
            .map_err(|err| {
                format!(
                    "Extension `{}` failed to instantiate WASM entry point `{}`: {}",
                    module.module_name, module.entry_path, err
                )
            })?;
        let memory = instance
            .get_memory(&mut store, WASM_EXPORT_MEMORY)
            .ok_or_else(|| {
                format!(
                    "Extension `{}` is missing memory export `{}`",
                    module.module_name, WASM_EXPORT_MEMORY
                )
            })?;
        let bts_alloc = instance
            .get_typed_func::<u32, u32>(&mut store, WASM_EXPORT_ALLOC)
            .map_err(|err| {
                format!(
                    "Extension `{}` export `{}` must have type fn(i32) -> i32: {}",
                    module.module_name, WASM_EXPORT_ALLOC, err
                )
            })?;
        let bts_call = instance
            .get_typed_func::<(u32, u32, u32), u64>(&mut store, WASM_EXPORT_CALL)
            .map_err(|err| {
                format!(
                    "Extension `{}` export `{}` must have type fn(i32, i32, i32) -> i64: {}",
                    module.module_name, WASM_EXPORT_CALL, err
                )
            })?;
        let bts_free = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, WASM_EXPORT_FREE)
            .map_err(|err| {
                format!(
                    "Extension `{}` export `{}` must have type fn(i32, i32): {}",
                    module.module_name, WASM_EXPORT_FREE, err
                )
            })?;
        call_optional_module_id_export(module, &instance, &mut store)?;
        let bts_init = get_optional_init_export(module, &instance, &mut store)?;
        let bts_shutdown = get_optional_shutdown_export(module, &instance, &mut store)?;
        let bts_stats = get_optional_stats_export(module, &instance, &mut store)?;

        let mut runtime = Self {
            store,
            _instance: instance,
            memory,
            bts_alloc,
            bts_call,
            bts_free,
            bts_init,
            bts_shutdown,
            bts_stats,
            timeout_abort,
        };
        runtime.call_optional_init(module)?;
        Ok(runtime)
    }

    /// Invokes the WASM extension dispatcher through `bts_call`.
    pub(crate) fn call_export_bytes(
        &mut self,
        module: &WasmRunnerModule,
        call_id: u32,
        call_label: &str,
        returns: &str,
        encoded_args: &[u8],
    ) -> Result<Vec<u8>, String> {
        self.call_export_bytes_controlled(module, call_id, call_label, returns, encoded_args)
            .map_err(|err| err.message)
    }

    /// Invokes the WASM extension dispatcher through `bts_call`, retaining shared timeout state.
    pub(crate) fn call_export_bytes_controlled(
        &mut self,
        module: &WasmRunnerModule,
        call_id: u32,
        call_label: &str,
        _returns: &str,
        encoded_args: &[u8],
    ) -> Result<Vec<u8>, WasmRunnerCallError> {
        self.arm_epoch_timeout();
        let args_len =
            usize_to_u32(encoded_args.len(), "argument ").map_err(WasmRunnerCallError::new)?;
        let args_ptr = self
            .alloc(module, args_len, "argument ")
            .map_err(WasmRunnerCallError::new)?;
        if let Err(err) = self.write_memory(module, args_ptr, encoded_args, "argument ") {
            let free_err = self.free(module, args_ptr, args_len, "argument ").err();
            return Err(WasmRunnerCallError::new(join_optional_error(err, free_err)));
        }

        let call_result = self
            .bts_call
            .call(&mut self.store, (call_id, args_ptr, args_len));
        let free_args_result = self.free(module, args_ptr, args_len, "argument ");
        let packed_result = match call_result {
            Ok(value) => value,
            Err(err) => {
                let timed_out = self.take_timeout_interrupt();
                let message = if timed_out {
                    format!(
                        "Extension `{}` call `{}` was interrupted by a shared-worker timeout",
                        module.module_name, call_label
                    )
                } else {
                    format!(
                        "Extension `{}` call `{}` encountered a WASM trap: {}",
                        module.module_name, call_label, err
                    )
                };
                let message = join_optional_error(message, free_args_result.err());
                return if timed_out {
                    Err(WasmRunnerCallError::timeout(message))
                } else {
                    Err(WasmRunnerCallError::new(message))
                };
            }
        };
        let free_args_error = free_args_result.err();

        let result = self.read_packed_output(module, packed_result, "result ");
        self.clear_timeout_interrupt();
        match (result, free_args_error) {
            (Ok(value), None) => Ok(value),
            (Ok(_), Some(free_err)) => Err(WasmRunnerCallError::new(free_err)),
            (Err(err), None) => Err(WasmRunnerCallError::new(err)),
            (Err(err), Some(free_err)) => Err(WasmRunnerCallError::new(join_optional_error(
                err,
                Some(free_err),
            ))),
        }
    }

    /// Calls the extension's optional `bts_shutdown` lifecycle export.
    pub(crate) fn shutdown(&mut self, module: &WasmRunnerModule) -> Result<(), String> {
        self.call_optional_no_arg_lifecycle(module, WASM_EXPORT_SHUTDOWN, self.bts_shutdown.clone())
            .map(|_| ())
    }

    /// Calls the extension's optional `bts_stats` lifecycle export.
    #[cfg(test)]
    pub(crate) fn call_optional_stats(
        &mut self,
        module: &WasmRunnerModule,
    ) -> Result<Option<Value>, String> {
        self.call_optional_no_arg_lifecycle(module, WASM_EXPORT_STATS, self.bts_stats.clone())
    }

    /// Calls the extension's optional `bts_init` lifecycle export.
    fn call_optional_init(&mut self, module: &WasmRunnerModule) -> Result<(), String> {
        let Some(func) = self.bts_init.clone() else {
            return Ok(());
        };
        let config = module.lifecycle_config_value()?;
        let encoded_config = encode_value(&config, module.args_limits).map_err(|err| {
            format!(
                "Extension `{}` failed to encode configuration for call `{}`: {}",
                module.module_name, WASM_EXPORT_INIT, err
            )
        })?;
        let config_len = usize_to_u32(encoded_config.len(), "initialization configuration ")?;
        let config_ptr = self.alloc(module, config_len, "initialization configuration ")?;
        if let Err(err) = self.write_memory(
            module,
            config_ptr,
            &encoded_config,
            "initialization configuration ",
        ) {
            let free_err = self
                .free(
                    module,
                    config_ptr,
                    config_len,
                    "initialization configuration ",
                )
                .err();
            return Err(join_optional_error(err, free_err));
        }

        let call_result = func.call(&mut self.store, (config_ptr, config_len));
        let free_config_result = self.free(
            module,
            config_ptr,
            config_len,
            "initialization configuration ",
        );
        let packed_result = call_result.map_err(|err| {
            join_optional_error(
                format!(
                    "Extension `{}` call `{}` failed to initialize the shared worker: {}",
                    module.module_name, WASM_EXPORT_INIT, err
                ),
                free_config_result.err(),
            )
        })?;
        let bytes = self.read_packed_output(module, packed_result, "initialization result ")?;
        let _ = decode_lifecycle_output(module, WASM_EXPORT_INIT, &bytes)?;
        Ok(())
    }

    /// Calls an optional zero-argument lifecycle export from the extension.
    fn call_optional_no_arg_lifecycle(
        &mut self,
        module: &WasmRunnerModule,
        export_name: &str,
        func: Option<TypedFunc<(), u64>>,
    ) -> Result<Option<Value>, String> {
        let Some(func) = func else {
            return Ok(None);
        };
        let packed_result = func.call(&mut self.store, ()).map_err(|err| {
            format!(
                "Extension `{}` call `{}` failed in a lifecycle export: {}",
                module.module_name, export_name, err
            )
        })?;
        let bytes = self.read_packed_output(module, packed_result, "lifecycle result ")?;
        decode_lifecycle_output(module, export_name, &bytes).map(Some)
    }

    /// Reads and frees a packed pointer/length result returned by WASM.
    fn read_packed_output(
        &mut self,
        module: &WasmRunnerModule,
        packed_result: u64,
        label: &str,
    ) -> Result<Vec<u8>, String> {
        let result_ptr = (packed_result >> 32) as u32;
        let result_len = packed_result as u32;
        let result = self.read_memory(
            module,
            result_ptr,
            result_len,
            module.result_limits.max_total_bytes,
            label,
        );
        let free_error = self.free(module, result_ptr, result_len, label).err();
        match (result, free_error) {
            (Ok(value), None) => Ok(value),
            (Ok(_), Some(free_err)) => Err(free_err),
            (Err(err), None) => Err(err),
            (Err(err), Some(free_err)) => Err(join_optional_error(err, Some(free_err))),
        }
    }

    /// Resets the epoch deadline for the next shared-worker call.
    fn arm_epoch_timeout(&mut self) {
        self.clear_timeout_interrupt();
        #[cfg(target_has_atomic = "64")]
        if self.timeout_abort.is_some() {
            self.store.set_epoch_deadline(SHARED_EPOCH_DEADLINE_TICKS);
        }
    }

    /// Reads and clears the shared-worker timeout-interrupt flag.
    fn take_timeout_interrupt(&self) -> bool {
        self.timeout_abort
            .as_ref()
            .map(|flag| flag.swap(false, Ordering::AcqRel))
            .unwrap_or(false)
    }

    /// Clears the shared-worker timeout-interrupt flag.
    fn clear_timeout_interrupt(&self) {
        if let Some(flag) = &self.timeout_abort {
            flag.store(false, Ordering::Release);
        }
    }

    /// Calls `bts_alloc` to allocate WASM linear memory.
    fn alloc(&mut self, module: &WasmRunnerModule, len: u32, label: &str) -> Result<u32, String> {
        if len == 0 {
            return Ok(0);
        }
        let ptr = self.bts_alloc.call(&mut self.store, len).map_err(|err| {
            format!(
                "Extension `{}` failed to allocate {}memory: {}",
                module.module_name, label, err
            )
        })?;
        if ptr == 0 {
            return Err(format!(
                "Extension `{}` failed to allocate {}memory: `{}` returned 0",
                module.module_name, label, WASM_EXPORT_ALLOC
            ));
        }
        self.check_memory_range(module, ptr, len, module.args_limits.max_total_bytes, label)?;
        Ok(ptr)
    }

    /// Calls `bts_free` to release WASM linear memory.
    fn free(
        &mut self,
        module: &WasmRunnerModule,
        ptr: u32,
        len: u32,
        label: &str,
    ) -> Result<(), String> {
        if len == 0 {
            return Ok(());
        }
        self.bts_free
            .call(&mut self.store, (ptr, len))
            .map_err(|err| {
                format!(
                    "Extension `{}` failed to free {}memory: {}",
                    module.module_name, label, err
                )
            })
    }

    /// Writes call arguments to WASM linear memory.
    fn write_memory(
        &mut self,
        module: &WasmRunnerModule,
        ptr: u32,
        bytes: &[u8],
        label: &str,
    ) -> Result<(), String> {
        let len = usize_to_u32(bytes.len(), label)?;
        let offset =
            self.check_memory_range(module, ptr, len, module.args_limits.max_total_bytes, label)?;
        self.memory
            .write(&mut self.store, offset, bytes)
            .map_err(|err| {
                format!(
                    "Extension `{}` failed to write {}memory: {}",
                    module.module_name, label, err
                )
            })
    }

    /// Reads a call result from WASM linear memory.
    fn read_memory(
        &self,
        module: &WasmRunnerModule,
        ptr: u32,
        len: u32,
        max_bytes: usize,
        label: &str,
    ) -> Result<Vec<u8>, String> {
        let offset = self.check_memory_range(module, ptr, len, max_bytes, label)?;
        let len = len as usize;
        let mut bytes = vec![0; len];
        self.memory
            .read(&self.store, offset, &mut bytes)
            .map_err(|err| {
                format!(
                    "Extension `{}` failed to read {}memory: {}",
                    module.module_name, label, err
                )
            })?;
        Ok(bytes)
    }

    /// Validates that a WASM linear-memory range is in bounds and within the encoding limit.
    fn check_memory_range(
        &self,
        module: &WasmRunnerModule,
        ptr: u32,
        len: u32,
        max_bytes: usize,
        label: &str,
    ) -> Result<usize, String> {
        let offset = ptr as usize;
        let len = len as usize;
        if len > max_bytes {
            return Err(format!(
                "Extension `{}` {}memory length {} exceeds the limit of {}",
                module.module_name, label, len, max_bytes
            ));
        }
        let end = offset.checked_add(len).ok_or_else(|| {
            format!(
                "Extension `{}` {}memory range overflows: ptr={}, len={}",
                module.module_name, label, ptr, len
            )
        })?;
        let memory_len = self.memory.data_size(&self.store);
        if end > memory_len {
            return Err(format!(
                "Extension `{}` {}memory is out of bounds: ptr={}, len={}, memory={}",
                module.module_name, label, ptr, len, memory_len
            ));
        }
        Ok(offset)
    }
}

/// Creates a WASM engine with Wasmtime's compilation cache, plus epoch interruption for shared workers.
fn create_wasm_engine(module_name: &str, timeout_interrupt: bool) -> Result<Engine, String> {
    let mut config = Config::new();
    if let Ok(cache) = Cache::new(CacheConfig::new()) {
        config.cache(Some(cache));
    }
    if timeout_interrupt {
        config.epoch_interruption(true);
    }
    Engine::new(&config).map_err(|err| {
        format!(
            "Extension `{}` failed to create the WASM engine: {}",
            module_name, err
        )
    })
}

/// Configures the epoch-deadline callback for a shared-worker store.
fn configure_epoch_timeout_callback(
    module: &WasmRunnerModule,
    store: &mut Store<WasiP1Ctx>,
    timeout_abort: Option<Arc<AtomicBool>>,
) {
    let Some(flag) = timeout_abort else {
        return;
    };
    #[cfg(target_has_atomic = "64")]
    {
        let module_name = module.module_name.clone();
        store.set_epoch_deadline(SHARED_EPOCH_DEADLINE_TICKS);
        store.epoch_deadline_callback(move |_store| {
            if flag.load(Ordering::Acquire) {
                return Err(wasmtime::format_err!(
                    "Extension `{}` shared worker received a timeout interrupt",
                    module_name
                ));
            }
            Ok(UpdateDeadline::Continue(SHARED_EPOCH_DEADLINE_TICKS))
        });
    }
}

/// Gets the optional `bts_init` export and validates its signature.
fn get_optional_init_export(
    module: &WasmRunnerModule,
    instance: &Instance,
    store: &mut Store<WasiP1Ctx>,
) -> Result<Option<TypedFunc<(u32, u32), u64>>, String> {
    let Some(func) = instance.get_func(&mut *store, WASM_EXPORT_INIT) else {
        return Ok(None);
    };
    func.typed::<(u32, u32), u64>(&mut *store)
        .map(Some)
        .map_err(|err| {
            format!(
                "Extension `{}` export `{}` must have type fn(i32, i32) -> i64: {}",
                module.module_name, WASM_EXPORT_INIT, err
            )
        })
}

/// Gets the optional `bts_shutdown` export and validates its signature.
fn get_optional_shutdown_export(
    module: &WasmRunnerModule,
    instance: &Instance,
    store: &mut Store<WasiP1Ctx>,
) -> Result<Option<TypedFunc<(), u64>>, String> {
    let Some(func) = instance.get_func(&mut *store, WASM_EXPORT_SHUTDOWN) else {
        return Ok(None);
    };
    func.typed::<(), u64>(&mut *store).map(Some).map_err(|err| {
        format!(
            "Extension `{}` export `{}` must have type fn() -> i64: {}",
            module.module_name, WASM_EXPORT_SHUTDOWN, err
        )
    })
}

/// Gets the optional `bts_stats` export and validates its signature.
fn get_optional_stats_export(
    module: &WasmRunnerModule,
    instance: &Instance,
    store: &mut Store<WasiP1Ctx>,
) -> Result<Option<TypedFunc<(), u64>>, String> {
    let Some(func) = instance.get_func(&mut *store, WASM_EXPORT_STATS) else {
        return Ok(None);
    };
    func.typed::<(), u64>(&mut *store).map(Some).map_err(|err| {
        format!(
            "Extension `{}` export `{}` must have type fn() -> i64: {}",
            module.module_name, WASM_EXPORT_STATS, err
        )
    })
}

/// Decodes the call-result envelope from a lifecycle export.
fn decode_lifecycle_output(
    module: &WasmRunnerModule,
    export_name: &str,
    bytes: &[u8],
) -> Result<Value, String> {
    decode_call_output(bytes, module.result_limits)
        .map_err(|err| {
            format!(
                "Extension `{}` lifecycle call `{}` failed to decode its result: {}",
                module.module_name, export_name, err
            )
        })
        .and_then(|output| match output {
            ExtensionCallOutput::Value(value) => Ok(value),
            ExtensionCallOutput::Error(message) => Err(format!(
                "Extension `{}` lifecycle call `{}` returned an error: {}",
                module.module_name, export_name, message
            )),
        })
}

/// Builds a plain object value for lifecycle configuration.
fn object_value(fields: Vec<(&str, Value)>) -> Value {
    let mut values = IndexMap::with_capacity(fields.len());
    for (key, value) in fields {
        values.insert(key.to_string(), value);
    }
    Value::Object(Rc::new(RefCell::new(values)))
}

/// Safely converts a u64 configuration value to a BT int.
fn value_int_from_u64(value: u64, label: &str) -> Result<Value, String> {
    i64::try_from(value).map(Value::Int).map_err(|_| {
        format!(
            "Extension runtime setting `{}` exceeds the i64 limit",
            label
        )
    })
}

/// Injects the host module ID after instantiation when WASM provides `bts_set_module_id`.
fn call_optional_module_id_export(
    module: &WasmRunnerModule,
    instance: &Instance,
    store: &mut Store<WasiP1Ctx>,
) -> Result<(), String> {
    let Some(func) = instance.get_func(&mut *store, WASM_EXPORT_SET_MODULE_ID) else {
        return Ok(());
    };
    let typed = func.typed::<u64, ()>(&mut *store).map_err(|err| {
        format!(
            "Extension `{}` export `{}` must have type fn(i64): {}",
            module.module_name, WASM_EXPORT_SET_MODULE_ID, err
        )
    })?;
    let module_id = u64::try_from(module.module_id).map_err(|_| {
        format!(
            "Extension `{}` module ID exceeds the u64 limit",
            module.module_name
        )
    })?;
    typed.call(&mut *store, module_id).map_err(|err| {
        format!(
            "Extension `{}` call `{}` failed to initialize the module ID: {}",
            module.module_name, WASM_EXPORT_SET_MODULE_ID, err
        )
    })
}

/// Validates the permissions supported for WASM extensions.
fn validate_wasm_permissions(
    module_name: &str,
    permissions: ExtensionPermissions,
) -> Result<(), String> {
    let mut unsupported = Vec::with_capacity(4);
    if permissions.net {
        unsupported.push("net");
    }
    if permissions.http {
        unsupported.push("http");
    }
    if permissions.process {
        unsupported.push("process");
    }
    if permissions.env {
        unsupported.push("env");
    }
    if !unsupported.is_empty() {
        return Err(format!(
            "Extension `{}` declares the `{}` permission, but the WASM runner does not currently expose that capability",
            module_name,
            unsupported.join(", ")
        ));
    }
    Ok(())
}

/// Validates and canonicalizes the project root for extensions requiring filesystem access.
fn canonicalize_project_root(module_name: &str, project_root: &Path) -> Result<PathBuf, String> {
    let canonical = fs::canonicalize(project_root).map_err(|err| {
        format!(
            "Extension `{}` failed to initialize the WASI filesystem: could not resolve project root `{}`: {}",
            module_name,
            bt_path::path_text(project_root),
            err
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|err| {
        format!(
            "Extension `{}` failed to initialize the WASI filesystem: could not read metadata for project root `{}`: {}",
            module_name,
            bt_path::path_text(&canonical),
            err
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "Extension `{}` failed to initialize the WASI filesystem: project root `{}` is not a directory",
            module_name,
            bt_path::path_text(&canonical)
        ));
    }
    Ok(canonical)
}

/// Preopens the project root in the WASI P1 context according to manifest permissions.
fn configure_wasi_preopens(
    module: &WasmRunnerModule,
    wasi: &mut WasiCtxBuilder,
) -> Result<(), String> {
    let Some(project_root) = &module.canonical_project_root else {
        return Ok(());
    };
    wasi.preopened_dir(
        project_root,
        ".",
        wasi_dir_perms(module.permissions),
        wasi_file_perms(module.permissions),
    )
    .map_err(|err| {
        format!(
            "Extension `{}` failed to preopen WASI project directory `{}`: {}",
            module.module_name,
            bt_path::path_text(project_root),
            err
        )
    })?;
    Ok(())
}

/// Converts extension manifest file permissions to WASI directory permissions.
fn wasi_dir_perms(permissions: ExtensionPermissions) -> DirPerms {
    let mut perms = DirPerms::empty();
    if permissions.uses_fs() {
        perms |= DirPerms::READ;
    }
    if permissions.fs_write {
        perms |= DirPerms::MUTATE;
    }
    perms
}

/// Converts extension manifest file permissions to WASI file permissions.
fn wasi_file_perms(permissions: ExtensionPermissions) -> FilePerms {
    let mut perms = FilePerms::empty();
    if permissions.fs_read {
        perms |= FilePerms::READ;
    }
    if permissions.fs_write {
        perms |= FilePerms::WRITE;
    }
    perms
}

/// Packs the permission set into a WASM runtime cache key.
fn permissions_cache_key(permissions: ExtensionPermissions) -> u8 {
    u8::from(permissions.fs_read)
        | (u8::from(permissions.fs_write) << 1)
        | (u8::from(permissions.net) << 2)
        | (u8::from(permissions.http) << 3)
        | (u8::from(permissions.process) << 4)
        | (u8::from(permissions.env) << 5)
}

/// Validates one component of a relative WASI path.
fn validate_wasi_path_component(component: &str) -> Result<(), String> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains(':')
        || component.contains('\0')
    {
        return Err(format!("Path component `{}` is unsafe", component));
    }
    Ok(())
}

/// Validates that a path passed to WASI is a safe relative path.
fn validate_wasi_relative_path(path: &str) -> Result<(), String> {
    if path == "." {
        return Ok(());
    }
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("`{}` is not a relative path", path));
    }
    for component in path.split('/') {
        validate_wasi_path_component(component)?;
    }
    Ok(())
}

/// Rejects WASM modules containing a start section.
fn reject_start_section(module_name: &str, wasm: &[u8]) -> Result<(), String> {
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(|err| {
            format!(
                "Extension `{}` failed to parse WASM entry-point sections: {}",
                module_name, err
            )
        })? {
            Payload::StartSection { .. } => {
                return Err(format!(
                    "Extension `{}` WASM entry point must not contain a start section",
                    module_name
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validates that WASM module exports satisfy the `bts-wasi-1` ABI.
fn validate_module_exports(module_name: &str, module: &Module) -> Result<(), String> {
    validate_memory_export(module_name, module)?;
    validate_func_export(
        module_name,
        module,
        WASM_EXPORT_ALLOC,
        &[ValType::I32],
        &[ValType::I32],
    )?;
    validate_func_export(
        module_name,
        module,
        WASM_EXPORT_CALL,
        &[ValType::I32, ValType::I32, ValType::I32],
        &[ValType::I64],
    )?;
    validate_func_export(
        module_name,
        module,
        WASM_EXPORT_FREE,
        &[ValType::I32, ValType::I32],
        &[],
    )?;
    validate_optional_func_export(
        module_name,
        module,
        WASM_EXPORT_SET_MODULE_ID,
        &[ValType::I64],
        &[],
    )?;
    validate_optional_func_export(
        module_name,
        module,
        WASM_EXPORT_INIT,
        &[ValType::I32, ValType::I32],
        &[ValType::I64],
    )?;
    validate_optional_func_export(
        module_name,
        module,
        WASM_EXPORT_SHUTDOWN,
        &[],
        &[ValType::I64],
    )?;
    validate_optional_func_export(module_name, module, WASM_EXPORT_STATS, &[], &[ValType::I64])
}

/// Validates the WASM linear-memory export.
fn validate_memory_export(module_name: &str, module: &Module) -> Result<(), String> {
    match module.get_export(WASM_EXPORT_MEMORY) {
        Some(ExternType::Memory(memory_type)) => {
            if memory_type.is_64() {
                return Err(format!(
                    "Extension `{}` export `{}` cannot use memory64",
                    module_name, WASM_EXPORT_MEMORY
                ));
            }
            if memory_type.is_shared() {
                return Err(format!(
                    "Extension `{}` export `{}` cannot use shared memory",
                    module_name, WASM_EXPORT_MEMORY
                ));
            }
            Ok(())
        }
        Some(_) => Err(format!(
            "Extension `{}` export `{}` must be a memory export",
            module_name, WASM_EXPORT_MEMORY
        )),
        None => Err(format!(
            "Extension `{}` is missing memory export `{}`",
            module_name, WASM_EXPORT_MEMORY
        )),
    }
}

/// Validates the signature of a WASM function export.
fn validate_func_export(
    module_name: &str,
    module: &Module,
    name: &str,
    expected_params: &[ValType],
    expected_results: &[ValType],
) -> Result<(), String> {
    match module.get_export(name) {
        Some(ExternType::Func(func_type)) => {
            let params_ok = val_types_match(func_type.params(), expected_params);
            let results_ok = val_types_match(func_type.results(), expected_results);
            if params_ok && results_ok {
                return Ok(());
            }
            Err(format!(
                "Extension `{}` export `{}` has the wrong signature; expected `{}`",
                module_name,
                name,
                signature_text(expected_params, expected_results)
            ))
        }
        Some(_) => Err(format!(
            "Extension `{}` export `{}` must be a function",
            module_name, name
        )),
        None => Err(format!(
            "Extension `{}` is missing function export `{}`",
            module_name, name
        )),
    }
}

/// Validates the signature of an optional WASM function export.
fn validate_optional_func_export(
    module_name: &str,
    module: &Module,
    name: &str,
    expected_params: &[ValType],
    expected_results: &[ValType],
) -> Result<(), String> {
    match module.get_export(name) {
        Some(ExternType::Func(func_type)) => {
            let params_ok = val_types_match(func_type.params(), expected_params);
            let results_ok = val_types_match(func_type.results(), expected_results);
            if params_ok && results_ok {
                return Ok(());
            }
            Err(format!(
                "Extension `{}` optional export `{}` has the wrong signature; expected `{}`",
                module_name,
                name,
                signature_text(expected_params, expected_results)
            ))
        }
        Some(_) => Err(format!(
            "Extension `{}` optional export `{}` must be a function",
            module_name, name
        )),
        None => Ok(()),
    }
}

/// Checks whether the actual WASM value types match the expected sequence.
fn val_types_match(actual: impl ExactSizeIterator<Item = ValType>, expected: &[ValType]) -> bool {
    actual.len() == expected.len()
        && actual
            .zip(expected.iter())
            .all(|(actual, expected)| actual.matches(expected))
}

/// Formats a WASM ABI function signature.
fn signature_text(params: &[ValType], results: &[ValType]) -> String {
    let params = params
        .iter()
        .map(ValType::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let results = results
        .iter()
        .map(ValType::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if results.is_empty() {
        format!("fn({})", params)
    } else {
        format!("fn({}) -> {}", params, results)
    }
}

/// Creates codec limits from manifest limits.
fn codec_limits_from_manifest(max_total_bytes: u64) -> Result<ValueCodecLimits, String> {
    let max_total_bytes = usize::try_from(max_total_bytes).map_err(|_| {
        "Extension codec byte limit exceeds this platform's usize limit".to_string()
    })?;
    Ok(ValueCodecLimits::with_total_bytes(max_total_bytes))
}

/// Validates that a primitive return value matches its declared type.
fn validate_primitive_return(
    module_name: &str,
    call_label: &str,
    returns: &str,
    value: &Value,
) -> Result<(), String> {
    let matched = match returns {
        "empty" => matches!(value, Value::Empty),
        "null" => matches!(value, Value::Null),
        "bool" => matches!(value, Value::Bool(_)),
        "int" => matches!(value, Value::Int(_)),
        "float" => matches!(value, Value::Float(_)),
        "string" => matches!(value, Value::Str(_)),
        "bytes" => matches!(value, Value::Bytes(_)),
        "array" => matches!(value, Value::Array(_)),
        "object" => matches!(value, Value::Object(_) | Value::Empty),
        _ => false,
    };
    if matched {
        Ok(())
    } else {
        Err(format!(
            "WASM extension `{}` call `{}` returned the wrong value type: bindings require `{}`, but got {}",
            module_name,
            call_label,
            returns,
            value.type_name()
        ))
    }
}

/// Converts a usize length to the u32 used by the WASM ABI.
fn usize_to_u32(value: usize, label: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| format!("{}length exceeds the u32 limit", label))
}

/// Combines a primary error with an optional cleanup error.
fn join_optional_error(message: String, optional_error: Option<String>) -> String {
    match optional_error {
        Some(error) => format!("{}; {}", message, error),
        None => message,
    }
}

/// Evicts the first encountered WASM runtime cache entry on the current thread.
fn evict_runtime_cache_if_needed(runtimes: &mut HashMap<WasmRunnerCacheKey, WasmRunnerRuntime>) {
    if runtimes.len() < MAX_WASM_RUNTIME_CACHE_ENTRIES {
        return;
    }
    if let Some(key) = runtimes.keys().next().cloned() {
        runtimes.remove(&key);
    }
}

/// Computes a fingerprint of the WASM binary contents.
fn wasm_fingerprint(wasm: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    wasm.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::extensions::bindings::ExtensionBindings;
    use crate::extensions::manifest::{ExtensionManifest, ExtensionRuntimeMode};
    use crate::extensions::package::{ExtensionPackage, PackageFileEntry};
    use crate::extensions::registry::RegisteredFunction;
    use crate::value::Value;

    /// Builds a WASM extension package for tests.
    fn make_module_id_package(wasm: Vec<u8>) -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            r#"{
                "format": "bts",
                "format_version": 1,
                "name": "module_id_pkg",
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
        let bindings = ExtensionBindings::parse(
            r#"{
                "api_version": 1,
                "functions": [
                    {
                        "name": "calc",
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
                            }
                        ]
                    }
                ]
            }"#,
            &manifest,
        )
        .unwrap();
        ExtensionPackage {
            path: PathBuf::from("module_id.bts"),
            manifest,
            bindings,
            entry_source: None,
            entry_wasm: Some(wasm),
            files: vec![PackageFileEntry {
                path: "module.wasm".to_string(),
                uncompressed_size: 0,
                compressed_size: 0,
            }],
        }
    }

    /// A WASM entry point containing a start section must be rejected.
    #[test]
    fn rejects_start_section() {
        let wasm = wat::parse_str(
            r#"
            (module
                (func $start)
                (start $start)
            )
            "#,
        )
        .unwrap();
        let err = reject_start_section("start_pkg", &wasm).unwrap_err();
        assert!(err.contains("start section"));
    }

    /// A shared runtime cannot fall back to the thread-local runner.
    #[test]
    fn rejects_shared_runtime_for_thread_local_runner() {
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "bts_alloc") (param $len i32) (result i32)
                    i32.const 1024
                )
                (func (export "bts_free") (param i32) (param i32))
                (func (export "bts_call") (param i32) (param i32) (param i32) (result i64)
                    i64.const 0
                )
            )
            "#,
        )
        .unwrap();
        let mut package = make_module_id_package(wasm);
        package.manifest.runtime.mode = ExtensionRuntimeMode::Shared;
        let err = WasmRunnerModule::from_package(7, Path::new("."), &package).unwrap_err();
        assert!(err.contains("thread-local"));
        assert!(err.contains("shared"));
    }

    /// The optional module ID initializer makes SDK-style objects return a nonzero module_id.
    #[test]
    fn optional_module_id_export_initializes_ext_object_module_id() {
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 1024))
                (global $module_id (mut i64) (i64.const 0))
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
                (func (export "bts_set_module_id") (param $id i64)
                    local.get $id
                    global.set $module_id
                )
                (data (i32.const 16) "\00\09\00\00\00\00\00\00\00\00\01\00\00\00\2a\00\00\00\00\00\00\00\04\00\00\00Calc")
                (func (export "bts_call") (param i32) (param i32) (param i32) (result i64)
                    i32.const 18
                    global.get $module_id
                    i64.store
                    i64.const 68719476766
                )
            )
            "#,
        )
        .unwrap();
        let package = make_module_id_package(wasm);
        let runner = WasmRunnerModule::from_package(7, Path::new("."), &package).unwrap();
        let value = runner
            .call_function(
                &RegisteredFunction {
                    module_id: 7,
                    name: "calc".to_string(),
                    function_id: 1,
                    params: Vec::new(),
                    returns: "Calc".to_string(),
                },
                Vec::new(),
                Path::new("."),
            )
            .unwrap();
        let Value::ExtObject(object) = value else {
            panic!("calc should return an extension object");
        };
        assert_eq!(object.module_id, 7);
        assert_eq!(object.type_id, 1);
        assert_eq!(object.object_id, 42);
        assert_eq!(object.type_name, "Calc");
    }

    /// Optional lifecycle exports are called when present and remain backward-compatible when absent.
    #[test]
    fn optional_lifecycle_exports_are_called_when_present() {
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 1024))
                (global $init_called (mut i64) (i64.const 0))
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
                (data (i32.const 16) "\00\00")
                (data (i32.const 32) "\00\08\01\00\00\00\04\00\00\00\69\6e\69\74\03\00\00\00\00\00\00\00\00")
                (func (export "bts_init") (param i32) (param i32) (result i64)
                    i64.const 1
                    global.set $init_called
                    i64.const 68719476738
                )
                (func (export "bts_shutdown") (result i64)
                    i64.const 68719476738
                )
                (func (export "bts_stats") (result i64)
                    i32.const 47
                    global.get $init_called
                    i64.store
                    i64.const 137438953495
                )
                (func (export "bts_call") (param i32) (param i32) (param i32) (result i64)
                    i64.const 68719476738
                )
            )
            "#,
        )
        .unwrap();
        let package = make_module_id_package(wasm);
        let module = WasmRunnerModule::from_package(7, Path::new("."), &package).unwrap();
        let mut runtime = WasmRunnerRuntime::new(&module).unwrap();
        let stats = runtime.call_optional_stats(&module).unwrap().unwrap();
        let Value::Object(values) = stats else {
            panic!("bts_stats should return an object");
        };
        assert_eq!(values.borrow().get("init"), Some(&Value::Int(1)));
        runtime.shutdown(&module).unwrap();
    }

    /// The primitive object return type accepts empty to represent no object result.
    #[test]
    fn object_return_accepts_empty_result() {
        validate_primitive_return("sqlite", "one", "object", &Value::Empty).unwrap();
        assert!(validate_primitive_return("sqlite", "one", "int", &Value::Empty).is_err());
    }
}
