//! Project-level shared extension service.
//!
//! The shared runtime operates only at the extension call boundary. The calling thread encodes VM
//! `Value`s as BtValueBinary bytes; worker threads receive only call metadata and byte arrays, then
//! return WASM result bytes to the calling thread for decoding. This keeps non-`Send` VM values out
//! of cross-thread queues.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::extensions::manager::ExtObject;
use crate::extensions::manifest::ExtensionRuntime;
use crate::extensions::package::ExtensionPackage;
use crate::extensions::registry::{
    is_extension_object_return, ExtensionModuleId, RegisteredFunction, RegisteredMethod,
};
use crate::extensions::wasm_runner::{WasmRunnerModule, WasmRunnerRuntime};
use crate::value::Value;

/// Statistics snapshot for a shared extension service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionServiceStats {
    /// Number of workers configured for the shared service.
    pub workers: u32,
    /// Number of calls currently waiting for a worker.
    pub queued: u64,
    /// Number of calls currently executing in workers.
    pub running: u64,
    /// Number of calls whose calling threads are still awaiting results.
    pub inflight: u64,
    /// Total calls accepted and submitted to queues.
    pub submitted: u64,
    /// Total calls for which workers successfully returned result bytes.
    pub completed: u64,
    /// Total calls that failed during worker execution.
    pub failed: u64,
    /// Total calls rejected because of shutdown, a full queue, or the inflight limit.
    pub rejected: u64,
    /// Total calls whose calling threads timed out awaiting results.
    pub timed_out: u64,
    /// Number of objects currently held by the host object routing table.
    pub objects: u64,
    /// Number of objects currently held by the host routing table for each worker.
    pub worker_objects: Vec<u32>,
}

/// Atomic statistics counters for a shared extension service.
#[derive(Default)]
struct ExtensionServiceStatsInner {
    /// Number of calls currently waiting for a worker.
    queued: AtomicU64,
    /// Number of calls currently executing in workers.
    running: AtomicU64,
    /// Number of calls whose calling threads are still awaiting results.
    inflight: AtomicU64,
    /// Total calls accepted and submitted to queues.
    submitted: AtomicU64,
    /// Total calls for which workers successfully returned result bytes.
    completed: AtomicU64,
    /// Total calls that failed during worker execution.
    failed: AtomicU64,
    /// Total calls rejected because of shutdown, a full queue, or the inflight limit.
    rejected: AtomicU64,
    /// Total calls whose calling threads timed out awaiting results.
    timed_out: AtomicU64,
}

impl ExtensionServiceStatsInner {
    /// Reads the current statistics snapshot.
    fn snapshot(
        &self,
        workers: u32,
        objects: u64,
        worker_objects: Vec<u32>,
    ) -> ExtensionServiceStats {
        ExtensionServiceStats {
            workers,
            queued: self.queued.load(Ordering::Acquire),
            running: self.running.load(Ordering::Acquire),
            inflight: self.inflight.load(Ordering::Acquire),
            submitted: self.submitted.load(Ordering::Acquire),
            completed: self.completed.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
            rejected: self.rejected.load(Ordering::Acquire),
            timed_out: self.timed_out.load(Ordering::Acquire),
            objects,
            worker_objects,
        }
    }
}

/// A single call message in a shared worker queue.
struct ExtensionServiceCall {
    /// WASM ABI call ID.
    call_id: u32,
    /// Call label used in error messages and statistics diagnostics.
    call_label: String,
    /// Return type text declared by the bindings.
    returns: String,
    /// Encoded BtValueBinary argument bytes.
    encoded_args: Vec<u8>,
    /// Bounded reply channel on which the calling thread awaits the result.
    reply: SyncSender<Result<Vec<u8>, String>>,
}

/// Host-level extension object route.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostObjectRoute {
    /// ID of the shared worker that created the object.
    worker_id: u32,
    /// Local object ID allocated by the worker's WASM SDK ObjectStore.
    local_object_id: u64,
    /// Extension object type ID.
    type_id: u32,
    /// Generation assigned by the host, reserved to guard against stale handle reuse.
    generation: u64,
}

/// Host-level object handle table maintained by the shared service.
struct HostObjectTable {
    /// Mapping from script-visible host object IDs to worker-local objects.
    routes: HashMap<u64, HostObjectRoute>,
    /// Number of objects currently held by the host routing table for each worker.
    worker_object_counts: Vec<u32>,
    /// Next script-visible host object ID; zero is reserved.
    next_host_object_id: u64,
    /// Next object route generation.
    next_generation: u64,
    /// Service-wide object limit.
    max_objects: u32,
    /// Per-worker object limit.
    max_worker_objects: u32,
}

impl HostObjectTable {
    /// Creates an object routing table from the manifest runtime configuration.
    fn new(runtime: ExtensionRuntime) -> Self {
        Self {
            routes: HashMap::new(),
            worker_object_counts: vec![0; runtime.workers as usize],
            next_host_object_id: 1,
            next_generation: 1,
            max_objects: runtime.max_objects,
            max_worker_objects: runtime.max_worker_objects,
        }
    }

    /// Registers a worker-local object and rewrites it as a script-visible host object handle.
    fn register(
        &mut self,
        module_name: &str,
        worker_id: u32,
        object: ExtObject,
    ) -> Result<ExtObject, String> {
        let worker_index = self.worker_index(module_name, worker_id)?;
        if self.routes.len() >= self.max_objects as usize {
            return Err(format!(
                "extension `{}` shared service exceeds object limit max_objects={}",
                module_name, self.max_objects
            ));
        }
        if self.worker_object_counts[worker_index] >= self.max_worker_objects {
            return Err(format!(
                "extension `{}` shared worker {} exceeds object limit max_worker_objects={}",
                module_name, worker_id, self.max_worker_objects
            ));
        }
        let host_object_id = self.next_host_object_id;
        self.next_host_object_id = self.next_host_object_id.checked_add(1).ok_or_else(|| {
            format!(
                "extension `{}` exhausted shared host object IDs",
                module_name
            )
        })?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.checked_add(1).ok_or_else(|| {
            format!(
                "extension `{}` exhausted shared host object generations",
                module_name
            )
        })?;
        let previous = self.routes.insert(
            host_object_id,
            HostObjectRoute {
                worker_id,
                local_object_id: object.object_id,
                type_id: object.type_id,
                generation,
            },
        );
        if previous.is_some() {
            return Err(format!(
                "Extension `{}` shared host object ID {} already exists",
                module_name, host_object_id
            ));
        }
        self.worker_object_counts[worker_index] += 1;
        Ok(ExtObject {
            object_id: host_object_id,
            ..object
        })
    }

    /// Resolves a host object handle returned by the script.
    fn resolve(&self, module_name: &str, object: &ExtObject) -> Result<HostObjectRoute, String> {
        let route = self.routes.get(&object.object_id).ok_or_else(|| {
            format!(
                "extension `{}` shared object `{}` handle {} is no longer valid",
                module_name, object.type_name, object.object_id
            )
        })?;
        if route.type_id != object.type_id {
            return Err(format!(
                "extension `{}` shared object handle {} has type ID mismatch: expected {}, got {}",
                module_name, object.object_id, route.type_id, object.type_id
            ));
        }
        Ok(route.clone())
    }

    /// Removes a host object route; remains idempotent if the object is already absent.
    fn remove(&mut self, module_name: &str, object: &ExtObject) {
        let Some(route) = self.routes.remove(&object.object_id) else {
            return;
        };
        if let Ok(worker_index) = self.worker_index(module_name, route.worker_id) {
            self.worker_object_counts[worker_index] =
                self.worker_object_counts[worker_index].saturating_sub(1);
        }
    }

    /// Returns a snapshot of route and per-worker object counts.
    fn stats_snapshot(&self) -> (u64, Vec<u32>) {
        (self.routes.len() as u64, self.worker_object_counts.clone())
    }

    /// Converts a worker ID to an object-count array index.
    fn worker_index(&self, module_name: &str, worker_id: u32) -> Result<usize, String> {
        let worker_index = worker_id as usize;
        if worker_index >= self.worker_object_counts.len() {
            return Err(format!(
                "extension `{}` shared object route references nonexistent worker {}",
                module_name, worker_id
            ));
        }
        Ok(worker_index)
    }
}

/// Guard for the inflight count while a calling thread waits.
struct InflightGuard<'a> {
    /// Inflight counter to decrement when leaving scope.
    counter: &'a AtomicU64,
}

impl Drop for InflightGuard<'_> {
    /// Decrements the inflight count after completion or timeout.
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Project-level shared extension service.
pub struct ExtensionService {
    /// Extension package name used in error messages.
    module_name: String,
    /// Shared WASM module metadata.
    module: Arc<WasmRunnerModule>,
    /// Validated shared runtime configuration from the manifest.
    runtime: ExtensionRuntime,
    /// Bounded call queue sender for each worker; `None` means the service is shut down.
    senders: Mutex<Option<Vec<SyncSender<ExtensionServiceCall>>>>,
    /// Shared worker thread handles.
    workers: Mutex<Vec<JoinHandle<()>>>,
    /// Independent shared WASM timeout interrupt flag for each worker.
    timeout_flags: Vec<Arc<AtomicBool>>,
    /// Receivers retained when tests do not start workers, preventing immediate channel closure.
    #[cfg(test)]
    test_receivers: Option<Vec<Arc<Mutex<Receiver<ExtensionServiceCall>>>>>,
    /// Host-level object handle routing table.
    object_routes: Mutex<HostObjectTable>,
    /// Round-robin worker allocation cursor for entry function calls.
    next_worker: AtomicU64,
    /// Service statistics counters.
    stats: Arc<ExtensionServiceStatsInner>,
    /// Whether service shutdown has begun.
    closed: AtomicBool,
}

impl fmt::Debug for ExtensionService {
    /// Emits debug information without thread handles or internal channel state.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionService")
            .field("module_name", &self.module_name)
            .field("runtime", &self.runtime)
            .field("stats", &self.stats())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl ExtensionService {
    /// Creates a project-level service from a shared WASM extension package.
    pub fn from_package(
        module_id: ExtensionModuleId,
        project_root: &Path,
        package: &ExtensionPackage,
    ) -> Result<Self, String> {
        let module = WasmRunnerModule::from_shared_package(module_id, project_root, package)?;
        Self::from_module(Arc::new(module), package.manifest.runtime, true)
    }

    /// Creates a service from WASM module metadata.
    fn from_module(
        module: Arc<WasmRunnerModule>,
        runtime: ExtensionRuntime,
        start_workers: bool,
    ) -> Result<Self, String> {
        let queue_limit = runtime.queue_limit as usize;
        let worker_count = runtime.workers as usize;
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        senders.try_reserve(worker_count).map_err(|_| {
            format!(
                "failed to allocate shared service worker queue senders for extension `{}`",
                module.module_name()
            )
        })?;
        receivers.try_reserve(worker_count).map_err(|_| {
            format!(
                "failed to allocate shared service worker queue receivers for extension `{}`",
                module.module_name()
            )
        })?;
        for _ in 0..runtime.workers {
            let (sender, receiver) = sync_channel(queue_limit);
            senders.push(sender);
            receivers.push(Arc::new(Mutex::new(receiver)));
        }
        let mut timeout_flags = Vec::new();
        timeout_flags.try_reserve(worker_count).map_err(|_| {
            format!(
                "failed to allocate shared service worker timeout flags for extension `{}`",
                module.module_name()
            )
        })?;
        for _ in 0..runtime.workers {
            timeout_flags.push(Arc::new(AtomicBool::new(false)));
        }
        #[cfg(test)]
        let test_receivers = if start_workers {
            None
        } else {
            Some(receivers.clone())
        };
        let stats = Arc::new(ExtensionServiceStatsInner::default());
        let mut workers: Vec<JoinHandle<()>> = Vec::new();
        if start_workers {
            workers.try_reserve(runtime.workers as usize).map_err(|_| {
                format!(
                    "failed to allocate shared service worker handles for extension `{}`",
                    module.module_name()
                )
            })?;
            for (worker_id, receiver) in receivers.iter().cloned().enumerate() {
                let worker_id = worker_id as u32;
                let timeout_flag = timeout_flags[worker_id as usize].clone();
                let worker = match spawn_service_worker(
                    worker_id,
                    module.clone(),
                    receiver,
                    stats.clone(),
                    timeout_flag,
                ) {
                    Ok(worker) => worker,
                    Err(err) => {
                        drop(senders);
                        for worker in workers.drain(..) {
                            let _ = worker.join();
                        }
                        return Err(err);
                    }
                };
                workers.push(worker);
            }
        }
        Ok(Self {
            module_name: module.module_name().to_string(),
            module,
            runtime,
            senders: Mutex::new(Some(senders)),
            workers: Mutex::new(workers),
            timeout_flags,
            #[cfg(test)]
            test_receivers,
            object_routes: Mutex::new(HostObjectTable::new(runtime)),
            next_worker: AtomicU64::new(0),
            stats,
            closed: AtomicBool::new(false),
        })
    }

    /// Calls a shared extension entry function.
    pub fn call_function(
        &self,
        function: &RegisteredFunction,
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Value, String> {
        if !self.module.contains_function_id(function.function_id) {
            return Err(format!(
                "extension `{}` has no WASM entry function ID `{}`",
                self.module_name, function.function_id
            ));
        }
        let args =
            self.module
                .prepare_call_args(&function.name, &function.params, args, source_dir)?;
        let encoded_args = self.module.encode_call_args(&function.name, args)?;
        let (worker_id, result) = self.call_encoded(
            None,
            function.function_id,
            &function.name,
            &function.returns,
            encoded_args,
        )?;
        let value = self
            .module
            .decode_call_result(&function.name, &function.returns, &result)?;
        self.rewrite_return_value(worker_id, &function.name, &function.returns, value)
    }

    /// Calls a shared extension object method.
    pub fn call_method(
        &self,
        object: &ExtObject,
        method: &RegisteredMethod,
        args: Vec<Value>,
        source_dir: &Path,
    ) -> Result<Value, String> {
        if method.module_id != object.module_id || method.type_id != object.type_id {
            return Err(format!(
                "extension `{}` shared object method `{}` registration does not match",
                self.module_name, method.name
            ));
        }
        let route = self.resolve_host_object(object)?;
        let call_label = format!("{}.{}", object.type_name, method.name);
        let args = self
            .module
            .prepare_call_args(&call_label, &method.params, args, source_dir)?;
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(Value::ExtObject(ExtObject {
            object_id: route.local_object_id,
            ..object.clone()
        }));
        call_args.extend(args);
        let encoded_args = self.module.encode_call_args(&call_label, call_args)?;
        let (worker_id, result) = self.call_encoded(
            Some(route.worker_id),
            method.method_id,
            &call_label,
            &method.returns,
            encoded_args,
        )?;
        let value = self
            .module
            .decode_call_result(&call_label, &method.returns, &result)?;
        let value = self.rewrite_return_value(worker_id, &call_label, &method.returns, value)?;
        if method.lifecycle.is_dispose() {
            self.remove_host_object(object);
        }
        Ok(value)
    }

    /// Returns a service statistics snapshot.
    pub fn stats(&self) -> ExtensionServiceStats {
        let (objects, worker_objects) = self
            .object_routes
            .lock()
            .map(|routes| routes.stats_snapshot())
            .unwrap_or_else(|_| (0, Vec::new()));
        self.stats
            .snapshot(self.runtime.workers, objects, worker_objects)
    }

    /// Shuts down the service and waits for workers to exit.
    pub fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut senders) = self.senders.lock() {
            senders.take();
        }
        if let Ok(mut workers) = self.workers.lock() {
            while let Some(worker) = workers.pop() {
                let _ = worker.join();
            }
        }
    }

    /// Submits encoded call bytes and waits for result bytes.
    fn call_encoded(
        &self,
        target_worker: Option<u32>,
        call_id: u32,
        call_label: &str,
        returns: &str,
        encoded_args: Vec<u8>,
    ) -> Result<(u32, Vec<u8>), String> {
        let _inflight = self.acquire_inflight(call_label)?;
        let worker_id = match target_worker {
            Some(worker_id) => {
                self.validate_worker_id(worker_id)?;
                worker_id
            }
            None => self.next_worker_id(),
        };
        let sender = match self.active_sender(worker_id) {
            Ok(sender) => sender,
            Err(err) => {
                self.stats.rejected.fetch_add(1, Ordering::AcqRel);
                return Err(err);
            }
        };
        let (reply, receiver) = sync_channel(1);
        let message = ExtensionServiceCall {
            call_id,
            call_label: call_label.to_string(),
            returns: returns.to_string(),
            encoded_args,
            reply,
        };
        self.acquire_queue_slot(call_label)?;
        match sender.try_send(message) {
            Ok(()) => {
                self.stats.submitted.fetch_add(1, Ordering::AcqRel);
            }
            Err(TrySendError::Full(_message)) => {
                self.stats.queued.fetch_sub(1, Ordering::AcqRel);
                self.stats.rejected.fetch_add(1, Ordering::AcqRel);
                return Err(format!(
                    "extension `{}` shared service queue is full, queue_limit={}",
                    self.module_name, self.runtime.queue_limit
                ));
            }
            Err(TrySendError::Disconnected(_message)) => {
                self.stats.queued.fetch_sub(1, Ordering::AcqRel);
                self.stats.rejected.fetch_add(1, Ordering::AcqRel);
                return Err(format!(
                    "extension `{}` shared service is shut down",
                    self.module_name
                ));
            }
        }

        match receiver.recv_timeout(Duration::from_millis(self.runtime.call_timeout_ms)) {
            Ok(result) => result.map(|bytes| (worker_id, bytes)),
            Err(RecvTimeoutError::Timeout) => {
                self.stats.timed_out.fetch_add(1, Ordering::AcqRel);
                self.interrupt_worker(worker_id);
                Err(format!(
                    "extension `{}` shared call `{}` waited over {}ms for a result; worker {} was interrupted",
                    self.module_name, call_label, self.runtime.call_timeout_ms, worker_id
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(format!(
                "extension `{}` shared worker has stopped",
                self.module_name
            )),
        }
    }

    /// Returns the open queue sender for the specified worker.
    fn active_sender(&self, worker_id: u32) -> Result<SyncSender<ExtensionServiceCall>, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err(format!(
                "extension `{}` shared service is shut down",
                self.module_name
            ));
        }
        let senders = self.senders.lock().map_err(|_| {
            format!(
                "extension `{}` shared service queue lock is poisoned",
                self.module_name
            )
        })?;
        if self.closed.load(Ordering::Acquire) {
            return Err(format!(
                "extension `{}` shared service is shut down",
                self.module_name
            ));
        }
        let senders = senders.as_ref().ok_or_else(|| {
            format!(
                "extension `{}` shared service is shut down",
                self.module_name
            )
        })?;
        let worker_index = self.validate_worker_id(worker_id)?;
        senders.get(worker_index).cloned().ok_or_else(|| {
            format!(
                "extension `{}` shared worker {} queue does not exist",
                self.module_name, worker_id
            )
        })
    }

    /// Increments the inflight count and checks its limit.
    fn acquire_inflight(&self, call_label: &str) -> Result<InflightGuard<'_>, String> {
        let max_inflight = u64::from(self.runtime.max_inflight_calls);
        match self
            .stats
            .inflight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current >= max_inflight {
                    None
                } else {
                    Some(current + 1)
                }
            }) {
            Ok(_) => Ok(InflightGuard {
                counter: &self.stats.inflight,
            }),
            Err(_) => {
                self.stats.rejected.fetch_add(1, Ordering::AcqRel);
                Err(format!(
                    "extension `{}` shared call `{}` exceeds inflight limit max_inflight_calls={}",
                    self.module_name, call_label, self.runtime.max_inflight_calls
                ))
            }
        }
    }

    /// Increments the service-wide queued count and checks the waiting queue limit.
    fn acquire_queue_slot(&self, call_label: &str) -> Result<(), String> {
        let queue_limit = u64::from(self.runtime.queue_limit);
        match self
            .stats
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current >= queue_limit {
                    None
                } else {
                    Some(current + 1)
                }
            }) {
            Ok(_) => Ok(()),
            Err(_) => {
                self.stats.rejected.fetch_add(1, Ordering::AcqRel);
                Err(format!(
                    "extension `{}` shared call `{}` waiting queue is full, queue_limit={}",
                    self.module_name, call_label, self.runtime.queue_limit
                ))
            }
        }
    }

    /// Selects a worker for an entry function call using round-robin allocation.
    fn next_worker_id(&self) -> u32 {
        let worker_count = u64::from(self.runtime.workers);
        (self.next_worker.fetch_add(1, Ordering::AcqRel) % worker_count) as u32
    }

    /// Validates a worker ID and returns its array index.
    fn validate_worker_id(&self, worker_id: u32) -> Result<usize, String> {
        if worker_id >= self.runtime.workers {
            return Err(format!(
                "extension `{}` shared worker {} does not exist",
                self.module_name, worker_id
            ));
        }
        Ok(worker_id as usize)
    }

    /// Marks the target worker as timed out and triggers a shared WASM epoch check.
    fn interrupt_worker(&self, worker_id: u32) {
        if let Some(flag) = self.timeout_flags.get(worker_id as usize) {
            flag.store(true, Ordering::Release);
            self.module.interrupt_epoch();
        }
    }

    /// Rewrites a worker-local extension object as a host object handle.
    fn rewrite_return_value(
        &self,
        worker_id: u32,
        call_label: &str,
        returns: &str,
        value: Value,
    ) -> Result<Value, String> {
        if !is_extension_object_return(returns) {
            return Ok(value);
        }
        let Value::ExtObject(object) = value else {
            return Err(format!(
                "extension `{}` call `{}` returned `{}` without an extension object handle",
                self.module_name, call_label, returns
            ));
        };
        let mut routes = self.object_routes.lock().map_err(|_| {
            format!(
                "extension `{}` shared object routing lock is poisoned",
                self.module_name
            )
        })?;
        routes
            .register(&self.module_name, worker_id, object)
            .map(Value::ExtObject)
    }

    /// Resolves a host extension object handle supplied by the script.
    fn resolve_host_object(&self, object: &ExtObject) -> Result<HostObjectRoute, String> {
        let routes = self.object_routes.lock().map_err(|_| {
            format!(
                "extension `{}` shared object routing lock is poisoned",
                self.module_name
            )
        })?;
        routes.resolve(&self.module_name, object)
    }

    /// Removes a script-visible host object handle route.
    fn remove_host_object(&self, object: &ExtObject) {
        if let Ok(mut routes) = self.object_routes.lock() {
            routes.remove(&self.module_name, object);
        }
    }
}

impl Drop for ExtensionService {
    /// Closes queues and waits for workers to exit when dropping the service.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts a shared extension worker thread.
fn spawn_service_worker(
    worker_id: u32,
    module: Arc<WasmRunnerModule>,
    receiver: Arc<Mutex<Receiver<ExtensionServiceCall>>>,
    stats: Arc<ExtensionServiceStatsInner>,
    timeout_flag: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, String> {
    let module_name = module.module_name().to_string();
    thread::Builder::new()
        .name(format!("bt-ext-{}-{}", module_name, worker_id))
        .spawn(move || run_service_worker(worker_id, module, receiver, stats, timeout_flag))
        .map_err(|err| {
            format!(
                "extension `{}` shared service failed to start worker {}: {}",
                module_name, worker_id, err
            )
        })
}

/// Runs the shared extension worker loop.
fn run_service_worker(
    worker_id: u32,
    module: Arc<WasmRunnerModule>,
    receiver: Arc<Mutex<Receiver<ExtensionServiceCall>>>,
    stats: Arc<ExtensionServiceStatsInner>,
    timeout_flag: Arc<AtomicBool>,
) {
    let mut runtime =
        WasmRunnerRuntime::new_shared_worker(&module, timeout_flag.clone()).map_err(|err| {
            format!(
                "extension `{}` shared worker {} failed to initialize: {}",
                module.module_name(),
                worker_id,
                err
            )
        });
    while let Some(message) = receive_service_call(&receiver) {
        stats.queued.fetch_sub(1, Ordering::AcqRel);
        stats.running.fetch_add(1, Ordering::AcqRel);
        let mut should_recreate_runtime = false;
        let result = match runtime.as_mut() {
            Ok(runtime) => match runtime.call_export_bytes_controlled(
                &module,
                message.call_id,
                &message.call_label,
                &message.returns,
                &message.encoded_args,
            ) {
                Ok(bytes) => Ok(bytes),
                Err(err) => {
                    should_recreate_runtime = err.timed_out;
                    Err(err.message)
                }
            },
            Err(err) => Err(err.clone()),
        };
        if should_recreate_runtime {
            timeout_flag.store(false, Ordering::Release);
            runtime = WasmRunnerRuntime::new_shared_worker(&module, timeout_flag.clone()).map_err(
                |err| {
                    format!(
                        "extension `{}` shared worker {} failed to rebuild after timeout: {}",
                        module.module_name(),
                        worker_id,
                        err
                    )
                },
            );
        }
        stats.running.fetch_sub(1, Ordering::AcqRel);
        if result.is_ok() {
            stats.completed.fetch_add(1, Ordering::AcqRel);
        } else {
            stats.failed.fetch_add(1, Ordering::AcqRel);
        }
        let _ = message.reply.send(result);
    }
    if let Ok(runtime) = runtime.as_mut() {
        let _ = runtime.shutdown(&module);
    }
}

/// Reads one call message from a shared receiver.
fn receive_service_call(
    receiver: &Mutex<Receiver<ExtensionServiceCall>>,
) -> Option<ExtensionServiceCall> {
    receiver.lock().ok()?.recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::extensions::bindings::{BindingMethodLifecycle, ExtensionBindings};
    use crate::extensions::manifest::ExtensionManifest;
    use crate::extensions::package::PackageFileEntry;

    /// Builds a shared WASM extension package that returns a fixed integer.
    fn make_shared_primitive_package(call_timeout_ms: u64) -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(&format!(
            r#"{{
                "format": "bts",
                "format_version": 1,
                "name": "shared_answer",
                "version": "1.0.0",
                "kind": "wasm",
                "abi": "bts-wasi-1",
                "bt_min_version": "1.1.0",
                "api_version": 1,
                "entry": "module.wasm",
                "bindings": "bindings.json",
                "permissions": [],
                "runtime": {{
                    "mode": "shared",
                    "workers": 1,
                    "queue_limit": 1,
                    "call_timeout_ms": {},
                    "idle_ttl_ms": 300000,
                    "max_objects": 16,
                    "max_worker_objects": 16,
                    "max_inflight_calls": 4
                }}
            }}"#,
            call_timeout_ms
        ))
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
            path: PathBuf::from("shared_answer.bts"),
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

    /// Builds entry function metadata for tests.
    fn answer_function() -> RegisteredFunction {
        RegisteredFunction {
            module_id: 0,
            name: "answer".to_string(),
            function_id: 1,
            params: Vec::new(),
            returns: "int".to_string(),
        }
    }

    /// Builds a shared WASM extension package containing an infinite-loop call.
    fn make_shared_timeout_package(call_timeout_ms: u64) -> ExtensionPackage {
        let mut package = make_shared_primitive_package(call_timeout_ms);
        package.bindings = ExtensionBindings::parse(
            r#"{
                "api_version": 1,
                "functions": [
                    {
                        "name": "spin",
                        "id": 1,
                        "params": [],
                        "returns": "int"
                    },
                    {
                        "name": "answer",
                        "id": 2,
                        "params": [],
                        "returns": "int"
                    }
                ],
                "objects": []
            }"#,
            &package.manifest,
        )
        .unwrap();
        package.entry_wasm = Some(
            wat::parse_str(
                r#"
                (module
                    (memory (export "memory") 1)
                    (func (export "bts_alloc") (param $len i32) (result i32)
                        i32.const 1024
                    )
                    (func (export "bts_free") (param i32) (param i32))
                    (data (i32.const 16) "\00\03\2a\00\00\00\00\00\00\00")
                    (func (export "bts_call") (param $call_id i32) (param i32) (param i32) (result i64)
                        (block $done
                            local.get $call_id
                            i32.const 1
                            i32.ne
                            br_if $done
                            (loop $spin
                                br $spin
                            )
                        )
                        i64.const 68719476746
                    )
                )
                "#,
            )
            .unwrap(),
        );
        package
    }

    /// Builds infinite-loop entry metadata for tests.
    fn spin_function() -> RegisteredFunction {
        RegisteredFunction {
            module_id: 0,
            name: "spin".to_string(),
            function_id: 1,
            params: Vec::new(),
            returns: "int".to_string(),
        }
    }

    /// Builds post-timeout verification entry metadata for tests.
    fn timeout_answer_function() -> RegisteredFunction {
        RegisteredFunction {
            module_id: 0,
            name: "answer".to_string(),
            function_id: 2,
            params: Vec::new(),
            returns: "int".to_string(),
        }
    }

    /// Builds a shared WASM extension package that returns extension objects.
    fn make_shared_object_package(
        workers: u32,
        max_objects: u32,
        max_worker_objects: u32,
    ) -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(&format!(
            r#"{{
                "format": "bts",
                "format_version": 1,
                "name": "shared_cell",
                "version": "1.0.0",
                "kind": "wasm",
                "abi": "bts-wasi-1",
                "bt_min_version": "1.1.0",
                "api_version": 1,
                "entry": "module.wasm",
                "bindings": "bindings.json",
                "permissions": [],
                "runtime": {{
                    "mode": "shared",
                    "workers": {},
                    "queue_limit": 8,
                    "call_timeout_ms": 1000,
                    "idle_ttl_ms": 300000,
                    "max_objects": {},
                    "max_worker_objects": {},
                    "max_inflight_calls": 8
                }}
            }}"#,
            workers, max_objects, max_worker_objects
        ))
        .unwrap();
        let bindings = ExtensionBindings::parse(
            r#"{
                "api_version": 1,
                "functions": [
                    {
                        "name": "make",
                        "id": 1,
                        "params": [],
                        "returns": "Cell"
                    }
                ],
                "objects": [
                    {
                        "name": "Cell",
                        "type_id": 1,
                        "methods": [
                            {
                                "name": "value",
                                "id": 2,
                                "params": [],
                                "returns": "int"
                            },
                            {
                                "name": "close",
                                "id": 3,
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
        let wasm = wat::parse_str(
            r#"
            (module
                (memory (export "memory") 1)
                (func (export "bts_alloc") (param $len i32) (result i32)
                    i32.const 1024
                )
                (func (export "bts_free") (param i32) (param i32))
                (data (i32.const 16) "\00\09\00\00\00\00\00\00\00\00\01\00\00\00\01\00\00\00\00\00\00\00\04\00\00\00\43\65\6c\6c")
                (data (i32.const 96) "\00\02\01")
                (func (export "bts_call") (param $call_id i32) (param $args_ptr i32) (param $args_len i32) (result i64)
                    (if (i32.eq (local.get $call_id) (i32.const 1))
                        (then
                            i64.const 68719476766
                            return
                        )
                    )
                    (if (i32.eq (local.get $call_id) (i32.const 2))
                        (then
                            (i32.store8 (i32.const 128) (i32.const 0))
                            (i32.store8 (i32.const 129) (i32.const 3))
                            (i64.store
                                (i32.const 130)
                                (i64.load
                                    (i32.add (local.get $args_ptr) (i32.const 18))
                                )
                            )
                            i64.const 549755813898
                            return
                        )
                    )
                    i64.const 412316860419
                )
            )
            "#,
        )
        .unwrap();
        ExtensionPackage {
            path: PathBuf::from("shared_cell.bts"),
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

    /// Builds object creation entry metadata for tests.
    fn make_function() -> RegisteredFunction {
        RegisteredFunction {
            module_id: 0,
            name: "make".to_string(),
            function_id: 1,
            params: Vec::new(),
            returns: "Cell".to_string(),
        }
    }

    /// Builds object `value` method metadata for tests.
    fn cell_value_method() -> RegisteredMethod {
        RegisteredMethod {
            module_id: 0,
            type_id: 1,
            name: "value".to_string(),
            method_id: 2,
            params: Vec::new(),
            returns: "int".to_string(),
            lifecycle: BindingMethodLifecycle::Call,
        }
    }

    /// Builds object `close` method metadata for tests.
    fn cell_close_method() -> RegisteredMethod {
        RegisteredMethod {
            module_id: 0,
            type_id: 1,
            name: "close".to_string(),
            method_id: 3,
            params: Vec::new(),
            returns: "bool".to_string(),
            lifecycle: BindingMethodLifecycle::Dispose,
        }
    }

    /// The shared service returns primitive values through a worker.
    #[test]
    fn shared_service_returns_primitive_value() {
        let package = make_shared_primitive_package(1_000);
        let service = ExtensionService::from_package(0, Path::new("."), &package).unwrap();
        let value = service
            .call_function(&answer_function(), Vec::new(), Path::new("."))
            .unwrap();
        assert_eq!(value, Value::Int(42));
        let stats = service.stats();
        assert_eq!(stats.workers, 1);
        assert_eq!(stats.submitted, 1);
        assert_eq!(stats.completed, 1);
        service.shutdown();
        let err = service
            .call_function(&answer_function(), Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("shut down"));
    }

    /// A full queue rejects new calls and increments `rejected`.
    #[test]
    fn shared_service_rejects_when_queue_is_full() {
        let package = make_shared_primitive_package(200);
        let module = WasmRunnerModule::from_shared_package(0, Path::new("."), &package).unwrap();
        let service =
            ExtensionService::from_module(Arc::new(module), package.manifest.runtime, false)
                .unwrap();
        let (reply, _receiver) = sync_channel(1);
        let queued_call = ExtensionServiceCall {
            call_id: 1,
            call_label: "answer".to_string(),
            returns: "int".to_string(),
            encoded_args: Vec::new(),
            reply,
        };
        service.stats.queued.fetch_add(1, Ordering::AcqRel);
        match service.active_sender(0).unwrap().try_send(queued_call) {
            Ok(()) => {}
            Err(_) => panic!("the test call should fill the shared service queue"),
        }
        assert_eq!(service.stats().queued, 1);
        let err = service
            .call_function(&answer_function(), Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("queue is full"));
        assert_eq!(service.stats().rejected, 1);
        service.shutdown();
    }

    /// A timed-out shared call interrupts its worker while allowing subsequent calls to proceed.
    #[test]
    fn shared_service_interrupts_and_rebuilds_worker_after_timeout() {
        let package = make_shared_timeout_package(200);
        let service = ExtensionService::from_package(0, Path::new("."), &package).unwrap();
        let err = service
            .call_function(&spin_function(), Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("over 200ms"));
        assert!(err.contains("worker 0 was interrupted"));

        let value = service
            .call_function(&timeout_answer_function(), Vec::new(), Path::new("."))
            .unwrap();
        assert_eq!(value, Value::Int(42));
        let stats = service.stats();
        assert_eq!(stats.timed_out, 1);
        assert!(stats.completed >= 1);
        service.shutdown();
    }

    /// Shared object results become host handles and are restored to worker-local handles for method calls.
    #[test]
    fn shared_service_rewrites_and_routes_host_objects() {
        let package = make_shared_object_package(2, 16, 16);
        let service = ExtensionService::from_package(0, Path::new("."), &package).unwrap();
        let first = service
            .call_function(&make_function(), Vec::new(), Path::new("."))
            .unwrap();
        let second = service
            .call_function(&make_function(), Vec::new(), Path::new("."))
            .unwrap();
        let Value::ExtObject(first) = first else {
            panic!("the first call should return an extension object");
        };
        let Value::ExtObject(second) = second else {
            panic!("the second call should return an extension object");
        };
        assert_eq!(first.object_id, 1);
        assert_eq!(second.object_id, 2);
        assert_eq!(service.stats().objects, 2);

        let value = service
            .call_method(&second, &cell_value_method(), Vec::new(), Path::new("."))
            .unwrap();
        assert_eq!(value, Value::Int(1));

        let closed = service
            .call_method(&second, &cell_close_method(), Vec::new(), Path::new("."))
            .unwrap();
        assert_eq!(closed, Value::Bool(true));
        let err = service
            .call_method(&second, &cell_value_method(), Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("no longer valid"));
        service.shutdown();
    }

    /// Exceeding the manifest's service-wide object limit rejects further host handle registration.
    #[test]
    fn shared_service_rejects_when_service_object_limit_is_reached() {
        let package = make_shared_object_package(2, 1, 1);
        let service = ExtensionService::from_package(0, Path::new("."), &package).unwrap();
        let first = service
            .call_function(&make_function(), Vec::new(), Path::new("."))
            .unwrap();
        assert!(matches!(first, Value::ExtObject(_)));
        let err = service
            .call_function(&make_function(), Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("max_objects"));
        service.shutdown();
    }

    /// Exceeding the manifest's per-worker object limit rejects further host handle registration.
    #[test]
    fn shared_service_rejects_when_worker_object_limit_is_reached() {
        let package = make_shared_object_package(1, 2, 1);
        let service = ExtensionService::from_package(0, Path::new("."), &package).unwrap();
        let first = service
            .call_function(&make_function(), Vec::new(), Path::new("."))
            .unwrap();
        assert!(matches!(first, Value::ExtObject(_)));
        let err = service
            .call_function(&make_function(), Vec::new(), Path::new("."))
            .unwrap_err();
        assert!(err.contains("max_worker_objects"));
        service.shutdown();
    }
}
