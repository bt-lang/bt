//! Parsing and validation for `.bts` extension package bindings descriptions.

use std::collections::HashSet;

use serde::Deserialize;

use crate::extensions::is_snake_case_identifier;
use crate::extensions::is_type_identifier;
use crate::extensions::manifest::{ExtensionManifest, SUPPORTED_API_VERSION};

/// Maximum number of entry functions a single extension package may declare.
pub const MAX_BINDING_FUNCTIONS: usize = 128;
/// Maximum number of object types a single extension package may declare.
pub const MAX_BINDING_OBJECTS: usize = 128;
/// Maximum number of methods a single object may declare.
pub const MAX_BINDING_METHODS: usize = 128;
/// Maximum number of parameters a single entry or method may declare.
pub const MAX_BINDING_PARAMS: usize = 32;

/// Root bindings object for an extension package.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionBindings {
    /// Bindings semantic version, currently must be `1`.
    pub api_version: u32,
    /// List of public extension entry functions.
    #[serde(default)]
    pub functions: Vec<BindingFunction>,
    /// List of extension object types.
    #[serde(default)]
    pub objects: Vec<BindingObject>,
}

/// Public extension entry function.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingFunction {
    /// User-visible entry name.
    pub name: String,
    /// Backend call ID.
    pub id: u32,
    /// Entry parameter list.
    #[serde(default)]
    pub params: Vec<BindingParam>,
    /// Entry return type.
    pub returns: String,
}

/// Extension object type.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingObject {
    /// Object type name.
    pub name: String,
    /// Object type ID.
    pub type_id: u32,
    /// Object method list.
    #[serde(default)]
    pub methods: Vec<BindingMethod>,
}

/// Extension object method.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingMethod {
    /// User-visible method name.
    pub name: String,
    /// Backend call ID.
    pub id: u32,
    /// Method parameter list.
    #[serde(default)]
    pub params: Vec<BindingParam>,
    /// Method return type.
    pub returns: String,
    /// Method lifecycle semantics, defaulting to a normal call.
    #[serde(default)]
    pub lifecycle: BindingMethodLifecycle,
}

/// Extension call parameter.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BindingParam {
    /// Parameter name.
    pub name: String,
    /// Parameter value type.
    #[serde(rename = "type")]
    pub value_type: BindingValueType,
    /// Host-side preprocessing role for the parameter.
    #[serde(default)]
    pub role: BindingParamRole,
}

/// Value type for an extension call parameter.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingValueType {
    /// Any BT value, only for parameter declarations.
    Any,
    /// BT `empty` value.
    Empty,
    /// BT `null` value.
    Null,
    /// Boolean value.
    Bool,
    /// Integer value.
    Int,
    /// Floating-point value.
    Float,
    /// String value.
    String,
    /// Binary bytes value.
    Bytes,
    /// Array value.
    Array,
    /// Plain object value.
    Object,
}

impl BindingValueType {
    /// Stable text name for the return type.
    pub fn name(self) -> &'static str {
        match self {
            BindingValueType::Any => "any",
            BindingValueType::Empty => "empty",
            BindingValueType::Null => "null",
            BindingValueType::Bool => "bool",
            BindingValueType::Int => "int",
            BindingValueType::Float => "float",
            BindingValueType::String => "string",
            BindingValueType::Bytes => "bytes",
            BindingValueType::Array => "array",
            BindingValueType::Object => "object",
        }
    }

    /// Check whether a string is a supported primitive return type.
    pub fn is_primitive_return_name(name: &str) -> bool {
        matches!(
            name,
            "empty" | "null" | "bool" | "int" | "float" | "string" | "bytes" | "array" | "object"
        )
    }
}

/// Host-side preprocessing roles for extension call parameters.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingParamRole {
    /// Plain value parameter; no path preprocessing.
    Value,
    /// File read path parameter.
    PathRead,
    /// File write path parameter.
    PathWrite,
    /// Directory path parameter.
    PathDir,
}

/// Lifecycle semantics for extension object methods.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BindingMethodLifecycle {
    /// Normal method call; does not change the host-side object handle lifecycle.
    Call,
    /// Release the current receiver object handle; it should become invalid after the method succeeds.
    Dispose,
}

impl Default for BindingMethodLifecycle {
    /// Return the default lifecycle semantics.
    fn default() -> Self {
        BindingMethodLifecycle::Call
    }
}

impl BindingMethodLifecycle {
    /// Return the stable text name for the lifecycle semantics.
    pub fn name(self) -> &'static str {
        match self {
            BindingMethodLifecycle::Call => "call",
            BindingMethodLifecycle::Dispose => "dispose",
        }
    }

    /// Check whether the current method is a disposing method.
    pub fn is_dispose(self) -> bool {
        matches!(self, BindingMethodLifecycle::Dispose)
    }
}

impl Default for BindingParamRole {
    /// Return the default parameter role.
    fn default() -> Self {
        BindingParamRole::Value
    }
}

impl BindingParamRole {
    /// Return the stable text name for the parameter role.
    pub fn name(self) -> &'static str {
        match self {
            BindingParamRole::Value => "value",
            BindingParamRole::PathRead => "path_read",
            BindingParamRole::PathWrite => "path_write",
            BindingParamRole::PathDir => "path_dir",
        }
    }

    /// Check whether the current role is a path role.
    pub fn is_path(self) -> bool {
        !matches!(self, BindingParamRole::Value)
    }
}

impl ExtensionBindings {
    /// Parse bindings from JSON text and validate them against the manifest.
    pub fn parse(raw: &str, manifest: &ExtensionManifest) -> Result<Self, String> {
        let bindings: Self = serde_json::from_str(raw)
            .map_err(|err| format!("Failed to parse bindings.json: {}", err))?;
        bindings.validate(manifest)?;
        Ok(bindings)
    }

    /// Validate bindings fields and ranges.
    pub fn validate(&self, manifest: &ExtensionManifest) -> Result<(), String> {
        if self.api_version != SUPPORTED_API_VERSION || self.api_version != manifest.api_version {
            return Err(format!(
                "bindings.api_version must be {}, and it must match manifest.api_version",
                SUPPORTED_API_VERSION
            ));
        }
        if self.functions.is_empty() {
            return Err("bindings.functions must declare at least one public entry".to_string());
        }
        if self.functions.len() > MAX_BINDING_FUNCTIONS {
            return Err(format!(
                "bindings.functions count cannot exceed {}",
                MAX_BINDING_FUNCTIONS
            ));
        }
        if self.objects.len() > MAX_BINDING_OBJECTS {
            return Err(format!(
                "bindings.objects count cannot exceed {}",
                MAX_BINDING_OBJECTS
            ));
        }

        let object_names = self.validate_objects_shape()?;
        let mut call_ids = HashSet::new();
        let mut function_names = HashSet::new();
        for function in &self.functions {
            validate_callable(
                "bindings.functions",
                &function.name,
                function.id,
                &function.params,
                &function.returns,
                &object_names,
                manifest,
                &mut call_ids,
            )?;
            if !function_names.insert(function.name.as_str()) {
                return Err(format!(
                    "bindings.functions contains a duplicate entry `{}`",
                    function.name
                ));
            }
        }

        for object in &self.objects {
            let mut method_names = HashSet::new();
            if object.methods.len() > MAX_BINDING_METHODS {
                return Err(format!(
                    "bindings.objects.{} method count cannot exceed {}",
                    object.name, MAX_BINDING_METHODS
                ));
            }
            if object.methods.is_empty() {
                return Err(format!(
                    "bindings.objects.{} must declare at least one method",
                    object.name
                ));
            }
            for method in &object.methods {
                validate_callable(
                    &format!("bindings.objects.{}.methods", object.name),
                    &method.name,
                    method.id,
                    &method.params,
                    &method.returns,
                    &object_names,
                    manifest,
                    &mut call_ids,
                )?;
                validate_method_lifecycle(&object.name, method)?;
                if !method_names.insert(method.name.as_str()) {
                    return Err(format!(
                        "bindings.objects.{} contains a duplicate method `{}`",
                        object.name, method.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// Validate object names and type IDs, then return the set of object names.
    fn validate_objects_shape(&self) -> Result<HashSet<&str>, String> {
        let mut object_names = HashSet::new();
        let mut type_ids = HashSet::new();
        for object in &self.objects {
            if !is_type_identifier(&object.name) {
                return Err(format!(
                    "bindings.objects.name `{}` must be a type name that starts with an uppercase letter",
                    object.name
                ));
            }
            if object.type_id == 0 {
                return Err(format!(
                    "bindings.objects.{} type_id cannot be 0",
                    object.name
                ));
            }
            if !object_names.insert(object.name.as_str()) {
                return Err(format!(
                    "bindings.objects contains a duplicate object `{}`",
                    object.name
                ));
            }
            if !type_ids.insert(object.type_id) {
                return Err(format!(
                    "bindings.objects contains a duplicate type_id `{}`",
                    object.type_id
                ));
            }
        }
        Ok(object_names)
    }
}

/// Validate an entry function or object method.
fn validate_callable(
    field: &str,
    name: &str,
    id: u32,
    params: &[BindingParam],
    returns: &str,
    object_names: &HashSet<&str>,
    manifest: &ExtensionManifest,
    call_ids: &mut HashSet<u32>,
) -> Result<(), String> {
    if !is_snake_case_identifier(name) {
        return Err(format!(
            "{} `{}` must use snake_case and must not start or end with an underscore",
            field, name
        ));
    }
    if id == 0 {
        return Err(format!("{}.{} id cannot be 0", field, name));
    }
    if !call_ids.insert(id) {
        return Err(format!("bindings contains a duplicate call ID `{}`", id));
    }
    validate_params(field, name, params, manifest)?;
    validate_return_type(field, name, returns, object_names)
}

/// Validate that an object method lifecycle declaration is safe.
fn validate_method_lifecycle(object_name: &str, method: &BindingMethod) -> Result<(), String> {
    if !method.lifecycle.is_dispose() {
        return Ok(());
    }
    if method.name != "close" && method.name != "dispose" {
        return Err(format!(
            "bindings.objects.{} method `{}` marked lifecycle=dispose must be named close or dispose",
            object_name, method.name
        ));
    }
    if !method.params.is_empty() {
        return Err(format!(
            "bindings.objects.{} method `{}` marked lifecycle=dispose cannot declare parameters",
            object_name, method.name
        ));
    }
    if !BindingValueType::is_primitive_return_name(&method.returns) {
        return Err(format!(
            "bindings.objects.{} method `{}` marked lifecycle=dispose must return a primitive type, got `{}`",
            object_name, method.name, method.returns
        ));
    }
    Ok(())
}

/// Validate a parameter list.
fn validate_params(
    field: &str,
    callable_name: &str,
    params: &[BindingParam],
    manifest: &ExtensionManifest,
) -> Result<(), String> {
    if params.len() > MAX_BINDING_PARAMS {
        return Err(format!(
            "{}.{} parameter count cannot exceed {}",
            field, callable_name, MAX_BINDING_PARAMS
        ));
    }
    let mut names = HashSet::new();
    for param in params {
        if !is_snake_case_identifier(&param.name) {
            return Err(format!(
                "{}.{} parameter `{}` must use snake_case",
                field, callable_name, param.name
            ));
        }
        if !names.insert(param.name.as_str()) {
            return Err(format!(
                "{}.{} contains a duplicate parameter `{}`",
                field, callable_name, param.name
            ));
        }
        validate_param_role(field, callable_name, param, manifest)?;
    }
    Ok(())
}

/// Validate that parameter roles match manifest permission declarations.
fn validate_param_role(
    field: &str,
    callable_name: &str,
    param: &BindingParam,
    manifest: &ExtensionManifest,
) -> Result<(), String> {
    if param.role.is_path() && param.value_type != BindingValueType::String {
        return Err(format!(
            "{}.{} parameter `{}` using {} role must have type string, got {}",
            field,
            callable_name,
            param.name,
            param.role.name(),
            param.value_type.name()
        ));
    }
    let permissions = manifest.permissions;
    match param.role {
        BindingParamRole::Value => Ok(()),
        BindingParamRole::PathRead if permissions.fs_read => Ok(()),
        BindingParamRole::PathWrite if permissions.fs_write => Ok(()),
        BindingParamRole::PathDir if permissions.uses_fs() => Ok(()),
        BindingParamRole::PathRead => Err(format!(
            "{}.{} parameter `{}` uses path_read role, but manifest.permissions.fs_read is not declared",
            field, callable_name, param.name
        )),
        BindingParamRole::PathWrite => Err(format!(
            "{}.{} parameter `{}` uses path_write role, but manifest.permissions.fs_write is not declared",
            field, callable_name, param.name
        )),
        BindingParamRole::PathDir => Err(format!(
            "{}.{} parameter `{}` uses path_dir role, but neither manifest.permissions.fs_read nor manifest.permissions.fs_write is declared",
            field, callable_name, param.name
        )),
    }
}

/// Validate that the return type is either primitive or a declared object type.
fn validate_return_type(
    field: &str,
    callable_name: &str,
    returns: &str,
    object_names: &HashSet<&str>,
) -> Result<(), String> {
    if BindingValueType::is_primitive_return_name(returns) || object_names.contains(returns) {
        return Ok(());
    }
    Err(format!(
        "{}.{} return type `{}` is neither a supported primitive type nor an object declared in bindings.objects",
        field, callable_name, returns
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::manifest::ExtensionManifest;

    /// Return a manifest suitable for bindings validation.
    fn valid_manifest() -> ExtensionManifest {
        ExtensionManifest::parse(
            r#"{
                "format": "bts",
                "format_version": 1,
                "name": "calc",
                "version": "1.0.0",
                "kind": "bt",
                "abi": "bts-bt-1",
                "bt_min_version": "1.1.0",
                "api_version": 1,
                "entry": "src/lib.bt",
                "bindings": "bindings.json",
                "permissions": ["fs_read"]
            }"#,
        )
        .unwrap()
    }

    /// Return valid calc bindings JSON.
    fn valid_bindings_json() -> &'static str {
        r#"{
            "api_version": 1,
            "functions": [
                {
                    "name": "calc",
                    "id": 1,
                    "params": [
                        { "name": "value", "type": "int" }
                    ],
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
                            "params": [
                                { "name": "value", "type": "int" }
                            ],
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
        }"#
    }

    /// Valid bindings should parse successfully.
    #[test]
    fn parses_valid_bindings() {
        let bindings = ExtensionBindings::parse(valid_bindings_json(), &valid_manifest()).unwrap();
        assert_eq!(bindings.functions[0].name, "calc");
        assert_eq!(bindings.objects[0].methods.len(), 3);
        assert!(bindings.objects[0].methods[2].lifecycle.is_dispose());
    }

    /// The any parameter type should allow declaring any-value parameters.
    #[test]
    fn allows_any_param_type() {
        let raw = valid_bindings_json().replace(
            "{ \"name\": \"value\", \"type\": \"int\" }",
            "{ \"name\": \"value\", \"type\": \"any\" }",
        );
        let bindings = ExtensionBindings::parse(&raw, &valid_manifest()).unwrap();
        assert_eq!(
            bindings.functions[0].params[0].value_type,
            BindingValueType::Any
        );
    }

    /// any cannot be used as a return type.
    #[test]
    fn rejects_any_return_type() {
        let raw = valid_bindings_json().replace("\"returns\": \"Calc\"", "\"returns\": \"any\"");
        let err = ExtensionBindings::parse(&raw, &valid_manifest()).unwrap_err();
        assert!(err.contains("return type"));
    }

    /// Path roles must be backed by manifest-declared permissions.
    #[test]
    fn rejects_path_role_without_permission() {
        let mut manifest = valid_manifest();
        manifest.permissions.fs_read = false;
        let raw = valid_bindings_json().replace(
            "{ \"name\": \"value\", \"type\": \"int\" }",
            "{ \"name\": \"input\", \"type\": \"string\", \"role\": \"path_read\" }",
        );
        let err = ExtensionBindings::parse(&raw, &manifest).unwrap_err();
        assert!(err.contains("fs_read is not declared"));
    }

    /// Returning an unknown object type should fail.
    #[test]
    fn rejects_unknown_return_object() {
        let raw =
            valid_bindings_json().replace("\"returns\": \"Calc\"", "\"returns\": \"Missing\"");
        let err = ExtensionBindings::parse(&raw, &valid_manifest()).unwrap_err();
        assert!(err.contains("return type"));
    }

    /// Disposing methods cannot return extension objects.
    #[test]
    fn rejects_dispose_method_returning_object() {
        let raw = valid_bindings_json().replace(
            r#""name": "close",
                            "id": 4,
                            "params": [],
                            "returns": "bool",
                            "lifecycle": "dispose""#,
            r#""name": "close",
                            "id": 4,
                            "params": [],
                            "returns": "Calc",
                            "lifecycle": "dispose""#,
        );
        let err = ExtensionBindings::parse(&raw, &valid_manifest()).unwrap_err();
        assert!(err.contains("must return a primitive type"));
    }
}
