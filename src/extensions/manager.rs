//! Extension manager: scans project extensions, injects entries, and dispatches functions and object methods.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::extensions::bt_runner::BtRunnerModule;
use crate::extensions::manifest::ExtensionKind;
use crate::extensions::package::ExtensionPackage;
use crate::extensions::registry::{
    ExtensionModuleId, ExtensionObjectKey, ExtensionRegistry, RegisteredFunction, RegisteredMethod,
};
use crate::extensions::service::ExtensionService;
use crate::extensions::wasm_runner::WasmRunnerModule;
use crate::extensions::{PACKAGE_EXTENSION, PROJECT_EXTENSIONS_DIR};
use crate::value::Value;

/// Lightweight reference to an extension entry function stored in a VM value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFunctionRef {
    /// Owning extension module ID.
    pub module_id: ExtensionModuleId,
    /// Extension backend call ID.
    pub function_id: u32,
    /// User-visible entry name.
    pub name: String,
}

/// Minimal extension object handle stored in the VM.
///
/// The object's actual state remains in its backend runner. The VM value stores only the module,
/// type, and backend object handle, keeping non-shareable BT `Value` or WASM instance state out of
/// the main VM.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtObject {
    /// Owning extension module ID.
    pub module_id: ExtensionModuleId,
    /// Extension object type ID.
    pub type_id: u32,
    /// Extension object type name.
    pub type_name: String,
    /// Backend object handle ID.
    pub object_id: u64,
}

/// Extension manager.
#[derive(Debug, Clone)]
pub struct ExtensionManager {
    /// Extension registry metadata.
    registry: ExtensionRegistry,
    /// Pure BT extension runners.
    bt_runners: HashMap<ExtensionModuleId, BtRunnerModule>,
    /// WASM/WASI extension runners.
    wasm_runners: HashMap<ExtensionModuleId, WasmRunnerModule>,
    /// Project-level shared extension services.
    shared_services: HashMap<ExtensionModuleId, Arc<ExtensionService>>,
}

impl ExtensionManager {
    /// Scans project-level `extensions/*.bts` and `extensions/<name>/*.bts` and builds the manager.
    pub fn load_project<I, S>(
        project_root: &Path,
        reserved_names: I,
    ) -> Result<Option<Self>, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let extension_dir = project_root.join(PROJECT_EXTENSIONS_DIR);
        if !extension_dir.exists() {
            return Ok(None);
        }
        if !extension_dir.is_dir() {
            return Err(format!(
                "extension path `{}` is not a directory",
                extension_dir.display()
            ));
        }

        let package_paths = scan_project_packages(&extension_dir)?;
        if package_paths.is_empty() {
            return Ok(None);
        }

        let mut packages = Vec::with_capacity(package_paths.len());
        let mut bt_runners = HashMap::new();
        let mut wasm_runners = HashMap::new();
        let mut shared_services = HashMap::new();
        for path in package_paths {
            let package = ExtensionPackage::read(&path).map_err(|message| {
                format!(
                    "Failed to load extension package `{}`: {}",
                    path.display(),
                    message
                )
            })?;
            let module_id = packages.len();
            match package.manifest.kind {
                ExtensionKind::Bt => {
                    let runner = BtRunnerModule::from_package(module_id, project_root, &package)
                        .map_err(|message| {
                            format!(
                                "Failed to load the pure BT runner for extension package `{}`: {}",
                                path.display(),
                                message
                            )
                        })?;
                    bt_runners.insert(module_id, runner);
                }
                ExtensionKind::Wasm => {
                    if package.manifest.runtime.mode.is_shared() {
                        let service =
                            ExtensionService::from_package(module_id, project_root, &package)
                                .map_err(|message| {
                                    format!(
                                "Failed to load the shared extension service for extension package `{}`: {}",
                                path.display(),
                                message
                            )
                                })?;
                        shared_services.insert(module_id, Arc::new(service));
                    } else {
                        let runner =
                            WasmRunnerModule::from_package(module_id, project_root, &package)
                                .map_err(|message| {
                                    format!(
                                "Failed to load the WASM runner for extension package `{}`: {}",
                                path.display(),
                                message
                            )
                                })?;
                        wasm_runners.insert(module_id, runner);
                    }
                }
            }
            packages.push(package);
        }
        let registry = ExtensionRegistry::from_packages(packages, reserved_names)?;
        Ok(Some(Self {
            registry,
            bt_runners,
            wasm_runners,
            shared_services,
        }))
    }

    /// Returns the number of extension entry functions.
    pub fn function_count(&self) -> usize {
        self.registry.function_count()
    }

    /// Shuts down all project-level shared extension services.
    pub fn shutdown(&self) {
        for service in self.shared_services.values() {
            service.shutdown();
        }
    }

    /// Returns extension entry functions that can be injected into the VM globals.
    pub fn function_values(&self) -> Vec<(String, ExtensionFunctionRef)> {
        self.registry
            .functions()
            .map(|function| {
                (
                    function.name.clone(),
                    ExtensionFunctionRef {
                        module_id: function.module_id,
                        function_id: function.function_id,
                        name: function.name.clone(),
                    },
                )
            })
            .collect()
    }

    /// Calls an extension entry function.
    pub fn call_function(
        &self,
        function_ref: &ExtensionFunctionRef,
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Value, String> {
        let function = self.registered_function(function_ref)?;
        self.check_arg_count(&function.name, function.params.len(), args.len())?;
        match self.module_kind(function.module_id)? {
            ExtensionKind::Bt => self
                .bt_runners
                .get(&function.module_id)
                .ok_or_else(|| {
                    format!("extension entry `{}` has no pure BT runner", function.name)
                })?
                .call_function(&function, args),
            ExtensionKind::Wasm => {
                if let Some(service) = self.shared_services.get(&function.module_id) {
                    service.call_function(&function, args, source_dir)
                } else {
                    self.wasm_runners
                        .get(&function.module_id)
                        .ok_or_else(|| {
                            format!("extension entry `{}` has no WASM runner", function.name)
                        })?
                        .call_function(&function, args, source_dir)
                }
            }
        }
    }

    /// Returns whether an extension object has the specified method.
    pub fn has_method(&self, object: &ExtObject, name: &str) -> bool {
        self.find_method(object, name).is_some()
    }

    /// Returns whether the VM must track disposal state for an extension object method.
    pub fn method_needs_vm_dispose_tracking(&self, object: &ExtObject, name: &str) -> bool {
        let Some(method) = self.find_method(object, name) else {
            return false;
        };
        if !method.lifecycle.is_dispose() {
            return false;
        }
        matches!(self.module_kind(object.module_id), Ok(ExtensionKind::Wasm))
    }

    /// Calls an extension object method.
    pub fn call_method(
        &self,
        object: &ExtObject,
        name: &str,
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Value, String> {
        let method = self.find_method(object, name).ok_or_else(|| {
            format!(
                "extension object `{}` has no method `{}`",
                object.type_name, name
            )
        })?;
        self.check_arg_count(name, method.params.len(), args.len())?;
        match self.module_kind(object.module_id)? {
            ExtensionKind::Bt => self
                .bt_runners
                .get(&object.module_id)
                .ok_or_else(|| {
                    format!(
                        "extension object `{}` has no pure BT runner",
                        object.type_name
                    )
                })?
                .call_method(object, &method, args),
            ExtensionKind::Wasm => {
                if let Some(service) = self.shared_services.get(&object.module_id) {
                    service.call_method(object, &method, args, source_dir)
                } else {
                    self.wasm_runners
                        .get(&object.module_id)
                        .ok_or_else(|| {
                            format!("extension object `{}` has no WASM runner", object.type_name)
                        })?
                        .call_method(object, &method, args, source_dir)
                }
            }
        }
    }

    /// Looks up and validates an entry function reference.
    fn registered_function(
        &self,
        function_ref: &ExtensionFunctionRef,
    ) -> Result<RegisteredFunction, String> {
        let function = self
            .registry
            .function(&function_ref.name)
            .ok_or_else(|| format!("extension entry `{}` is not registered", function_ref.name))?;
        if function.module_id != function_ref.module_id
            || function.function_id != function_ref.function_id
        {
            return Err(format!(
                "extension entry `{}` registration does not match",
                function_ref.name
            ));
        }
        Ok(function.clone())
    }

    /// Looks up an extension object method.
    fn find_method(&self, object: &ExtObject, name: &str) -> Option<RegisteredMethod> {
        self.registry
            .object(ExtensionObjectKey {
                module_id: object.module_id,
                type_id: object.type_id,
            })?
            .methods
            .iter()
            .find(|method| method.name == name)
            .cloned()
    }

    /// Returns the module backend type.
    fn module_kind(&self, module_id: ExtensionModuleId) -> Result<ExtensionKind, String> {
        self.registry
            .module(module_id)
            .map(|module| module.kind)
            .ok_or_else(|| format!("extension module {} is not registered", module_id))
    }

    /// Validates the call argument count.
    fn check_arg_count(&self, name: &str, expected: usize, actual: usize) -> Result<(), String> {
        if expected != actual {
            return Err(format!(
                "extension call `{}` requires {} arguments, received {}",
                name, expected, actual
            ));
        }
        Ok(())
    }
}

/// Scans regular `.bts` files in the project extension directory.
fn scan_project_packages(extension_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(extension_dir).map_err(|err| {
        format!(
            "Failed to read extension directory `{}`: {}",
            extension_dir.display(),
            err
        )
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "Failed to read an entry in extension directory `{}`: {}",
                extension_dir.display(),
                err
            )
        })?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|err| {
            format!(
                "Failed to read metadata for extension package `{}`: {}",
                path.display(),
                err
            )
        })?;
        if metadata.is_dir() {
            scan_project_package_child_dir(&path, &mut paths)?;
            continue;
        }
        if metadata.is_file() && is_package_file(&path) {
            paths.push(path);
        }
    }
    paths.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    Ok(paths)
}

/// Scans regular `.bts` files in one extension subdirectory.
fn scan_project_package_child_dir(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|err| {
        format!(
            "Failed to read extension subdirectory `{}`: {}",
            dir.display(),
            err
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "Failed to read an entry in extension subdirectory `{}`: {}",
                dir.display(),
                err
            )
        })?;
        let path = entry.path();
        let metadata = entry.metadata().map_err(|err| {
            format!(
                "Failed to read metadata for extension package `{}`: {}",
                path.display(),
                err
            )
        })?;
        if metadata.is_file() && is_package_file(&path) {
            paths.push(path);
        }
    }
    Ok(())
}

/// Returns whether a path is a `.bts` extension package file.
fn is_package_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(PACKAGE_EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::bindings::ExtensionBindings;
    use crate::extensions::manifest::ExtensionManifest;
    use crate::extensions::package::PackageFileEntry;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Creates a unique temporary directory.
    fn temp_root(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        std::env::temp_dir().join(format!("bt_ext_manager_{}_{}", name, millis))
    }

    /// Builds an extension package for tests.
    fn make_package() -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            r#"{
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
        let bindings = ExtensionBindings::parse(
            r#"{
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
            &manifest,
        )
        .unwrap();
        ExtensionPackage {
            path: PathBuf::from("calc.bts"),
            manifest,
            bindings,
            entry_source: Some(
                r#"
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
                "#
                .to_string(),
            ),
            entry_wasm: None,
            files: vec![PackageFileEntry {
                path: "src/lib.bt".to_string(),
                uncompressed_size: 0,
                compressed_size: 0,
            }],
        }
    }

    /// The pure BT runner preserves state through a chained `calc` call.
    #[test]
    fn bt_runner_handles_calc_chain() {
        let package = make_package();
        let runner = BtRunnerModule::from_package(0, Path::new("."), &package).unwrap();
        let registry = ExtensionRegistry::from_packages(vec![package], ["print"]).unwrap();
        let manager = ExtensionManager {
            registry,
            bt_runners: HashMap::from([(0, runner)]),
            wasm_runners: HashMap::new(),
            shared_services: HashMap::new(),
        };
        let (_, function) = manager.function_values().pop().unwrap();
        let object = manager
            .call_function(&function, vec![Value::Int(1)], Path::new("."))
            .unwrap();
        let Value::ExtObject(object) = object else {
            panic!("calc should return an extension object");
        };
        let object = manager
            .call_method(&object, "add", vec![Value::Int(2)], Path::new("."))
            .unwrap();
        let Value::ExtObject(object) = object else {
            panic!("add should return an extension object");
        };
        assert_eq!(
            manager
                .call_method(&object, "value", Vec::new(), Path::new("."))
                .unwrap(),
            Value::Int(3)
        );
        assert_eq!(
            manager
                .call_method(&object, "close", Vec::new(), Path::new("."))
                .unwrap(),
            Value::Bool(true)
        );
        let err = manager
            .call_method(&object, "value", Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("no longer valid"));
    }

    /// Builds a WASM extension package for tests.
    fn make_wasm_package() -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            r#"{
                "format": "bts",
                "format_version": 1,
                "name": "calc_wasm_pkg",
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
                            }
                        ]
                    }
                ]
            }"#,
            &manifest,
        )
        .unwrap();
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
                (data (i32.const 64) "\00\03\03\00\00\00\00\00\00\00")
                (func (export "bts_call") (param $id i32) (param $args_ptr i32) (param $args_len i32) (result i64)
                    local.get $id
                    i32.const 3
                    i32.eq
                    if (result i64)
                        i64.const 274877906954
                    else
                        i64.const 68719476766
                    end
                )
            )
            "#,
        )
        .unwrap();
        ExtensionPackage {
            path: PathBuf::from("calc_wasm.bts"),
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

    /// Builds a shared WASM primitive-value extension package for tests.
    fn make_shared_wasm_primitive_package() -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            r#"{
                "format": "bts",
                "format_version": 1,
                "name": "shared_primitive_pkg",
                "version": "1.0.0",
                "kind": "wasm",
                "abi": "bts-wasi-1",
                "bt_min_version": "1.1.0",
                "api_version": 1,
                "entry": "module.wasm",
                "bindings": "bindings.json",
                "permissions": [],
                "runtime": {
                    "mode": "shared",
                    "workers": 1,
                    "queue_limit": 4,
                    "call_timeout_ms": 1000,
                    "idle_ttl_ms": 300000,
                    "max_objects": 16,
                    "max_worker_objects": 16,
                    "max_inflight_calls": 4
                }
            }"#,
        )
        .unwrap();
        let bindings = ExtensionBindings::parse(
            r#"{
                "api_version": 1,
                "functions": [
                    {
                        "name": "answer",
                        "id": 1,
                        "params": [],
                        "returns": "int"
                    }
                ],
                "objects": []
            }"#,
            &manifest,
        )
        .unwrap();
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "bts_alloc") (param $len i32) (result i32)
                    i32.const 1024
                )
                (func (export "bts_free") (param i32) (param i32))
                (data (i32.const 16) "\00\03\2a\00\00\00\00\00\00\00")
                (func (export "bts_call") (param i32) (param i32) (param i32) (result i64)
                    i64.const 68719476746
                )
            )
            "#,
        )
        .unwrap();
        ExtensionPackage {
            path: PathBuf::from("shared_primitive.bts"),
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

    /// The WASM runner dispatches a chained `calc` call through the ABI.
    #[test]
    fn wasm_runner_handles_calc_chain() {
        let package = make_wasm_package();
        let runner = WasmRunnerModule::from_package(0, Path::new("."), &package).unwrap();
        let registry = ExtensionRegistry::from_packages(vec![package], ["print"]).unwrap();
        let manager = ExtensionManager {
            registry,
            bt_runners: HashMap::new(),
            wasm_runners: HashMap::from([(0, runner)]),
            shared_services: HashMap::new(),
        };
        let (_, function) = manager.function_values().pop().unwrap();
        let object = manager
            .call_function(&function, vec![Value::Int(1)], Path::new("."))
            .unwrap();
        let Value::ExtObject(object) = object else {
            panic!("calc should return an extension object");
        };
        let object = manager
            .call_method(&object, "add", vec![Value::Int(2)], Path::new("."))
            .unwrap();
        let Value::ExtObject(object) = object else {
            panic!("add should return an extension object");
        };
        assert_eq!(
            manager
                .call_method(&object, "value", Vec::new(), Path::new("."))
                .unwrap(),
            Value::Int(3)
        );
    }

    /// An ExtensionService worker executes a shared WASM extension entry.
    #[test]
    fn shared_wasm_function_uses_extension_service() {
        let package = make_shared_wasm_primitive_package();
        let service = ExtensionService::from_package(0, Path::new("."), &package).unwrap();
        let registry = ExtensionRegistry::from_packages(vec![package], ["print"]).unwrap();
        let manager = ExtensionManager {
            registry,
            bt_runners: HashMap::new(),
            wasm_runners: HashMap::new(),
            shared_services: HashMap::from([(0, Arc::new(service))]),
        };
        let (_, function) = manager.function_values().pop().unwrap();
        let value = manager
            .call_function(&function, Vec::new(), Path::new("."))
            .unwrap();
        assert_eq!(value, Value::Int(42));
        manager.shutdown();
        let err = manager
            .call_function(&function, Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("shut down"));
    }

    /// Project extension scanning supports legacy top-level packages and versioned subdirectory packages.
    #[test]
    fn scan_project_packages_includes_child_directories() {
        let root = temp_root("scan_child");
        let extension_dir = root.join("extensions");
        let child_dir = extension_dir.join("calc_pkg");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(extension_dir.join("legacy.bts"), [1_u8]).unwrap();
        fs::write(child_dir.join("calc_pkg-1.0.0.bts"), [2_u8]).unwrap();
        fs::write(child_dir.join("readme.md"), "ignored").unwrap();

        let paths = scan_project_packages(&extension_dir).unwrap();
        let names = paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["calc_pkg-1.0.0.bts".to_string(), "legacy.bts".to_string()]
        );

        let _ = fs::remove_dir_all(root);
    }
}
