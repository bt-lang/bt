//! BT runtime value.
//!
//! The bytecode VM started with scalar values; arrays, objects, functions, and classes now build on the same value model.
//! This module is deliberately kept small and direct to avoid VM hot paths relying on large runtime structures of older interpreters.

#[cfg(feature = "extensions")]
use crate::extensions::manager::{ExtObject, ExtensionFunctionRef};
#[cfg(feature = "ffi")]
use crate::libs::ffi::BtFfiValue;
use crate::libs::{
    base64::BtBase64,
    bt::BtRuntime,
    bytes::BtBytes,
    crypto::BtCrypto,
    date::BtDate,
    device::BtDevice,
    fs::BtFs,
    html::BtHtml,
    math::BtMath,
    md5::BtMd5,
    modbus::BtModbus,
    mysql::{BtMysql, BtMysqlTransaction},
    net::BtNet,
    path::BtPath,
    process::BtProcess,
    reqwest::BtReqwest,
    url::BtUrl,
};
use crate::net::{
    tcp::{TcpClientHandle, TcpServerHandle},
    udp::UdpSocketHandle,
    web::WebServerHandle,
    ws::{WsServerHandle, WsSocketHandle},
};
use crate::task::BtTask;
use crate::timer::BtTimer;
use indexmap::IndexMap;
use regex::Regex;
use std::cell::RefCell;
use std::collections::HashSet;
use std::mem;
use std::rc::Rc;

/// Lightweight iteration state used by VM `for` loops.
///
/// Collection traversal snapshots key-value pairs so mutations during a loop do not affect the current iteration. Integer counts and ranges retain only the
/// current index, value, endpoint, and step, avoiding eager allocation for large ranges.
#[derive(Debug, Clone, PartialEq)]
pub enum IterState {
    /// Materialized collection items as `(key, value)` pairs.
    Items {
        /// Expanded `(key, value)` item.
        items: Vec<(Value, Value)>,
        /// The index to be read next.
        index: usize,
    },
    /// Repeats an integer number of times, with `current` serving as both key and value.
    Count {
        /// The number of times it has been returned.
        index: i64,
        /// The total number of executions, using left-closed and right-open semantics.
        count: i64,
        /// The integer to be returned next time.
        current: i64,
        /// Positive integer step size that increases each round.
        step: i64,
    },
    /// Lazy range-loop state.
    Range(RangeState),
    /// Lazy byte traversal state.
    Bytes {
        /// The byte value being traversed.
        data: BtBytes,
        /// The index to be read next.
        index: usize,
    },
}

/// Lazy state for a range loop.
///
/// `a..b` includes both endpoints and derives its direction from their order.
/// `a..` has no endpoint and always advances forward.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeState {
    /// Range value to return next.
    pub current: i64,
    /// Optional inclusive endpoint; `None` represents an open-ended range.
    pub end: Option<i64>,
    /// Positive integer step size.
    pub step: i64,
    /// Range direction: `1` for ascending and `-1` for descending.
    pub direction: i8,
    /// Whether the iteration has been completed.
    pub finished: bool,
    /// Zero-based sequence number for the next value, exposed as the iteration key.
    pub index: i64,
}

/// Runtime metadata for class members.
///
/// The bytecode stage stores `pub` visibility here so instantiated fields and methods retain the
/// metadata, allowing the VM to distinguish external access from access through `this`.
#[derive(Debug, Clone)]
pub struct ClassMember {
    /// The actual value of the member, which may be a field value or a user function.
    pub value: Value,
    /// Whether access is allowed outside the class.
    pub is_public: bool,
}

/// Class instance object.
///
/// Unlike ordinary objects, class instances retain their class name and member
/// visibility so private fields and methods can be enforced with useful errors.
#[derive(Debug, Clone)]
pub struct InstanceObject {
    /// The class name to which the instance belongs.
    pub class_name: String,
    /// Instance member table.
    pub members: IndexMap<String, ClassMember>,
}

/// Unified runtime value type for the BT bytecode VM.
///
/// Mutable reference values use shared ownership so assignment follows familiar
/// scripting-language object semantics without copying the underlying value.
#[derive(Debug, Clone)]
pub enum Value {
    /// Explicit `null`, used for external nulls and failed conversions or parsing.
    Null,
    /// Missing `empty` value: no expression result, no initialization, or no such field/index.
    Empty,
    /// Integer value.
    Int(i64),
    /// Floating point value.
    Float(f64),
    /// Boolean value.
    Bool(bool),
    /// String value.
    Str(String),
    /// Binary byte values use a shared read-only buffer to avoid implicit lossy string conversion.
    Bytes(BtBytes),
    /// Array value, using reference semantics, variable assignment only copies the pointer.
    Array(Rc<RefCell<Vec<Value>>>),
    /// Object value, using reference semantics, variable assignment only copies the pointer.
    Object(Rc<RefCell<IndexMap<String, Value>>>),
    /// Class instance value, holding fields, methods and visibility.
    Instance(Rc<RefCell<InstanceObject>>),
    /// User function identified by its function-table index.
    Function(usize),
    /// User function bound to the bytecode block that owns it.
    ///
    /// `include()` compiles another file at runtime, and an exported function index
    /// still belongs to that file's chunk. Carrying the owner here prevents calls
    /// from accidentally indexing the caller's function table while keeping ordinary
    /// function calls on the simpler hot path.
    BoundFunction(usize, Rc<crate::bytecode::Chunk>),
    /// User function that has captured outer local variables.
    ///
    /// Arrow and anonymous functions may read outer locals. Captures are stored by
    /// the child function's symbol slot and merged with arguments at call time, so
    /// ordinary variable reads do not need to walk a closure chain.
    Closure(
        usize,
        Rc<crate::bytecode::Chunk>,
        Rc<Vec<Option<Rc<RefCell<Option<Value>>>>>>,
    ),
    /// Built-in function identified by its script-visible name.
    NativeFunction(String),
    /// Extension entry function with a stable registry reference.
    #[cfg(feature = "extensions")]
    ExtensionFunction(ExtensionFunctionRef),
    /// Extension object handle whose methods are dispatched by the VM's extension manager.
    #[cfg(feature = "extensions")]
    ExtObject(ExtObject),
    /// FFI value representing a static object, dynamic library, or native pointer.
    #[cfg(feature = "ffi")]
    Ffi(BtFfiValue),
    /// Class definition containing the class name and member table.
    Class(String, Rc<IndexMap<String, ClassMember>>),
    /// Regular expression value, saves compiled regular expressions, original patterns and modifiers.
    Regex(Rc<Regex>, String, String),
    /// Date and time standard library object.
    Date(BtDate),
    /// Base64 standard library object.
    Base64(BtBase64),
    /// File system standard library object.
    Fs(BtFs),
    /// HTML text processing standard library object.
    Html(BtHtml),
    /// Cryptographic digest standard library object.
    Crypto(BtCrypto),
    /// URL text standard library object.
    Url(BtUrl),
    /// Path text standard library object.
    Path(BtPath),
    /// BT runtime information standard library object.
    Bt(BtRuntime),
    /// Mathematics standard library object.
    Math(BtMath),
    /// MD5 standard library object.
    Md5(BtMd5),
    /// Modbus RTU/TCP protocol auxiliary library object.
    Modbus(BtModbus),
    /// MySQL standard library object.
    Mysql(BtMysql),
    /// MySQL transaction object.
    MysqlTransaction(BtMysqlTransaction),
    /// Network standard library object.
    Net(BtNet),
    /// Web server handle.
    NetWebServer(WebServerHandle),
    /// TCP server handle.
    NetTcpServer(TcpServerHandle),
    /// TCP connection object.
    NetTcpClient(TcpClientHandle),
    /// UDP socket object.
    NetUdpSocket(UdpSocketHandle),
    /// WebSocket server handle.
    NetWsServer(WsServerHandle),
    /// WebSocket connection object.
    NetWsSocket(WsSocketHandle),
    /// Process standard library object.
    Process(BtProcess),
    /// HTTP request standard library object.
    Reqwest(BtReqwest),
    /// Device communication standard library object.
    Device(BtDevice),
    /// Background task object.
    Task(BtTask),
    /// Timer object.
    Timer(BtTimer),
    /// Iterator state passed only between internal VM instructions.
    Iterator(Rc<RefCell<IterState>>),
    /// Built-in methods for bound receivers.
    NativeMethod {
        /// Method receiver.
        receiver: Box<Value>,
        /// Method name.
        name: String,
        /// Whether to allow access to private methods of class instances.
        allow_private: bool,
    },
}

/// The collection of visited references in a single reference scan.
///
/// BT arrays, objects, instances, and closure captures use `Rc` for reference
/// semantics. Nested scans record visited nodes so cyclic data cannot overflow the
/// stack during memory accounting or debug output.
#[derive(Default)]
struct ReferenceVisitSet {
    /// Pointer to array `Rc` accessed.
    arrays: HashSet<usize>,
    /// Pointer to the object `Rc` accessed.
    objects: HashSet<usize>,
    /// Pointer to the accessed class instance `Rc`.
    instances: HashSet<usize>,
    /// Pointer to the accessed class member table `Rc`.
    classes: HashSet<usize>,
    /// The accessed iterator state `Rc` pointer.
    iterators: HashSet<usize>,
    /// Accessed closure local capture slot `Rc` pointer.
    local_cells: HashSet<usize>,
}

/// Reference target used when checking an assignment for cycles.
///
/// Before writing a field, the VM converts the destination to this lightweight
/// pointer set and scans the right-hand value for the same target. A match would
/// create an `Rc` cycle that a long-running process could not reclaim automatically.
#[derive(Default)]
struct ReferenceTarget {
    /// Target array `Rc` pointer.
    array: Option<usize>,
    /// Target object `Rc` pointer.
    object: Option<usize>,
    /// Pointer to target class instance `Rc`.
    instance: Option<usize>,
    /// Target class member table `Rc` pointer.
    class: Option<usize>,
    /// Target iterator status `Rc` pointer.
    iterator: Option<usize>,
}

impl ReferenceTarget {
    /// Extracts a target pointer from a runtime value that can be strongly referenced by other values.
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Array(values) => Self {
                array: Some(Rc::as_ptr(values) as usize),
                ..Self::default()
            },
            Value::Object(values) => Self {
                object: Some(Rc::as_ptr(values) as usize),
                ..Self::default()
            },
            Value::Instance(value) => Self {
                instance: Some(Rc::as_ptr(value) as usize),
                ..Self::default()
            },
            Value::Class(_, members) => Self {
                class: Some(Rc::as_ptr(members) as usize),
                ..Self::default()
            },
            Value::Iterator(state) => Self {
                iterator: Some(Rc::as_ptr(state) as usize),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }

    /// Returns whether no reference target is present.
    fn is_empty(&self) -> bool {
        self.array.is_none()
            && self.object.is_none()
            && self.instance.is_none()
            && self.class.is_none()
            && self.iterator.is_none()
    }
}

impl PartialEq for Value {
    /// Compares two runtime values for equality.
    ///
    /// Reference types follow common scripting-language identity rules. Regular
    /// expressions compare their source pattern and flags rather than relying on
    /// `regex::Regex` internals.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) | (Value::Empty, Value::Empty) => true,
            (Value::Int(left), Value::Int(right)) => left == right,
            (Value::Float(left), Value::Float(right)) => left == right,
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::Str(left), Value::Str(right)) => left == right,
            (Value::Bytes(left), Value::Bytes(right)) => left == right,
            (Value::Array(left), Value::Array(right)) => Rc::ptr_eq(left, right),
            (Value::Object(left), Value::Object(right)) => Rc::ptr_eq(left, right),
            (Value::Instance(left), Value::Instance(right)) => Rc::ptr_eq(left, right),
            (Value::Function(left), Value::Function(right)) => left == right,
            (Value::BoundFunction(left, _), Value::BoundFunction(right, _)) => left == right,
            (Value::Closure(left, left_owner, _), Value::Closure(right, right_owner, _)) => {
                left == right && Rc::ptr_eq(left_owner, right_owner)
            }
            (Value::Function(left), Value::BoundFunction(right, _))
            | (Value::BoundFunction(left, _), Value::Function(right))
            | (Value::Function(left), Value::Closure(right, _, _))
            | (Value::Closure(left, _, _), Value::Function(right))
            | (Value::BoundFunction(left, _), Value::Closure(right, _, _))
            | (Value::Closure(left, _, _), Value::BoundFunction(right, _)) => left == right,
            (Value::NativeFunction(left), Value::NativeFunction(right)) => left == right,
            #[cfg(feature = "extensions")]
            (Value::ExtensionFunction(left), Value::ExtensionFunction(right)) => left == right,
            #[cfg(feature = "extensions")]
            (Value::ExtObject(left), Value::ExtObject(right)) => left == right,
            #[cfg(feature = "ffi")]
            (Value::Ffi(left), Value::Ffi(right)) => left == right,
            (Value::Class(left_name, left), Value::Class(right_name, right)) => {
                left_name == right_name && Rc::ptr_eq(left, right)
            }
            (
                Value::Regex(_, left_pattern, left_flags),
                Value::Regex(_, right_pattern, right_flags),
            ) => left_pattern == right_pattern && left_flags == right_flags,
            (Value::Date(left), Value::Date(right)) => left == right,
            (Value::Base64(left), Value::Base64(right)) => left == right,
            (Value::Fs(left), Value::Fs(right)) => left == right,
            (Value::Html(left), Value::Html(right)) => left == right,
            (Value::Crypto(left), Value::Crypto(right)) => left == right,
            (Value::Url(left), Value::Url(right)) => left == right,
            (Value::Path(left), Value::Path(right)) => left == right,
            (Value::Bt(left), Value::Bt(right)) => left == right,
            (Value::Math(left), Value::Math(right)) => left == right,
            (Value::Md5(left), Value::Md5(right)) => left == right,
            (Value::Modbus(left), Value::Modbus(right)) => left == right,
            (Value::Mysql(left), Value::Mysql(right)) => left == right,
            (Value::MysqlTransaction(left), Value::MysqlTransaction(right)) => left == right,
            (Value::Net(left), Value::Net(right)) => left == right,
            (Value::NetWebServer(left), Value::NetWebServer(right)) => left == right,
            (Value::NetTcpServer(left), Value::NetTcpServer(right)) => left == right,
            (Value::NetTcpClient(left), Value::NetTcpClient(right)) => left == right,
            (Value::NetUdpSocket(left), Value::NetUdpSocket(right)) => left == right,
            (Value::NetWsServer(left), Value::NetWsServer(right)) => left == right,
            (Value::NetWsSocket(left), Value::NetWsSocket(right)) => left == right,
            (Value::Process(left), Value::Process(right)) => left == right,
            (Value::Reqwest(left), Value::Reqwest(right)) => left == right,
            (Value::Device(left), Value::Device(right)) => left == right,
            (Value::Task(left), Value::Task(right)) => left == right,
            (Value::Timer(left), Value::Timer(right)) => left == right,
            (Value::Iterator(left), Value::Iterator(right)) => Rc::ptr_eq(left, right),
            (
                Value::NativeMethod {
                    receiver: left_receiver,
                    name: left_name,
                    allow_private: left_private,
                },
                Value::NativeMethod {
                    receiver: right_receiver,
                    name: right_name,
                    allow_private: right_private,
                },
            ) => {
                left_name == right_name
                    && left_private == right_private
                    && left_receiver == right_receiver
            }
            _ => false,
        }
    }
}

impl Value {
    /// Creates an independent copy of the runtime mutable default value.
    ///
    /// Bytecode caches retain parameter defaults. A plain `clone()` of an array or
    /// object would copy only its `Rc`, leaking mutations across calls or requests.
    /// This method recursively copies mutable containers while cloning scalar,
    /// function, regex, and standard-library values normally.
    pub fn clone_mutable_literal(&self) -> Value {
        match self {
            Value::Array(values) => Value::Array(Rc::new(RefCell::new(
                values
                    .borrow()
                    .iter()
                    .map(Value::clone_mutable_literal)
                    .collect(),
            ))),
            Value::Object(values) => {
                let mut cloned = IndexMap::with_capacity(values.borrow().len());
                for (key, value) in values.borrow().iter() {
                    cloned.insert(key.clone(), value.clone_mutable_literal());
                }
                Value::Object(Rc::new(RefCell::new(cloned)))
            }
            Value::Instance(instance) => {
                let instance = instance.borrow();
                let mut members = IndexMap::with_capacity(instance.members.len());
                for (key, member) in instance.members.iter() {
                    members.insert(
                        key.clone(),
                        ClassMember {
                            value: member.value.clone_mutable_literal(),
                            is_public: member.is_public,
                        },
                    );
                }
                Value::Instance(Rc::new(RefCell::new(InstanceObject {
                    class_name: instance.class_name.clone(),
                    members,
                })))
            }
            Value::Iterator(state) => {
                let state = state.borrow();
                let state = match &*state {
                    IterState::Items { items, index } => IterState::Items {
                        items: items
                            .iter()
                            .map(|(key, value)| {
                                (key.clone_mutable_literal(), value.clone_mutable_literal())
                            })
                            .collect(),
                        index: *index,
                    },
                    IterState::Count {
                        index,
                        count,
                        current,
                        step,
                    } => IterState::Count {
                        index: *index,
                        count: *count,
                        current: *current,
                        step: *step,
                    },
                    IterState::Range(range) => IterState::Range(range.clone()),
                    IterState::Bytes { data, index } => IterState::Bytes {
                        data: data.clone(),
                        index: *index,
                    },
                };
                Value::Iterator(Rc::new(RefCell::new(state)))
            }
            _ => self.clone(),
        }
    }

    /// Estimates the number of bytes of heap memory held by the current literal value.
    ///
    /// Used only for compilation-cache accounting, never by ordinary runtime
    /// instructions. It recursively counts arrays, objects, class members, and
    /// closure captures while tracking visited references to handle cycles safely.
    pub fn estimated_literal_heap_bytes(&self) -> usize {
        let mut visited = ReferenceVisitSet::default();
        self.estimated_literal_heap_bytes_inner(&mut visited)
    }

    /// Recursively estimates the number of bytes of heap memory held by the current value.
    fn estimated_literal_heap_bytes_inner(&self, visited: &mut ReferenceVisitSet) -> usize {
        match self {
            Value::Str(value) => value.len(),
            Value::Bytes(value) => value.len(),
            Value::NativeFunction(name) => name.len(),
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(function) => function.name.len(),
            #[cfg(feature = "extensions")]
            Value::ExtObject(object) => object.type_name.len(),
            Value::BoundFunction(_, chunk) => chunk.estimated_heap_bytes(),
            Value::Closure(_, chunk, captures) => {
                let pointer = Rc::as_ptr(captures) as usize;
                if !visited.local_cells.insert(pointer) {
                    return 0;
                }
                captures
                    .iter()
                    .fold(chunk.estimated_heap_bytes(), |total, cell| {
                        let Some(cell) = cell else {
                            return total;
                        };
                        let pointer = Rc::as_ptr(cell) as usize;
                        if !visited.local_cells.insert(pointer) {
                            return total;
                        }
                        let value_bytes = cell
                            .borrow()
                            .as_ref()
                            .map(|value| value.estimated_literal_heap_bytes_inner(visited))
                            .unwrap_or(0);
                        total
                            .saturating_add(mem::size_of::<Option<Value>>())
                            .saturating_add(value_bytes)
                    })
            }
            Value::Array(values) => {
                let pointer = Rc::as_ptr(values) as usize;
                if !visited.arrays.insert(pointer) {
                    return 0;
                }
                let values = values.borrow();
                values.iter().fold(
                    values.capacity() * mem::size_of::<Value>(),
                    |total, value| {
                        total.saturating_add(value.estimated_literal_heap_bytes_inner(visited))
                    },
                )
            }
            Value::Object(values) => {
                let pointer = Rc::as_ptr(values) as usize;
                if !visited.objects.insert(pointer) {
                    return 0;
                }
                let values = values.borrow();
                values.iter().fold(
                    values.capacity() * mem::size_of::<(String, Value)>(),
                    |total, (key, value)| {
                        total
                            .saturating_add(key.len())
                            .saturating_add(value.estimated_literal_heap_bytes_inner(visited))
                    },
                )
            }
            Value::Instance(instance) => {
                let pointer = Rc::as_ptr(instance) as usize;
                if !visited.instances.insert(pointer) {
                    return 0;
                }
                let instance = instance.borrow();
                instance.members.iter().fold(
                    instance.class_name.len().saturating_add(
                        instance.members.capacity() * mem::size_of::<(String, ClassMember)>(),
                    ),
                    |total, (key, member)| {
                        total.saturating_add(key.len()).saturating_add(
                            member.value.estimated_literal_heap_bytes_inner(visited),
                        )
                    },
                )
            }
            Value::Class(name, members) => {
                let pointer = Rc::as_ptr(members) as usize;
                if !visited.classes.insert(pointer) {
                    return 0;
                }
                members.iter().fold(
                    name.len().saturating_add(
                        members.capacity() * mem::size_of::<(String, ClassMember)>(),
                    ),
                    |total, (key, member)| {
                        total.saturating_add(key.len()).saturating_add(
                            member.value.estimated_literal_heap_bytes_inner(visited),
                        )
                    },
                )
            }
            Value::Regex(_, pattern, flags) => pattern.len().saturating_add(flags.len()),
            Value::Iterator(state) => {
                let pointer = Rc::as_ptr(state) as usize;
                if !visited.iterators.insert(pointer) {
                    return 0;
                }
                match &*state.borrow() {
                    IterState::Items { items, .. } => items.iter().fold(
                        items.capacity() * mem::size_of::<(Value, Value)>(),
                        |total, (key, value)| {
                            total
                                .saturating_add(key.estimated_literal_heap_bytes_inner(visited))
                                .saturating_add(value.estimated_literal_heap_bytes_inner(visited))
                        },
                    ),
                    IterState::Bytes { data, .. } => data.len(),
                    IterState::Count { .. } | IterState::Range(_) => 0,
                }
            }
            Value::NativeMethod { receiver, name, .. } => name
                .len()
                .saturating_add(receiver.estimated_literal_heap_bytes_inner(visited)),
            _ => 0,
        }
    }

    /// Returns whether this value already strongly references the target.
    ///
    /// The VM calls this before `obj.key = value`. If the right-hand value already
    /// leads back to `obj`, the assignment would form an `Rc` cycle, as in
    /// `obj.self = obj` or a closure captured by the object it references.
    pub fn contains_reference_to(&self, target: &Value) -> bool {
        let target = ReferenceTarget::from_value(target);
        if target.is_empty() {
            return false;
        }
        let mut visited = ReferenceVisitSet::default();
        self.contains_reference_to_inner(&target, &mut visited)
    }

    /// Recursively scans whether the current value internally refers to the target.
    fn contains_reference_to_inner(
        &self,
        target: &ReferenceTarget,
        visited: &mut ReferenceVisitSet,
    ) -> bool {
        match self {
            Value::Array(values) => {
                let pointer = Rc::as_ptr(values) as usize;
                if target.array == Some(pointer) {
                    return true;
                }
                if !visited.arrays.insert(pointer) {
                    return false;
                }
                values
                    .borrow()
                    .iter()
                    .any(|value| value.contains_reference_to_inner(target, visited))
            }
            Value::Object(values) => {
                let pointer = Rc::as_ptr(values) as usize;
                if target.object == Some(pointer) {
                    return true;
                }
                if !visited.objects.insert(pointer) {
                    return false;
                }
                values
                    .borrow()
                    .values()
                    .any(|value| value.contains_reference_to_inner(target, visited))
            }
            Value::Instance(instance) => {
                let pointer = Rc::as_ptr(instance) as usize;
                if target.instance == Some(pointer) {
                    return true;
                }
                if !visited.instances.insert(pointer) {
                    return false;
                }
                instance
                    .borrow()
                    .members
                    .values()
                    .any(|member| member.value.contains_reference_to_inner(target, visited))
            }
            Value::Class(_, members) => {
                let pointer = Rc::as_ptr(members) as usize;
                if target.class == Some(pointer) {
                    return true;
                }
                if !visited.classes.insert(pointer) {
                    return false;
                }
                members
                    .values()
                    .any(|member| member.value.contains_reference_to_inner(target, visited))
            }
            Value::Iterator(state) => {
                let pointer = Rc::as_ptr(state) as usize;
                if target.iterator == Some(pointer) {
                    return true;
                }
                if !visited.iterators.insert(pointer) {
                    return false;
                }
                match &*state.borrow() {
                    IterState::Items { items, .. } => items.iter().any(|(key, value)| {
                        key.contains_reference_to_inner(target, visited)
                            || value.contains_reference_to_inner(target, visited)
                    }),
                    IterState::Count { .. } | IterState::Range(_) | IterState::Bytes { .. } => {
                        false
                    }
                }
            }
            Value::Closure(_, _, captures) => captures.iter().flatten().any(|cell| {
                let pointer = Rc::as_ptr(cell) as usize;
                if !visited.local_cells.insert(pointer) {
                    return false;
                }
                cell.borrow()
                    .as_ref()
                    .is_some_and(|value| value.contains_reference_to_inner(target, visited))
            }),
            Value::NativeMethod { receiver, .. } => {
                receiver.contains_reference_to_inner(target, visited)
            }
            _ => false,
        }
    }

    /// Serializes values to standard JSON text.
    ///
    /// This path is specially used for `json()`, object/array `to_string()` and file writing; string value and object key
    /// All are handed over to `serde_json` for escaping to ensure that the output is always in the standard JSON form of `"key":"value"`.
    pub fn to_json_string(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Empty => "null".to_string(),
            Value::Int(value) => value.to_string(),
            Value::Float(value) => {
                if value.is_finite() {
                    value.to_string()
                } else {
                    "null".to_string()
                }
            }
            Value::Bool(value) => value.to_string(),
            Value::Str(value) => {
                serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
            }
            Value::Bytes(value) => value.to_json_string(),
            #[cfg(feature = "ffi")]
            Value::Ffi(_) => "null".to_string(),
            Value::Array(values) => {
                let values = values.borrow();
                let mut text = String::with_capacity(values.len().saturating_mul(8) + 2);
                text.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    text.push_str(&value.to_json_string());
                }
                text.push(']');
                text
            }
            Value::Object(values) => {
                let values = values.borrow();
                let mut text = String::with_capacity(values.len().saturating_mul(16) + 2);
                text.push('{');
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        text.push(',');
                    }
                    text.push_str(
                        &serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                    );
                    text.push(':');
                    text.push_str(&value.to_json_string());
                }
                text.push('}');
                text
            }
            Value::Instance(instance) => {
                let instance = instance.borrow();
                let mut text = String::with_capacity(instance.members.len().saturating_mul(16) + 2);
                text.push('{');
                let mut wrote = false;
                for (key, member) in instance.members.iter() {
                    if !member.is_public {
                        continue;
                    }
                    if wrote {
                        text.push(',');
                    }
                    wrote = true;
                    text.push_str(
                        &serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                    );
                    text.push(':');
                    text.push_str(&member.value.to_json_string());
                }
                text.push('}');
                text
            }
            other => {
                serde_json::to_string(&other.to_string()).unwrap_or_else(|_| "\"\"".to_string())
            }
        }
    }

    /// Converts a value to a script-visible string.
    ///
    /// BT follows familiar scripting-language stringification rules: numbers become numeric text, while strings remain unchanged.
    /// Arrays, objects, and class instances use standard JSON so debugging output, Web responses, files, `String.parse_json()`, and external systems
    /// all see a stable representation instead of non-standard text such as `{a: 1}`.
    pub fn to_string(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Empty => String::new(),
            Value::Int(value) => value.to_string(),
            Value::Float(value) => {
                let mut text = value.to_string();
                if text.ends_with(".0") {
                    text.truncate(text.len() - 2);
                }
                text
            }
            Value::Bool(value) => value.to_string(),
            Value::Str(value) => value.clone(),
            Value::Bytes(value) => value.to_hex_string(""),
            Value::Array(_) | Value::Object(_) | Value::Instance(_) => self.to_json_string(),
            Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _) => {
                "fn".to_string()
            }
            Value::NativeFunction(name) => format!("native:{}", name),
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(function) => format!("extension:{}", function.name),
            #[cfg(feature = "extensions")]
            Value::ExtObject(object) => format!("ext_object:{}", object.type_name),
            #[cfg(feature = "ffi")]
            Value::Ffi(value) => value.to_string().to_string(),
            Value::Class(name, _) => format!("class {}", name),
            Value::Regex(_, pattern, flags) => format!("/{}/{}", pattern, flags),
            Value::Date(date) => date.format("Y-m-d H:i:s"),
            Value::Base64(_) => "base64".to_string(),
            Value::Fs(value) => value.to_string(),
            Value::Html(value) => value
                .call_method("to_string", Vec::new())
                .map(|value| value.to_string())
                .unwrap_or_default(),
            Value::Crypto(_) => "crypto".to_string(),
            Value::Url(value) => value
                .call_method("to_string", Vec::new())
                .map(|value| value.to_string())
                .unwrap_or_default(),
            Value::Path(value) => value
                .call_method("to_string", Vec::new())
                .map(|value| value.to_string())
                .unwrap_or_default(),
            Value::Bt(_) => "BT".to_string(),
            Value::Math(_) => "Math".to_string(),
            Value::Md5(value) => value
                .call_method("ok", Vec::new())
                .map(|value| value.to_string())
                .unwrap_or_default(),
            Value::Modbus(_) => "modbus".to_string(),
            Value::Mysql(_) => "mysql".to_string(),
            Value::MysqlTransaction(_) => "mysql transaction".to_string(),
            Value::Net(_) => "net".to_string(),
            Value::NetWebServer(_) => "web server".to_string(),
            Value::NetTcpServer(_) => "tcp server".to_string(),
            Value::NetTcpClient(_) => "tcp client".to_string(),
            Value::NetUdpSocket(_) => "udp socket".to_string(),
            Value::NetWsServer(_) => "ws server".to_string(),
            Value::NetWsSocket(_) => "ws socket".to_string(),
            Value::Process(_) => "process".to_string(),
            Value::Reqwest(_) => "reqwest".to_string(),
            Value::Device(_) => "device".to_string(),
            Value::Task(_) => "task".to_string(),
            Value::Timer(_) => "timer".to_string(),
            Value::Iterator(_) => "iterator".to_string(),
            Value::NativeMethod { name, .. } => format!("native:{}", name),
        }
    }

    /// Converts a value into a string visible when the user actively outputs it.
    ///
    /// `to_string()` also serves implicit conversions such as concatenation, path
    /// arguments, and template interpolation, where `empty` remains an empty string.
    /// Explicit output operations display it as `empty` to make missing values visible.
    pub fn to_output_string(&self) -> String {
        match self {
            Value::Empty => "empty".to_string(),
            other => other.to_string(),
        }
    }

    /// Returns the truthiness of this value.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null | Value::Empty => false,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            Value::Bool(value) => *value,
            Value::Str(value) => !value.is_empty(),
            Value::Bytes(value) => !value.is_empty(),
            Value::Array(values) => !values.borrow().is_empty(),
            Value::Object(values) => !values.borrow().is_empty(),
            Value::Instance(values) => !values.borrow().members.is_empty(),
            Value::Function(_)
            | Value::BoundFunction(_, _)
            | Value::Closure(_, _, _)
            | Value::NativeFunction(_) => true,
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(_) | Value::ExtObject(_) => true,
            #[cfg(feature = "ffi")]
            Value::Ffi(_) => true,
            Value::Class(_, _)
            | Value::Regex(_, _, _)
            | Value::Date(_)
            | Value::Base64(_)
            | Value::Fs(_)
            | Value::Html(_)
            | Value::Crypto(_)
            | Value::Url(_)
            | Value::Path(_)
            | Value::Bt(_)
            | Value::Math(_)
            | Value::Md5(_)
            | Value::Modbus(_)
            | Value::Mysql(_)
            | Value::MysqlTransaction(_)
            | Value::Net(_)
            | Value::NetWebServer(_)
            | Value::NetTcpServer(_)
            | Value::NetTcpClient(_)
            | Value::NetUdpSocket(_)
            | Value::NetWsServer(_)
            | Value::NetWsSocket(_)
            | Value::Process(_)
            | Value::Reqwest(_)
            | Value::Device(_)
            | Value::Task(_)
            | Value::Timer(_)
            | Value::Iterator(_)
            | Value::NativeMethod { .. } => true,
        }
    }

    /// Returns the script-visible type name.
    ///
    /// The `type()` system function and debug output share this mapping so type names
    /// are not hard-coded in several places.
    pub fn type_name(&self) -> &str {
        match self {
            Value::Null => "Null",
            Value::Empty => "Empty",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Bool(_) => "Bool",
            Value::Str(_) => "String",
            Value::Bytes(_) => "Bytes",
            Value::Array(_) => "Array",
            Value::Object(_) => "Object",
            Value::Instance(_) => "Object",
            Value::Function(_) | Value::BoundFunction(_, _) | Value::Closure(_, _, _) => "Fn",
            Value::NativeFunction(_) => "Fn",
            #[cfg(feature = "extensions")]
            Value::ExtensionFunction(_) => "Fn",
            #[cfg(feature = "extensions")]
            Value::ExtObject(object) => object.type_name.as_str(),
            #[cfg(feature = "ffi")]
            Value::Ffi(value) => value.type_name(),
            Value::Class(_, _) => "Class",
            Value::Regex(_, _, _) => "Regex",
            Value::Date(_) => "Date",
            Value::Base64(_) => "Base64",
            Value::Fs(_) => "Fs",
            Value::Html(_) => "Html",
            Value::Crypto(_) => "Crypto",
            Value::Url(_) => "Url",
            Value::Path(_) => "Path",
            Value::Bt(_) => "BT",
            Value::Math(_) => "Math",
            Value::Md5(_) => "Md5",
            Value::Modbus(_) => "Modbus",
            Value::Mysql(_) => "Mysql",
            Value::MysqlTransaction(_) => "MysqlTransaction",
            Value::Net(_) => "Net",
            Value::NetWebServer(_) => "WebServer",
            Value::NetTcpServer(_) => "TcpServer",
            Value::NetTcpClient(_) => "TcpClient",
            Value::NetUdpSocket(_) => "UdpSocket",
            Value::NetWsServer(_) => "WsServer",
            Value::NetWsSocket(_) => "WsSocket",
            Value::Process(_) => "Process",
            Value::Reqwest(_) => "Reqwest",
            Value::Device(_) => "Device",
            Value::Task(_) => "Task",
            Value::Timer(_) => "Timer",
            Value::Iterator(_) => "Iterator",
            Value::NativeMethod { .. } => "Fn",
        }
    }

    /// Converts runtime values to numbers according to BT's relaxed rules.
    ///
    /// Strings are parsed as integers first, then as floating-point numbers; invalid, empty, or missing values return `null`.
    pub fn to_number_value(&self) -> Value {
        match self {
            Value::Int(_) | Value::Float(_) => self.clone(),
            Value::Bool(value) => Value::Int(if *value { 1 } else { 0 }),
            Value::Str(value) => value
                .parse::<i64>()
                .map(Value::Int)
                .or_else(|_| value.parse::<f64>().map(Value::Float))
                .unwrap_or(Value::Null),
            Value::Null | Value::Empty => Value::Null,
            _ => Value::Null,
        }
    }

    /// Converts a runtime value to an integer.
    ///
    /// This method mainly serves `int()`, array subscripts and some prototype functions; it returns `0` when string parsing fails.
    pub fn to_i64_lossy(&self) -> i64 {
        match self {
            Value::Int(value) => *value,
            Value::Float(value) => value.trunc() as i64,
            Value::Bool(value) => i64::from(*value),
            Value::Str(value) => value.parse::<i64>().unwrap_or(0),
            Value::Null | Value::Empty => 0,
            _ => 0,
        }
    }

    /// Converts a runtime value to a floating point number.
    ///
    /// This method is used on `float()` and built-in functions that require relaxed numeric arguments, returning `0.0` if they cannot be parsed.
    pub fn to_f64_lossy(&self) -> f64 {
        match self {
            Value::Int(value) => *value as f64,
            Value::Float(value) => *value,
            Value::Bool(value) => {
                if *value {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Str(value) => value.parse::<f64>().unwrap_or(0.0),
            Value::Null | Value::Empty => 0.0,
            _ => 0.0,
        }
    }

    /// Returns whether an operation between these values requires floating-point math.
    fn is_float_math(&self, other: &Value) -> bool {
        matches!(self, Value::Float(_)) || matches!(other, Value::Float(_))
    }

    /// Reads the value as an integer, returning an error when conversion is not supported.
    fn as_i64(&self) -> Result<i64, String> {
        match self {
            Value::Int(value) => Ok(*value),
            Value::Float(value) => Ok(*value as i64),
            Value::Bool(value) => Ok(if *value { 1 } else { 0 }),
            other => Err(format!(
                "value `{}` cannot be used as an integer for this operation",
                other.to_string()
            )),
        }
    }

    /// Reads the value as a floating-point number or returns a conversion error.
    fn as_f64(&self) -> Result<f64, String> {
        match self {
            Value::Int(value) => Ok(*value as f64),
            Value::Float(value) => Ok(*value),
            Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
            other => Err(format!(
                "value `{}` cannot be used as a number",
                other.to_string()
            )),
        }
    }

    /// Adds two values, concatenating them when either operand is a string.
    pub fn add(&self, other: &Value) -> Result<Value, String> {
        if matches!(self, Value::Str(_)) || matches!(other, Value::Str(_)) {
            return Ok(Value::Str(format!(
                "{}{}",
                self.to_string(),
                other.to_string()
            )));
        }
        if self.is_float_math(other) {
            Ok(Value::Float(self.as_f64()? + other.as_f64()?))
        } else {
            Ok(Value::Int(self.as_i64()? + other.as_i64()?))
        }
    }

    /// Subtraction operation.
    pub fn sub(&self, other: &Value) -> Result<Value, String> {
        if self.is_float_math(other) {
            Ok(Value::Float(self.as_f64()? - other.as_f64()?))
        } else {
            Ok(Value::Int(self.as_i64()? - other.as_i64()?))
        }
    }

    /// Multiplication operation.
    pub fn mul(&self, other: &Value) -> Result<Value, String> {
        if self.is_float_math(other) {
            Ok(Value::Float(self.as_f64()? * other.as_f64()?))
        } else {
            Ok(Value::Int(self.as_i64()? * other.as_i64()?))
        }
    }

    /// Division operations uniformly return floating point numbers to avoid loss of precision in integer division.
    pub fn div(&self, other: &Value) -> Result<Value, String> {
        let rhs = other.as_f64()?;
        if rhs == 0.0 {
            return Err("division by zero".to_string());
        }
        Ok(Value::Float(self.as_f64()? / rhs))
    }

    /// Modulo operation.
    pub fn modulo(&self, other: &Value) -> Result<Value, String> {
        if self.is_float_math(other) {
            let rhs = other.as_f64()?;
            if rhs == 0.0 {
                return Err("modulo by zero".to_string());
            }
            Ok(Value::Float(self.as_f64()? % rhs))
        } else {
            let rhs = other.as_i64()?;
            if rhs == 0 {
                return Err("modulo by zero".to_string());
            }
            Ok(Value::Int(self.as_i64()? % rhs))
        }
    }

    /// Convert the scalar value to a number according to ordinary size comparison rules.
    ///
    /// This path is used only by `<`, `<=`, `>`, and `>=`, allowing numeric strings
    /// to participate in comparisons. It does not affect arithmetic or equality.
    fn loose_number(&self) -> Option<f64> {
        match self {
            Value::Int(value) => Some(*value as f64),
            Value::Float(value) => Some(*value),
            Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
            Value::Str(value) => {
                let value = value.trim();
                if value.is_empty() {
                    Some(0.0)
                } else {
                    value.parse::<f64>().ok()
                }
            }
            Value::Null | Value::Empty => None,
            _ => None,
        }
    }

    /// Convert a scalar value to a number according to ordinary equality rules.
    ///
    /// `==` performs numeric coercion only between different scalar types; two
    /// strings are still compared as strings.
    fn loose_equal_number(&self) -> Option<f64> {
        match self {
            Value::Int(value) => Some(*value as f64),
            Value::Float(value) => Some(*value),
            Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
            Value::Str(value) => {
                let value = value.trim();
                if value.is_empty() {
                    Some(0.0)
                } else {
                    value.parse::<f64>().ok()
                }
            }
            _ => None,
        }
    }

    /// Compares two values for loose equality.
    pub fn equal(&self, other: &Value) -> Value {
        if self == other {
            return Value::Bool(true);
        }
        let value = match (self, other) {
            (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_)) => {
                self.loose_equal_number() == other.loose_equal_number()
            }
            (Value::Str(_), Value::Str(_)) | (Value::Bool(_), Value::Bool(_)) => false,
            _ => match (self.loose_equal_number(), other.loose_equal_number()) {
                (Some(left), Some(right)) => left == right,
                _ => false,
            },
        };
        Value::Bool(value)
    }

    /// Compares two values for loose inequality.
    pub fn not_equal(&self, other: &Value) -> Value {
        match self.equal(other) {
            Value::Bool(value) => Value::Bool(!value),
            _ => Value::Bool(false),
        }
    }

    /// Compares two values for strict equality.
    ///
    /// `===` performs no string-to-number coercion; both type and value must match.
    pub fn strict_equal(&self, other: &Value) -> Value {
        Value::Bool(self == other)
    }

    /// Compares two values for strict inequality.
    pub fn strict_not_equal(&self, other: &Value) -> Value {
        Value::Bool(self != other)
    }

    /// Size comparison.
    pub fn compare_number(&self, other: &Value, op: &str) -> Result<Value, String> {
        let left = self
            .loose_number()
            .ok_or_else(|| format!("value `{}` cannot be used as a number", self.to_string()))?;
        let right = other
            .loose_number()
            .ok_or_else(|| format!("value `{}` cannot be used as a number", other.to_string()))?;
        let value = match op {
            "<" => left < right,
            "<=" => left <= right,
            ">" => left > right,
            ">=" => left >= right,
            _ => false,
        };
        Ok(Value::Bool(value))
    }

    /// Bit operations.
    pub fn bitwise(&self, other: &Value, op: &str) -> Result<Value, String> {
        let left = self.as_i64()?;
        let right = other.as_i64()?;
        let value = match op {
            "&" => left & right,
            "|" => left | right,
            "^" => left ^ right,
            "<<" => left << right,
            ">>" => left >> right,
            _ => 0,
        };
        Ok(Value::Int(value))
    }

    /// Bitwise negation.
    pub fn bitwise_not(&self) -> Result<Value, String> {
        Ok(Value::Int(!self.as_i64()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variable default value should have an independent object after cloning to avoid the default value in the bytecode cache being polluted by the caller.
    #[test]
    fn clone_mutable_literal_separates_object_references() {
        let mut source = IndexMap::new();
        source.insert("name".to_string(), Value::Str("bt".to_string()));
        let source = Value::Object(Rc::new(RefCell::new(source)));

        let cloned = source.clone_mutable_literal();
        let (Value::Object(source_values), Value::Object(cloned_values)) = (&source, &cloned)
        else {
            panic!("cloning an object default should still produce an object");
        };

        cloned_values
            .borrow_mut()
            .insert("cached".to_string(), Value::Bool(true));

        assert!(!Rc::ptr_eq(source_values, cloned_values));
        assert!(!source_values.borrow().contains_key("cached"));
    }

    /// The extended object should expose the script type name declared in bindings to avoid `type()` losing existing object information.
    #[cfg(feature = "extensions")]
    #[test]
    fn type_name_returns_extension_object_declared_name() {
        let value = Value::ExtObject(ExtObject {
            module_id: 0,
            type_id: 1,
            type_name: "Calc".to_string(),
            object_id: 7,
        });

        assert_eq!(value.type_name(), "Calc");
    }

    /// When writing an object directly back to itself, the reference scan must be able to find the cycle.
    #[test]
    fn contains_reference_to_detects_direct_self_reference() {
        let value = Value::Array(Rc::new(RefCell::new(Vec::new())));

        assert!(value.contains_reference_to(&value));
    }

    /// NativeMethod will hold the receiver, and you must continue to enter the receiver during scanning to prevent the method from being stored.
    #[test]
    fn contains_reference_to_scans_native_method_receiver() {
        let receiver = Value::Array(Rc::new(RefCell::new(Vec::new())));
        let method = Value::NativeMethod {
            receiver: Box::new(receiver.clone()),
            name: "push".to_string(),
            allow_private: false,
        };

        assert!(method.contains_reference_to(&receiver));
    }

    /// The first 64-bit Value layout baseline must remain unchanged before and after the FFI switch.
    #[test]
    fn value_size_baseline() {
        let size = std::mem::size_of::<Value>();
        println!("BT_VALUE_SIZE={}", size);
        #[cfg(feature = "ffi")]
        assert!(size >= std::mem::size_of::<BtFfiValue>());
    }
}
