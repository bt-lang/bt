//! BT lightweight background task runtime.
//!
//! This module stores only snapshots, task state, and bounded executors that can move across threads. It does not retain the VM, `Value` reference containers,
//! or `Rc<Chunk>`, keeping the synchronous interpreter's hot path out of a cross-thread sharing model.

use crate::bytecode::{
    Chunk, FunctionChunk, FunctionParam, Instruction, Register, SourceSpan, SymbolPool,
};
use crate::value::Value;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

/// Default background task queue length.
const DEFAULT_TASK_QUEUE_LIMIT: usize = 256;
/// Maximum background task queue length.
const MAX_TASK_QUEUE_LIMIT: usize = 4096;
/// The upper limit of the number of background task threads.
const MAX_TASK_WORKERS: usize = 32;
/// The maximum recursion depth allowed for a single task snapshot.
const TASK_SNAPSHOT_MAX_DEPTH: usize = 128;
/// The maximum number of nodes allowed for a single task snapshot.
const TASK_SNAPSHOT_MAX_NODES: usize = 100_000;
/// The rough number of string bytes allowed for a single task snapshot.
const TASK_SNAPSHOT_MAX_BYTES: usize = 16 * 1024 * 1024;
/// The number of completed subscriptions allowed to be mounted on a single task.
const TASK_COMPLETION_SUBSCRIBER_LIMIT: usize = 1024;

/// Lazy initialized global task executor.
static EXECUTOR: OnceLock<Result<TaskExecutor, String>> = OnceLock::new();
/// The number of tasks currently queued for execution.
static TASK_QUEUED: AtomicUsize = AtomicUsize::new(0);
/// The number of tasks currently being executed.
static TASK_RUNNING: AtomicUsize = AtomicUsize::new(0);
/// The number of tasks completed.
static TASK_COMPLETED: AtomicUsize = AtomicUsize::new(0);
/// The number of rejected task submissions.
static TASK_REJECTED: AtomicUsize = AtomicUsize::new(0);

/// A snapshot of BT values that can be passed across threads.
///
/// Snapshots cover scalars, Bytes, arrays, and ordinary objects. Arrays and objects
/// are rebuilt as fresh `Rc<RefCell<...>>` values in the background VM or when a
/// result is read. Bytes are rebuilt as a shared read-only buffer, so mutable state
/// is never shared with the VM that created the task.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TaskValueSnapshot {
    /// Explicit null value.
    Null,
    /// Missing value.
    Empty,
    /// Integer value.
    Int(i64),
    /// Floating point value.
    Float(f64),
    /// Boolean value.
    Bool(bool),
    /// String value.
    Str(String),
    /// Binary byte value.
    Bytes(Vec<u8>),
    /// Array snapshot.
    Array(Vec<TaskValueSnapshot>),
    /// Object snapshot, saving fields in insertion order.
    Object(Vec<(String, TaskValueSnapshot)>),
}

/// Snapshot of a captured variable slot.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TaskCaptureSnapshot {
    /// The capture slot exists but has not been initialized.
    Uninitialized,
    /// The capture slot already has a value.
    Value(TaskValueSnapshot),
}

/// Closure captures a scope snapshot.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskCaptureScopeSnapshot {
    /// Captured value for each function symbol slot; `None` marks a non-captured slot.
    pub(crate) slots: Vec<Option<TaskCaptureSnapshot>>,
}

/// Snapshot of function parameters that can be passed across threads.
#[derive(Debug, Clone)]
pub(crate) struct TaskFunctionParamSnapshot {
    /// Parameter name symbol number.
    pub(crate) symbol: crate::bytecode::SymbolId,
    /// Snapshot of parameter default values.
    pub(crate) default: Option<TaskValueSnapshot>,
}

/// Snapshot of a function block that can be passed across threads.
#[derive(Debug, Clone)]
pub(crate) struct TaskFunctionChunkSnapshot {
    /// Function name.
    pub(crate) name: String,
    /// Parameter list.
    pub(crate) params: Vec<TaskFunctionParamSnapshot>,
    /// Function body bytecode snapshot.
    pub(crate) chunk: Box<TaskChunkSnapshot>,
}

/// Snapshot of bytecode chunks that can be passed across threads.
#[derive(Debug, Clone)]
pub(crate) struct TaskChunkSnapshot {
    /// The source code file to which the current bytecode block belongs.
    pub(crate) source_file: String,
    /// The source code directory where the current bytecode block belongs.
    pub(crate) source_dir: String,
    /// Constant pool snapshot.
    pub(crate) constants: Vec<TaskValueSnapshot>,
    /// Symbol pool.
    pub(crate) symbols: SymbolPool,
    /// Instruction sequence.
    pub(crate) code: Vec<Instruction>,
    /// Instruction source code location.
    pub(crate) spans: Vec<Option<SourceSpan>>,
    /// Function bytecode table.
    pub(crate) functions: Vec<TaskFunctionChunkSnapshot>,
    /// The current bytecode block's own local symbol mark.
    pub(crate) local_symbols: Vec<bool>,
    /// The number of registers allocated at compile time.
    pub(crate) register_count: Register,
}

/// Background task entry snapshot.
#[derive(Debug, Clone)]
pub(crate) struct TaskFunctionSnapshot {
    /// Entry function number.
    pub(crate) function_id: usize,
    /// The bytecode block to which the entry function belongs.
    pub(crate) owner: TaskChunkSnapshot,
    /// Closure capture snapshot.
    pub(crate) captures: Option<TaskCaptureScopeSnapshot>,
    /// Snapshot of the actual parameters passed into the entry function when creating the task.
    pub(crate) args: Vec<TaskValueSnapshot>,
    /// A snapshot of global variables visible when the task is created.
    pub(crate) globals: Vec<(String, TaskValueSnapshot)>,
    /// Global constant name visible when creating a task.
    pub(crate) global_constants: Vec<String>,
    /// The project root directory when the task was created.
    pub(crate) project_root: PathBuf,
}

/// Background task execution result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TaskRunOutcome {
    /// The task returns normally.
    Success(TaskValueSnapshot),
    /// Task internal active `throw`.
    Thrown(TaskValueSnapshot),
    /// A general runtime error or executor error occurred in the task.
    Failed(String),
}

/// Background task object visible to scripts.
#[derive(Clone)]
pub struct BtTask {
    /// Task sharing status.
    shared: Arc<TaskSharedState>,
}

/// Task sharing status.
#[derive(Debug)]
struct TaskSharedState {
    /// Task status.
    state: Mutex<TaskState>,
    /// Task completion notification.
    ready: Condvar,
}

/// Task status.
#[derive(Debug)]
enum TaskState {
    /// The task has been submitted but not completed.
    Pending(Vec<TaskCompletionSubscriber>),
    /// The task has completed and its result remains available for the task's lifetime.
    Done(Arc<TaskRunOutcome>),
}

/// Task completion subscriber.
#[derive(Debug)]
struct TaskCompletionSubscriber {
    /// The lightweight event number customized by the caller.
    event: usize,
    /// Completes the event delivery channel.
    sender: SyncSender<usize>,
    /// Whether the subscription is still valid.
    active: Arc<AtomicBool>,
}

/// Token for a task-completion subscription.
///
/// Dropping or canceling the token invalidates the subscription. The worker then
/// skips event delivery, avoiding sends to a channel whose VM callback has already
/// been released.
#[derive(Debug)]
pub(crate) struct TaskCompletionSubscription {
    /// Whether the subscription is still valid.
    active: Arc<AtomicBool>,
}

/// Bounded task executor.
#[derive(Clone)]
struct TaskExecutor {
    /// Task submission channel.
    sender: SyncSender<QueuedTask>,
}

/// Statistics snapshot when the background task is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStats {
    /// Whether the global executor has been initialized.
    pub executor_started: bool,
    /// The maximum length of the current task queue.
    pub queue_limit: usize,
    /// The number of current task worker threads.
    pub workers: usize,
    /// The number of tasks currently queued for execution.
    pub queued: usize,
    /// The number of tasks currently being executed.
    pub running: usize,
    /// The number of tasks completed.
    pub completed: usize,
    /// The number of rejected task submissions.
    pub rejected: usize,
}

/// Tasks queued for background execution.
struct QueuedTask {
    /// Script side task status.
    task: BtTask,
    /// Closure to execute in the background.
    job: TaskJob,
}

/// Background task closure type.
type TaskJob = Box<dyn FnOnce() -> TaskRunOutcome + Send + 'static>;

/// Safety count and loop detection status during snapshot recursion.
#[derive(Default)]
struct TaskSnapshotVisit {
    /// The array pointer in the current recursion stack.
    arrays: HashSet<usize>,
    /// The object pointer in the current recursive stack.
    objects: HashSet<usize>,
    /// The number of value nodes scanned.
    nodes: usize,
    /// Number of string and object key bytes scanned.
    bytes: usize,
}

impl TaskValueSnapshot {
    /// Creates a sendable snapshot from a VM value.
    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        let mut visit = TaskSnapshotVisit::default();
        Self::from_value_inner(value, 0, &mut visit)
    }

    /// Recursively creates sendable snapshots.
    fn from_value_inner(
        value: &Value,
        depth: usize,
        visit: &mut TaskSnapshotVisit,
    ) -> Result<Self, String> {
        visit.enter_value(depth)?;
        match value {
            Value::Null => Ok(Self::Null),
            Value::Empty => Ok(Self::Empty),
            Value::Int(value) => Ok(Self::Int(*value)),
            Value::Float(value) => Ok(Self::Float(*value)),
            Value::Bool(value) => Ok(Self::Bool(*value)),
            Value::Str(value) => {
                visit.add_bytes(value.len())?;
                Ok(Self::Str(value.clone()))
            }
            Value::Bytes(value) => {
                visit.add_bytes(value.len())?;
                Ok(Self::Bytes(value.as_slice().to_vec()))
            }
            Value::Array(values) => {
                let pointer = Rc::as_ptr(values) as usize;
                if !visit.arrays.insert(pointer) {
                    return Err(
                        "Task snapshots do not support circular array references".to_string()
                    );
                }
                let values = values.borrow();
                let mut items = Vec::with_capacity(values.len());
                for value in values.iter() {
                    items.push(Self::from_value_inner(value, depth + 1, visit)?);
                }
                visit.arrays.remove(&pointer);
                Ok(Self::Array(items))
            }
            Value::Object(values) => {
                let pointer = Rc::as_ptr(values) as usize;
                if !visit.objects.insert(pointer) {
                    return Err(
                        "Task snapshots do not support circular object references".to_string()
                    );
                }
                let values = values.borrow();
                let mut items = Vec::with_capacity(values.len());
                for (key, value) in values.iter() {
                    visit.add_bytes(key.len())?;
                    items.push((
                        key.clone(),
                        Self::from_value_inner(value, depth + 1, visit)?,
                    ));
                }
                visit.objects.remove(&pointer);
                Ok(Self::Object(items))
            }
            other => Err(format!(
                "Task snapshots do not support values of type `{}`",
                other.type_name()
            )),
        }
    }

    /// Restores ordinary values inside the current VM.
    pub(crate) fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Empty => Value::Empty,
            Self::Int(value) => Value::Int(*value),
            Self::Float(value) => Value::Float(*value),
            Self::Bool(value) => Value::Bool(*value),
            Self::Str(value) => Value::Str(value.clone()),
            Self::Bytes(value) => {
                Value::Bytes(crate::libs::bytes::BtBytes::unchecked(value.clone()))
            }
            Self::Array(values) => Value::Array(Rc::new(RefCell::new(
                values.iter().map(Self::to_value).collect(),
            ))),
            Self::Object(values) => {
                let mut object = IndexMap::with_capacity(values.len());
                for (key, value) in values {
                    object.insert(key.clone(), value.to_value());
                }
                Value::Object(Rc::new(RefCell::new(object)))
            }
        }
    }
}

impl TaskChunkSnapshot {
    /// Creates a sendable snapshot from a chunk of bytecode.
    pub(crate) fn from_chunk(chunk: &Chunk) -> Result<Self, String> {
        let mut constants = Vec::with_capacity(chunk.constants.len());
        for value in &chunk.constants {
            constants.push(TaskValueSnapshot::from_value(value).map_err(|message| {
                format!(
                    "Task function bytecode constants cannot be snapshotted: {}",
                    message
                )
            })?);
        }

        let mut functions = Vec::with_capacity(chunk.functions.len());
        for function in &chunk.functions {
            functions.push(TaskFunctionChunkSnapshot {
                name: function.name.clone(),
                params: snapshot_params(&function.params)?,
                chunk: Box::new(Self::from_chunk(&function.chunk)?),
            });
        }

        Ok(Self {
            source_file: chunk.source_file.clone(),
            source_dir: chunk.source_dir.clone(),
            constants,
            symbols: chunk.symbols.clone(),
            code: chunk.code.clone(),
            spans: chunk.spans.clone(),
            functions,
            local_symbols: chunk.local_symbols.clone(),
            register_count: chunk.register_count,
        })
    }

    /// Rebuilds an ordinary bytecode block in the current thread.
    pub(crate) fn to_chunk(&self) -> Chunk {
        Chunk {
            source_file: self.source_file.clone(),
            source_dir: self.source_dir.clone(),
            constants: self
                .constants
                .iter()
                .map(TaskValueSnapshot::to_value)
                .collect(),
            symbols: self.symbols.clone(),
            code: self.code.clone(),
            spans: self.spans.clone(),
            functions: self
                .functions
                .iter()
                .map(|function| FunctionChunk {
                    name: function.name.clone(),
                    params: function
                        .params
                        .iter()
                        .map(|param| FunctionParam {
                            symbol: param.symbol,
                            default: param.default.as_ref().map(TaskValueSnapshot::to_value),
                        })
                        .collect(),
                    chunk: Box::new(function.chunk.to_chunk()),
                })
                .collect(),
            local_symbols: self.local_symbols.clone(),
            register_count: self.register_count,
        }
    }
}

impl BtTask {
    /// Creates a task that is waiting for an execution result.
    fn pending() -> Self {
        Self {
            shared: Arc::new(TaskSharedState {
                state: Mutex::new(TaskState::Pending(Vec::new())),
                ready: Condvar::new(),
            }),
        }
    }

    /// Stores the completion result and wakes all waiters.
    fn complete(&self, outcome: TaskRunOutcome) {
        let subscribers = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &mut *state {
                TaskState::Pending(subscribers) => {
                    let subscribers = std::mem::take(subscribers);
                    *state = TaskState::Done(Arc::new(outcome));
                    subscribers
                }
                TaskState::Done(_) => return,
            }
        };
        self.shared.ready.notify_all();
        for subscriber in subscribers {
            if subscriber.active.load(Ordering::Acquire) {
                let _ = subscriber.sender.try_send(subscriber.event);
            }
        }
    }

    /// Determines whether the task has been completed.
    pub(crate) fn done(&self) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        matches!(*state, TaskState::Done(_))
    }

    /// Blocks until the task completes, then returns the saved result.
    pub(crate) fn wait(&self) -> Arc<TaskRunOutcome> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let TaskState::Done(outcome) = &*state {
                return outcome.clone();
            }
            state = self
                .shared
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Reads a completed result without blocking.
    pub(crate) fn result(&self) -> Option<Arc<TaskRunOutcome>> {
        let state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*state {
            TaskState::Done(outcome) => Some(outcome.clone()),
            TaskState::Pending(_) => None,
        }
    }

    /// Subscribe to task completion events.
    ///
    /// Returns `Ok(None)` if the task has already completed, in which case the caller
    /// should read the saved result directly. Otherwise, the returned token keeps
    /// the subscription active for its lifetime.
    pub(crate) fn subscribe(
        &self,
        event: usize,
        sender: SyncSender<usize>,
    ) -> Result<Option<TaskCompletionSubscription>, String> {
        let active = Arc::new(AtomicBool::new(true));
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *state {
            TaskState::Done(_) => Ok(None),
            TaskState::Pending(subscribers) => {
                subscribers.retain(|subscriber| subscriber.active.load(Ordering::Acquire));
                if subscribers.len() >= TASK_COMPLETION_SUBSCRIBER_LIMIT {
                    return Err(format!(
                        "Task completion subscriptions exceed the limit of {}",
                        TASK_COMPLETION_SUBSCRIBER_LIMIT
                    ));
                }
                subscribers.push(TaskCompletionSubscriber {
                    event,
                    sender,
                    active: active.clone(),
                });
                Ok(Some(TaskCompletionSubscription { active }))
            }
        }
    }
}

impl TaskCompletionSubscription {
    /// Actively cancel the completion subscription.
    pub(crate) fn cancel(&self) {
        self.active.store(false, Ordering::Release);
    }
}

impl Drop for TaskCompletionSubscription {
    /// Expires the subscription when the token is dropped.
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Debug for BtTask {
    /// Outputs the task debugging status.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtTask")
            .field("done", &self.done())
            .finish()
    }
}

impl PartialEq for BtTask {
    /// Compare by task object identity.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl TaskSnapshotVisit {
    /// Enter a value node and check the size limit.
    fn enter_value(&mut self, depth: usize) -> Result<(), String> {
        if depth > TASK_SNAPSHOT_MAX_DEPTH {
            return Err(format!(
                "The task snapshot recursion depth exceeds {}",
                TASK_SNAPSHOT_MAX_DEPTH
            ));
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > TASK_SNAPSHOT_MAX_NODES {
            return Err(format!(
                "The number of task snapshot values exceeds {}",
                TASK_SNAPSHOT_MAX_NODES
            ));
        }
        Ok(())
    }

    /// Increase the string bytes and check the size limit.
    fn add_bytes(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > TASK_SNAPSHOT_MAX_BYTES {
            return Err(format!(
                "Task snapshot string data exceeds {} bytes",
                TASK_SNAPSHOT_MAX_BYTES
            ));
        }
        Ok(())
    }
}

/// Submits a background task.
pub(crate) fn submit(
    job: impl FnOnce() -> TaskRunOutcome + Send + 'static,
) -> Result<BtTask, String> {
    let executor = match executor() {
        Ok(executor) => executor,
        Err(err) => {
            TASK_REJECTED.fetch_add(1, Ordering::Relaxed);
            return Err(err);
        }
    };
    let task = BtTask::pending();
    let queued = QueuedTask {
        task: task.clone(),
        job: Box::new(job),
    };
    match executor.sender.try_send(queued) {
        Ok(()) => {
            TASK_QUEUED.fetch_add(1, Ordering::Relaxed);
            Ok(task)
        }
        Err(TrySendError::Full(_)) => {
            TASK_REJECTED.fetch_add(1, Ordering::Relaxed);
            Err(format!(
                "The background task queue is full (limit: {})",
                configured_queue_limit()
            ))
        }
        Err(TrySendError::Disconnected(_)) => {
            TASK_REJECTED.fetch_add(1, Ordering::Relaxed);
            Err("The background task executor has stopped".to_string())
        }
    }
}

/// Returns the statistics snapshot when the background task is running.
pub fn stats() -> TaskStats {
    TaskStats {
        executor_started: EXECUTOR.get().is_some(),
        queue_limit: configured_queue_limit(),
        workers: configured_worker_count(),
        queued: TASK_QUEUED.load(Ordering::Relaxed),
        running: TASK_RUNNING.load(Ordering::Relaxed),
        completed: TASK_COMPLETED.load(Ordering::Relaxed),
        rejected: TASK_REJECTED.load(Ordering::Relaxed),
    }
}

/// Returns the lazy-loaded global task executor.
fn executor() -> Result<&'static TaskExecutor, String> {
    match EXECUTOR.get_or_init(init_executor) {
        Ok(executor) => Ok(executor),
        Err(message) => Err(message.clone()),
    }
}

/// Initializes the global task executor.
fn init_executor() -> Result<TaskExecutor, String> {
    let (sender, receiver) = mpsc::sync_channel(configured_queue_limit());
    let receiver = Arc::new(Mutex::new(receiver));
    let workers = configured_worker_count();
    let mut started = 0usize;
    for index in 0..workers {
        let receiver = receiver.clone();
        let name = format!("bt-task-{}", index + 1);
        if thread::Builder::new()
            .name(name)
            .spawn(move || worker_loop(receiver))
            .is_ok()
        {
            started += 1;
        }
    }
    if started == 0 {
        Err("Background task executor thread creation failed".to_string())
    } else {
        Ok(TaskExecutor { sender })
    }
}

/// Background worker thread loop.
fn worker_loop(receiver: Arc<Mutex<Receiver<QueuedTask>>>) {
    loop {
        let task = {
            let receiver = receiver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            receiver.recv()
        };
        let Ok(task) = task else {
            break;
        };
        TASK_QUEUED.fetch_sub(1, Ordering::Relaxed);
        TASK_RUNNING.fetch_add(1, Ordering::Relaxed);
        run_queued_task(task);
        TASK_RUNNING.fetch_sub(1, Ordering::Relaxed);
        TASK_COMPLETED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Execute queued tasks and catch panics.
fn run_queued_task(queued: QueuedTask) {
    let QueuedTask { task, job } = queued;
    let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || job())) {
        Ok(outcome) => outcome,
        Err(payload) => {
            TaskRunOutcome::Failed(format!("Background task panic: {}", panic_message(payload)))
        }
    };
    task.complete(outcome);
}

/// Parse panic text.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "Unknown error".to_string()
    }
}

/// Reads the task queue length configuration.
fn configured_queue_limit() -> usize {
    std::env::var("BT_TASK_QUEUE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_TASK_QUEUE_LIMIT))
        .unwrap_or(DEFAULT_TASK_QUEUE_LIMIT)
}

/// Read task worker thread number configuration.
fn configured_worker_count() -> usize {
    let cpus = thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .max(1);
    std::env::var("BT_TASK_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(cpus)
        .min(cpus)
        .min(MAX_TASK_WORKERS)
        .max(1)
}

/// Snapshot function parameter list.
fn snapshot_params(params: &[FunctionParam]) -> Result<Vec<TaskFunctionParamSnapshot>, String> {
    let mut output = Vec::with_capacity(params.len());
    for param in params {
        output.push(TaskFunctionParamSnapshot {
            symbol: param.symbol,
            default: param
                .default
                .as_ref()
                .map(TaskValueSnapshot::from_value)
                .transpose()
                .map_err(|message| {
                    format!(
                        "Task function parameter defaults cannot be snapshotted: {}",
                        message
                    )
                })?,
        });
    }
    Ok(output)
}
