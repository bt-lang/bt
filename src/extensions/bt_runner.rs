//! Pure BT extension runner.
//!
//! `kind=bt` extensions reuse the existing parser, compiler, and VM. The shared
//! `ExtensionManager` stores only source text and export metadata; the `Chunk` and VM,
//! which contain `Rc` values, live in a thread-local cache to preserve site-wide shared state.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::bytecode::Chunk;
use crate::compiler::Compiler;
use crate::extensions::bindings::BindingValueType;
use crate::extensions::package::ExtensionPackage;
use crate::extensions::registry::{ExtensionModuleId, RegisteredFunction, RegisteredMethod};
use crate::lexer::tokenize;
use crate::parser::{Expr, Parser, PosExpr, Statement};
use crate::path as bt_path;
use crate::value::Value;
use crate::vm::Vm;

/// Maximum number of pure BT extension object handles per thread.
const MAX_BT_OBJECT_HANDLES: usize = 4096;
/// Maximum number of pure BT runner runtime cache entries per thread.
const MAX_BT_RUNTIME_CACHE_ENTRIES: usize = 64;

thread_local! {
    /// Thread-local pure BT runner runtimes.
    ///
    /// The VM, chunk, BT objects, and class instances all contain `Rc` and cannot be shared
    /// across threads. Per-thread caching avoids reparsing and executing entry declarations on
    /// every call while allowing `ExtensionManager` to remain site-wide shared metadata.
    static BT_RUNNER_RUNTIMES: RefCell<HashMap<BtRunnerCacheKey, BtRunnerRuntime>> = RefCell::new(HashMap::new());
}

/// Shared module metadata for a pure BT runner.
#[derive(Debug, Clone)]
pub struct BtRunnerModule {
    /// Extension module ID.
    module_id: ExtensionModuleId,
    /// Extension name used in error messages.
    module_name: String,
    /// Current project root.
    project_root: PathBuf,
    /// Virtual source filename.
    source_file: String,
    /// Pure BT entry source.
    source: String,
    /// Mapping from binding function IDs to source function names.
    functions: HashMap<u32, String>,
    /// Mapping from binding object names to object type metadata.
    objects: HashMap<String, BtRunnerObject>,
    /// Thread-local cache key.
    cache_key: BtRunnerCacheKey,
}

/// Type metadata for a pure BT extension object.
#[derive(Debug, Clone)]
struct BtRunnerObject {
    /// Object type ID.
    type_id: u32,
    /// Object type name.
    name: String,
}

/// Thread-local runner cache key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BtRunnerCacheKey {
    /// Extension module ID.
    module_id: ExtensionModuleId,
    /// Extension package's local path as text.
    package_path: String,
    /// Entry path within the package.
    entry_path: String,
    /// Entry source content fingerprint.
    source_hash: u64,
}

/// Thread-local pure BT runner runtime.
struct BtRunnerRuntime {
    /// Initialized extension VM.
    vm: Vm,
    /// Compiled entry chunk, reused with the runtime cache.
    _chunk: Rc<Chunk>,
    /// Extension object handle table for the current thread.
    objects: HashMap<u64, Value>,
    /// Next object handle ID.
    next_object_id: u64,
}

impl BtRunnerModule {
    /// Creates a pure BT runner module from an extension package.
    pub fn from_package(
        module_id: ExtensionModuleId,
        project_root: &Path,
        package: &ExtensionPackage,
    ) -> Result<Self, String> {
        let source = package.entry_source.as_ref().ok_or_else(|| {
            format!(
                "extension `{}` uses the pure BT backend but is missing entry source `{}`",
                package.manifest.name, package.manifest.entry
            )
        })?;
        let package_path = bt_path::path_text(&bt_path::normalize_path(&package.path));
        let source_file = format!("{}!{}", package_path, package.manifest.entry);
        let source_hash = source_fingerprint(source);
        let statements = parse_entry_source(&source_file, source)?;
        validate_restricted_entry(&package.manifest.name, &statements, package)?;
        let _ = compile_entry_source(&source_file, project_root, &statements)?;

        let functions = package
            .bindings
            .functions
            .iter()
            .map(|function| (function.id, function.name.clone()))
            .collect();
        let objects = package
            .bindings
            .objects
            .iter()
            .map(|object| {
                (
                    object.name.clone(),
                    BtRunnerObject {
                        type_id: object.type_id,
                        name: object.name.clone(),
                    },
                )
            })
            .collect();
        let cache_key = BtRunnerCacheKey {
            module_id,
            package_path: package_path.clone(),
            entry_path: package.manifest.entry.clone(),
            source_hash,
        };

        Ok(Self {
            module_id,
            module_name: package.manifest.name.clone(),
            project_root: project_root.to_path_buf(),
            source_file,
            source: source.clone(),
            functions,
            objects,
            cache_key,
        })
    }

    /// Calls a pure BT extension entry function.
    pub fn call_function(
        &self,
        function: &RegisteredFunction,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        let name = self
            .functions
            .get(&function.function_id)
            .ok_or_else(|| {
                format!(
                    "extension entry `{}` has no pure BT export mapping",
                    function.name
                )
            })?
            .clone();
        self.with_runtime(|runtime| {
            let value = runtime
                .vm
                .call_global(&name, args)
                .map_err(|err| err.to_string())?;
            runtime.vm.clear_output();
            runtime.convert_return(self, None, &function.returns, value)
        })
    }

    /// Calls a pure BT extension object method.
    pub fn call_method(
        &self,
        object: &crate::extensions::manager::ExtObject,
        method: &RegisteredMethod,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        self.with_runtime(|runtime| {
            let receiver = runtime
                .objects
                .get(&object.object_id)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "extension object `{}` handle {} is no longer valid",
                        object.type_name, object.object_id
                    )
                })?;
            let value = runtime
                .vm
                .call_value_method_for_extension(&receiver, &method.name, args)
                .map_err(|err| err.to_string())?;
            runtime.vm.clear_output();
            let value =
                runtime.convert_return(self, Some((object, &receiver)), &method.returns, value)?;
            if method.lifecycle.is_dispose() {
                runtime.objects.remove(&object.object_id);
            }
            Ok(value)
        })
    }

    /// Retrieves or creates the runner runtime for this thread, then performs the call.
    fn with_runtime<T>(
        &self,
        action: impl FnOnce(&mut BtRunnerRuntime) -> Result<T, String>,
    ) -> Result<T, String> {
        BT_RUNNER_RUNTIMES.with(|runtimes| {
            let mut runtimes = runtimes.borrow_mut();
            if !runtimes.contains_key(&self.cache_key) {
                let runtime = BtRunnerRuntime::new(self)?;
                evict_runtime_cache_if_needed(&mut runtimes);
                runtimes.insert(self.cache_key.clone(), runtime);
            }
            let runtime = runtimes
                .get_mut(&self.cache_key)
                .expect("the newly inserted pure BT runner runtime must exist");
            action(runtime)
        })
    }

    /// Looks up object metadata by return type name.
    fn object_type(&self, name: &str) -> Option<&BtRunnerObject> {
        self.objects.get(name)
    }
}

impl BtRunnerRuntime {
    /// Creates and initializes a pure BT extension VM for the current thread.
    fn new(module: &BtRunnerModule) -> Result<Self, String> {
        let statements = parse_entry_source(&module.source_file, &module.source)?;
        let chunk = compile_entry_source(&module.source_file, &module.project_root, &statements)?;
        let chunk = Rc::new(chunk);
        let mut vm = Vm::with_project_root(module.project_root.clone());
        vm.run_with_value_owned(chunk.clone())
            .map_err(|err| err.to_string())?;
        vm.clear_output();
        Ok(Self {
            vm,
            _chunk: chunk,
            objects: HashMap::new(),
            next_object_id: 1,
        })
    }

    /// Converts a pure BT return value according to its binding return type.
    fn convert_return(
        &mut self,
        module: &BtRunnerModule,
        receiver: Option<(&crate::extensions::manager::ExtObject, &Value)>,
        returns: &str,
        value: Value,
    ) -> Result<Value, String> {
        if BindingValueType::is_primitive_return_name(returns) {
            validate_primitive_return(returns, &value)?;
            return Ok(value);
        }
        let object_type = module.object_type(returns).ok_or_else(|| {
            format!(
                "extension `{}` return type `{}` has no corresponding object declaration",
                module.module_name, returns
            )
        })?;
        validate_object_return(returns, &value)?;
        if let Some((object, receiver_value)) = receiver {
            if same_runtime_object(receiver_value, &value) {
                return Ok(Value::ExtObject(object.clone()));
            }
        }
        if self.objects.len() >= MAX_BT_OBJECT_HANDLES {
            return Err(format!(
                "extension `{}` exceeds the pure BT object handle limit of {}",
                module.module_name, MAX_BT_OBJECT_HANDLES
            ));
        }
        let object_id = self.next_object_id;
        self.next_object_id = self
            .next_object_id
            .checked_add(1)
            .ok_or_else(|| "pure BT object handle ID overflow".to_string())?;
        self.objects.insert(object_id, value);
        Ok(Value::ExtObject(crate::extensions::manager::ExtObject {
            module_id: module.module_id,
            type_id: object_type.type_id,
            type_name: object_type.name.clone(),
            object_id,
        }))
    }
}

/// Parses pure BT extension entry source.
fn parse_entry_source(source_file: &str, source: &str) -> Result<Vec<Statement>, String> {
    let tokens = tokenize(source).collect::<Vec<_>>();
    let mut parser = Parser::new(source_file.to_string(), source, tokens);
    parser.parse().map_err(|err| err.to_string())
}

/// Compiles pure BT extension entry source.
fn compile_entry_source(
    source_file: &str,
    project_root: &Path,
    statements: &[Statement],
) -> Result<Chunk, String> {
    Compiler::with_source_file(source_file.to_string(), project_root)
        .compile(statements)
        .map_err(|err| err.to_string())
}

/// Validates that a pure BT extension entry contains only permitted export declarations.
fn validate_restricted_entry(
    module_name: &str,
    statements: &[Statement],
    package: &ExtensionPackage,
) -> Result<(), String> {
    let mut functions = HashMap::new();
    let mut classes = HashMap::new();
    for statement in statements {
        match statement {
            Statement::Empty => {}
            Statement::Fn(name, params, _) => {
                functions.insert(name.clone(), params.len());
            }
            Statement::Class(name, members) => {
                let methods = validate_class_members(module_name, name, members)?;
                classes.insert(name.clone(), methods);
            }
            Statement::Assign(target, value)
                if is_constant_assignment_target(target) && is_literal_expr(value) => {}
            other => {
                return Err(format!(
                    "extension `{}` pure BT entry forbids top-level `{}`; only fn, class, or uppercase constant literals may be declared",
                    module_name,
                    statement_name(other)
                ));
            }
        }
    }

    for function in &package.bindings.functions {
        let param_count = functions.get(&function.name).ok_or_else(|| {
            format!(
                "extension `{}` binding entry `{}` has no matching fn in the BT source",
                module_name, function.name
            )
        })?;
        if *param_count != function.params.len() {
            return Err(format!(
                "extension `{}` entry `{}` parameter count differs from bindings: source has {}, bindings have {}",
                module_name,
                function.name,
                param_count,
                function.params.len()
            ));
        }
    }
    for object in &package.bindings.objects {
        let methods = classes.get(&object.name).ok_or_else(|| {
            format!(
                "extension `{}` binding object `{}` has no matching class in the BT source",
                module_name, object.name
            )
        })?;
        for method in &object.methods {
            let param_count = methods.get(&method.name).ok_or_else(|| {
                format!(
                    "extension `{}` object `{}` method `{}` has no matching public method in the BT source",
                    module_name, object.name, method.name
                )
            })?;
            if *param_count != method.params.len() {
                return Err(format!(
                    "extension `{}` method `{}.{}` parameter count differs from bindings: source has {}, bindings have {}",
                    module_name,
                    object.name,
                    method.name,
                    param_count,
                    method.params.len()
                ));
            }
        }
    }
    Ok(())
}

/// Validates class members and returns a table of public method parameter counts.
fn validate_class_members(
    module_name: &str,
    class_name: &str,
    members: &indexmap::IndexMap<String, (bool, Statement)>,
) -> Result<HashMap<String, usize>, String> {
    let mut methods = HashMap::new();
    for (name, (is_private, statement)) in members {
        match statement {
            Statement::Expr(expr) if is_literal_expr(expr) => {}
            Statement::Empty => {}
            Statement::Fn(_, params, _) => {
                if !*is_private {
                    methods.insert(name.clone(), params.len());
                }
            }
            other => {
                return Err(format!(
                    "extension `{}` class `{}` member `{}` must be a literal field or method, but is `{}`",
                    module_name,
                    class_name,
                    name,
                    statement_name(other)
                ));
            }
        }
    }
    Ok(methods)
}

/// Returns whether the assignment target is an uppercase constant name.
fn is_constant_assignment_target(target: &PosExpr) -> bool {
    matches!(&target.expr, Expr::Variable(name) if is_constant_name(name))
}

/// Returns whether a name follows the BT uppercase constant convention.
fn is_constant_name(name: &str) -> bool {
    let Some(first) = name.as_bytes().first() else {
        return false;
    };
    first.is_ascii_uppercase()
        && name
            .as_bytes()
            .get(1..)
            .unwrap_or_default()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

/// Returns whether an expression is a side-effect-free literal.
fn is_literal_expr(expr: &PosExpr) -> bool {
    match &expr.expr {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Empty
        | Expr::Regex(_, _) => true,
        Expr::Array(items) => items.iter().all(is_literal_expr),
        Expr::Object(entries) => entries.iter().all(|(_, value)| is_literal_expr(value)),
        _ => false,
    }
}

/// Validates that a primitive return value matches its declared type.
fn validate_primitive_return(returns: &str, value: &Value) -> Result<(), String> {
    let matched = match returns {
        "empty" => matches!(value, Value::Empty),
        "null" => matches!(value, Value::Null),
        "bool" => matches!(value, Value::Bool(_)),
        "int" => matches!(value, Value::Int(_)),
        "float" => matches!(value, Value::Float(_)),
        "string" => matches!(value, Value::Str(_)),
        "bytes" => matches!(value, Value::Bytes(_)),
        "array" => matches!(value, Value::Array(_)),
        "object" => matches!(value, Value::Object(_) | Value::Instance(_) | Value::Empty),
        _ => false,
    };
    if matched {
        Ok(())
    } else {
        Err(format!(
            "pure BT extension return type mismatch: bindings require `{}`, got {}",
            returns,
            value.type_name()
        ))
    }
}

/// Validates an object return value.
fn validate_object_return(returns: &str, value: &Value) -> Result<(), String> {
    if matches!(value, Value::Object(_) | Value::Instance(_)) {
        Ok(())
    } else {
        Err(format!(
            "pure BT extension returning `{}` must return an object or class instance, got {}",
            returns,
            value.type_name()
        ))
    }
}

/// Returns whether two runtime objects refer to the same allocation.
fn same_runtime_object(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => Rc::ptr_eq(left, right),
        (Value::Instance(left), Value::Instance(right)) => Rc::ptr_eq(left, right),
        _ => false,
    }
}

/// Returns a statement name for restricted-export error messages.
fn statement_name(statement: &Statement) -> &'static str {
    match statement {
        Statement::Empty => "empty",
        Statement::Expr(_) => "expr",
        Statement::Let(_, _) => "let",
        Statement::Use(_, _) => "use",
        Statement::Declare(_, _) => "declare",
        Statement::Assign(_, _) => "assign",
        Statement::Print(_) => "print",
        Statement::Println(_) => "println",
        Statement::If(_, _, _) => "if",
        Statement::Try(_, _, _, _) => "try",
        Statement::Throw(_) => "throw",
        Statement::Fn(_, _, _) => "fn",
        Statement::Class(_, _) => "class",
        Statement::For(_, _, _, _, _, _) => "for",
        Statement::ForCount(_, _, _, _) => "for_count",
        Statement::ForRange(_, _, _, _, _, _, _) => "for_range",
        Statement::ForDestructure(_, _, _, _) => "for_destructure",
        Statement::While(_, _, _) => "while",
        Statement::Loop(_, _) => "loop",
        Statement::Return(_) => "return",
        Statement::Break(_) => "break",
        Statement::Continue(_) => "continue",
    }
}

/// Computes a source content fingerprint.
fn source_fingerprint(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

/// Evicts an old runner runtime when the cache reaches its limit.
fn evict_runtime_cache_if_needed(cache: &mut HashMap<BtRunnerCacheKey, BtRunnerRuntime>) {
    if cache.len() < MAX_BT_RUNTIME_CACHE_ENTRIES {
        return;
    }
    if let Some(key) = cache.keys().next().cloned() {
        cache.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::bindings::ExtensionBindings;
    use crate::extensions::manifest::ExtensionManifest;
    use crate::extensions::package::PackageFileEntry;

    /// Builds a pure BT runner test package.
    fn make_package(source: &str, returns: &str) -> ExtensionPackage {
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
        let bindings_raw = format!(
            r#"{{
                "api_version": 1,
                "functions": [
                    {{
                        "name": "calc",
                        "id": 1,
                        "params": [{{ "name": "value", "type": "int" }}],
                        "returns": "{}"
                    }}
                ],
                "objects": [
                    {{
                        "name": "Calc",
                        "type_id": 1,
                        "methods": [
                            {{
                                "name": "add",
                                "id": 2,
                                "params": [{{ "name": "value", "type": "int" }}],
                                "returns": "Calc"
                            }},
                            {{
                                "name": "value",
                                "id": 3,
                                "params": [],
                                "returns": "int"
                            }}
                        ]
                    }}
                ]
            }}"#,
            returns
        );
        let bindings = ExtensionBindings::parse(&bindings_raw, &manifest).unwrap();
        ExtensionPackage {
            path: PathBuf::from("calc.bts"),
            manifest,
            bindings,
            entry_source: Some(source.to_string()),
            entry_wasm: None,
            files: vec![PackageFileEntry {
                path: "src/lib.bt".to_string(),
                uncompressed_size: source.len() as u64,
                compressed_size: source.len() as u64,
            }],
        }
    }

    /// Builds a pure BT runner test package that returns only primitive types.
    fn make_primitive_package(source: &str, returns: &str) -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            r#"{
                "format": "bts",
                "format_version": 1,
                "name": "primitive_pkg",
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
        let bindings_raw = format!(
            r#"{{
                "api_version": 1,
                "functions": [
                    {{
                        "name": "calc",
                        "id": 1,
                        "params": [{{ "name": "value", "type": "int" }}],
                        "returns": "{}"
                    }}
                ],
                "objects": []
            }}"#,
            returns
        );
        let bindings = ExtensionBindings::parse(&bindings_raw, &manifest).unwrap();
        ExtensionPackage {
            path: PathBuf::from("primitive.bts"),
            manifest,
            bindings,
            entry_source: Some(source.to_string()),
            entry_wasm: None,
            files: vec![PackageFileEntry {
                path: "src/lib.bt".to_string(),
                uncompressed_size: source.len() as u64,
                compressed_size: source.len() as u64,
            }],
        }
    }

    /// A restricted entry rejects top-level statements with side effects.
    #[test]
    fn rejects_top_level_side_effects() {
        let package = make_package("value = 1\nfn calc(value) { value }", "int");
        let err = BtRunnerModule::from_package(0, Path::new("."), &package).unwrap_err();

        assert!(err.contains("forbids top-level"));
    }

    /// A restricted entry requires binding functions to exist in the source.
    #[test]
    fn rejects_missing_export_function() {
        let package = make_package("fn other(value) { value }", "int");
        let err = BtRunnerModule::from_package(0, Path::new("."), &package).unwrap_err();

        assert!(err.contains("no matching fn"));
    }

    /// A return type that differs from the bindings produces an error.
    #[test]
    fn rejects_return_type_mismatch() {
        let package = make_primitive_package("fn calc(value) { 'bad' }", "int");
        let runner = BtRunnerModule::from_package(0, Path::new("."), &package).unwrap();
        let function = RegisteredFunction {
            module_id: 0,
            name: "calc".to_string(),
            function_id: 1,
            params: package.bindings.functions[0].params.clone(),
            returns: "int".to_string(),
        };
        let err = runner
            .call_function(&function, vec![Value::Int(1)])
            .unwrap_err();

        assert!(err.contains("return type mismatch"));
    }

    /// The primitive `object` return type accepts `empty` to represent no object result.
    #[test]
    fn object_return_accepts_empty_result() {
        let package = make_primitive_package("fn calc(value) { empty }", "object");
        let runner = BtRunnerModule::from_package(0, Path::new("."), &package).unwrap();
        let function = RegisteredFunction {
            module_id: 0,
            name: "calc".to_string(),
            function_id: 1,
            params: package.bindings.functions[0].params.clone(),
            returns: "object".to_string(),
        };

        let value = runner
            .call_function(&function, vec![Value::Int(1)])
            .unwrap();

        assert_eq!(value, Value::Empty);
    }
}
