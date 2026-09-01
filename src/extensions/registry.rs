//! Registry metadata for extension entries, objects, and methods.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::extensions::bindings::{BindingMethodLifecycle, BindingParam, BindingValueType};
use crate::extensions::manifest::ExtensionKind;
use crate::extensions::package::ExtensionPackage;

/// Stable ID for an extension module in the registry.
pub type ExtensionModuleId = usize;

/// Extension registry.
#[derive(Debug, Clone)]
pub struct ExtensionRegistry {
    /// Extension module metadata in load order.
    modules: Vec<RegisteredExtension>,
    /// Entry function metadata indexed by user-visible entry name.
    functions: HashMap<String, RegisteredFunction>,
    /// Object metadata indexed by module ID and object type_id.
    objects: HashMap<ExtensionObjectKey, RegisteredObject>,
}

/// Registered extension module.
#[derive(Debug, Clone)]
pub struct RegisteredExtension {
    /// Extension module ID.
    pub module_id: ExtensionModuleId,
    /// Extension name.
    pub name: String,
    /// Extension version.
    pub version: String,
    /// Extension backend type.
    pub kind: ExtensionKind,
    /// Local extension package path.
    pub path: PathBuf,
}

/// Registered entry function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredFunction {
    /// Owning extension module ID.
    pub module_id: ExtensionModuleId,
    /// User-visible entry name.
    pub name: String,
    /// Backend call ID.
    pub function_id: u32,
    /// Parameter metadata.
    pub params: Vec<BindingParam>,
    /// Return type text.
    pub returns: String,
}

/// Extension object lookup key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtensionObjectKey {
    /// Owning extension module ID.
    pub module_id: ExtensionModuleId,
    /// Extension object type_id.
    pub type_id: u32,
}

/// Registered object type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredObject {
    /// Owning extension module ID.
    pub module_id: ExtensionModuleId,
    /// Object type name.
    pub name: String,
    /// Object type ID.
    pub type_id: u32,
    /// Object method list.
    pub methods: Vec<RegisteredMethod>,
}

/// Registered object method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredMethod {
    /// Owning extension module ID.
    pub module_id: ExtensionModuleId,
    /// Owning object type ID.
    pub type_id: u32,
    /// User-visible method name.
    pub name: String,
    /// Backend call ID.
    pub method_id: u32,
    /// Parameter metadata.
    pub params: Vec<BindingParam>,
    /// Return type text.
    pub returns: String,
    /// Method lifecycle semantics.
    pub lifecycle: BindingMethodLifecycle,
}

impl ExtensionRegistry {
    /// Build the registry from a parsed extension package.
    pub fn from_packages<I, S>(
        packages: Vec<ExtensionPackage>,
        reserved_names: I,
    ) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let reserved_names: HashSet<String> = reserved_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect();
        let mut modules = Vec::with_capacity(packages.len());
        let mut functions = HashMap::new();
        let mut objects = HashMap::new();

        for package in packages {
            let module_id = modules.len();
            register_package_functions(module_id, &package, &reserved_names, &mut functions)?;
            register_package_objects(module_id, &package, &mut objects)?;
            modules.push(RegisteredExtension {
                module_id,
                name: package.manifest.name,
                version: package.manifest.version,
                kind: package.manifest.kind,
                path: package.path,
            });
        }

        Ok(Self {
            modules,
            functions,
            objects,
        })
    }

    /// Return the number of registered modules.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Return the number of registered entry functions.
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Iterate over registered entry function metadata.
    pub fn functions(&self) -> impl Iterator<Item = &RegisteredFunction> {
        self.functions.values()
    }

    /// Read module metadata by module ID.
    pub fn module(&self, module_id: ExtensionModuleId) -> Option<&RegisteredExtension> {
        self.modules.get(module_id)
    }

    /// Read entry function metadata by entry name.
    pub fn function(&self, name: &str) -> Option<&RegisteredFunction> {
        self.functions.get(name)
    }

    /// Read object metadata by object key.
    pub fn object(&self, key: ExtensionObjectKey) -> Option<&RegisteredObject> {
        self.objects.get(&key)
    }

    /// Read object metadata by module ID and object type name.
    pub fn object_by_name(
        &self,
        module_id: ExtensionModuleId,
        name: &str,
    ) -> Option<&RegisteredObject> {
        self.objects
            .values()
            .find(|object| object.module_id == module_id && object.name == name)
    }

    /// Check whether an entry name is already registered.
    pub fn contains_function(&self, name: &str) -> bool {
        self.functions.contains_key(name)
    }
}

/// Register the public entry functions from one extension package.
fn register_package_functions(
    module_id: ExtensionModuleId,
    package: &ExtensionPackage,
    reserved_names: &HashSet<String>,
    functions: &mut HashMap<String, RegisteredFunction>,
) -> Result<(), String> {
    for function in &package.bindings.functions {
        if reserved_names.contains(&function.name) {
            return Err(format!(
                "Extension `{}` entry `{}` conflicts with a reserved name",
                package.manifest.name, function.name
            ));
        }
        if functions.contains_key(&function.name) {
            return Err(format!(
                "Extension entry `{}` has already been registered by another extension",
                function.name
            ));
        }
        functions.insert(
            function.name.clone(),
            RegisteredFunction {
                module_id,
                name: function.name.clone(),
                function_id: function.id,
                params: function.params.clone(),
                returns: function.returns.clone(),
            },
        );
    }
    Ok(())
}

/// Register object and method metadata from one extension package.
fn register_package_objects(
    module_id: ExtensionModuleId,
    package: &ExtensionPackage,
    objects: &mut HashMap<ExtensionObjectKey, RegisteredObject>,
) -> Result<(), String> {
    for object in &package.bindings.objects {
        let key = ExtensionObjectKey {
            module_id,
            type_id: object.type_id,
        };
        if objects.contains_key(&key) {
            return Err(format!(
                "Extension `{}` has a duplicate object type_id `{}`",
                package.manifest.name, object.type_id
            ));
        }
        let methods = object
            .methods
            .iter()
            .map(|method| RegisteredMethod {
                module_id,
                type_id: object.type_id,
                name: method.name.clone(),
                method_id: method.id,
                params: method.params.clone(),
                returns: method.returns.clone(),
                lifecycle: method.lifecycle,
            })
            .collect();
        objects.insert(
            key,
            RegisteredObject {
                module_id,
                name: object.name.clone(),
                type_id: object.type_id,
                methods,
            },
        );
    }
    Ok(())
}

/// Check whether a return type denotes an extension object.
pub fn is_extension_object_return(returns: &str) -> bool {
    !BindingValueType::is_primitive_return_name(returns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::bindings::ExtensionBindings;
    use crate::extensions::manifest::ExtensionManifest;
    use crate::extensions::package::PackageFileEntry;

    /// Build a test extension package.
    fn make_package(extension_name: &str, function_name: &str) -> ExtensionPackage {
        let manifest_raw = format!(
            r#"{{
                "format": "bts",
                "format_version": 1,
                "name": "{}",
                "version": "1.0.0",
                "kind": "bt",
                "abi": "bts-bt-1",
                "bt_min_version": "1.1.0",
                "api_version": 1,
                "entry": "src/lib.bt",
                "bindings": "bindings.json",
                "permissions": []
            }}"#,
            extension_name
        );
        let manifest = ExtensionManifest::parse(&manifest_raw).unwrap();
        let bindings_raw = format!(
            r#"{{
                "api_version": 1,
                "functions": [
                    {{
                        "name": "{}",
                        "id": 1,
                        "params": [
                            {{ "name": "value", "type": "int" }}
                        ],
                        "returns": "Calc"
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
                                "params": [
                                    {{ "name": "value", "type": "int" }}
                                ],
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
            function_name
        );
        let bindings = ExtensionBindings::parse(&bindings_raw, &manifest).unwrap();
        ExtensionPackage {
            path: PathBuf::from(format!("{}.bts", extension_name)),
            manifest,
            bindings,
            entry_source: Some("fn calc(value) { value }".to_string()),
            entry_wasm: None,
            files: vec![PackageFileEntry {
                path: "src/lib.bt".to_string(),
                uncompressed_size: 0,
                compressed_size: 0,
            }],
        }
    }

    /// The registry should record entry function and object method metadata.
    #[test]
    fn builds_registry_metadata() {
        let registry =
            ExtensionRegistry::from_packages(vec![make_package("calc_pkg", "calc")], ["print"])
                .unwrap();
        assert_eq!(registry.module_count(), 1);
        assert!(registry.contains_function("calc"));
        assert_eq!(registry.function("calc").unwrap().function_id, 1);
        let object = registry
            .object(ExtensionObjectKey {
                module_id: 0,
                type_id: 1,
            })
            .unwrap();
        assert_eq!(object.methods.len(), 2);
    }

    /// Entry names that conflict with reserved names should fail.
    #[test]
    fn rejects_reserved_function_name() {
        let err =
            ExtensionRegistry::from_packages(vec![make_package("calc_pkg", "calc")], ["calc"])
                .unwrap_err();
        assert!(err.contains("conflicts with a reserved name"));
    }

    /// Registering the same entry name from multiple extensions should fail.
    #[test]
    fn rejects_duplicate_function_name() {
        let err = ExtensionRegistry::from_packages(
            vec![
                make_package("calc_one", "calc"),
                make_package("calc_two", "calc"),
            ],
            ["print"],
        )
        .unwrap_err();
        assert!(err.contains("registered by another extension"));
    }

    /// Primitive return values should not be treated as extension objects.
    #[test]
    fn detects_extension_object_returns() {
        assert!(is_extension_object_return("Calc"));
        assert!(!is_extension_object_return("int"));
    }
}
