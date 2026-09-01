//! BT FFI standard library.
//!
//! has complete explicit signature, limited declaration exemption, return type hint, stable writable Buffer, Pointer owner, string return and
//! fixed resource quota to reuse the same libffi calling engine.

#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
    all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
)))]
compile_error!(
    "BT ffi currently only supports x86_64-pc-windows-msvc, x86_64-unknown-linux-gnu, x86_64-apple-darwin and aarch64-apple-darwin"
);

use crate::libs::bytes::BtBytes;
use crate::value::Value;
use indexmap::IndexMap;
use libffi::middle::{Arg, Cif, CodePtr, Ret, Type};
use libloading::Library;
use std::array;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The maximum number of functions allowed in a single schema.
const MAX_SCHEMA_FUNCTIONS: usize = 128;
/// The maximum number of cached functions in a single dynamic library.
const MAX_CACHED_FUNCTIONS: usize = 256;
/// The maximum number of parameters allowed in a single function.
const MAX_FUNCTION_ARGS: usize = 16;
/// The maximum number of UTF-8 bytes allowed for an exported symbol name.
const MAX_SYMBOL_BYTES: usize = 255;
/// The maximum number of UTF-8 bytes allowed for a complete signature.
const MAX_SIGNATURE_BYTES: usize = 512;
/// The upper limit of the dynamic library that the process can logically open at the same time.
const MAX_OPEN_LIBRARIES: usize = 32;
/// The upper limit of FFI Buffer that the process can survive at the same time.
const MAX_BUFFERS: usize = 256;
/// The upper limit of the number of bytes actually allocated by all FFI Buffers of the process.
const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;

/// The number of dynamic libraries opened logically by the current process.
static OPEN_LIBRARIES: AtomicUsize = AtomicUsize::new(0);
/// The number of FFI Buffers alive in the current process.
static BUFFERS: AtomicUsize = AtomicUsize::new(0);
/// The actual number of allocated bytes of the current process FFI Buffer.
static BUFFER_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Serialization will create tests for FFI resources to prevent process-level credit statistics from contaminating each other between parallel tests.
#[cfg(test)]
static TEST_RESOURCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Obtains the FFI test resource lock; allows subsequent tests to continue validating the cleanup path even if the previous test panics.
#[cfg(test)]
pub(crate) fn lock_test_resources() -> std::sync::MutexGuard<'static, ()> {
    TEST_RESOURCE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// C ABI types supported by explicit full signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FfiType {
    /// has no return value and is only allowed for return types.
    Void,
    /// Signed 8-bit integer.
    I8,
    /// Signed 16-bit integer.
    I16,
    /// Signed 32-bit integer.
    I32,
    /// Signed 64-bit integer.
    I64,
    /// Unsigned 8-bit integer.
    U8,
    /// Unsigned 16-bit integer.
    U16,
    /// Unsigned 32-bit integer.
    U32,
    /// Unsigned 64-bit integer.
    U64,
    /// Pointer width signed integer.
    ISize,
    /// Pointer width unsigned integer.
    USize,
    /// 32-bit floating point number.
    F32,
    /// 64-bit floating point number.
    F64,
    /// Ordinary native pointer.
    Ptr,
    /// UTF-8 NUL terminated input string pointer.
    CStr,
    /// Windows UTF-16 NUL terminated input string pointer.
    WStr,
}

impl FfiType {
    /// Returns the native type used by the libffi call description.
    fn libffi_type(self) -> Type {
        match self {
            Self::Void => Type::void(),
            Self::I8 => Type::i8(),
            Self::I16 => Type::i16(),
            Self::I32 => Type::i32(),
            Self::I64 => Type::i64(),
            Self::U8 => Type::u8(),
            Self::U16 => Type::u16(),
            Self::U32 => Type::u32(),
            Self::U64 => Type::u64(),
            Self::ISize => Type::isize(),
            Self::USize => Type::usize(),
            Self::F32 => Type::f32(),
            Self::F64 => Type::f64(),
            Self::Ptr | Self::CStr | Self::WStr => Type::pointer(),
        }
    }

    /// returns the schema type name used in the error message.
    fn name(self) -> &'static str {
        match self {
            Self::Void => "void",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::ISize => "isize",
            Self::USize => "usize",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Ptr => "ptr",
            Self::CStr => "cstr",
            Self::WStr => "wstr",
        }
    }
}

/// The type of inference after the first call lock for limited declaration-free parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InferredKind {
    /// BT Int in range i32.
    I32,
    /// Null pointer, FfiPointer, or FfiBuffer.
    Ptr,
    /// Windows `W` exports allowed UTF-16 strings or null pointers.
    WStr,
    /// Windows `A` exports allowed ASCII strings or null pointers.
    AnsiAscii,
}

/// Limited inference of generated ABI parameter types and first-call locking kinds.
type InferredParams = (Box<[FfiType]>, Box<[InferredKind]>);

/// Resolved full function signature or just return type hint.
#[derive(Debug, Clone)]
struct FunctionSpec {
    /// Compact table of fully signed parameter types; returns a hint that there are no parameter declarations.
    params: Option<Box<[FfiType]>>,
    /// return type.
    result: FfiType,
}

/// The cached call description after the first successful symbol resolution.
#[derive(Debug)]
struct FfiFunction {
    /// exports symbol names exactly.
    symbol: Box<str>,
    /// Compact table of parameter types.
    params: Box<[FfiType]>,
    /// return type.
    result: FfiType,
    /// First call lock table for finite inference functions; full signature is None.
    inferred: Option<Box<[InferredKind]>>,
    /// Native function entry address.
    code: CodePtr,
    /// libffi fixed call description.
    cif: Cif,
}

impl FfiFunction {
    /// Creates cache entries from validated parameters, return types, and function addresses.
    fn new(
        symbol: &str,
        params: Box<[FfiType]>,
        result: FfiType,
        inferred: Option<Box<[InferredKind]>>,
        code: CodePtr,
    ) -> Self {
        let cif = Cif::new(
            params.iter().copied().map(FfiType::libffi_type),
            result.libffi_type(),
        );
        Self {
            symbol: symbol.into(),
            params,
            result,
            inferred,
            code,
            cif,
        }
    }
}

/// Dynamic library calling mode.
#[derive(Debug)]
enum LibraryMode {
    /// Only allow full signatures listed in the schema or return hints.
    StrictSchema(HashMap<Box<str>, FunctionSpec>),
    /// No schema, limited inference based on fixed conservative rules.
    LimitedImplicit,
}

/// The dynamic library and corresponding process quota guard have been loaded.
#[derive(Debug)]
struct LoadedLibrary {
    /// System dynamic library handle.
    library: Library,
    /// Dynamic library logic turns on the number guard.
    quota: LibraryQuotaGuard,
}

impl LoadedLibrary {
    /// Explicitly close the system dynamic library and release the logical quota before returning.
    fn close(self) -> Result<(), String> {
        let Self { library, quota } = self;
        let result = library
            .close()
            .map_err(|error| format!("Failed to close dynamic library: {}", error));
        drop(quota);
        result
    }
}

/// The calling mode, cache and life cycle status of a single dynamic library.
#[derive(Debug)]
struct FfiLibraryState {
    /// The strict schema or limited declaration-free schema determined at load time.
    mode: LibraryMode,
    /// Function buffer with at most one entry per exact symbol.
    functions: HashMap<Box<str>, Rc<FfiFunction>>,
    /// System dynamic library handle; empty after logic is closed.
    loaded: Option<LoadedLibrary>,
    /// Whether the script side has been logically closed.
    closed: bool,
}

/// Dynamic library logic turns on the number guard.
#[derive(Debug)]
struct LibraryQuotaGuard;

impl LibraryQuotaGuard {
    /// atomically reserves a dynamic library quota.
    fn reserve() -> Result<Self, String> {
        OPEN_LIBRARIES
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                (current < MAX_OPEN_LIBRARIES).then_some(current + 1)
            })
            .map_err(|_| {
                format!(
                    "FFI can open at most {} dynamic libraries at once",
                    MAX_OPEN_LIBRARIES
                )
            })?;
        Ok(Self)
    }
}

/// The quota guard that simultaneously limits the number of Buffers and the actual number of allocated bytes.
#[derive(Debug)]
struct BufferQuotaGuard {
    /// The actual number of bytes allocated by the current Buffer rounded up into 16-byte blocks.
    bytes: usize,
}

impl BufferQuotaGuard {
    /// atomically reserves a Buffer and the corresponding actual byte quota. Any failure will roll back the completed reservation.
    fn reserve(bytes: usize) -> Result<Self, String> {
        BUFFERS
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                (current < MAX_BUFFERS).then_some(current + 1)
            })
            .map_err(|_| format!("FFI can keep at most {} Buffers alive at once", MAX_BUFFERS))?;
        if BUFFER_BYTES
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= MAX_BUFFER_BYTES)
            })
            .is_err()
        {
            BUFFERS.fetch_sub(1, Ordering::AcqRel);
            return Err(format!(
                "FFI Buffer allocations exceed the byte limit of {}",
                MAX_BUFFER_BYTES
            ));
        }
        Ok(Self { bytes })
    }
}

impl Drop for BufferQuotaGuard {
    /// Release the Buffer quantity and actual byte quota at the same time.
    fn drop(&mut self) {
        BUFFER_BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
        BUFFERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// guarantees that each element and the entire segment is stored in a fixed memory block of at least 16 bytes alignment.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Default)]
struct AlignedBlock {
    /// The raw bytes actually carried by the Buffer.
    bytes: [u8; 16],
}

/// BT managed address stable and writable Buffer status.
#[derive(Debug)]
struct FfiBufferState {
    /// Fixed allocation; empty and released immediately after logic is closed.
    blocks: Option<Box<[AlignedBlock]>>,
    /// The length of visible bytes requested by the script, without alignment padding.
    len: usize,
    /// Buffer quantity and actual allocated byte quota guard; empty after closing.
    quota: Option<BufferQuotaGuard>,
    /// Whether the script side has been logically closed.
    closed: bool,
}

/// Owner that keeps an FFI pointer alive.
#[derive(Debug, Clone)]
enum PointerOwner {
    /// Address owned by a dynamic library or third-party resource.
    Library(Rc<RefCell<FfiLibraryState>>),
    /// Address inside a fixed buffer owned by BT.
    Buffer(Rc<RefCell<FfiBufferState>>),
}

impl Drop for LibraryQuotaGuard {
    /// releases the dynamic library logic opening quota.
    fn drop(&mut self) {
        OPEN_LIBRARIES.fetch_sub(1, Ordering::AcqRel);
    }
}

/// BT managed non-null native pointer.
#[derive(Debug)]
struct FfiPointer {
    /// Native address not exposed to scripts.
    address: NonNull<c_void>,
    /// ensures that the BT known carrier is alive and can check the owner of the closed state.
    owner: PointerOwner,
}

/// The specific resource type within a single FFI Value.
#[derive(Debug, Clone)]
enum BtFfiKind {
    /// Global read-only `ffi` static object.
    Static,
    /// The dynamic library has been loaded.
    Library(Rc<RefCell<FfiLibraryState>>),
    /// Non-null pointer returned by native function.
    Pointer(Rc<FfiPointer>),
    /// Fixed writable Buffer with stable address and can be explicitly closed.
    Buffer(Rc<RefCell<FfiBufferState>>),
}

/// Single FFI indirect value in the BT runtime.
#[derive(Debug, Clone)]
pub struct BtFfiValue {
    /// The specific type of FFI resource.
    kind: BtFfiKind,
}

impl PartialEq for BtFfiValue {
    /// Compares FFI values by static object, resource identity, or address and owner identity.
    fn eq(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (BtFfiKind::Static, BtFfiKind::Static) => true,
            (BtFfiKind::Library(left), BtFfiKind::Library(right)) => Rc::ptr_eq(left, right),
            (BtFfiKind::Pointer(left), BtFfiKind::Pointer(right)) => {
                left.address == right.address && pointer_owner_eq(&left.owner, &right.owner)
            }
            (BtFfiKind::Buffer(left), BtFfiKind::Buffer(right)) => Rc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl BtFfiValue {
    /// Creates a global read-only `ffi` static object.
    pub fn static_value() -> Self {
        Self {
            kind: BtFfiKind::Static,
        }
    }

    /// determines whether the current value is an FFI static object.
    pub fn is_static(&self) -> bool {
        matches!(self.kind, BtFfiKind::Static)
    }

    /// Determine whether the current value is a dynamic library object.
    pub fn is_library(&self) -> bool {
        matches!(self.kind, BtFfiKind::Library(_))
    }

    /// Determines whether the current value is an FFI Buffer.
    pub fn is_buffer(&self) -> bool {
        matches!(self.kind, BtFfiKind::Buffer(_))
    }

    /// Returns whether the property should be bound as a NativeMethod, performing strict schema name checking.
    pub fn has_method(&self, name: &str) -> Result<bool, String> {
        match &self.kind {
            BtFfiKind::Static => Ok(matches!(name, "load" | "close" | "buffer")),
            BtFfiKind::Library(state) => {
                validate_symbol(name)?;
                let state = state.borrow();
                if state.closed {
                    return Err("FFI dynamic library has been closed".to_string());
                }
                if let LibraryMode::StrictSchema(schema) = &state.mode {
                    if !schema.contains_key(name) {
                        return Err(format!(
                            "Strict schema does not declare the export symbol `{}`, the current mode will not fall back to declaration-free calling",
                            name
                        ));
                    }
                }
                Ok(true)
            }
            BtFfiKind::Buffer(state) => {
                if state.borrow().closed {
                    return Err("FFI Buffer has been closed".to_string());
                }
                Ok(matches!(
                    name,
                    "len" | "ptr" | "write" | "to_bytes" | "to_string" | "to_wstring"
                ))
            }
            BtFfiKind::Pointer(_) => Ok(false),
        }
    }

    /// Loads the dynamic library; when the schema is omitted, it enters limited declaration-free mode.
    pub fn load(args: Vec<Value>) -> Result<Value, String> {
        if !(1..=2).contains(&args.len()) {
            return Err("ffi.load() requires path, optional schema".to_string());
        }
        let path = match &args[0] {
            Value::Str(path) if !path.is_empty() => path,
            Value::Str(_) => {
                return Err("ffi.load() dynamic library path cannot be empty".to_string())
            }
            other => {
                return Err(format!(
                    "ffi.load() path must be String, currently {}",
                    other.type_name()
                ));
            }
        };
        if path.contains('\0') {
            return Err("ffi.load() dynamic library path cannot contain NUL".to_string());
        }
        let mode = match args.get(1) {
            Some(Value::Object(schema)) => {
                LibraryMode::StrictSchema(parse_schema(&schema.borrow())?)
            }
            Some(other) => {
                return Err(format!(
                    "ffi.load() schema must be Object, currently it is {}",
                    other.type_name()
                ));
            }
            None => LibraryMode::LimitedImplicit,
        };

        let quota = LibraryQuotaGuard::reserve()?;
        // SAFETY: Permissions and Web context are checked by the VM before entering this function; `path` is an explicit user input without NUL, and the
        // life cycle covers this system load call. Dynamic library initialization code can still perform arbitrary native behavior, which is an explicit boundary of FFI.
        let library = unsafe { Library::new(path.as_str()) }.map_err(|error| {
            format!(
                "Failed to load dynamic library `{}` ({}/{}): {}",
                path,
                std::env::consts::OS,
                std::env::consts::ARCH,
                error
            )
        })?;
        let state = FfiLibraryState {
            mode,
            functions: HashMap::new(),
            loaded: Some(LoadedLibrary { library, quota }),
            closed: false,
        };
        Ok(Value::Ffi(Self {
            kind: BtFfiKind::Library(Rc::new(RefCell::new(state))),
        }))
    }

    /// Close the FFI resource created by BT itself.
    pub fn close(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 1 {
            return Err("ffi.close() requires a FfiLibrary or FfiBuffer argument".to_string());
        }
        let Value::Ffi(value) = &args[0] else {
            return Err(format!(
                "ffi.close() only supports FfiLibrary or FfiBuffer, currently {}",
                args[0].type_name()
            ));
        };
        let closed = match &value.kind {
            BtFfiKind::Library(state) => close_library(state)?,
            BtFfiKind::Buffer(state) => close_buffer(state),
            _ => {
                return Err(format!(
                    "ffi.close() only supports FfiLibrary or FfiBuffer, currently {}",
                    value.type_name()
                ));
            }
        };
        Ok(Value::Bool(closed))
    }

    /// Creates a fixed-length, zero-padded, and at least 16-byte aligned FFI Buffer.
    pub fn buffer(args: Vec<Value>) -> Result<Value, String> {
        if args.len() != 1 {
            return Err("ffi.buffer() requires a size parameter".to_string());
        }
        let Value::Int(size) = args[0] else {
            return Err(format!(
                "ffi.buffer() size must be Int, currently {}",
                args[0].type_name()
            ));
        };
        let size = usize::try_from(size)
            .ok()
            .filter(|size| *size > 0)
            .ok_or_else(|| "ffi.buffer() size must be greater than 0".to_string())?;
        let limit = super::bytes::limit()?;
        if size > limit {
            return Err(format!(
                "ffi.buffer() size {} exceeds BT_BYTES_LIMIT {}",
                size, limit
            ));
        }
        let block_count = size
            .checked_add(15)
            .ok_or_else(|| "ffi.buffer() size calculation overflow".to_string())?
            / 16;
        let allocated = block_count
            .checked_mul(16)
            .ok_or_else(|| "ffi.buffer() actual allocated length overflows".to_string())?;
        let quota = BufferQuotaGuard::reserve(allocated)?;
        let blocks = vec![AlignedBlock::default(); block_count].into_boxed_slice();
        Ok(Value::Ffi(Self {
            kind: BtFfiKind::Buffer(Rc::new(RefCell::new(FfiBufferState {
                blocks: Some(blocks),
                len: size,
                quota: Some(quota),
                closed: false,
            }))),
        }))
    }

    /// and calls the exact exported symbol in the dynamic library.
    pub fn call(&self, symbol: &str, args: Vec<Value>) -> Result<Value, String> {
        match &self.kind {
            BtFfiKind::Library(owner) => {
                let (function, needs_cache) = function_for_call(owner, symbol, &args)?;
                let result = invoke_function(owner, &function, &args)?;
                if needs_cache {
                    owner
                        .borrow_mut()
                        .functions
                        .insert(function.symbol.clone(), function);
                }
                Ok(result)
            }
            BtFfiKind::Buffer(state) => call_buffer_method(state, symbol, &args),
            _ => Err(format!(
                "{} does not support calling `{}`",
                self.type_name(),
                symbol
            )),
        }
    }

    /// returns plain implicit stringified text.
    pub fn to_string(&self) -> &'static str {
        match self.kind {
            BtFfiKind::Static => "ffi",
            BtFfiKind::Library(_) => "ffi_library",
            BtFfiKind::Pointer(_) => "ffi_pointer",
            BtFfiKind::Buffer(_) => "ffi_buffer",
        }
    }

    /// returns the exact type name visible to the script.
    pub fn type_name(&self) -> &'static str {
        match self.kind {
            BtFfiKind::Static => "Ffi",
            BtFfiKind::Library(_) => "FfiLibrary",
            BtFfiKind::Pointer(_) => "FfiPointer",
            BtFfiKind::Buffer(_) => "FfiBuffer",
        }
    }
}

/// Parses strict schema objects and rejects all invalid entries before system loading.
fn parse_schema(
    values: &IndexMap<String, Value>,
) -> Result<HashMap<Box<str>, FunctionSpec>, String> {
    if values.len() > MAX_SCHEMA_FUNCTIONS {
        return Err(format!(
            "ffi.load() schema exceeds the limit of {} entries",
            MAX_SCHEMA_FUNCTIONS
        ));
    }
    let mut schema = HashMap::with_capacity(values.len());
    for (symbol, value) in values {
        validate_symbol(symbol)?;
        let Value::Str(signature) = value else {
            return Err(format!(
                "The signature of `{}` in FFI schema must be String, currently it is {}",
                symbol,
                value.type_name()
            ));
        };
        let spec = parse_schema_value(symbol, signature)?;
        schema.insert(symbol.as_str().into(), spec);
    }
    Ok(schema)
}

/// Verify the accurate export symbol name.
fn validate_symbol(symbol: &str) -> Result<(), String> {
    if symbol.is_empty() {
        return Err("FFI exported symbol name cannot be empty".to_string());
    }
    if symbol.contains('\0') {
        return Err("FFI exported symbol name cannot contain NUL".to_string());
    }
    if symbol.len() > MAX_SYMBOL_BYTES {
        return Err(format!(
            "FFI exported symbol name `{}` exceeds {} UTF-8 bytes",
            symbol, MAX_SYMBOL_BYTES
        ));
    }
    Ok(())
}

/// parsing schema the full signature in or just return a type hint.
fn parse_schema_value(symbol: &str, value: &str) -> Result<FunctionSpec, String> {
    if value.len() > MAX_SIGNATURE_BYTES {
        return Err(format!(
            "The signature for FFI symbol `{}` exceeds {} UTF-8 bytes",
            symbol, MAX_SIGNATURE_BYTES
        ));
    }
    if value
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() && byte != b' ')
    {
        return Err(format!(
            "The signature for FFI symbol `{}` is only allowed to use ASCII whitespace",
            symbol
        ));
    }
    let compact = value.replace(' ', "");
    if !compact.contains(['(', ')']) {
        return Ok(FunctionSpec {
            params: None,
            result: parse_type(symbol, &compact, true)?,
        });
    }
    parse_signature(symbol, value)
}

/// Parsing an explicit full signature is allowed.
fn parse_signature(symbol: &str, signature: &str) -> Result<FunctionSpec, String> {
    if signature.len() > MAX_SIGNATURE_BYTES {
        return Err(format!(
            "The signature for FFI symbol `{}` exceeds {} UTF-8 bytes",
            symbol, MAX_SIGNATURE_BYTES
        ));
    }
    if signature
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() && byte != b' ')
    {
        return Err(format!(
            "The signature for FFI symbol `{}` is only allowed to use ASCII whitespace",
            symbol
        ));
    }
    let compact = signature.replace(' ', "");
    let Some(open) = compact.find('(') else {
        return Err(format!(
            "FFI symbol `{}` currently only supports full signatures, such as i32(i32)",
            symbol
        ));
    };
    if !compact.ends_with(')')
        || compact[open + 1..compact.len() - 1].contains(['(', ')'])
        || compact[..open].contains(')')
    {
        return Err(format!(
            "The full signature syntax of FFI symbol `{}` is invalid",
            symbol
        ));
    }
    let result = parse_type(symbol, &compact[..open], true)?;
    let parameter_text = &compact[open + 1..compact.len() - 1];
    let params = if parameter_text.is_empty() {
        Vec::new()
    } else {
        let mut params =
            Vec::with_capacity(parameter_text.bytes().filter(|byte| *byte == b',').count() + 1);
        for parameter in parameter_text.split(',') {
            if parameter.is_empty() {
                return Err(format!(
                    "The parameter type of FFI symbol `{}` cannot be empty",
                    symbol
                ));
            }
            let kind = parse_type(symbol, parameter, false)?;
            if kind == FfiType::Void {
                return Err(format!(
                    "The parameters of FFI symbol `{}` cannot be declared as void",
                    symbol
                ));
            }
            params.push(kind);
        }
        params
    };
    if params.len() > MAX_FUNCTION_ARGS {
        return Err(format!(
            "The number of parameters of FFI symbol `{}` exceeds the fixed upper limit {}",
            symbol, MAX_FUNCTION_ARGS
        ));
    }
    Ok(FunctionSpec {
        params: Some(params.into_boxed_slice()),
        result,
    })
}

/// resolves a single type name from the explicit full signature whitelist.
fn parse_type(symbol: &str, name: &str, is_result: bool) -> Result<FfiType, String> {
    let kind = match name {
        "void" => FfiType::Void,
        "i8" => FfiType::I8,
        "i16" => FfiType::I16,
        "i32" => FfiType::I32,
        "i64" => FfiType::I64,
        "u8" => FfiType::U8,
        "u16" => FfiType::U16,
        "u32" => FfiType::U32,
        "u64" => FfiType::U64,
        "isize" => FfiType::ISize,
        "usize" => FfiType::USize,
        "f32" => FfiType::F32,
        "f64" => FfiType::F64,
        "ptr" => FfiType::Ptr,
        "cstr" => FfiType::CStr,
        "wstr" if cfg!(windows) => FfiType::WStr,
        "wstr" => {
            return Err(format!(
                "FFI symbol `{}`'s wstr only supports Windows targets",
                symbol
            ));
        }
        _ => {
            let position = if is_result { "return" } else { "parameter" };
            return Err(format!(
                "FFI symbol `{}` {} type `{}` is not supported",
                symbol, position, name
            ));
        }
    };
    Ok(kind)
}

/// Infers the first call parameters according to the limited declaration-free rules, and returns the actual ABI type and lock kind.
fn infer_arguments(symbol: &str, args: &[Value]) -> Result<InferredParams, String> {
    if args.len() > MAX_FUNCTION_ARGS {
        return Err(format!(
            "The number of parameters of FFI symbol `{}` exceeds the fixed upper limit {}",
            symbol, MAX_FUNCTION_ARGS
        ));
    }
    let mut params = Vec::with_capacity(args.len());
    let mut inferred = Vec::with_capacity(args.len());
    for (index, value) in args.iter().enumerate() {
        let (kind, ffi_type) = match value {
            Value::Int(value) if i32::try_from(*value).is_ok() => (InferredKind::I32, FfiType::I32),
            Value::Int(_) => {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "Int is outside the i32 range allowed for signature inference",
                ));
            }
            Value::Null => (InferredKind::Ptr, FfiType::Ptr),
            Value::Ffi(value)
                if matches!(value.kind, BtFfiKind::Pointer(_) | BtFfiKind::Buffer(_)) =>
            {
                (InferredKind::Ptr, FfiType::Ptr)
            }
            Value::Str(_) if cfg!(windows) && symbol.ends_with('W') => {
                (InferredKind::WStr, FfiType::WStr)
            }
            Value::Str(_) if cfg!(windows) && symbol.ends_with('A') => {
                (InferredKind::AnsiAscii, FfiType::CStr)
            }
            Value::Str(_) => {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "string encoding cannot be inferred from the export name; on Windows, only uppercase W/A suffixes provide an encoding hint",
                ));
            }
            Value::Bool(_) => {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "Bool may map to _Bool, BOOL, or an integer of another width",
                ));
            }
            Value::Float(_) => {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "Float does not distinguish C float from double",
                ));
            }
            _ => {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "the current BT type has no single safe C ABI mapping",
                ));
            }
        };
        params.push(ffi_type);
        inferred.push(kind);
    }
    Ok((params.into_boxed_slice(), inferred.into_boxed_slice()))
}

/// Verifies that subsequent calls still comply with the first call locked parameter kind and encoding boundaries.
#[cfg(test)]
fn validate_inferred_arguments(
    symbol: &str,
    inferred: &[InferredKind],
    args: &[Value],
) -> Result<(), String> {
    for (index, (kind, value)) in inferred.iter().zip(args).enumerate() {
        match kind {
            InferredKind::I32 => match value {
                Value::Int(value) if i32::try_from(*value).is_ok() => {}
                Value::Int(_) => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "The parameter is locked as i32, but the current Int is out of i32 range",
                    ));
                }
                _ => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "the parameter was locked as i32 by the first call",
                    ));
                }
            },
            InferredKind::Ptr => match value {
                Value::Null => {}
                Value::Ffi(ffi_value)
                    if matches!(ffi_value.kind, BtFfiKind::Pointer(_) | BtFfiKind::Buffer(_)) =>
                {
                    pointer_argument(symbol, index, value)?;
                }
                _ => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "the parameter was locked as ptr by the first call; only null, FfiPointer, or FfiBuffer is allowed",
                    ));
                }
            },
            InferredKind::WStr => match value {
                Value::Null => {}
                Value::Str(text) => {
                    if text.contains('\0') {
                        return Err(inference_error(
                            symbol,
                            index,
                            value,
                            "wstr string cannot contain internal NUL",
                        ));
                    }
                    let bytes = text
                        .encode_utf16()
                        .count()
                        .checked_mul(std::mem::size_of::<u16>())
                        .ok_or_else(|| {
                            inference_error(symbol, index, value, "wstr encoding length overflow")
                        })?;
                    ensure_string_argument_limit(symbol, index, "wstr", bytes)?;
                }
                _ => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "the parameter was locked as wstr by the first call; only String or null is allowed",
                    ));
                }
            },
            InferredKind::AnsiAscii => match value {
                Value::Null => {}
                Value::Str(text) if text.is_ascii() && !text.contains('\0') => {
                    ensure_string_argument_limit(symbol, index, "A ASCII", text.len())?;
                }
                Value::Str(text) if text.contains('\0') => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "A string cannot contain internal NUL",
                    ));
                }
                Value::Str(_) => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "Windows A export only allows ASCII; please use W export and wstr in preference",
                    ));
                }
                _ => {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "the parameter was locked as ansi_ascii by the first call; only an ASCII String or null is allowed",
                    ));
                }
            },
        }
    }
    Ok(())
}

/// Constructs an inference error containing symbol, ordinal number, actual type, reason, and full signature hint.
fn inference_error(symbol: &str, index: usize, actual: &Value, reason: &str) -> String {
    format!(
        "FFI symbol `{}` parameter {} ({}) cannot be inferred safely: {}; provide a full signature matching the native prototype, such as i32(i32)",
        symbol,
        index + 1,
        actual.type_name(),
        reason
    )
}

/// Find or create the function cache entry for the first time, and ensure that failure is not written to the cache.
fn function_for_call(
    owner: &Rc<RefCell<FfiLibraryState>>,
    symbol: &str,
    args: &[Value],
) -> Result<(Rc<FfiFunction>, bool), String> {
    validate_symbol(symbol)?;
    let state = owner.borrow();
    if state.closed {
        return Err("FFI dynamic library has been closed".to_string());
    }
    if let Some(function) = state.functions.get(symbol) {
        if args.len() != function.params.len() {
            return Err(format!(
                "FFI symbol `{}` expects {} arguments, but received {}",
                symbol,
                function.params.len(),
                args.len()
            ));
        }
        return Ok((function.clone(), false));
    }
    let (params, result, inferred) = match &state.mode {
        LibraryMode::StrictSchema(schema) => {
            let spec = schema.get(symbol).ok_or_else(|| {
                format!(
                    "Strict schema does not declare the export symbol `{}`, the current mode will not fall back to declaration-free calling",
                    symbol
                )
            })?;
            match &spec.params {
                Some(params) => {
                    if args.len() != params.len() {
                        return Err(format!(
                            "FFI symbol `{}` expects {} arguments, but received {}",
                            symbol,
                            params.len(),
                            args.len()
                        ));
                    }
                    (params.clone(), spec.result, None)
                }
                None => {
                    let (params, inferred) = infer_arguments(symbol, args)?;
                    (params, spec.result, Some(inferred))
                }
            }
        }
        LibraryMode::LimitedImplicit => {
            let (params, inferred) = infer_arguments(symbol, args)?;
            (params, FfiType::I32, Some(inferred))
        }
    };
    if state.functions.len() >= MAX_CACHED_FUNCTIONS {
        return Err(format!(
            "FFI function cache for one dynamic library exceeds the limit of {} entries",
            MAX_CACHED_FUNCTIONS
        ));
    }
    let loaded = state
        .loaded
        .as_ref()
        .ok_or_else(|| "FFI dynamic library has been closed".to_string())?;
    // SAFETY: `symbol` is non-empty, contains no NUL bytes, and has a bounded length. The
    // `Library` stored in `state` outlives the generated address and `Cif`. The system symbol
    // table cannot prove that the target is a function, so the caller's complete schema must match
    // the native prototype.
    let code = {
        let loaded_symbol = unsafe {
            loaded
                .library
                .get::<unsafe extern "C" fn()>(symbol.as_bytes())
        }
        .map_err(|error| {
            format!(
                "The export symbol `{}` cannot be found in the dynamic library: {}",
                symbol, error
            )
        })?;
        CodePtr(*loaded_symbol as *mut c_void)
    };
    let function = Rc::new(FfiFunction::new(symbol, params, result, inferred, code));
    Ok((function, true))
}

/// Explicitly close the dynamic library, first invalidate all script side handles, and then request the system to uninstall.
fn close_library(state: &Rc<RefCell<FfiLibraryState>>) -> Result<bool, String> {
    let loaded = {
        let mut state = state.borrow_mut();
        if state.closed {
            return Ok(false);
        }
        state.closed = true;
        if let LibraryMode::StrictSchema(schema) = &mut state.mode {
            schema.clear();
        }
        state.functions.clear();
        state.loaded.take()
    };
    if let Some(loaded) = loaded {
        loaded.close()?;
    }
    Ok(true)
}

/// Determines whether two Pointer owners point to the same BT managed resource.
fn pointer_owner_eq(left: &PointerOwner, right: &PointerOwner) -> bool {
    match (left, right) {
        (PointerOwner::Library(left), PointerOwner::Library(right)) => Rc::ptr_eq(left, right),
        (PointerOwner::Buffer(left), PointerOwner::Buffer(right)) => Rc::ptr_eq(left, right),
        _ => false,
    }
}

/// determines whether the Pointer owner has been logically closed.
fn pointer_owner_closed(owner: &PointerOwner) -> bool {
    match owner {
        PointerOwner::Library(state) => state.borrow().closed,
        PointerOwner::Buffer(state) => state.borrow().closed,
    }
}

/// Explicitly close the Buffer, first mark it as invalid, and then release the memory and atomic quota.
fn close_buffer(state: &Rc<RefCell<FfiBufferState>>) -> bool {
    let mut state = state.borrow_mut();
    if state.closed {
        return false;
    }
    state.closed = true;
    state.blocks.take();
    state.quota.take();
    true
}

/// calls a fixed set of methods of FfiBuffer.
fn call_buffer_method(
    owner: &Rc<RefCell<FfiBufferState>>,
    method: &str,
    args: &[Value],
) -> Result<Value, String> {
    if owner.borrow().closed {
        return Err("FFI Buffer has been closed".to_string());
    }
    match method {
        "len" => {
            ensure_arg_count(method, args, 0, 0)?;
            Ok(Value::Int(owner.borrow().len as i64))
        }
        "ptr" => {
            ensure_arg_count(method, args, 0, 1)?;
            let offset = optional_usize_arg(method, args.first(), 0)?;
            let state = owner.borrow();
            if offset > state.len {
                return Err(format!(
                    "FfiBuffer.ptr() offset {} is outside the valid range 0..={}",
                    offset, state.len
                ));
            }
            let base = buffer_base(&state)?;
            // SAFETY: Buffer allocates at least one 16-byte block; offset has been verified not to exceed the script-visible length, allowing
            // to construct a one-past-end address. The owner will be saved with the Pointer, and will be rejected again after it is closed and before being called.
            let address = unsafe { NonNull::new_unchecked(base.add(offset).cast()) };
            Ok(Value::Ffi(BtFfiValue {
                kind: BtFfiKind::Pointer(Rc::new(FfiPointer {
                    address,
                    owner: PointerOwner::Buffer(owner.clone()),
                })),
            }))
        }
        "write" => {
            ensure_arg_count(method, args, 1, 2)?;
            let Value::Bytes(data) = &args[0] else {
                return Err(format!(
                    "FfiBuffer.write() data must be Bytes, currently {}",
                    args[0].type_name()
                ));
            };
            let offset = optional_usize_arg(method, args.get(1), 0)?;
            let end = offset.checked_add(data.len()).ok_or_else(|| {
                "FfiBuffer.write() offset and length calculation overflow".to_string()
            })?;
            let mut state = owner.borrow_mut();
            if end > state.len {
                return Err(format!(
                    "FfiBuffer.write() range {}..{} exceeds the Buffer length {}",
                    offset, end, state.len
                ));
            }
            let target = buffer_bytes_mut(&mut state)?;
            target[offset..end].copy_from_slice(data.as_slice());
            Ok(Value::Int(data.len() as i64))
        }
        "to_bytes" => {
            ensure_arg_count(method, args, 0, 2)?;
            let offset = optional_usize_arg(method, args.first(), 0)?;
            let state = owner.borrow();
            if offset > state.len {
                return Err(format!(
                    "FfiBuffer.to_bytes() offset {} exceeds the Buffer length {}",
                    offset, state.len
                ));
            }
            let length = optional_usize_arg(method, args.get(1), state.len - offset)?;
            let end = offset
                .checked_add(length)
                .filter(|end| *end <= state.len)
                .ok_or_else(|| {
                    "FfiBuffer.to_bytes() offset and length exceed the Buffer range".to_string()
                })?;
            let bytes = buffer_bytes(&state)?[offset..end].to_vec();
            Ok(Value::Bytes(BtBytes::unchecked(bytes)))
        }
        "to_string" => {
            ensure_arg_count(method, args, 0, 1)?;
            let offset = optional_usize_arg(method, args.first(), 0)?;
            let state = owner.borrow();
            let bytes = buffer_tail(&state, offset, method)?;
            let end = bytes.iter().position(|byte| *byte == 0).ok_or_else(|| {
                "FfiBuffer.to_string(): no NUL terminator was found within the visible buffer range".to_string()
            })?;
            Ok(std::str::from_utf8(&bytes[..end])
                .map(|text| Value::Str(text.to_string()))
                .unwrap_or(Value::Null))
        }
        "to_wstring" => buffer_to_wstring(owner, args),
        _ => Err(format!("FfiBuffer has no method `{}`", method)),
    }
}

/// Verify that the number of parameters of the Buffer method is within a closed range.
fn ensure_arg_count(method: &str, args: &[Value], min: usize, max: usize) -> Result<(), String> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(format!(
            "FfiBuffer.{}() expects {}..={} arguments, but received {}",
            method,
            min,
            max,
            args.len()
        ))
    }
}

/// reads the optional non-negative usize parameter.
fn optional_usize_arg(
    method: &str,
    value: Option<&Value>,
    default: usize,
) -> Result<usize, String> {
    match value {
        None => Ok(default),
        Some(Value::Int(value)) => usize::try_from(*value)
            .map_err(|_| format!("FfiBuffer.{}() parameter must be non-negative Int", method)),
        Some(other) => Err(format!(
            "FfiBuffer.{}() parameter must be Int, currently {}",
            method,
            other.type_name()
        )),
    }
}

/// Returns the stable base address of the Buffer.
fn buffer_base(state: &FfiBufferState) -> Result<*mut u8, String> {
    state
        .blocks
        .as_ref()
        .map(|blocks| blocks.as_ptr().cast::<u8>().cast_mut())
        .ok_or_else(|| "FFI Buffer has been closed".to_string())
}

/// Creates a read-only byte slice of the visible range of Buffer.
fn buffer_bytes(state: &FfiBufferState) -> Result<&[u8], String> {
    let base = buffer_base(state)?;
    // SAFETY: The actual allocated length of blocks is rounded up to 16 bytes and not less than len;
    // will not be released or moved during the Buffer borrowing period, and the returned slice only covers the visible length of the script.
    Ok(unsafe { std::slice::from_raw_parts(base.cast_const(), state.len) })
}

/// Creates a writable byte slice of the visible range of Buffer.
fn buffer_bytes_mut(state: &mut FfiBufferState) -> Result<&mut [u8], String> {
    let base = buffer_base(state)?;
    // SAFETY: Currently holding FfiBufferState exclusive borrowing; the actual length of blocks is no less than len, the address is stable and the slice only covers the
    // script visible range, so it will not overlap with other safe writable references.
    Ok(unsafe { std::slice::from_raw_parts_mut(base, state.len) })
}

/// returns the visible tail slice of the Buffer after the specified offset.
fn buffer_tail<'a>(
    state: &'a FfiBufferState,
    offset: usize,
    method: &str,
) -> Result<&'a [u8], String> {
    if offset > state.len {
        return Err(format!(
            "FfiBuffer.{}() offset {} exceeds Buffer length {}",
            method, offset, state.len
        ));
    }
    Ok(&buffer_bytes(state)?[offset..])
}

/// Bounded read of NUL-terminated UTF-16 string from Buffer on Windows.
fn buffer_to_wstring(owner: &Rc<RefCell<FfiBufferState>>, args: &[Value]) -> Result<Value, String> {
    #[cfg(not(windows))]
    {
        let _ = (owner, args);
        Err("FfiBuffer.to_wstring() only supports Windows targets".to_string())
    }
    #[cfg(windows)]
    {
        ensure_arg_count("to_wstring", args, 0, 1)?;
        let offset = optional_usize_arg("to_wstring", args.first(), 0)?;
        if offset % 2 != 0 {
            return Err("FfiBuffer.to_wstring() offset must be 2-byte aligned".to_string());
        }
        let state = owner.borrow();
        let bytes = buffer_tail(&state, offset, "to_wstring")?;
        let mut units = Vec::with_capacity(bytes.len() / 2);
        let mut found_nul = false;
        for chunk in bytes.chunks_exact(2) {
            let unit = u16::from_ne_bytes([chunk[0], chunk[1]]);
            if unit == 0 {
                found_nul = true;
                break;
            }
            units.push(unit);
        }
        if !found_nul {
            return Err(
                "FfiBuffer.to_wstring(): no NUL terminator was found within the visible buffer range"
                    .to_string(),
            );
        }
        Ok(String::from_utf16(&units)
            .map(Value::Str)
            .unwrap_or(Value::Null))
    }
}

/// FFI process-level minimum resource statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfiStats {
    /// The current build has FFI enabled.
    pub enabled: bool,
    /// The number of dynamic libraries currently opened logically.
    pub open_libraries: usize,
    /// The number of currently alive Buffers.
    pub buffers: usize,
    /// The total number of bytes actually allocated by the current Buffer.
    pub buffer_bytes: usize,
}

/// Returns the process-level minimum resource statistics snapshot of FFI.
pub fn stats() -> FfiStats {
    FfiStats {
        enabled: true,
        open_libraries: OPEN_LIBRARIES.load(Ordering::Acquire),
        buffers: BUFFERS.load(Ordering::Acquire),
        buffer_bytes: BUFFER_BYTES.load(Ordering::Acquire),
    }
}

/// At least 16-byte aligned fixed argument or return slot.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
union FfiSlot {
    /// Signed 8-bit integer slot.
    i8_value: i8,
    /// Signed 16-bit integer slot.
    i16_value: i16,
    /// Signed 32-bit integer slot.
    i32_value: i32,
    /// Signed 64-bit integer slot.
    i64_value: i64,
    /// Unsigned 8-bit integer slot.
    u8_value: u8,
    /// Unsigned 16-bit integer slot.
    u16_value: u16,
    /// Unsigned 32-bit integer slot.
    u32_value: u32,
    /// Unsigned 64-bit integer slot.
    u64_value: u64,
    /// Pointer width signed integer slot.
    isize_value: isize,
    /// Pointer width unsigned integer slot.
    usize_value: usize,
    /// 32-bit floating point slot.
    f32_value: f32,
    /// 64-bit floating point slot.
    f64_value: f64,
    /// Native pointer slot.
    pointer: *mut c_void,
    /// Zeroes out and fixes the raw bytes used by the slot size.
    bytes: [u8; 16],
}

impl FfiSlot {
    /// creates an all-zero call slot.
    const fn zeroed() -> Self {
        Self { bytes: [0; 16] }
    }
}

/// Temporary string encoding buffer held during a single call.
enum TempString {
    /// UTF-8 NUL terminating byte.
    Utf8(Vec<u8>),
    /// Windows UTF-16 NUL terminated unit.
    #[cfg(windows)]
    Wide(Vec<u16>),
}

impl TempString {
    /// returns the first address of the buffer; the owner itself must live until the native call returns.
    fn pointer(&self) -> *mut c_void {
        match self {
            Self::Utf8(value) => value.as_ptr().cast_mut().cast(),
            #[cfg(windows)]
            Self::Wide(value) => value.as_ptr().cast_mut().cast(),
        }
    }

    /// determines whether the native return address falls within the temporary string buffer of this call.
    fn contains_address(&self, address: NonNull<c_void>) -> bool {
        let (start, length) = match self {
            Self::Utf8(value) => (value.as_ptr() as usize, value.len()),
            #[cfg(windows)]
            Self::Wide(value) => (
                value.as_ptr() as usize,
                value.len().saturating_mul(std::mem::size_of::<u16>()),
            ),
        };
        let target = address.as_ptr() as usize;
        target >= start && target < start.saturating_add(length)
    }
}

/// uses a fixed stack call frame to marshal parameters, perform native calls, and convert return values.
fn invoke_function(
    owner: &Rc<RefCell<FfiLibraryState>>,
    function: &FfiFunction,
    values: &[Value],
) -> Result<Value, String> {
    if owner.borrow().closed {
        return Err("FFI dynamic library has been closed".to_string());
    }
    if values.len() != function.params.len() {
        return Err(format!(
            "FFI symbol `{}` expects {} arguments, but received {}",
            function.symbol,
            function.params.len(),
            values.len()
        ));
    }
    let mut slots = [FfiSlot::zeroed(); MAX_FUNCTION_ARGS];
    let mut strings: [Option<TempString>; MAX_FUNCTION_ARGS] = array::from_fn(|_| None);
    for (index, (kind, value)) in function.params.iter().zip(values).enumerate() {
        match &function.inferred {
            Some(inferred) => marshal_inferred_argument(
                &function.symbol,
                index,
                inferred[index],
                value,
                &mut slots[index],
                &mut strings[index],
            )?,
            None => marshal_argument(
                &function.symbol,
                index,
                *kind,
                value,
                &mut slots[index],
                &mut strings[index],
            )?,
        }
    }
    let ffi_args: [Arg<'_>; MAX_FUNCTION_ARGS] = array::from_fn(|index| Arg::new(&slots[index]));
    let mut result = FfiSlot::zeroed();
    let return_target = if function.result == FfiType::Void {
        Ret::void()
    } else {
        Ret::new(&mut result)
    };
    // SAFETY: The Cif of `function` is constructed together with the fixed whitelist signature; the number of parameters is accurately verified again above,
    // Each Arg points to a 16-byte aligned slot in this stack frame and is not moved during the call, the string owner lives until the end of the call; the
    // return slot is also 16-byte aligned and writable. The user is responsible for ensuring the true native prototype, function address and third-party behavior.
    unsafe {
        function.cif.call_return_into(
            function.code,
            &ffi_args[..function.params.len()],
            return_target,
        );
    }
    convert_result(
        owner,
        &function.symbol,
        function.result,
        result,
        &strings,
        values,
    )
}

/// Completes single verification and marshaling according to the first call lock type to avoid repeated range checks and owner queries on the hot path.
fn marshal_inferred_argument(
    symbol: &str,
    index: usize,
    kind: InferredKind,
    value: &Value,
    slot: &mut FfiSlot,
    string_owner: &mut Option<TempString>,
) -> Result<(), String> {
    match kind {
        InferredKind::I32 => match value {
            Value::Int(raw) => {
                slot.i32_value = i32::try_from(*raw).map_err(|_| {
                    inference_error(
                        symbol,
                        index,
                        value,
                        "The parameter is locked as i32, but the current Int is out of i32 range",
                    )
                })?;
                Ok(())
            }
            _ => Err(inference_error(
                symbol,
                index,
                value,
                "the parameter was locked as i32 by the first call",
            )),
        },
        InferredKind::Ptr => match value {
            Value::Null => {
                slot.pointer = std::ptr::null_mut();
                Ok(())
            }
            Value::Ffi(ffi_value)
                if matches!(ffi_value.kind, BtFfiKind::Pointer(_) | BtFfiKind::Buffer(_)) =>
            {
                slot.pointer = pointer_argument(symbol, index, value)?;
                Ok(())
            }
            _ => Err(inference_error(
                symbol,
                index,
                value,
                "the parameter was locked as ptr by the first call; only null, FfiPointer, or FfiBuffer is allowed",
            )),
        },
        InferredKind::WStr => {
            if !matches!(value, Value::Str(_) | Value::Null) {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "the parameter was locked as wstr by the first call; only String or null is allowed",
                ));
            }
            slot.pointer = string_argument(symbol, index, value, true, string_owner)?;
            Ok(())
        }
        InferredKind::AnsiAscii => {
            if let Value::Str(text) = value {
                if !text.is_ascii() {
                    return Err(inference_error(
                        symbol,
                        index,
                        value,
                        "Windows A export only allows ASCII; please use W export and wstr in preference",
                    ));
                }
            } else if !matches!(value, Value::Null) {
                return Err(inference_error(
                    symbol,
                    index,
                    value,
                    "the parameter was locked as ansi_ascii by the first call; only an ASCII String or null is allowed",
                ));
            }
            slot.pointer = string_argument(symbol, index, value, false, string_owner)?;
            Ok(())
        }
    }
}

/// Writes a single BT argument into a fixed call slot.
fn marshal_argument(
    symbol: &str,
    index: usize,
    kind: FfiType,
    value: &Value,
    slot: &mut FfiSlot,
    string_owner: &mut Option<TempString>,
) -> Result<(), String> {
    match kind {
        FfiType::Void => Err(format!(
            "The parameters of FFI symbol `{}` cannot be declared as void",
            symbol
        )),
        FfiType::I8 => {
            slot.i8_value = signed_argument::<i8>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::I16 => {
            slot.i16_value = signed_argument::<i16>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::I32 => {
            slot.i32_value = signed_argument::<i32>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::I64 => {
            slot.i64_value = signed_argument::<i64>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::U8 => {
            slot.u8_value = unsigned_argument::<u8>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::U16 => {
            slot.u16_value = unsigned_argument::<u16>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::U32 => {
            slot.u32_value = unsigned_argument::<u32>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::U64 => {
            slot.u64_value = unsigned_argument::<u64>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::ISize => {
            slot.isize_value = signed_argument::<isize>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::USize => {
            slot.usize_value = unsigned_argument::<usize>(symbol, index, kind, value)?;
            Ok(())
        }
        FfiType::F32 => {
            slot.f32_value = match value {
                Value::Int(value) => *value as f32,
                Value::Float(value) => *value as f32,
                other => return Err(argument_type_error(symbol, index, kind, other)),
            };
            Ok(())
        }
        FfiType::F64 => {
            slot.f64_value = match value {
                Value::Int(value) => *value as f64,
                Value::Float(value) => *value,
                other => return Err(argument_type_error(symbol, index, kind, other)),
            };
            Ok(())
        }
        FfiType::Ptr => {
            slot.pointer = pointer_argument(symbol, index, value)?;
            Ok(())
        }
        FfiType::CStr => {
            slot.pointer = string_argument(symbol, index, value, false, string_owner)?;
            Ok(())
        }
        FfiType::WStr => {
            slot.pointer = string_argument(symbol, index, value, true, string_owner)?;
            Ok(())
        }
    }
}

/// Converts a BT Int or Bool to the exact declared signed integer width.
fn signed_argument<T>(symbol: &str, index: usize, kind: FfiType, value: &Value) -> Result<T, String>
where
    T: TryFrom<i64>,
{
    let raw = match value {
        Value::Int(value) => *value,
        Value::Bool(value) => i64::from(*value),
        other => return Err(argument_type_error(symbol, index, kind, other)),
    };
    T::try_from(raw).map_err(|_| {
        format!(
            "FFI symbol `{}` argument {} is outside the {} range",
            symbol,
            index + 1,
            kind.name()
        )
    })
}

/// Converts a BT Int or Bool to the exact declared unsigned integer width.
fn unsigned_argument<T>(
    symbol: &str,
    index: usize,
    kind: FfiType,
    value: &Value,
) -> Result<T, String>
where
    T: TryFrom<i64>,
{
    let raw = match value {
        Value::Int(value) => *value,
        Value::Bool(value) => i64::from(*value),
        other => return Err(argument_type_error(symbol, index, kind, other)),
    };
    T::try_from(raw).map_err(|_| {
        format!(
            "FFI symbol `{}` argument {} is outside the {} range",
            symbol,
            index + 1,
            kind.name()
        )
    })
}

/// reads the normal pointer argument and checks that owner is still valid.
fn pointer_argument(symbol: &str, index: usize, value: &Value) -> Result<*mut c_void, String> {
    match value {
        Value::Null => Ok(std::ptr::null_mut()),
        Value::Ffi(ffi_value) => match &ffi_value.kind {
            BtFfiKind::Pointer(pointer) => {
                if pointer_owner_closed(&pointer.owner) {
                    return Err(format!(
                        "FFI symbol `{}` parameter {} refers to an invalid FfiPointer",
                        symbol,
                        index + 1
                    ));
                }
                Ok(pointer.address.as_ptr())
            }
            BtFfiKind::Buffer(state) => {
                let state = state.borrow();
                Ok(buffer_base(&state)?.cast())
            }
            _ => Err(argument_type_error(symbol, index, FfiType::Ptr, value)),
        },
        other => Err(argument_type_error(symbol, index, FfiType::Ptr, other)),
    }
}

/// encodes the cstr or Windows wstr input parameter and saves the owner in the call frame.
fn string_argument(
    symbol: &str,
    index: usize,
    value: &Value,
    wide: bool,
    owner: &mut Option<TempString>,
) -> Result<*mut c_void, String> {
    if matches!(value, Value::Null) {
        return Ok(std::ptr::null_mut());
    }
    let Value::Str(value) = value else {
        return Err(argument_type_error(
            symbol,
            index,
            if wide { FfiType::WStr } else { FfiType::CStr },
            value,
        ));
    };
    if wide {
        #[cfg(not(windows))]
        return Err("wstr only supports Windows targets".to_string());
        #[cfg(windows)]
        {
            let limit = super::bytes::limit()?;
            let unit_limit = limit / std::mem::size_of::<u16>();
            let mut encoded = Vec::with_capacity(value.len().min(unit_limit).saturating_add(1));
            for unit in value.encode_utf16() {
                if unit == 0 {
                    return Err(format!(
                        "FFI symbol `{}` wstr parameter {} cannot contain an internal NUL",
                        symbol,
                        index + 1
                    ));
                }
                if encoded.len() >= unit_limit {
                    return Err(format!(
                        "FFI symbol `{}` parameter {} (wstr) exceeds BT_BYTES_LIMIT {} after encoding",
                        symbol,
                        index + 1,
                        limit
                    ));
                }
                encoded.push(unit);
            }
            encoded.push(0);
            *owner = Some(TempString::Wide(encoded));
        }
    } else {
        if value.as_bytes().contains(&0) {
            return Err(format!(
                "FFI symbol `{}` cstr parameter {} cannot contain an internal NUL",
                symbol,
                index + 1
            ));
        }
        ensure_string_argument_limit(symbol, index, "cstr", value.len())?;
        let mut encoded = Vec::with_capacity(value.len() + 1);
        encoded.extend_from_slice(value.as_bytes());
        encoded.push(0);
        *owner = Some(TempString::Utf8(encoded));
    }
    owner.as_ref().map(TempString::pointer).ok_or_else(|| {
        "FFI string parameter encoding does not generate a temporary owner".to_string()
    })
}

/// Verifies that the encoding length of a single temporary string does not exceed the global Bytes upper limit.
fn ensure_string_argument_limit(
    symbol: &str,
    index: usize,
    kind: &str,
    bytes: usize,
) -> Result<(), String> {
    let limit = super::bytes::limit()?;
    if bytes > limit {
        return Err(format!(
            "FFI symbol `{}` parameter {} ({}) encodes to {} bytes, exceeding BT_BYTES_LIMIT {}",
            symbol,
            index + 1,
            kind,
            bytes,
            limit
        ));
    }
    Ok(())
}

/// Convert the fixed return slot to a BT value.
fn convert_result(
    owner: &Rc<RefCell<FfiLibraryState>>,
    symbol: &str,
    kind: FfiType,
    result: FfiSlot,
    strings: &[Option<TempString>; MAX_FUNCTION_ARGS],
    values: &[Value],
) -> Result<Value, String> {
    // SAFETY: The return slot is only read according to the same whitelist type declared when constructing Cif; libffi has written the corresponding width of
    // before the synchronous call returns. Slots are 16-byte aligned and contain no Rust values that require destruction.
    unsafe {
        match kind {
            FfiType::Void => Ok(Value::Empty),
            FfiType::I8 => Ok(Value::Int(result.i8_value as i64)),
            FfiType::I16 => Ok(Value::Int(result.i16_value as i64)),
            FfiType::I32 => Ok(Value::Int(result.i32_value as i64)),
            FfiType::I64 => Ok(Value::Int(result.i64_value)),
            FfiType::U8 => Ok(Value::Int(result.u8_value as i64)),
            FfiType::U16 => Ok(Value::Int(result.u16_value as i64)),
            FfiType::U32 => Ok(Value::Int(result.u32_value as i64)),
            FfiType::U64 => i64::try_from(result.u64_value)
                .map(Value::Int)
                .map_err(|_| {
                    format!(
                        "u64 return value of FFI symbol `{}` cannot be represented as a BT Int",
                        symbol
                    )
                }),
            FfiType::ISize => Ok(Value::Int(result.isize_value as i64)),
            FfiType::USize => i64::try_from(result.usize_value)
                .map(Value::Int)
                .map_err(|_| {
                    format!(
                        "usize return value of FFI symbol `{}` cannot be represented as a BT Int",
                        symbol
                    )
                }),
            FfiType::F32 => Ok(Value::Float(result.f32_value as f64)),
            FfiType::F64 => Ok(Value::Float(result.f64_value)),
            FfiType::Ptr => match NonNull::new(result.pointer) {
                Some(address) => {
                    if let Some(pointer_owner) = returned_pointer_owner(address, values)? {
                        return Ok(Value::Ffi(BtFfiValue {
                            kind: BtFfiKind::Pointer(Rc::new(FfiPointer {
                                address,
                                owner: pointer_owner,
                            })),
                        }));
                    }
                    if strings
                        .iter()
                        .flatten()
                        .any(|string| string.contains_address(address))
                    {
                        return Err(format!(
                            "ptr return value of FFI symbol `{}` points into a temporary string owned by this call; the address is invalid once the call completes",
                            symbol
                        ));
                    }
                    Ok(Value::Ffi(BtFfiValue {
                        kind: BtFfiKind::Pointer(Rc::new(FfiPointer {
                            address,
                            owner: PointerOwner::Library(owner.clone()),
                        })),
                    }))
                }
                None => Ok(Value::Null),
            },
            FfiType::CStr => match NonNull::new(result.pointer.cast::<u8>()) {
                Some(address) => copy_cstr_result(symbol, address),
                None => Ok(Value::Null),
            },
            FfiType::WStr => match NonNull::new(result.pointer.cast::<u16>()) {
                Some(address) => copy_wstr_result(symbol, address),
                None => Ok(Value::Null),
            },
        }
    }
}

/// Select ptr return value owner according to the priority of Buffer range and Pointer precise address.
fn returned_pointer_owner(
    address: NonNull<c_void>,
    values: &[Value],
) -> Result<Option<PointerOwner>, String> {
    let target = address.as_ptr() as usize;
    for value in values {
        let Value::Ffi(value) = value else {
            continue;
        };
        if let BtFfiKind::Buffer(owner) = &value.kind {
            let state = owner.borrow();
            let start = buffer_base(&state)? as usize;
            let end = start
                .checked_add(state.len)
                .ok_or_else(|| "FFI Buffer address range calculation overflow".to_string())?;
            if (start..=end).contains(&target) {
                return Ok(Some(PointerOwner::Buffer(owner.clone())));
            }
        }
    }
    for value in values {
        let Value::Ffi(value) = value else {
            continue;
        };
        if let BtFfiKind::Pointer(pointer) = &value.kind {
            if pointer.address == address {
                return Ok(Some(pointer.owner.clone()));
            }
        }
    }
    Ok(None)
}

/// bounded copy UTF-8 NUL terminated return string from native non-null address.
fn copy_cstr_result(symbol: &str, address: NonNull<u8>) -> Result<Value, String> {
    let limit = super::bytes::limit()?;
    let mut bytes = Vec::new();
    for index in 0..limit {
        // SAFETY: The user's full signature declares that the address points to a readable C string when the synchronous call returns; scanning is strictly limited to
        // BT_BYTES_LIMIT, and the address is not saved. Arbitrary wrong addresses may still trigger a process-level access exception, which is an FFI boundary.
        let byte = unsafe { *address.as_ptr().add(index) };
        if byte == 0 {
            return Ok(String::from_utf8(bytes)
                .map(Value::Str)
                .unwrap_or(Value::Null));
        }
        bytes.push(byte);
    }
    Err(format!(
        "FFI symbol `{}` returned a cstr with no NUL terminator within BT_BYTES_LIMIT {}",
        symbol, limit
    ))
}

/// Bounded copy of Windows UTF-16 NUL terminated return string from native non-null address.
fn copy_wstr_result(symbol: &str, address: NonNull<u16>) -> Result<Value, String> {
    #[cfg(not(windows))]
    {
        let _ = (symbol, address);
        Err("wstr return type is only supported on Windows targets".to_string())
    }
    #[cfg(windows)]
    {
        let byte_limit = super::bytes::limit()?;
        let unit_limit = byte_limit / std::mem::size_of::<u16>();
        let mut units = Vec::new();
        for index in 0..unit_limit {
            // SAFETY: The user's full signature declares that the address points to a readable UTF-16 string when the synchronous call returns; scanning
            // is strictly subject to BT_BYTES_LIMIT and copied immediately. Wrong addresses are still an FFI risk that could kill the process.
            let unit = unsafe { *address.as_ptr().add(index) };
            if unit == 0 {
                return Ok(String::from_utf16(&units)
                    .map(Value::Str)
                    .unwrap_or(Value::Null));
            }
            units.push(unit);
        }
        Err(format!(
            "FFI symbol `{}` returned a wstr with no NUL terminator within BT_BYTES_LIMIT {}",
            symbol, byte_limit
        ))
    }
}

/// constructor uniform argument type error.
fn argument_type_error(symbol: &str, index: usize, expected: FfiType, actual: &Value) -> String {
    format!(
        "FFI symbol `{}` parameter {} requires {}, but received {}",
        symbol,
        index + 1,
        expected.name(),
        actual.type_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a test owner that does not hold the system dynamic library.
    fn test_owner() -> Rc<RefCell<FfiLibraryState>> {
        Rc::new(RefCell::new(FfiLibraryState {
            mode: LibraryMode::StrictSchema(HashMap::new()),
            functions: HashMap::new(),
            loaded: None,
            closed: false,
        }))
    }

    /// Creates a call description with the test function address and full signature.
    fn test_function(symbol: &str, signature: &str, code: *mut c_void) -> FfiFunction {
        let spec = parse_signature(symbol, signature)
            .expect("The test signature should parse successfully");
        FfiFunction::new(
            symbol,
            spec.params
                .expect("The complete signature must contain a parameter table"),
            spec.result,
            None,
            CodePtr(code),
        )
    }

    /// Tests a signed 32-bit integer call.
    extern "C" fn add_i32(left: i32, right: i32) -> i32 {
        left + right
    }

    /// Tests unsigned 32-bit integer calls.
    extern "C" fn add_u32(left: u32, right: u32) -> u32 {
        left + right
    }

    /// A real libffi call that tests the 16-argument limit for a single function.
    #[allow(clippy::too_many_arguments)]
    extern "C" fn sum_sixteen_i32(
        a01: i32,
        a02: i32,
        a03: i32,
        a04: i32,
        a05: i32,
        a06: i32,
        a07: i32,
        a08: i32,
        a09: i32,
        a10: i32,
        a11: i32,
        a12: i32,
        a13: i32,
        a14: i32,
        a15: i32,
        a16: i32,
    ) -> i32 {
        a01 + a02
            + a03
            + a04
            + a05
            + a06
            + a07
            + a08
            + a09
            + a10
            + a11
            + a12
            + a13
            + a14
            + a15
            + a16
    }

    /// Tests narrow integer return slots and sign extension bounds.
    extern "C" fn return_i8() -> i8 {
        -7
    }

    /// Tests the signed 16-bit return slot.
    extern "C" fn return_i16() -> i16 {
        -300
    }

    /// Tests the unsigned 8-bit return slot.
    extern "C" fn return_u8() -> u8 {
        250
    }

    /// Tests the unsigned 16-bit return slot.
    extern "C" fn return_u16() -> u16 {
        60_000
    }

    /// tests 64-bit integer and pointer-width integer arguments and returns.
    extern "C" fn add_i64(left: i64, right: i64) -> i64 {
        left + right
    }

    /// tests a u64 return that cannot be mapped to a BT Int.
    extern "C" fn return_u64_max() -> u64 {
        u64::MAX
    }

    /// Test cannot be mapped to the usize return of a BT Int.
    extern "C" fn return_usize_max() -> usize {
        usize::MAX
    }

    /// tests 32-bit floating point arguments and returns.
    extern "C" fn add_f32(left: f32, right: f32) -> f32 {
        left + right
    }

    /// tests 64-bit floating point parameters and returns.
    extern "C" fn add_f64(left: f64, right: f64) -> f64 {
        left + right
    }

    /// Tests calls with null pointer arguments.
    extern "C" fn is_null(pointer: *const c_void) -> i32 {
        i32::from(pointer.is_null())
    }

    /// Test non-null pointer return.
    extern "C" fn static_pointer() -> *mut c_void {
        static VALUE: u8 = 1;
        std::ptr::addr_of!(VALUE).cast_mut().cast()
    }

    /// tests a native function that returns the input address unchanged.
    extern "C" fn echo_pointer(pointer: *mut c_void) -> *mut c_void {
        pointer
    }

    /// Returns a stable UTF-8 C string.
    extern "C" fn static_cstr() -> *const u8 {
        b"BT\0".as_ptr()
    }

    /// returns illegal UTF-8 for validating `null` failure semantics.
    extern "C" fn invalid_cstr() -> *const u8 {
        static INVALID: [u8; 2] = [0xff, 0];
        INVALID.as_ptr()
    }

    /// writes `BT` and NUL to the writable Buffer.
    unsafe extern "C" fn write_bt(pointer: *mut u8) -> i32 {
        // SAFETY: Test that the caller passes in at least three writable bytes and the address is stable during the call, the address is not saved.
        unsafe {
            *pointer = b'B';
            *pointer.add(1) = b'T';
            *pointer.add(2) = 0;
        }
        3
    }

    /// Tests UTF-8 C string input calls.
    unsafe extern "C" fn is_bt_text(text: *const u8) -> i32 {
        if text.is_null() {
            return 0;
        }
        // SAFETY: The production marshaling logic allocates three readable bytes for the test input and ensures that the third byte is NUL. The function does not save the address.
        unsafe { i32::from(*text == b'B' && *text.add(1) == b'T' && *text.add(2) == 0) }
    }

    /// Tests Windows UTF-16 input calls.
    #[cfg(windows)]
    unsafe extern "C" fn is_bt_wtext(text: *const u16) -> i32 {
        if text.is_null() {
            return 0;
        }
        // SAFETY: The production marshaling logic assigns three readable u16s to the test input and ensures that the third unit is NUL, the function does not save the address.
        unsafe {
            i32::from(*text == b'B' as u16 && *text.add(1) == b'T' as u16 && *text.add(2) == 0)
        }
    }

    /// returns a stable Windows UTF-16 string.
    #[cfg(windows)]
    extern "C" fn static_wstr() -> *const u16 {
        static TEXT: [u16; 3] = [b'B' as u16, b'T' as u16, 0];
        TEXT.as_ptr()
    }

    /// returns an illegal UTF-16 string containing an orphaned high surrogate.
    #[cfg(windows)]
    extern "C" fn invalid_wstr() -> *const u16 {
        static TEXT: [u16; 2] = [0xd800, 0];
        TEXT.as_ptr()
    }

    /// Tests calls without return values.
    extern "C" fn no_result() {}

    /// The full signature should accept empty arguments, spaces, and 16 arguments.
    #[test]
    fn parses_complete_signatures() {
        let empty = parse_signature("empty", " void ( ) ").unwrap();
        assert_eq!(empty.result, FfiType::Void);
        assert!(empty.params.as_ref().unwrap().is_empty());

        let sixteen = parse_signature(
            "sixteen",
            "i32(i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32)",
        )
        .unwrap();
        assert_eq!(sixteen.params.as_ref().unwrap().len(), MAX_FUNCTION_ARGS);
    }

    /// Return hints must be open, and unknown types, void parameters, and redundant text must be rejected before load.
    #[test]
    fn parses_return_hints_and_rejects_invalid_signatures() {
        let hint = parse_schema_value("hint", "i32").unwrap();
        assert_eq!(hint.result, FfiType::I32);
        assert!(hint.params.is_none());
        let pointer_hint = parse_schema_value("pointer", " ptr ").unwrap();
        assert_eq!(pointer_hint.result, FfiType::Ptr);
        assert!(pointer_hint.params.is_none());
        assert!(parse_signature("bad", "long()")
            .unwrap_err()
            .contains("is not supported"));
        assert!(parse_signature("bad", "i32(void)")
            .unwrap_err()
            .contains("cannot be declared"));
        assert!(parse_signature("bad", "i32()x").is_err());
        assert!(parse_signature("bad", "i32(i32,)").is_err());
    }

    /// Schemas, symbol names, signature texts, and function caches must adhere to fixed boundaries exactly.
    #[test]
    fn schema_symbol_signature_and_cache_limits_are_enforced() {
        let mut schema = IndexMap::new();
        for index in 0..MAX_SCHEMA_FUNCTIONS {
            schema.insert(format!("symbol_{}", index), Value::Str("i32()".to_string()));
        }
        assert_eq!(parse_schema(&schema).unwrap().len(), MAX_SCHEMA_FUNCTIONS);
        schema.insert(
            "symbol_overflow".to_string(),
            Value::Str("i32()".to_string()),
        );
        assert!(parse_schema(&schema)
            .unwrap_err()
            .contains("schema exceeds the limit"));

        let maximum_symbol = "s".repeat(MAX_SYMBOL_BYTES);
        assert!(validate_symbol(&maximum_symbol).is_ok());
        assert!(validate_symbol(&"s".repeat(MAX_SYMBOL_BYTES + 1))
            .unwrap_err()
            .contains("UTF-8 bytes"));

        let maximum_signature = format!("i32(){}", " ".repeat(MAX_SIGNATURE_BYTES - "i32()".len()));
        assert_eq!(maximum_signature.len(), MAX_SIGNATURE_BYTES);
        assert!(parse_schema_value("maximum", &maximum_signature).is_ok());
        assert!(parse_schema_value("overflow", &(maximum_signature + " "))
            .unwrap_err()
            .contains("signature for FFI symbol `overflow` exceeds"));

        let mut functions = HashMap::with_capacity(MAX_CACHED_FUNCTIONS);
        for index in 0..MAX_CACHED_FUNCTIONS {
            let symbol = format!("cached_{}", index);
            functions.insert(
                symbol.clone().into_boxed_str(),
                Rc::new(test_function(&symbol, "void()", no_result as *mut c_void)),
            );
        }
        let owner = Rc::new(RefCell::new(FfiLibraryState {
            mode: LibraryMode::LimitedImplicit,
            functions,
            loaded: None,
            closed: false,
        }));
        assert!(function_for_call(&owner, "cache_overflow", &[])
            .unwrap_err()
            .contains("function cache for one dynamic library exceeds"));
    }

    /// Limited declaration only allows i32 Int, normal pointers, and A/W strings allowed by the platform.
    #[test]
    fn limited_inference_accepts_only_conservative_mappings() {
        let _resource_guard = lock_test_resources();
        let (params, inferred) =
            infer_arguments("NativeCall", &[Value::Int(7), Value::Null]).unwrap();
        assert_eq!(&*params, &[FfiType::I32, FfiType::Ptr]);
        assert_eq!(&*inferred, &[InferredKind::I32, InferredKind::Ptr]);

        let Value::Ffi(buffer) = BtFfiValue::buffer(vec![Value::Int(16)]).unwrap() else {
            panic!("Expected FfiBuffer");
        };
        let (params, inferred) = infer_arguments("NativeCall", &[Value::Ffi(buffer)]).unwrap();
        assert_eq!(&*params, &[FfiType::Ptr]);
        assert_eq!(&*inferred, &[InferredKind::Ptr]);

        for value in [
            Value::Bool(true),
            Value::Float(1.0),
            Value::Int(i64::MAX),
            Value::Empty,
            Value::Bytes(BtBytes::unchecked(vec![1])),
            Value::Array(Rc::new(RefCell::new(Vec::new()))),
            Value::Object(Rc::new(RefCell::new(IndexMap::new()))),
            Value::Function(0),
            Value::Ffi(BtFfiValue::static_value()),
        ] {
            let error = infer_arguments("NativeCall", &[value]).unwrap_err();
            assert!(error.contains("parameter 1"), "{}", error);
            assert!(error.contains("full signature"), "{}", error);
        }
        let error = infer_arguments("NativeCall", &[Value::Str("BT".to_string())]).unwrap_err();
        assert!(error.contains("string encoding"), "{}", error);
    }

    /// The first call must lock the inferred kind, null The first occurrence can only be locked as a normal ptr.
    #[test]
    fn limited_inference_locks_first_successful_kinds() {
        let (_, pointer_kind) = infer_arguments("FindWindowW", &[Value::Null]).unwrap();
        let error = validate_inferred_arguments(
            "FindWindowW",
            &pointer_kind,
            &[Value::Str("BT".to_string())],
        )
        .unwrap_err();
        assert!(error.contains("locked as ptr"), "{}", error);

        assert!(
            validate_inferred_arguments("NativeCall", &[InferredKind::I32], &[Value::Int(1)])
                .is_ok()
        );
        let error =
            validate_inferred_arguments("NativeCall", &[InferredKind::I32], &[Value::Bool(true)])
                .unwrap_err();
        assert!(error.contains("locked as i32"), "{}", error);
    }

    /// Windows A/W rules only affect String parameters, W accepts Unicode, A only accepts ASCII.
    #[cfg(windows)]
    #[test]
    fn windows_aw_inference_is_parameter_only_and_ascii_bounded() {
        let (wide_params, wide_kinds) =
            infer_arguments("FindWindowW", &[Value::Str("window".to_string())]).unwrap();
        assert_eq!(&*wide_params, &[FfiType::WStr]);
        assert_eq!(&*wide_kinds, &[InferredKind::WStr]);
        assert!(validate_inferred_arguments("FindWindowW", &wide_kinds, &[Value::Null]).is_ok());

        let (ansi_params, ansi_kinds) =
            infer_arguments("FindWindowA", &[Value::Str("BT".to_string())]).unwrap();
        assert_eq!(&*ansi_params, &[FfiType::CStr]);
        assert_eq!(&*ansi_kinds, &[InferredKind::AnsiAscii]);
        let error = validate_inferred_arguments(
            "FindWindowA",
            &ansi_kinds,
            &[Value::Str("café".to_string())],
        )
        .unwrap_err();
        assert!(error.contains("only allows ASCII"), "{}", error);
        let error = infer_arguments("findwindoww", &[Value::Str("BT".to_string())]).unwrap_err();
        assert!(error.contains("string encoding"), "{}", error);
    }

    /// The same symbol with limited declaration can only generate one cache entry. Missing symbols and repeated calls cannot increase the cache.
    #[cfg(windows)]
    #[test]
    fn limited_implicit_cache_has_one_entry_per_symbol() {
        let _resource_guard = lock_test_resources();
        let Value::Ffi(library) =
            BtFfiValue::load(vec![Value::Str("user32.dll".to_string())]).unwrap()
        else {
            panic!("expects FfiLibrary");
        };
        let BtFfiKind::Library(state) = &library.kind else {
            panic!("expects FfiLibrary kind");
        };

        assert!(matches!(
            library.call("GetSystemMetrics", vec![Value::Int(0)]),
            Ok(Value::Int(_))
        ));
        assert_eq!(state.borrow().functions.len(), 1);
        assert!(matches!(
            library.call("GetSystemMetrics", vec![Value::Int(1)]),
            Ok(Value::Int(_))
        ));
        assert_eq!(state.borrow().functions.len(), 1);
        assert!(library.call("BtMissingSymbol", vec![]).is_err());
        assert_eq!(state.borrow().functions.len(), 1);
        assert!(close_library(state).unwrap());
        assert!(!close_library(state).unwrap());
    }

    /// call_return_into should handle i32, u32 and void returns correctly.
    #[test]
    fn calls_scalar_and_void_functions() {
        let owner = test_owner();
        let signed = test_function("add_i32", "i32(i32,i32)", add_i32 as *mut c_void);
        let unsigned = test_function("add_u32", "u32(u32,u32)", add_u32 as *mut c_void);
        let void = test_function("no_result", "void()", no_result as *mut c_void);

        assert_eq!(
            invoke_function(&owner, &signed, &[Value::Int(-2), Value::Int(5)]).unwrap(),
            Value::Int(3)
        );
        assert_eq!(
            invoke_function(&owner, &unsigned, &[Value::Int(2), Value::Int(5)]).unwrap(),
            Value::Int(7)
        );
        assert_eq!(invoke_function(&owner, &void, &[]).unwrap(), Value::Empty);
    }

    /// 16 Argument caps must be passed through real libffi calls, not just through signature resolution.
    #[test]
    fn calls_maximum_sixteen_arguments() {
        let owner = test_owner();
        let function = test_function(
            "sum_sixteen_i32",
            "i32(i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32,i32)",
            sum_sixteen_i32 as *mut c_void,
        );
        let values = (1..=MAX_FUNCTION_ARGS)
            .map(|value| Value::Int(value as i64))
            .collect::<Vec<_>>();

        assert_eq!(
            invoke_function(&owner, &function, &values).unwrap(),
            Value::Int(136)
        );
    }

    /// All explicit integer widths, floating point numbers, and return errors beyond BT Int must be stable.
    #[test]
    fn calls_all_explicit_scalar_types() {
        let owner = test_owner();
        for (signature, code, expected) in [
            ("i8()", return_i8 as *mut c_void, Value::Int(-7)),
            ("i16()", return_i16 as *mut c_void, Value::Int(-300)),
            ("u8()", return_u8 as *mut c_void, Value::Int(250)),
            ("u16()", return_u16 as *mut c_void, Value::Int(60_000)),
        ] {
            let function = test_function("narrow", signature, code);
            assert_eq!(invoke_function(&owner, &function, &[]).unwrap(), expected);
        }

        let i64_add = test_function("add_i64", "i64(i64,i64)", add_i64 as *mut c_void);
        assert_eq!(
            invoke_function(&owner, &i64_add, &[Value::Int(-9), Value::Bool(true)]).unwrap(),
            Value::Int(-8)
        );
        let f32_add = test_function("add_f32", "f32(f32,f32)", add_f32 as *mut c_void);
        let f64_add = test_function("add_f64", "f64(f64,f64)", add_f64 as *mut c_void);
        assert_eq!(
            invoke_function(&owner, &f32_add, &[Value::Int(1), Value::Float(2.5)]).unwrap(),
            Value::Float(3.5)
        );
        assert_eq!(
            invoke_function(&owner, &f64_add, &[Value::Int(1), Value::Float(2.5)]).unwrap(),
            Value::Float(3.5)
        );

        let u64_max = test_function("u64_max", "u64()", return_u64_max as *mut c_void);
        let usize_max = test_function("usize_max", "usize()", return_usize_max as *mut c_void);
        assert!(invoke_function(&owner, &u64_max, &[])
            .unwrap_err()
            .contains("cannot be represented as a BT Int"));
        assert!(invoke_function(&owner, &usize_max, &[])
            .unwrap_err()
            .contains("cannot be represented as a BT Int"));

        let all = parse_signature(
            "all",
            "void(i8,i16,i32,i64,u8,u16,u32,u64,isize,usize,f32,f64,ptr,cstr)",
        )
        .unwrap();
        assert_eq!(all.params.as_ref().unwrap().len(), 14);
    }

    /// cstr return should be copied immediately, illegal UTF-8 mapping is null.
    #[test]
    fn copies_utf8_string_results() {
        let owner = test_owner();
        let valid = test_function("static_cstr", "cstr()", static_cstr as *mut c_void);
        let invalid = test_function("invalid_cstr", "cstr()", invalid_cstr as *mut c_void);
        assert_eq!(
            invoke_function(&owner, &valid, &[]).unwrap(),
            Value::Str("BT".to_string())
        );
        assert_eq!(invoke_function(&owner, &invalid, &[]).unwrap(), Value::Null);

        let bytes = vec![b'X'; super::super::bytes::limit().unwrap()];
        let address = NonNull::new(bytes.as_ptr().cast_mut()).unwrap();
        assert!(copy_cstr_result("unterminated", address)
            .unwrap_err()
            .contains("NUL"));
    }

    /// not found All explicit integer argument widths must accept bounds and reject before native calls if out of bounds.
    #[test]
    fn checks_all_integer_argument_boundaries() {
        let mut slot = FfiSlot::zeroed();
        let mut string = None;
        for (kind, min, max) in [
            (FfiType::I8, i8::MIN as i64, i8::MAX as i64),
            (FfiType::I16, i16::MIN as i64, i16::MAX as i64),
            (FfiType::I32, i32::MIN as i64, i32::MAX as i64),
            (FfiType::I64, i64::MIN, i64::MAX),
            (FfiType::ISize, isize::MIN as i64, isize::MAX as i64),
        ] {
            marshal_argument("signed", 0, kind, &Value::Int(min), &mut slot, &mut string).unwrap();
            marshal_argument("signed", 0, kind, &Value::Int(max), &mut slot, &mut string).unwrap();
            if min > i64::MIN {
                assert!(marshal_argument(
                    "signed",
                    0,
                    kind,
                    &Value::Int(min - 1),
                    &mut slot,
                    &mut string,
                )
                .is_err());
            }
            if max < i64::MAX {
                assert!(marshal_argument(
                    "signed",
                    0,
                    kind,
                    &Value::Int(max + 1),
                    &mut slot,
                    &mut string,
                )
                .is_err());
            }
        }
        for (kind, max) in [
            (FfiType::U8, u8::MAX as i64),
            (FfiType::U16, u16::MAX as i64),
            (FfiType::U32, u32::MAX as i64),
            (FfiType::U64, i64::MAX),
            (FfiType::USize, i64::MAX),
        ] {
            marshal_argument("unsigned", 0, kind, &Value::Int(0), &mut slot, &mut string).unwrap();
            marshal_argument(
                "unsigned",
                0,
                kind,
                &Value::Int(max),
                &mut slot,
                &mut string,
            )
            .unwrap();
            assert!(
                marshal_argument("unsigned", 0, kind, &Value::Int(-1), &mut slot, &mut string,)
                    .is_err()
            );
            if max < i64::MAX {
                assert!(marshal_argument(
                    "unsigned",
                    0,
                    kind,
                    &Value::Int(max + 1),
                    &mut slot,
                    &mut string,
                )
                .is_err());
            }
        }
    }

    /// Buffer must maintain 16-byte alignment, fixed range, owner inheritance and credit recycling after closing.
    #[test]
    fn buffer_is_aligned_bounded_and_invalidated_on_close() {
        let _resource_guard = lock_test_resources();
        let baseline = stats();
        let Value::Ffi(buffer) = BtFfiValue::buffer(vec![Value::Int(17)]).unwrap() else {
            panic!("Expected FfiBuffer");
        };
        let BtFfiKind::Buffer(state) = &buffer.kind else {
            panic!("expected FfiBuffer kind");
        };
        assert_eq!(buffer_base(&state.borrow()).unwrap() as usize % 16, 0);
        let after_create = stats();
        assert_eq!(after_create.buffers, baseline.buffers + 1);
        assert_eq!(after_create.buffer_bytes, baseline.buffer_bytes + 32);

        assert_eq!(
            buffer
                .call(
                    "write",
                    vec![Value::Bytes(BtBytes::unchecked(vec![1, 2, 3]))]
                )
                .unwrap(),
            Value::Int(3)
        );
        assert_eq!(buffer.call("len", vec![]).unwrap(), Value::Int(17));
        let Value::Bytes(copied) = buffer
            .call("to_bytes", vec![Value::Int(1), Value::Int(2)])
            .unwrap()
        else {
            panic!("expected Bytes");
        };
        assert_eq!(copied.as_slice(), &[2, 3]);
        assert!(buffer.call("ptr", vec![Value::Int(18)]).is_err());
        assert!(buffer
            .call("to_bytes", vec![Value::Int(16), Value::Int(i64::MAX)])
            .is_err());

        let pointer = buffer.call("ptr", vec![]).unwrap();
        let native_write = test_function("write_bt", "i32(ptr)", write_bt as *mut c_void);
        let library_owner = test_owner();
        assert_eq!(
            invoke_function(&library_owner, &native_write, &[pointer.clone()]).unwrap(),
            Value::Int(3)
        );
        assert_eq!(
            buffer.call("to_string", vec![]).unwrap(),
            Value::Str("BT".to_string())
        );

        let echo = test_function("echo", "ptr(ptr)", echo_pointer as *mut c_void);
        let echoed = invoke_function(&library_owner, &echo, &[Value::Ffi(buffer.clone())]).unwrap();
        let Value::Ffi(echoed) = echoed else {
            panic!("expected FfiPointer");
        };
        let BtFfiKind::Pointer(echoed) = &echoed.kind else {
            panic!("expected FfiPointer kind");
        };
        assert!(matches!(echoed.owner, PointerOwner::Buffer(_)));

        assert!(close_buffer(state));
        assert!(!close_buffer(state));
        assert!(pointer_argument("closed", 0, &pointer)
            .unwrap_err()
            .contains("invalid"));
        let after_close = stats();
        assert_eq!(after_close.buffers, baseline.buffers);
        assert_eq!(after_close.buffer_bytes, baseline.buffer_bytes);
    }

    /// The ptr and cstr parameters must preserve null pointer semantics and temporary string lifetime respectively.
    #[test]
    fn calls_pointer_and_utf8_input_functions() {
        let owner = test_owner();
        let pointer = test_function("is_null", "i32(ptr)", is_null as *mut c_void);
        let return_pointer =
            test_function("static_pointer", "ptr()", static_pointer as *mut c_void);
        let text = test_function("is_bt_text", "i32(cstr)", is_bt_text as *mut c_void);

        assert_eq!(
            invoke_function(&owner, &pointer, &[Value::Null]).unwrap(),
            Value::Int(1)
        );
        let returned = invoke_function(&owner, &return_pointer, &[]).unwrap();
        assert_eq!(
            invoke_function(&owner, &pointer, &[returned.clone()]).unwrap(),
            Value::Int(0)
        );
        assert_eq!(
            invoke_function(&owner, &text, &[Value::Str("BT".to_string())]).unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            invoke_function(&owner, &text, &[Value::Null]).unwrap(),
            Value::Int(0)
        );
        close_library(&owner).unwrap();
        let caller = test_owner();
        assert!(invoke_function(&caller, &pointer, &[returned])
            .unwrap_err()
            .contains("invalid FfiPointer"));
    }

    /// The native function must not wrap the temporary string address of this call into a FfiPointer that can continue to be used.
    #[test]
    fn rejects_pointer_return_into_temporary_string() {
        let owner = test_owner();
        let echo = test_function("echo_pointer", "ptr(cstr)", echo_pointer as *mut c_void);

        assert!(
            invoke_function(&owner, &echo, &[Value::Str("BT".to_string())])
                .unwrap_err()
                .contains("temporary string")
        );
    }

    /// The Windows wstr parameter must be encoded as NUL-terminated UTF-16.
    #[cfg(windows)]
    #[test]
    fn calls_windows_utf16_input_function() {
        let _resource_guard = lock_test_resources();
        let owner = test_owner();
        let text = test_function("is_bt_wtext", "i32(wstr)", is_bt_wtext as *mut c_void);
        let returned = test_function("static_wstr", "wstr()", static_wstr as *mut c_void);
        let invalid = test_function("invalid_wstr", "wstr()", invalid_wstr as *mut c_void);

        assert_eq!(
            invoke_function(&owner, &text, &[Value::Str("BT".to_string())]).unwrap(),
            Value::Int(1)
        );
        assert_eq!(
            invoke_function(&owner, &returned, &[]).unwrap(),
            Value::Str("BT".to_string())
        );
        assert_eq!(invoke_function(&owner, &invalid, &[]).unwrap(), Value::Null);
        assert_eq!(
            invoke_function(&owner, &text, &[Value::Null]).unwrap(),
            Value::Int(0)
        );
        assert!(
            invoke_function(&owner, &text, &[Value::Str("B\0T".to_string())])
                .unwrap_err()
                .contains("internal NUL")
        );

        let Value::Ffi(buffer) = BtFfiValue::buffer(vec![Value::Int(6)]).unwrap() else {
            panic!("Expected FfiBuffer");
        };
        buffer
            .call(
                "write",
                vec![Value::Bytes(BtBytes::unchecked(vec![66, 0, 84, 0, 0, 0]))],
            )
            .unwrap();
        assert_eq!(
            buffer.call("to_wstring", vec![]).unwrap(),
            Value::Str("BT".to_string())
        );
        buffer
            .call(
                "write",
                vec![Value::Bytes(BtBytes::unchecked(vec![
                    0x00, 0xd8, 0, 0, 0, 0,
                ]))],
            )
            .unwrap();
        assert_eq!(buffer.call("to_wstring", vec![]).unwrap(), Value::Null);
        buffer
            .call(
                "write",
                vec![Value::Bytes(BtBytes::unchecked(vec![66, 0, 84, 0, 88, 0]))],
            )
            .unwrap();
        assert!(buffer
            .call("to_wstring", vec![])
            .unwrap_err()
            .contains("NUL"));

        let unit_limit = super::super::bytes::limit().unwrap() / std::mem::size_of::<u16>();
        let unterminated = vec![b'X' as u16; unit_limit];
        let address = NonNull::new(unterminated.as_ptr().cast_mut()).unwrap();
        assert!(copy_wstr_result("unterminated", address)
            .unwrap_err()
            .contains("NUL"));
    }

    /// Library and Buffer must be rejected when the quota reaches the upper limit. Failure and Drop must reclaim all counts.
    #[test]
    fn resource_quotas_roll_back_and_drop_release() {
        let _resource_guard = lock_test_resources();
        let baseline = stats();

        let library_capacity = MAX_OPEN_LIBRARIES - baseline.open_libraries;
        let library_guards = (0..library_capacity)
            .map(|_| LibraryQuotaGuard::reserve().unwrap())
            .collect::<Vec<_>>();
        assert!(LibraryQuotaGuard::reserve()
            .unwrap_err()
            .contains("open at most"));
        drop(library_guards);
        assert_eq!(stats().open_libraries, baseline.open_libraries);

        let missing = BtFfiValue::load(vec![Value::Str(
            "__bt_missing_library_for_quota_test__.dll".to_string(),
        )]);
        assert!(missing.is_err());
        assert_eq!(stats().open_libraries, baseline.open_libraries);

        let buffer_capacity = MAX_BUFFERS - baseline.buffers;
        let buffer_guards = (0..buffer_capacity)
            .map(|_| BufferQuotaGuard::reserve(16).unwrap())
            .collect::<Vec<_>>();
        assert!(BufferQuotaGuard::reserve(16)
            .unwrap_err()
            .contains("keep at most"));
        drop(buffer_guards);
        assert_eq!(stats(), baseline);

        assert!(BufferQuotaGuard::reserve(MAX_BUFFER_BYTES + 16).is_err());
        assert_eq!(stats(), baseline);
        {
            let value = BtFfiValue::buffer(vec![Value::Int(1)]).unwrap();
            assert!(matches!(value, Value::Ffi(_)));
            assert_eq!(stats().buffers, baseline.buffers + 1);
        }
        assert_eq!(stats(), baseline);
    }

    /// The number of parameters, type, range, and internal NUL errors must be returned before the native call.
    #[test]
    fn rejects_invalid_arguments_before_call() {
        let owner = test_owner();
        let signed = test_function("add_i32", "i32(i32,i32)", add_i32 as *mut c_void);
        let text = test_function("is_bt_text", "i32(cstr)", is_bt_text as *mut c_void);

        assert!(invoke_function(&owner, &signed, &[Value::Int(1)]).is_err());
        assert!(
            invoke_function(&owner, &signed, &[Value::Int(i64::MAX), Value::Int(1)])
                .unwrap_err()
                .contains("i32 range")
        );
        assert!(
            invoke_function(&owner, &text, &[Value::Str("B\0T".to_string())])
                .unwrap_err()
                .contains("internal NUL")
        );
        let limit = super::super::bytes::limit().unwrap();
        assert!(
            ensure_string_argument_limit("is_bt_text", 0, "cstr", limit + 1)
                .unwrap_err()
                .contains("BT_BYTES_LIMIT")
        );
    }
}
