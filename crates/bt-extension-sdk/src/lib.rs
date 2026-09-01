//! BT WASM extension Rust SDK.
//!
//! This SDK targets `kind=wasm / bts-wasi-1` extensions and provides BtValueBinary
//! encoding/decoding, WASM linear-memory allocation and release, call-result envelopes,
//! and explicit call-ID dispatch. Extension authors only need `bt_extension!` to bind
//! the call IDs in `bindings.json` to Rust handlers.

use std::alloc::{alloc, dealloc, Layout};
use std::collections::{HashMap, HashSet};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

/// `empty` value tag.
const TAG_EMPTY: u8 = 0x00;
/// `null` value tag.
const TAG_NULL: u8 = 0x01;
/// Boolean value tag.
const TAG_BOOL: u8 = 0x02;
/// Integer value tag.
const TAG_INT: u8 = 0x03;
/// Floating-point value tag.
const TAG_FLOAT: u8 = 0x04;
/// String value tag.
const TAG_STRING: u8 = 0x05;
/// Bytes value tag.
const TAG_BYTES: u8 = 0x06;
/// Array value tag.
const TAG_ARRAY: u8 = 0x07;
/// Plain object value tag.
const TAG_OBJECT: u8 = 0x08;
/// Extension-object handle tag.
const TAG_EXT_OBJECT: u8 = 0x09;

/// Success marker in a WASM call-result envelope.
const CALL_RESULT_OK: u8 = 0x00;
/// Extension business-error marker in a WASM call-result envelope.
const CALL_RESULT_ERR: u8 = 0x01;

/// Default maximum total bytes for one encoding.
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
/// Default maximum UTF-8 bytes for one string.
pub const DEFAULT_MAX_STRING_BYTES: usize = 4 * 1024 * 1024;
/// Default maximum bytes for one Bytes buffer.
pub const DEFAULT_MAX_BYTES_BYTES: usize = 16 * 1024 * 1024;
/// Default maximum array-item count.
pub const DEFAULT_MAX_ARRAY_ITEMS: usize = 65_536;
/// Default maximum object-field count.
pub const DEFAULT_MAX_OBJECT_FIELDS: usize = 65_536;
/// Default maximum nesting depth.
pub const DEFAULT_MAX_DEPTH: usize = 64;

/// ID assigned to this WASM extension module by the current host.
static CURRENT_MODULE_ID: AtomicU64 = AtomicU64::new(0);

/// Shared result type used by SDK handlers.
pub type BtResult<T> = Result<T, String>;

/// Handler signature in the explicit dispatch registry.
pub type BtHandler = fn(Vec<BtValue>) -> BtResult<BtValue>;

/// `bts_init` lifecycle-handler signature.
pub type BtInitHandler = fn(BtValue) -> BtResult<BtValue>;

/// No-argument lifecycle-handler signature.
pub type BtNoArgHandler = fn() -> BtResult<BtValue>;

/// BtValueBinary encoding and decoding limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueCodecLimits {
    /// Maximum total bytes allowed for one encoding or decoding operation.
    pub max_total_bytes: usize,
    /// Maximum UTF-8 bytes allowed for one string.
    pub max_string_bytes: usize,
    /// Maximum bytes allowed for one Bytes value.
    pub max_bytes_bytes: usize,
    /// Maximum elements allowed in one array.
    pub max_array_items: usize,
    /// Maximum fields allowed in one object.
    pub max_object_fields: usize,
    /// Maximum nesting depth allowed during recursion.
    pub max_depth: usize,
}

impl Default for ValueCodecLimits {
    /// Return conservative default limits for the extension ABI.
    fn default() -> Self {
        Self {
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_string_bytes: DEFAULT_MAX_STRING_BYTES,
            max_bytes_bytes: DEFAULT_MAX_BYTES_BYTES,
            max_array_items: DEFAULT_MAX_ARRAY_ITEMS,
            max_object_fields: DEFAULT_MAX_OBJECT_FIELDS,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

impl ValueCodecLimits {
    /// Create limits with the given total-byte cap and defaults for everything else.
    pub fn with_total_bytes(max_total_bytes: usize) -> Self {
        Self {
            max_total_bytes,
            ..Self::default()
        }
    }
}

/// Subset of BT values that can cross the WASM extension boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum BtValue {
    /// BT `empty`, meaning no value is present.
    Empty,
    /// BT `null`, meaning an explicit null or failure value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// 64-bit integer value.
    Int(i64),
    /// 64-bit floating-point value; encoding rejects NaN and Infinity.
    Float(f64),
    /// UTF-8 string.
    String(String),
    /// Raw binary bytes.
    Bytes(Vec<u8>),
    /// Array value.
    Array(Vec<BtValue>),
    /// Plain object value; field order is preserved by the Vec.
    Object(Vec<(String, BtValue)>),
    /// Extension-object handle.
    ExtObject(ExtObject),
}

impl BtValue {
    /// Return a stable textual name for the value type.
    pub fn type_name(&self) -> &'static str {
        match self {
            BtValue::Empty => "empty",
            BtValue::Null => "null",
            BtValue::Bool(_) => "bool",
            BtValue::Int(_) => "int",
            BtValue::Float(_) => "float",
            BtValue::String(_) => "string",
            BtValue::Bytes(_) => "bytes",
            BtValue::Array(_) => "array",
            BtValue::Object(_) => "object",
            BtValue::ExtObject(_) => "ext_object",
        }
    }

    /// Return the integer if the current value is an integer.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            BtValue::Int(value) => Some(*value),
            _ => None,
        }
    }

    /// Return the string slice if the current value is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            BtValue::String(value) => Some(value),
            _ => None,
        }
    }

    /// Return the handle reference if the current value is an extension object.
    pub fn as_ext_object(&self) -> Option<&ExtObject> {
        match self {
            BtValue::ExtObject(value) => Some(value),
            _ => None,
        }
    }
}

impl From<bool> for BtValue {
    /// Convert a Boolean to a BT ABI value.
    fn from(value: bool) -> Self {
        BtValue::Bool(value)
    }
}

impl From<i64> for BtValue {
    /// Convert an i64 to a BT ABI integer value.
    fn from(value: i64) -> Self {
        BtValue::Int(value)
    }
}

impl From<i32> for BtValue {
    /// Convert an i32 to a BT ABI integer value.
    fn from(value: i32) -> Self {
        BtValue::Int(i64::from(value))
    }
}

impl From<f64> for BtValue {
    /// Convert an f64 to a BT ABI floating-point value.
    fn from(value: f64) -> Self {
        BtValue::Float(value)
    }
}

impl From<String> for BtValue {
    /// Convert a String to a BT ABI string value.
    fn from(value: String) -> Self {
        BtValue::String(value)
    }
}

impl From<&str> for BtValue {
    /// Convert a string slice to a BT ABI string value.
    fn from(value: &str) -> Self {
        BtValue::String(value.to_string())
    }
}

impl From<Vec<BtValue>> for BtValue {
    /// Convert a Vec to a BT ABI array value.
    fn from(value: Vec<BtValue>) -> Self {
        BtValue::Array(value)
    }
}

impl From<ExtObject> for BtValue {
    /// Convert an extension-object handle to a BT ABI value.
    fn from(value: ExtObject) -> Self {
        BtValue::ExtObject(value)
    }
}

/// Minimal handle transferred for an extension object between the BT VM and WASM SDK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtObject {
    /// Owning extension-module ID, injected by the host after instantiation.
    pub module_id: u64,
    /// Extension-object type ID; must match `type_id` in `bindings.json`.
    pub type_id: u32,
    /// Extension backend-object ID, maintained by the extension.
    pub object_id: u64,
    /// Extension-object type name; must match the object name in `bindings.json`.
    pub type_name: String,
}

impl ExtObject {
    /// Create an extension-object handle using the module ID injected by the current host.
    pub fn new(type_id: u32, object_id: u64, type_name: impl Into<String>) -> Self {
        Self {
            module_id: current_module_id(),
            type_id,
            object_id,
            type_name: type_name.into(),
        }
    }

    /// Create an extension-object handle with a specified module ID, mainly for tests or custom ABI adapters.
    pub fn with_module_id(
        module_id: u64,
        type_id: u32,
        object_id: u64,
        type_name: impl Into<String>,
    ) -> Self {
        Self {
            module_id,
            type_id,
            object_id,
            type_name: type_name.into(),
        }
    }

    /// Validate that the handle belongs to the current module and specified object type.
    pub fn validate_current_type(
        &self,
        type_id: u32,
        type_name: &str,
        label: &str,
    ) -> BtResult<()> {
        if self.module_id != current_module_id() {
            return Err(format!(
                "{} must belong to the current extension module {}, but belongs to {}",
                label,
                current_module_id(),
                self.module_id
            ));
        }
        if self.type_id != type_id || self.type_name != type_name {
            return Err(format!(
                "{} must be {}#{}, but is {}#{}",
                label, type_name, type_id, self.type_name, self.type_id
            ));
        }
        Ok(())
    }
}

/// Simple extension-side object-handle storage.
pub struct ObjectStore<T> {
    /// Maps backend object IDs to their live state.
    values: HashMap<u64, T>,
    /// Next object ID to allocate.
    next_id: u64,
    /// Maximum number of objects this store may hold.
    max_objects: usize,
}

impl<T> ObjectStore<T> {
    /// Create a store with an object-count limit.
    pub fn new(max_objects: usize) -> Self {
        Self {
            values: HashMap::new(),
            next_id: 1,
            max_objects,
        }
    }

    /// Insert object state and return a new object ID.
    pub fn insert(&mut self, value: T) -> BtResult<u64> {
        if self.values.len() >= self.max_objects {
            return Err(format!(
                "extension object count exceeds limit {}",
                self.max_objects
            ));
        }
        let object_id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "extension object IDs are exhausted".to_string())?;
        if self.values.insert(object_id, value).is_some() {
            return Err(format!("Extension object ID {} already exists", object_id));
        }
        Ok(object_id)
    }

    /// Read object state by object ID.
    pub fn get(&self, object_id: u64) -> Option<&T> {
        self.values.get(&object_id)
    }

    /// Read object state by object ID; return an error when the handle is missing.
    pub fn get_required(&self, object_id: u64, label: &str) -> BtResult<&T> {
        self.values
            .get(&object_id)
            .ok_or_else(|| format!("{} handle {} is no longer valid", label, object_id))
    }

    /// Mutably read object state by object ID.
    pub fn get_mut(&mut self, object_id: u64) -> Option<&mut T> {
        self.values.get_mut(&object_id)
    }

    /// Mutably read object state by object ID; return an error when the handle is missing.
    pub fn get_mut_required(&mut self, object_id: u64, label: &str) -> BtResult<&mut T> {
        self.values
            .get_mut(&object_id)
            .ok_or_else(|| format!("{} handle {} is no longer valid", label, object_id))
    }

    /// Remove object state and return the removed value.
    pub fn remove(&mut self, object_id: u64) -> Option<T> {
        self.values.remove(&object_id)
    }

    /// Remove object state; return an error when the handle is missing.
    pub fn remove_required(&mut self, object_id: u64, label: &str) -> BtResult<T> {
        self.values
            .remove(&object_id)
            .ok_or_else(|| format!("{} handle {} is no longer valid", label, object_id))
    }

    /// Check whether an object ID is still valid in the store.
    pub fn contains(&self, object_id: u64) -> bool {
        self.values.contains_key(&object_id)
    }

    /// Return the current number of stored objects.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Check whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Return the extension-module ID injected by the current host.
pub fn current_module_id() -> u64 {
    CURRENT_MODULE_ID.load(Ordering::Relaxed)
}

/// Set the current extension-module ID.
pub fn set_current_module_id(module_id: u64) {
    CURRENT_MODULE_ID.store(module_id, Ordering::Relaxed);
}

/// Implementation exported as `bts_alloc`.
pub fn bts_alloc_impl(len: u32) -> u32 {
    allocate_wasm_bytes(len as usize)
        .map(|ptr| ptr as usize as u32)
        .unwrap_or(0)
}

/// Implementation exported as `bts_free`.
pub fn bts_free_impl(ptr: u32, len: u32) {
    free_wasm_bytes(ptr, len);
}

/// Implementation exported as `bts_set_module_id`.
pub fn bts_set_module_id_impl(module_id: u64) {
    set_current_module_id(module_id);
}

/// Decode a plain BT ABI value.
pub fn decode_value(data: &[u8], limits: ValueCodecLimits) -> BtResult<BtValue> {
    let mut decoder = Decoder::new(data, limits)?;
    let value = decoder.decode_value(0)?;
    decoder.finish()?;
    Ok(value)
}

/// Encode a plain BT ABI value.
pub fn encode_value(value: &BtValue, limits: ValueCodecLimits) -> BtResult<Vec<u8>> {
    let mut encoder = Encoder::new(limits);
    encoder.encode_value(value, 0)?;
    Ok(encoder.into_output())
}

/// Encode a successful extension-call envelope.
pub fn encode_call_success(value: &BtValue, limits: ValueCodecLimits) -> BtResult<Vec<u8>> {
    let mut encoder = Encoder::new(limits);
    encoder.write_u8(CALL_RESULT_OK)?;
    encoder.encode_value(value, 0)?;
    Ok(encoder.into_output())
}

/// Encode an extension-call business-error envelope.
pub fn encode_call_error(message: &str, limits: ValueCodecLimits) -> BtResult<Vec<u8>> {
    let mut encoder = Encoder::new(limits);
    encoder.write_u8(CALL_RESULT_ERR)?;
    encoder.encode_value(&BtValue::String(message.to_string()), 0)?;
    Ok(encoder.into_output())
}

/// Decode an extension-call result envelope.
pub fn decode_call_output(data: &[u8], limits: ValueCodecLimits) -> BtResult<ExtensionCallOutput> {
    let mut decoder = Decoder::new(data, limits)?;
    let marker = decoder.read_u8()?;
    let value = decoder.decode_value(0)?;
    decoder.finish()?;
    match marker {
        CALL_RESULT_OK => Ok(ExtensionCallOutput::Value(value)),
        CALL_RESULT_ERR => match value {
            BtValue::String(message) => Ok(ExtensionCallOutput::Error(message)),
            other => Err(format!(
                "BtValueBinary call-error envelope must contain a string, but contains {}",
                other.type_name()
            )),
        },
        other => Err(format!(
            "BtValueBinary call-result marker 0x{other:02x} is unsupported"
        )),
    }
}

/// Result decoded from an extension-call envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionCallOutput {
    /// Extension call succeeded and carries a return value.
    Value(BtValue),
    /// Extension returned a business error with error text.
    Error(String),
}

/// Validate the number of handler arguments.
pub fn expect_arg_count(args: &[BtValue], expected: usize, label: &str) -> BtResult<()> {
    if args.len() == expected {
        return Ok(());
    }
    Err(format!(
        "{} requires {} arguments, but received {}",
        label,
        expected,
        args.len()
    ))
}

/// Read an integer argument from the argument list.
pub fn expect_int(args: &[BtValue], index: usize, name: &str) -> BtResult<i64> {
    match args.get(index) {
        Some(BtValue::Int(value)) => Ok(*value),
        Some(other) => Err(format!(
            "argument `{}` must be int, but is {}",
            name,
            other.type_name()
        )),
        None => Err(format!("Missing argument `{}`", name)),
    }
}

/// Read a string argument from the argument list.
pub fn expect_string(args: &[BtValue], index: usize, name: &str) -> BtResult<String> {
    match args.get(index) {
        Some(BtValue::String(value)) => Ok(value.clone()),
        Some(other) => Err(format!(
            "argument `{}` must be string, but is {}",
            name,
            other.type_name()
        )),
        None => Err(format!("Missing argument `{}`", name)),
    }
}

/// Read an extension-object handle from the argument list.
pub fn expect_ext_object(args: &[BtValue], index: usize, name: &str) -> BtResult<ExtObject> {
    match args.get(index) {
        Some(BtValue::ExtObject(value)) => Ok(value.clone()),
        Some(other) => Err(format!(
            "argument `{}` must be ext_object, but is {}",
            name,
            other.type_name()
        )),
        None => Err(format!("Missing argument `{}`", name)),
    }
}

/// Read an extension-object handle and validate its type against the current module.
pub fn expect_ext_object_type(
    args: &[BtValue],
    index: usize,
    name: &str,
    type_id: u32,
    type_name: &str,
) -> BtResult<ExtObject> {
    let object = expect_ext_object(args, index, name)?;
    object.validate_current_type(type_id, type_name, name)?;
    Ok(object)
}

/// Process encoded BtValueBinary arguments with one handler.
pub fn dispatch_handler_bytes(handler: BtHandler, encoded_args: &[u8]) -> Vec<u8> {
    encode_dispatch_output(decode_call_args(encoded_args).and_then(handler))
}

/// Read arguments from WASM linear memory, run the handler, and return a packed result pointer.
pub fn dispatch_handler(handler: BtHandler, args_ptr: u32, args_len: u32) -> u64 {
    let result = read_wasm_slice(args_ptr, args_len)
        .and_then(decode_call_args)
        .and_then(handler);
    pack_wasm_output(encode_dispatch_output(result))
}

/// Read initialization config from WASM linear memory, run the `bts_init` handler, and return a packed result.
pub fn dispatch_init_handler(handler: BtInitHandler, config_ptr: u32, config_len: u32) -> u64 {
    let result = read_wasm_slice(config_ptr, config_len)
        .and_then(|bytes| decode_value(bytes, ValueCodecLimits::default()))
        .and_then(handler);
    pack_wasm_output(encode_dispatch_output(result))
}

/// Run the `bts_shutdown` handler and return a packed result.
pub fn dispatch_shutdown_handler(handler: BtNoArgHandler) -> u64 {
    pack_wasm_output(encode_dispatch_output(handler()))
}

/// Run the `bts_stats` handler and return a packed result.
pub fn dispatch_stats_handler(handler: BtNoArgHandler) -> u64 {
    pack_wasm_output(encode_dispatch_output(handler()))
}

/// Generate a business-error envelope for an unknown call ID.
pub fn dispatch_unknown_call(call_id: u32) -> u64 {
    pack_wasm_output(encode_dispatch_output(Err(format!(
        "unknown extension call ID {}",
        call_id
    ))))
}

/// Explicitly register BT WASM extension handlers.
///
/// The left side of each entry must match the function or method `id` in `bindings.json`;
/// the right side is a handler shaped like `fn(Vec<BtValue>) -> BtResult<BtValue>`. The
/// macro exports `bts_alloc`, `bts_free`, `bts_set_module_id`, and `bts_call`.
#[macro_export]
macro_rules! bt_extension {
    ($($id:literal => $handler:path),+ $(,)?) => {
        #[doc = "BT WASM ABI memory-allocation export."]
        #[no_mangle]
        pub extern "C" fn bts_alloc(len: u32) -> u32 {
            $crate::bts_alloc_impl(len)
        }

        #[doc = "BT WASM ABI memory-release export."]
        #[no_mangle]
        pub extern "C" fn bts_free(ptr: u32, len: u32) {
            $crate::bts_free_impl(ptr, len)
        }

        #[doc = "BT WASM ABI optional module-ID initialization export."]
        #[no_mangle]
        pub extern "C" fn bts_set_module_id(module_id: u64) {
            $crate::bts_set_module_id_impl(module_id)
        }

        #[doc = "BT WASM ABI call-dispatch export."]
        #[no_mangle]
        pub extern "C" fn bts_call(call_id: u32, args_ptr: u32, args_len: u32) -> u64 {
            match call_id {
                $($id => $crate::dispatch_handler($handler, args_ptr, args_len),)+
                _ => $crate::dispatch_unknown_call(call_id),
            }
        }
    };
}

/// Export the optional `bts_init(config_ptr, config_len) -> u64` lifecycle function.
#[macro_export]
macro_rules! bt_extension_init {
    ($handler:path $(,)?) => {
        #[doc = "BT WASM ABI optional worker-initialization export."]
        #[no_mangle]
        pub extern "C" fn bts_init(config_ptr: u32, config_len: u32) -> u64 {
            $crate::dispatch_init_handler($handler, config_ptr, config_len)
        }
    };
}

/// Export the optional `bts_shutdown() -> u64` lifecycle function.
#[macro_export]
macro_rules! bt_extension_shutdown {
    ($handler:path $(,)?) => {
        #[doc = "BT WASM ABI optional worker-shutdown export."]
        #[no_mangle]
        pub extern "C" fn bts_shutdown() -> u64 {
            $crate::dispatch_shutdown_handler($handler)
        }
    };
}

/// Export the optional `bts_stats() -> u64` lifecycle function.
#[macro_export]
macro_rules! bt_extension_stats {
    ($handler:path $(,)?) => {
        #[doc = "BT WASM ABI optional worker-statistics export."]
        #[no_mangle]
        pub extern "C" fn bts_stats() -> u64 {
            $crate::dispatch_stats_handler($handler)
        }
    };
}

/// BtValueBinary encoder.
struct Encoder {
    /// Encoded-output buffer.
    output: Vec<u8>,
    /// Encoding limits.
    limits: ValueCodecLimits,
}

impl Encoder {
    /// Create a new encoder.
    fn new(limits: ValueCodecLimits) -> Self {
        Self {
            output: Vec::new(),
            limits,
        }
    }

    /// Take the encoded output.
    fn into_output(self) -> Vec<u8> {
        self.output
    }

    /// Recursively encode a BT ABI value.
    fn encode_value(&mut self, value: &BtValue, depth: usize) -> BtResult<()> {
        self.ensure_depth(depth)?;
        match value {
            BtValue::Empty => self.write_u8(TAG_EMPTY),
            BtValue::Null => self.write_u8(TAG_NULL),
            BtValue::Bool(value) => {
                self.write_u8(TAG_BOOL)?;
                self.write_u8(u8::from(*value))
            }
            BtValue::Int(value) => {
                self.write_u8(TAG_INT)?;
                self.write_i64(*value)
            }
            BtValue::Float(value) => {
                if !value.is_finite() {
                    return Err(
                        "BtValueBinary does not support transmitting NaN or Infinity".to_string(),
                    );
                }
                self.write_u8(TAG_FLOAT)?;
                self.write_f64(*value)
            }
            BtValue::String(value) => {
                self.write_u8(TAG_STRING)?;
                self.write_len_prefixed_bytes(
                    value.as_bytes(),
                    self.limits.max_string_bytes,
                    "string",
                )
            }
            BtValue::Bytes(value) => {
                self.write_u8(TAG_BYTES)?;
                self.write_len_prefixed_bytes(value, self.limits.max_bytes_bytes, "Bytes")
            }
            BtValue::Array(values) => self.encode_array(values, depth),
            BtValue::Object(values) => self.encode_object(values, depth),
            BtValue::ExtObject(object) => self.encode_ext_object(object),
        }
    }

    /// Encode an array value.
    fn encode_array(&mut self, values: &[BtValue], depth: usize) -> BtResult<()> {
        self.ensure_count(values.len(), self.limits.max_array_items, "array items")?;
        self.write_u8(TAG_ARRAY)?;
        self.write_u32(usize_to_u32(values.len(), "array item count")?)?;
        for value in values {
            self.encode_value(value, depth + 1)?;
        }
        Ok(())
    }

    /// Encode a plain object value.
    fn encode_object(&mut self, values: &[(String, BtValue)], depth: usize) -> BtResult<()> {
        self.ensure_count(values.len(), self.limits.max_object_fields, "object fields")?;
        let mut names = HashSet::with_capacity(values.len());
        self.write_u8(TAG_OBJECT)?;
        self.write_u32(usize_to_u32(values.len(), "object field count")?)?;
        for (key, value) in values {
            if !names.insert(key.as_str()) {
                return Err(format!(
                    "BtValueBinary object field `{}` is duplicated",
                    key
                ));
            }
            self.write_len_prefixed_bytes(
                key.as_bytes(),
                self.limits.max_string_bytes,
                "object field name",
            )?;
            self.encode_value(value, depth + 1)?;
        }
        Ok(())
    }

    /// Encode an extension-object handle.
    fn encode_ext_object(&mut self, object: &ExtObject) -> BtResult<()> {
        self.write_u8(TAG_EXT_OBJECT)?;
        self.write_u64(object.module_id)?;
        self.write_u32(object.type_id)?;
        self.write_u64(object.object_id)?;
        self.write_len_prefixed_bytes(
            object.type_name.as_bytes(),
            self.limits.max_string_bytes,
            "extension object type name",
        )
    }

    /// Write one byte.
    fn write_u8(&mut self, value: u8) -> BtResult<()> {
        self.reserve_output(1)?;
        self.output.push(value);
        Ok(())
    }

    /// Write a little-endian u32.
    fn write_u32(&mut self, value: u32) -> BtResult<()> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Write a little-endian u64.
    fn write_u64(&mut self, value: u64) -> BtResult<()> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Write a little-endian i64.
    fn write_i64(&mut self, value: i64) -> BtResult<()> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Write a little-endian f64.
    fn write_f64(&mut self, value: f64) -> BtResult<()> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Write a byte slice with a u32 length prefix.
    fn write_len_prefixed_bytes(
        &mut self,
        bytes: &[u8],
        limit: usize,
        label: &str,
    ) -> BtResult<()> {
        if bytes.len() > limit {
            return Err(format!(
                "BtValueBinary {} length {} exceeds limit {}",
                label,
                bytes.len(),
                limit
            ));
        }
        self.write_u32(usize_to_u32(bytes.len(), label)?)?;
        self.write_raw_bytes(bytes)
    }

    /// Write raw bytes.
    fn write_raw_bytes(&mut self, bytes: &[u8]) -> BtResult<()> {
        self.reserve_output(bytes.len())?;
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    /// Ensure the next write stays within the total-byte limit.
    fn reserve_output(&mut self, additional: usize) -> BtResult<()> {
        let next_len = self
            .output
            .len()
            .checked_add(additional)
            .ok_or_else(|| "BtValueBinary encoding length overflow".to_string())?;
        if next_len > self.limits.max_total_bytes {
            return Err(format!(
                "BtValueBinary total encoded bytes {} exceed limit {}",
                next_len, self.limits.max_total_bytes
            ));
        }
        self.output
            .try_reserve(additional)
            .map_err(|_| "BtValueBinary output-buffer allocation failed".to_string())
    }

    /// Validate the current nesting depth.
    fn ensure_depth(&self, depth: usize) -> BtResult<()> {
        if depth > self.limits.max_depth {
            return Err(format!(
                "BtValueBinary nesting depth {} exceeds limit {}",
                depth, self.limits.max_depth
            ));
        }
        Ok(())
    }

    /// Validate an element count.
    fn ensure_count(&self, count: usize, limit: usize, label: &str) -> BtResult<()> {
        if count > limit {
            return Err(format!(
                "BtValueBinary {} count {} exceeds limit {}",
                label, count, limit
            ));
        }
        Ok(())
    }
}

/// BtValueBinary decoder.
struct Decoder<'a> {
    /// Input awaiting decoding.
    input: &'a [u8],
    /// Current read offset.
    offset: usize,
    /// Decoding limits.
    limits: ValueCodecLimits,
}

impl<'a> Decoder<'a> {
    /// Create a new decoder.
    fn new(input: &'a [u8], limits: ValueCodecLimits) -> BtResult<Self> {
        if input.len() > limits.max_total_bytes {
            return Err(format!(
                "BtValueBinary total input bytes {} exceed limit {}",
                input.len(),
                limits.max_total_bytes
            ));
        }
        Ok(Self {
            input,
            offset: 0,
            limits,
        })
    }

    /// Recursively decode a BT ABI value.
    fn decode_value(&mut self, depth: usize) -> BtResult<BtValue> {
        self.ensure_depth(depth)?;
        let tag = self.read_u8()?;
        match tag {
            TAG_EMPTY => Ok(BtValue::Empty),
            TAG_NULL => Ok(BtValue::Null),
            TAG_BOOL => self.decode_bool(),
            TAG_INT => Ok(BtValue::Int(self.read_i64()?)),
            TAG_FLOAT => self.decode_float(),
            TAG_STRING => self.decode_string_value(),
            TAG_BYTES => self.decode_bytes_value(),
            TAG_ARRAY => self.decode_array(depth),
            TAG_OBJECT => self.decode_object(depth),
            TAG_EXT_OBJECT => self.decode_ext_object(),
            other => Err(format!(
                "BtValueBinary value tag 0x{other:02x} is unsupported"
            )),
        }
    }

    /// Decode a Boolean value.
    fn decode_bool(&mut self) -> BtResult<BtValue> {
        match self.read_u8()? {
            0 => Ok(BtValue::Bool(false)),
            1 => Ok(BtValue::Bool(true)),
            other => Err(format!("BtValueBinary bool value {} is invalid", other)),
        }
    }

    /// Decode a floating-point value.
    fn decode_float(&mut self) -> BtResult<BtValue> {
        let value = self.read_f64()?;
        if !value.is_finite() {
            return Err("BtValueBinary does not support transmitting NaN or Infinity".to_string());
        }
        Ok(BtValue::Float(value))
    }

    /// Decode a string value.
    fn decode_string_value(&mut self) -> BtResult<BtValue> {
        Ok(BtValue::String(self.read_string("string")?))
    }

    /// Decode a Bytes value.
    fn decode_bytes_value(&mut self) -> BtResult<BtValue> {
        Ok(BtValue::Bytes(
            self.read_len_prefixed_bytes(self.limits.max_bytes_bytes, "Bytes")?
                .to_vec(),
        ))
    }

    /// Decode an array value.
    fn decode_array(&mut self, depth: usize) -> BtResult<BtValue> {
        let count = self.read_count(self.limits.max_array_items, "array items")?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.decode_value(depth + 1)?);
        }
        Ok(BtValue::Array(values))
    }

    /// Decode a plain object value.
    fn decode_object(&mut self, depth: usize) -> BtResult<BtValue> {
        let count = self.read_count(self.limits.max_object_fields, "object fields")?;
        let mut seen = HashSet::with_capacity(count);
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            let key = self.read_string("object field name")?;
            if !seen.insert(key.clone()) {
                return Err(format!(
                    "BtValueBinary object field `{}` is duplicated",
                    key
                ));
            }
            let value = self.decode_value(depth + 1)?;
            values.push((key, value));
        }
        Ok(BtValue::Object(values))
    }

    /// Decode an extension-object handle.
    fn decode_ext_object(&mut self) -> BtResult<BtValue> {
        let module_id = self.read_u64()?;
        let type_id = self.read_u32()?;
        if type_id == 0 {
            return Err("BtValueBinary extension-object type_id cannot be 0".to_string());
        }
        let object_id = self.read_u64()?;
        let type_name = self.read_string("extension object type name")?;
        if type_name.is_empty() {
            return Err("BtValueBinary extension-object type name cannot be empty".to_string());
        }
        Ok(BtValue::ExtObject(ExtObject {
            module_id,
            type_id,
            object_id,
            type_name,
        }))
    }

    /// Read one byte.
    fn read_u8(&mut self) -> BtResult<u8> {
        Ok(self.read_raw_bytes(1)?[0])
    }

    /// Read a little-endian u32.
    fn read_u32(&mut self) -> BtResult<u32> {
        let bytes = self.read_raw_bytes(4)?;
        Ok(u32::from_le_bytes(copy_array(bytes)?))
    }

    /// Read a little-endian u64.
    fn read_u64(&mut self) -> BtResult<u64> {
        let bytes = self.read_raw_bytes(8)?;
        Ok(u64::from_le_bytes(copy_array(bytes)?))
    }

    /// Read a little-endian i64.
    fn read_i64(&mut self) -> BtResult<i64> {
        let bytes = self.read_raw_bytes(8)?;
        Ok(i64::from_le_bytes(copy_array(bytes)?))
    }

    /// Read a little-endian f64.
    fn read_f64(&mut self) -> BtResult<f64> {
        let bytes = self.read_raw_bytes(8)?;
        Ok(f64::from_le_bytes(copy_array(bytes)?))
    }

    /// Read a byte slice with a u32 length prefix.
    fn read_len_prefixed_bytes(&mut self, limit: usize, label: &str) -> BtResult<&'a [u8]> {
        let len = self.read_count(limit, label)?;
        self.read_raw_bytes(len)
    }

    /// Read a UTF-8 string.
    fn read_string(&mut self, label: &str) -> BtResult<String> {
        let bytes = self.read_len_prefixed_bytes(self.limits.max_string_bytes, label)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|err| format!("BtValueBinary {} is not UTF-8: {}", label, err))
    }

    /// Read a count and validate its limit.
    fn read_count(&mut self, limit: usize, label: &str) -> BtResult<usize> {
        let count = self.read_u32()? as usize;
        if count > limit {
            return Err(format!(
                "BtValueBinary {} count {} exceeds limit {}",
                label, count, limit
            ));
        }
        Ok(count)
    }

    /// Read a raw byte slice.
    fn read_raw_bytes(&mut self, len: usize) -> BtResult<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "BtValueBinary decoding offset overflow".to_string())?;
        if end > self.input.len() {
            return Err(format!(
                "BtValueBinary decoding needs {} bytes, but only {} remain",
                len,
                self.input.len().saturating_sub(self.offset)
            ));
        }
        let bytes = &self.input[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    /// Validate the current nesting depth.
    fn ensure_depth(&self, depth: usize) -> BtResult<()> {
        if depth > self.limits.max_depth {
            return Err(format!(
                "BtValueBinary nesting depth {} exceeds limit {}",
                depth, self.limits.max_depth
            ));
        }
        Ok(())
    }

    /// Confirm that the input was fully consumed.
    fn finish(&self) -> BtResult<()> {
        if self.offset == self.input.len() {
            return Ok(());
        }
        Err(format!(
            "BtValueBinary has {} unconsumed bytes after decoding",
            self.input.len() - self.offset
        ))
    }
}

/// Convert usize to u32 while preserving error context.
fn usize_to_u32(value: usize, label: &str) -> BtResult<u32> {
    u32::try_from(value)
        .map_err(|_| format!("BtValueBinary {} length exceeds the u32 limit", label))
}

/// Copy a slice into a fixed-length array.
fn copy_array<const N: usize>(bytes: &[u8]) -> BtResult<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| "BtValueBinary internal fixed-length copy failed".to_string())
}

/// Extract the call-argument array from an encoded parameter value.
fn decode_call_args(encoded_args: &[u8]) -> BtResult<Vec<BtValue>> {
    match decode_value(encoded_args, ValueCodecLimits::default())? {
        BtValue::Array(values) => Ok(values),
        other => Err(format!(
            "BtValueBinary call arguments must be an array, but are {}",
            other.type_name()
        )),
    }
}

/// Encode a handler result as a call-result envelope.
fn encode_dispatch_output(result: BtResult<BtValue>) -> Vec<u8> {
    let limits = ValueCodecLimits::default();
    match result {
        Ok(value) => encode_call_success(&value, limits).unwrap_or_else(|err| {
            encode_fallback_error(&format!("return-value encoding failed: {}", err))
        }),
        Err(message) => encode_call_error(&message, limits)
            .unwrap_or_else(|_| encode_fallback_error("extension-error encoding failed")),
    }
}

/// Encode a compact fallback error envelope.
fn encode_fallback_error(message: &str) -> Vec<u8> {
    encode_call_error(message, ValueCodecLimits::with_total_bytes(1024)).unwrap_or_else(|_| {
        vec![
            CALL_RESULT_ERR,
            TAG_STRING,
            15,
            0,
            0,
            0,
            b'e',
            b'x',
            b't',
            b'e',
            b'n',
            b's',
            b'i',
            b'o',
            b'n',
            b' ',
            b'e',
            b'r',
            b'r',
            b'o',
            b'r',
        ]
    })
}

/// Construct a read-only slice from a WASM pointer and length.
fn read_wasm_slice(ptr: u32, len: u32) -> BtResult<&'static [u8]> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr == 0 {
        return Err("WASM argument pointer is 0".to_string());
    }
    let ptr = ptr as usize as *const u8;
    let len = len as usize;
    // Safety: the host validates that ptr/len are within current WASM linear memory before calling `bts_call`.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Copy the encoded call result into releasable WASM memory and return it packed.
fn pack_wasm_output(bytes: Vec<u8>) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let Some(ptr) = allocate_wasm_bytes(bytes.len()) else {
        return 0;
    };
    // Safety: this function allocated ptr for bytes.len(), so the source and destination do not overlap.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    }
    pack_ptr_len(ptr, bytes.len())
}

/// Allocate WASM linear memory releasable by `bts_free_impl`.
fn allocate_wasm_bytes(len: usize) -> Option<*mut u8> {
    if len == 0 {
        return Some(ptr::null_mut());
    }
    let layout = Layout::from_size_align(len, 1).ok()?;
    // Safety: the standard library validated layout; the returned pointer is used only as a raw byte buffer.
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr)
    }
}

/// Release WASM linear memory allocated by the SDK.
fn free_wasm_bytes(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 {
        return;
    }
    let Ok(layout) = Layout::from_size_align(len as usize, 1) else {
        return;
    };
    // Safety: the host returns only ptr/len from `bts_alloc_impl` or an SDK result buffer to this function.
    unsafe {
        dealloc(ptr as usize as *mut u8, layout);
    }
}

/// Pack a return pointer and length into the u64 required by the ABI.
fn pack_ptr_len(ptr: *mut u8, len: usize) -> u64 {
    let ptr = u32::try_from(ptr as usize).unwrap_or(0);
    let len = u32::try_from(len).unwrap_or(0);
    (u64::from(ptr) << 32) | u64::from(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create encoding limits for tests.
    fn limits() -> ValueCodecLimits {
        ValueCodecLimits::with_total_bytes(4096)
    }

    /// Test handler: add one to an integer argument.
    fn add_one(args: Vec<BtValue>) -> BtResult<BtValue> {
        expect_arg_count(&args, 1, "add_one")?;
        Ok(BtValue::Int(expect_int(&args, 0, "value")? + 1))
    }

    /// BtValueBinary should preserve plain values and extension-object handles.
    #[test]
    fn round_trips_values() {
        let value = BtValue::Object(vec![
            ("empty".to_string(), BtValue::Empty),
            ("null".to_string(), BtValue::Null),
            ("bool".to_string(), BtValue::Bool(true)),
            ("int".to_string(), BtValue::Int(-7)),
            ("float".to_string(), BtValue::Float(1.5)),
            ("string".to_string(), BtValue::String("BT".to_string())),
            ("bytes".to_string(), BtValue::Bytes(vec![1, 2, 3])),
            (
                "array".to_string(),
                BtValue::Array(vec![BtValue::Int(1), BtValue::String("x".to_string())]),
            ),
            (
                "object".to_string(),
                BtValue::Object(vec![("nested".to_string(), BtValue::Bool(false))]),
            ),
            (
                "ext".to_string(),
                BtValue::ExtObject(ExtObject::with_module_id(9, 1, 42, "Calc")),
            ),
        ]);

        let encoded = encode_value(&value, limits()).expect("encoding should succeed");
        let decoded = decode_value(&encoded, limits()).expect("decoding should succeed");
        assert_eq!(decoded, value);
    }

    /// Call-result envelopes should distinguish success from business errors.
    #[test]
    fn call_output_envelope_round_trips() {
        let ok = encode_call_success(&BtValue::Int(3), limits()).unwrap();
        assert_eq!(
            decode_call_output(&ok, limits()).unwrap(),
            ExtensionCallOutput::Value(BtValue::Int(3))
        );

        let err = encode_call_error("invalid arguments", limits()).unwrap();
        assert_eq!(
            decode_call_output(&err, limits()).unwrap(),
            ExtensionCallOutput::Error("invalid arguments".to_string())
        );
    }

    /// Explicit handler dispatch should decode arguments and encode the return value.
    #[test]
    fn dispatch_handler_bytes_returns_success() {
        let args = encode_value(&BtValue::Array(vec![BtValue::Int(2)]), limits()).unwrap();
        let output = dispatch_handler_bytes(add_one, &args);
        assert_eq!(
            decode_call_output(&output, limits()).unwrap(),
            ExtensionCallOutput::Value(BtValue::Int(3))
        );
    }

    /// A new object handle should use the injected module ID.
    #[test]
    fn ext_object_uses_current_module_id() {
        set_current_module_id(12);
        let object = ExtObject::new(1, 7, "Calc");
        assert_eq!(object.module_id, 12);
        assert_eq!(object.type_id, 1);
        assert_eq!(object.object_id, 7);
    }

    /// The object store should enforce its limit and support reading, writing, and removing objects.
    #[test]
    fn object_store_tracks_handles() {
        let mut store = ObjectStore::new(1);
        let id = store.insert(10).unwrap();
        *store.get_mut_required(id, "Calc").unwrap() += 5;
        assert_eq!(store.get_required(id, "Calc"), Ok(&15));
        assert!(store.contains(id));
        assert!(store.insert(20).unwrap_err().contains("limit"));
        assert_eq!(store.remove_required(id, "Calc"), Ok(15));
        assert!(store
            .remove_required(id, "Calc")
            .unwrap_err()
            .contains("no longer valid"));
        assert!(store.is_empty());
    }

    /// Extension-object argument validation should check module ID and object type.
    #[test]
    fn expect_ext_object_type_validates_receiver() {
        set_current_module_id(8);
        let args = vec![BtValue::ExtObject(ExtObject::new(1, 9, "Calc"))];
        let object = expect_ext_object_type(&args, 0, "self", 1, "Calc").unwrap();
        assert_eq!(object.object_id, 9);

        let err = expect_ext_object_type(&args, 0, "self", 2, "Image").unwrap_err();
        assert!(err.contains("must be"));
    }

    /// The encoder should reject duplicate object fields.
    #[test]
    fn rejects_duplicate_object_keys() {
        let value = BtValue::Object(vec![
            ("x".to_string(), BtValue::Int(1)),
            ("x".to_string(), BtValue::Int(2)),
        ]);
        let err = encode_value(&value, limits()).unwrap_err();
        assert!(err.contains("duplicated"));
    }

    /// The encoder should reject non-finite floating-point values.
    #[test]
    fn rejects_non_finite_float() {
        let err = encode_value(&BtValue::Float(f64::NAN), limits()).unwrap_err();
        assert!(err.contains("NaN"));
    }
}
