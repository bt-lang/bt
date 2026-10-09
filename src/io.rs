//! BT process-level I/O runtime boundaries.
//!
//! The shared Tokio runtime and bounded blocking pool are initialized lazily, only when a standard-library operation performs I/O.
//! The VM instruction hot path never reads this global state, so it incurs no additional lock contention.

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle as ThreadJoinHandle};
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime, RuntimeFlavor};
use tokio::task::JoinHandle as TokioJoinHandle;

/// The default maximum number of I/O asynchronous running threads.
const MAX_IO_WORKERS: usize = 64;
/// Default maximum number of blocked I/O worker threads.
const MAX_BLOCKING_WORKERS: usize = 64;
/// Default blocking I/O queue length.
const DEFAULT_BLOCKING_QUEUE_LIMIT: usize = 256;
/// Hard upper limit on blocking I/O queue length.
const MAX_BLOCKING_QUEUE_LIMIT: usize = 8192;
/// Default I/O sync wait timeout.
const DEFAULT_IO_TIMEOUT_MS: u64 = 30_000;
/// Default blocking pool shutdown waiting time.
const DEFAULT_IO_SHUTDOWN_TIMEOUT_MS: u64 = 1_000;

/// Global asynchronous runtime.
static ASYNC_RUNTIME: OnceLock<Result<Arc<AsyncRuntime>, String>> = OnceLock::new();
/// Global bounded blocking thread pool.
static BLOCKING_POOL: OnceLock<Result<Arc<BlockingPool>, String>> = OnceLock::new();
/// The number of asynchronous I/O calls currently waiting synchronously.
static ASYNC_ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// The number of asynchronous I/O calls completed.
static ASYNC_COMPLETED: AtomicUsize = AtomicUsize::new(0);
/// Number of asynchronous I/O calls that returned an error.
static ASYNC_FAILED: AtomicUsize = AtomicUsize::new(0);
/// The number of asynchronous I/O calls that terminated due to timeout.
static ASYNC_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);
/// The number of asynchronous I/O calls that were rejected due to configuration or runner unavailability.
static ASYNC_REJECTED: AtomicUsize = AtomicUsize::new(0);
/// Whether the rustls cryptography provider has been installed.
static RUSTLS_PROVIDER_INSTALLED: OnceLock<()> = OnceLock::new();

thread_local! {
    /// The current blocking job's budget; consulted at VM creation and I/O boundaries only.
    static EXECUTION_BUDGET: RefCell<Option<Arc<ExecutionBudget>>> = const { RefCell::new(None) };
}

/// One request's queue-inclusive deadline and cooperative cancellation signal.
pub(crate) struct ExecutionBudget {
    /// Deadline shared by nested VMs and asynchronous I/O.
    deadline: Instant,
    /// Original duration retained for consistent user-facing diagnostics.
    timeout: Duration,
    /// Set when the awaiting request is dropped or finishes waiting.
    cancelled: AtomicBool,
    /// Wakes the single foreground I/O operation when its request is cancelled.
    wake: tokio::sync::Notify,
}

impl ExecutionBudget {
    /// Creates a queue-inclusive budget, rejecting durations outside the monotonic clock range.
    pub(crate) fn new(timeout: Duration) -> Result<Arc<Self>, String> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| "I/O timeout exceeds the supported monotonic clock range".to_string())?;
        Ok(Arc::new(Self {
            deadline,
            timeout,
            cancelled: AtomicBool::new(false),
            wake: tokio::sync::Notify::new(),
        }))
    }

    /// Rejects execution after cancellation or expiry without allocating on success.
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) || Instant::now() >= self.deadline {
            Err(self.error())
        } else {
            Ok(())
        }
    }

    /// Returns the remaining request time, including time spent waiting for a worker.
    fn remaining(&self) -> Result<Duration, String> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }

    /// Produces the same timeout diagnostic for queued work, VM execution and I/O.
    fn error(&self) -> String {
        format!(
            "I/O blocking task execution exceeds {} milliseconds",
            self.timeout.as_millis()
        )
    }

    /// Signals cancellation; Notify retains a permit if I/O has not started waiting yet.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }

    /// Waits for cancellation of the single foreground operation in this blocking job.
    async fn cancelled(&self) {
        if !self.cancelled.load(Ordering::Relaxed) {
            self.wake.notified().await;
        }
    }

    /// Sleeps in bounded slices so cancellation never waits for a full user sleep interval.
    pub(crate) fn sleep(&self, duration: Duration) -> Result<(), String> {
        let until = Instant::now().checked_add(duration).ok_or_else(|| {
            "sleep duration exceeds the supported monotonic clock range".to_string()
        })?;
        loop {
            let remaining = self.remaining()?;
            let sleep = until.saturating_duration_since(Instant::now());
            if sleep.is_zero() {
                return Ok(());
            }
            thread::sleep(sleep.min(remaining).min(Duration::from_millis(10)));
        }
    }

    /// Installs a request budget on its worker, restoring any previous context on drop.
    pub(crate) fn enter(self: &Arc<Self>) -> ExecutionBudgetGuard {
        ExecutionBudgetGuard(EXECUTION_BUDGET.with(|slot| slot.replace(Some(self.clone()))))
    }
}

/// Restores thread-local state even when a blocking job panics.
pub(crate) struct ExecutionBudgetGuard(Option<Arc<ExecutionBudget>>);

impl Drop for ExecutionBudgetGuard {
    /// Prevents one request's deadline from leaking into the next worker job.
    fn drop(&mut self) {
        EXECUTION_BUDGET.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

/// Cancels a job when its asynchronous waiter is dropped, including request cancellation.
struct CancelExecutionOnDrop(Arc<ExecutionBudget>);

impl Drop for CancelExecutionOnDrop {
    /// Wakes I/O and marks the next VM checkpoint as cancelled.
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Captures the current budget once per VM rather than consulting TLS on each instruction.
pub(crate) fn current_execution_budget() -> Option<Arc<ExecutionBudget>> {
    EXECUTION_BUDGET.with(|slot| slot.borrow().clone())
}

/// I/O run boundary configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoConfig {
    /// The number of Tokio runtime worker threads.
    pub async_workers: usize,
    /// Number of blocked I/O worker threads.
    pub blocking_workers: usize,
    /// Blocking I/O wait queue length.
    pub blocking_queue_limit: usize,
    /// Default synchronization wait timeout, in milliseconds.
    pub default_timeout_ms: u64,
    /// Blocking pool shutdown waiting time, in milliseconds.
    pub shutdown_timeout_ms: u64,
}

/// I/O runtime boundary statistics snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoStats {
    /// The currently effective configuration.
    pub config: IoConfig,
    /// Whether the asynchronous runtime has been initialized.
    pub async_runtime_started: bool,
    /// The number of asynchronous I/O calls currently waiting synchronously.
    pub async_active: usize,
    /// The number of asynchronous I/O calls completed.
    pub async_completed: usize,
    /// Number of asynchronous I/O calls that returned an error.
    pub async_failed: usize,
    /// The number of asynchronous I/O calls that terminated due to timeout.
    pub async_timeouts: usize,
    /// The number of asynchronous I/O calls that were rejected due to configuration or runner unavailability.
    pub async_rejected: usize,
    /// Whether the blocking thread pool has been initialized.
    pub blocking_pool_started: bool,
    /// The number of tasks waiting to be executed in the current blocking queue.
    pub blocking_queued: usize,
    /// The number of blocking tasks currently running.
    pub blocking_running: usize,
    /// The number of completed blocking tasks.
    pub blocking_completed: usize,
    /// The number of blocking tasks that were rejected because the queue was full or closed.
    pub blocking_rejected: usize,
    /// The number of blocking task synchronization wait timeouts.
    pub blocking_timeouts: usize,
    /// Whether the blocking thread pool has entered the closed state.
    pub blocking_shutdown: bool,
}

/// Shared Tokio runtime.
struct AsyncRuntime {
    /// Tokio multi-threaded runtime.
    runtime: Runtime,
    /// Configuration used during runtime initialization.
    config: IoConfig,
}

/// Bounded blocking thread pool.
struct BlockingPool {
    /// Task sending end; when closed, leave it empty and discard the sending end, allowing the worker thread to exit naturally.
    sender: Mutex<Option<SyncSender<BlockingJob>>>,
    /// Worker thread handle.
    workers: Mutex<Vec<ThreadJoinHandle<()>>>,
    /// Blocking pool sharing statistics.
    stats: Arc<BlockingPoolStats>,
    /// Blocking pool configuration.
    config: IoConfig,
}

/// Blocking pool sharing statistics.
struct BlockingPoolStats {
    /// The number of tasks waiting to be executed in the current queue.
    queued: AtomicUsize,
    /// The number of tasks currently running.
    running: AtomicUsize,
    /// The number of tasks completed.
    completed: AtomicUsize,
    /// The number of rejected tasks.
    rejected: AtomicUsize,
    /// Synchronization wait timeout number.
    timeouts: AtomicUsize,
    /// Whether the blocking pool has been closed.
    shutdown: AtomicBool,
}

/// Blocking pool internal task type.
type BlockingJob = Box<dyn FnOnce() + Send + 'static>;

impl IoConfig {
    /// Reads I/O configuration from environment variables.
    fn from_env() -> Result<Self, String> {
        let cpus = thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1)
            .max(1);
        Ok(Self {
            async_workers: read_usize_env("BT_IO_WORKERS", cpus, 1, MAX_IO_WORKERS)?,
            blocking_workers: read_usize_env(
                "BT_IO_BLOCKING_WORKERS",
                cpus,
                1,
                MAX_BLOCKING_WORKERS,
            )?,
            blocking_queue_limit: read_usize_env(
                "BT_IO_BLOCKING_QUEUE",
                DEFAULT_BLOCKING_QUEUE_LIMIT,
                1,
                MAX_BLOCKING_QUEUE_LIMIT,
            )?,
            default_timeout_ms: read_u64_env("BT_IO_TIMEOUT_MS", DEFAULT_IO_TIMEOUT_MS, 1)?,
            shutdown_timeout_ms: read_u64_env(
                "BT_IO_SHUTDOWN_TIMEOUT_MS",
                DEFAULT_IO_SHUTDOWN_TIMEOUT_MS,
                1,
            )?,
        })
    }
}

impl AsyncRuntime {
    /// Creates a shared Tokio runtime.
    fn new(config: &IoConfig) -> Result<Self, String> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(config.async_workers)
            .thread_name("bt-io")
            .enable_all()
            .build()
            .map_err(|err| format!("Failed to create the asynchronous I/O runtime: {}", err))?;
        Ok(Self {
            runtime,
            config: config.clone(),
        })
    }

    /// Waits synchronously for a future to complete on a shared runtime.
    fn block_on<F>(&self, future: F) -> Result<F::Output, String>
    where
        F: Future,
    {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
                Ok(tokio::task::block_in_place(|| {
                    self.runtime.block_on(future)
                }))
            }
            Ok(_) => Err("The current thread is already inside a single-threaded Tokio runner and cannot synchronously wait for BT I/O tasks".to_string()),
            Err(_) => Ok(self.runtime.block_on(future)),
        }
    }

    /// Submits a long-term asynchronous task on the shared runtime.
    fn spawn<F>(&self, future: F) -> TokioJoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future)
    }
}

impl BlockingPool {
    /// Creates a bounded blocking thread pool.
    fn start(config: IoConfig) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(config.blocking_queue_limit);
        let receiver = Arc::new(Mutex::new(receiver));
        let stats = Arc::new(BlockingPoolStats {
            queued: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
            timeouts: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
        });
        let mut workers = Vec::with_capacity(config.blocking_workers);
        for index in 0..config.blocking_workers {
            let worker_receiver = receiver.clone();
            let worker_stats = stats.clone();
            let name = format!("bt-blocking-{}", index + 1);
            let handle = thread::Builder::new()
                .name(name)
                .spawn(move || blocking_worker_loop(worker_receiver, worker_stats))
                .map_err(|err| {
                    format!("Failed to create an I/O blocking worker thread: {}", err)
                })?;
            workers.push(handle);
        }
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            workers: Mutex::new(workers),
            stats,
            config,
        })
    }

    /// Submitted a blocking task.
    fn submit(&self, job: BlockingJob) -> Result<(), String> {
        if self.stats.shutdown.load(Ordering::Acquire) {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return Err("The I/O blocking thread pool has been closed".to_string());
        }
        let sender = self
            .sender
            .lock()
            .map_err(|_| "The I/O blocking thread pool sender lock is poisoned".to_string())?;
        let Some(sender) = sender.as_ref() else {
            self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            return Err("The I/O blocking thread pool has been closed".to_string());
        };
        // Publish the count before the worker can receive and decrement it.
        self.stats.queued.fetch_add(1, Ordering::Relaxed);
        match sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.stats.queued.fetch_sub(1, Ordering::Relaxed);
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                Err(format!(
                    "The I/O blocking task queue is full (limit: {})",
                    self.config.blocking_queue_limit
                ))
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stats.queued.fetch_sub(1, Ordering::Relaxed);
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                Err("The I/O blocking thread pool has stopped".to_string())
            }
        }
    }

    /// Runs request work with a queue-inclusive deadline and cancellation on waiter drop.
    async fn run_async<T>(
        &self,
        timeout: Duration,
        job: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String>
    where
        T: Send + 'static,
    {
        let budget = ExecutionBudget::new(timeout)?;
        let cancel = CancelExecutionOnDrop(budget.clone());
        let worker_budget = budget.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.submit(Box::new(move || {
            // Expired queued work must never run user code or its side effects.
            let result = worker_budget.check().and_then(|()| {
                let _scope = worker_budget.enter();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or_else(
                    |payload| Err(format!("I/O blocking task panic: {}", panic_text(payload))),
                )
            });
            let _ = sender.send(result);
        }))?;
        let result = match tokio::time::timeout_at(budget.deadline.into(), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("The I/O blocking task result channel has been closed".to_string()),
            Err(_) => Err(budget.error()),
        };
        let result = if Instant::now() >= budget.deadline {
            self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
            // A delayed Tokio poll may observe an already-ready result after the deadline.
            // Do not let that race turn an expired request into a successful response.
            Err(budget.error())
        } else {
            result
        };
        drop(cancel);
        result
    }

    /// Execute the blocking closure in the thread pool and wait for the result synchronously.
    fn run<T>(
        &self,
        timeout: Duration,
        job: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String>
    where
        T: Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.submit(Box::new(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
                .unwrap_or_else(|payload| {
                    Err(format!("I/O blocking task panic: {}", panic_text(payload)))
                });
            let _ = sender.send(result);
        }))?;
        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                Err(format!(
                    "I/O blocking task execution exceeds {} milliseconds",
                    timeout.as_millis()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("The I/O blocking task result channel has been closed".to_string())
            }
        }
    }

    /// Actively close the blocking thread pool and wait for the worker thread to exit.
    fn shutdown(&self, timeout: Duration) -> bool {
        self.stats.shutdown.store(true, Ordering::Release);
        if let Ok(mut sender) = self.sender.lock() {
            sender.take();
        }
        let deadline = Instant::now() + timeout;
        let mut all_joined = true;
        let mut workers = match self.workers.lock() {
            Ok(workers) => workers,
            Err(poisoned) => poisoned.into_inner(),
        };
        while let Some(handle) = workers.pop() {
            if Instant::now() > deadline {
                all_joined = false;
                workers.push(handle);
                break;
            }
            if handle.join().is_err() {
                all_joined = false;
            }
        }
        all_joined
    }

    /// Returns a snapshot of blocking pool statistics.
    fn snapshot(&self) -> BlockingPoolSnapshot {
        BlockingPoolSnapshot {
            queued: self.stats.queued.load(Ordering::Relaxed),
            running: self.stats.running.load(Ordering::Relaxed),
            completed: self.stats.completed.load(Ordering::Relaxed),
            rejected: self.stats.rejected.load(Ordering::Relaxed),
            timeouts: self.stats.timeouts.load(Ordering::Relaxed),
            shutdown: self.stats.shutdown.load(Ordering::Acquire),
        }
    }
}

impl Drop for BlockingPool {
    /// Try to close the worker thread when releasing the blocking pool.
    fn drop(&mut self) {
        let _ = self.shutdown(Duration::from_millis(self.config.shutdown_timeout_ms));
    }
}

/// Blocking pool statistics snapshot.
struct BlockingPoolSnapshot {
    /// The number of tasks waiting to be executed in the current queue.
    queued: usize,
    /// The number of tasks currently running.
    running: usize,
    /// The number of tasks completed.
    completed: usize,
    /// The number of rejected tasks.
    rejected: usize,
    /// Synchronization wait timeout number.
    timeouts: usize,
    /// Whether the blocking pool has been closed.
    shutdown: bool,
}

/// Synchronously waits for asynchronous I/O to complete on a shared asynchronous runtime.
pub fn run_async<F, T>(future: F, timeout: Option<Duration>) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    let budget = current_execution_budget();
    let timeout = match (&budget, timeout) {
        (Some(budget), timeout) => {
            let remaining = budget.remaining()?;
            Some(timeout.map_or(remaining, |value| value.min(remaining)))
        }
        (None, timeout) => timeout,
    };
    let runtime = match async_runtime() {
        Ok(runtime) => runtime,
        Err(err) => {
            ASYNC_REJECTED.fetch_add(1, Ordering::Relaxed);
            return Err(err);
        }
    };
    ASYNC_ACTIVE.fetch_add(1, Ordering::Relaxed);
    let operation = async move {
        match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, future).await {
                Ok(result) => result,
                Err(_) => {
                    ASYNC_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                    Err(format!(
                        "Asynchronous I/O task execution exceeded {} milliseconds",
                        timeout.as_millis()
                    ))
                }
            },
            None => future.await,
        }
    };
    let result = runtime.block_on(async move {
        if let Some(budget) = budget {
            tokio::select! {
                biased;
                _ = budget.cancelled() => Err(budget.error()),
                result = operation => {
                    budget.check()?;
                    result
                }
            }
        } else {
            operation.await
        }
    });
    ASYNC_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    match result {
        Ok(Ok(value)) => {
            ASYNC_COMPLETED.fetch_add(1, Ordering::Relaxed);
            Ok(value)
        }
        Ok(Err(err)) => {
            ASYNC_FAILED.fetch_add(1, Ordering::Relaxed);
            Err(err)
        }
        Err(err) => {
            ASYNC_FAILED.fetch_add(1, Ordering::Relaxed);
            Err(err)
        }
    }
}

/// Submitted a long-running background I/O task on a shared asynchronous runtime.
///
/// This entry is only used for background tasks such as network services that survive cross-script execution flows; ordinary synchronous standard library calls continue to use
/// `run_async()` to avoid changing the VM call surface to asynchronous.
pub fn spawn_async<F>(future: F) -> Result<TokioJoinHandle<()>, String>
where
    F: Future<Output = ()> + Send + 'static,
{
    let runtime = match async_runtime() {
        Ok(runtime) => runtime,
        Err(err) => {
            ASYNC_REJECTED.fetch_add(1, Ordering::Relaxed);
            return Err(err);
        }
    };
    ASYNC_ACTIVE.fetch_add(1, Ordering::Relaxed);
    Ok(runtime.spawn(async move {
        future.await;
        ASYNC_ACTIVE.fetch_sub(1, Ordering::Relaxed);
        ASYNC_COMPLETED.fetch_add(1, Ordering::Relaxed);
    }))
}

/// Execute closures in a global bounded blocking pool.
#[allow(dead_code)]
pub fn run_blocking<T>(
    timeout: Option<Duration>,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let pool = match blocking_pool() {
        Ok(pool) => pool,
        Err(err) => return Err(err),
    };
    let timeout = timeout.unwrap_or_else(default_timeout);
    pool.run(timeout, job)
}

/// Execute the closure in the global bounded blocking pool and wait for the result asynchronously.
///
/// The Web handler is already running on the Tokio worker and cannot be used to synchronize `recv_timeout()` to wait for blocking tasks;
/// This entry reuses the same blocking pool, but hands the waiting process to the Tokio timer to avoid blocking the Web worker.
pub async fn run_blocking_async<T>(
    timeout: Option<Duration>,
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let pool = match blocking_pool() {
        Ok(pool) => pool,
        Err(err) => return Err(err),
    };
    pool.run_async(timeout.unwrap_or_else(default_timeout), job)
        .await
}

/// Returns the current default I/O timeout.
#[allow(dead_code)]
pub fn default_timeout() -> Duration {
    config()
        .map(|config| Duration::from_millis(config.default_timeout_ms))
        .unwrap_or_else(|_| Duration::from_millis(DEFAULT_IO_TIMEOUT_MS))
}

/// Ensures that the rustls ring cryptography provider is installed.
///
/// With `rustls-no-provider`, reqwest does not choose a cryptography backend on its
/// own. The provider must be installed before the CLI creates its first HTTPS client.
/// `OnceLock` makes this idempotent; repeated-installation errors can be ignored.
pub fn ensure_rustls_provider() {
    RUSTLS_PROVIDER_INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Returns a snapshot of I/O runtime boundary statistics.
pub fn stats() -> IoStats {
    let config = config().unwrap_or_else(|_| fallback_config());
    let blocking_snapshot = BLOCKING_POOL
        .get()
        .and_then(|result| result.as_ref().ok())
        .map(|pool| pool.snapshot())
        .unwrap_or(BlockingPoolSnapshot {
            queued: 0,
            running: 0,
            completed: 0,
            rejected: 0,
            timeouts: 0,
            shutdown: false,
        });
    IoStats {
        config,
        async_runtime_started: matches!(ASYNC_RUNTIME.get(), Some(Ok(_))),
        async_active: ASYNC_ACTIVE.load(Ordering::Relaxed),
        async_completed: ASYNC_COMPLETED.load(Ordering::Relaxed),
        async_failed: ASYNC_FAILED.load(Ordering::Relaxed),
        async_timeouts: ASYNC_TIMEOUTS.load(Ordering::Relaxed),
        async_rejected: ASYNC_REJECTED.load(Ordering::Relaxed),
        blocking_pool_started: matches!(BLOCKING_POOL.get(), Some(Ok(_))),
        blocking_queued: blocking_snapshot.queued,
        blocking_running: blocking_snapshot.running,
        blocking_completed: blocking_snapshot.completed,
        blocking_rejected: blocking_snapshot.rejected,
        blocking_timeouts: blocking_snapshot.timeouts,
        blocking_shutdown: blocking_snapshot.shutdown,
    }
}

/// Returns the shared asynchronous runtime.
fn async_runtime() -> Result<&'static Arc<AsyncRuntime>, String> {
    match ASYNC_RUNTIME.get_or_init(|| {
        let config = IoConfig::from_env()?;
        AsyncRuntime::new(&config).map(Arc::new)
    }) {
        Ok(runtime) => Ok(runtime),
        Err(err) => Err(err.clone()),
    }
}

/// Returns the shared blocking thread pool.
fn blocking_pool() -> Result<&'static Arc<BlockingPool>, String> {
    match BLOCKING_POOL.get_or_init(|| {
        let config = IoConfig::from_env()?;
        BlockingPool::start(config).map(Arc::new)
    }) {
        Ok(pool) => Ok(pool),
        Err(err) => Err(err.clone()),
    }
}

/// Returns the current configuration.
fn config() -> Result<IoConfig, String> {
    if let Some(Ok(pool)) = BLOCKING_POOL.get() {
        return Ok(pool.config.clone());
    }
    if let Some(Ok(runtime)) = ASYNC_RUNTIME.get() {
        return Ok(runtime.config.clone());
    }
    IoConfig::from_env()
}

/// Returns a conservative configuration used for statistics display when environment variables cannot be resolved.
fn fallback_config() -> IoConfig {
    let cpus = thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1)
        .max(1);
    IoConfig {
        async_workers: cpus.min(MAX_IO_WORKERS),
        blocking_workers: cpus.min(MAX_BLOCKING_WORKERS),
        blocking_queue_limit: DEFAULT_BLOCKING_QUEUE_LIMIT,
        default_timeout_ms: DEFAULT_IO_TIMEOUT_MS,
        shutdown_timeout_ms: DEFAULT_IO_SHUTDOWN_TIMEOUT_MS,
    }
}

/// Runs the blocking-worker loop.
fn blocking_worker_loop(
    receiver: Arc<Mutex<Receiver<BlockingJob>>>,
    stats: Arc<BlockingPoolStats>,
) {
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
        stats.queued.fetch_sub(1, Ordering::Relaxed);
        stats.running.fetch_add(1, Ordering::Relaxed);
        task();
        stats.running.fetch_sub(1, Ordering::Relaxed);
        stats.completed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reads a `usize` environment variable.
fn read_usize_env(name: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("{} must be an integer between {} and {}", name, min, max))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{} must be an integer between {} and {}",
            name, min, max
        ));
    }
    Ok(parsed)
}

/// Reads a `u64` environment variable.
fn read_u64_env(name: &str, default: u64, min: u64) -> Result<u64, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    let parsed = value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{} must be an integer no less than {}", name, min))?;
    if parsed < min {
        return Err(format!("{} must be an integer no less than {}", name, min));
    }
    Ok(parsed)
}

/// Extracts the text from the panic payload.
fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "Unknown error".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create an I/O configuration for testing.
    fn test_config(queue_limit: usize) -> IoConfig {
        IoConfig {
            async_workers: 1,
            blocking_workers: 1,
            blocking_queue_limit: queue_limit,
            default_timeout_ms: 50,
            shutdown_timeout_ms: 500,
        }
    }

    /// New tasks should be rejected immediately when the blocking pool queue is full.
    #[test]
    fn blocking_pool_rejects_when_queue_is_full() {
        let pool = BlockingPool::start(test_config(1)).unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<()>();

        pool.submit(Box::new(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(1));
        }))
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        pool.submit(Box::new(|| {})).unwrap();

        let err = pool.submit(Box::new(|| {})).unwrap_err();
        assert!(err.contains("queue is full"));
        assert_eq!(pool.snapshot().rejected, 1);

        release_tx.send(()).unwrap();
        assert!(pool.shutdown(Duration::from_secs(1)));
    }

    /// When the blocking pool synchronization wait exceeds the timeout period, an error should be returned and the timeout should be recorded.
    #[test]
    fn blocking_pool_reports_timeout() {
        let pool = BlockingPool::start(test_config(1)).unwrap();
        let err = pool
            .run(Duration::from_millis(10), || {
                thread::sleep(Duration::from_millis(80));
                Ok::<_, String>(1)
            })
            .unwrap_err();

        assert!(err.contains("exceeds 10 milliseconds"));
        thread::sleep(Duration::from_millis(100));
        let stats = pool.snapshot();
        assert_eq!(stats.timeouts, 1);
        assert_eq!(stats.completed, 1);
        assert!(pool.shutdown(Duration::from_secs(1)));
    }

    /// New tasks should be rejected after the blocking pool is closed, and the worker thread can exit.
    #[test]
    fn blocking_pool_shutdown_rejects_new_jobs() {
        let pool = BlockingPool::start(test_config(1)).unwrap();
        assert!(pool.shutdown(Duration::from_secs(1)));
        let err = pool
            .run(Duration::from_millis(10), || Ok::<_, String>(1))
            .unwrap_err();
        assert!(err.contains("has been closed"));
        assert!(pool.snapshot().shutdown);
    }

    /// The shared asynchronous runner should be able to execute futures in ordinary threads.
    #[test]
    fn async_runtime_runs_future_on_plain_thread() {
        let value = run_async(async { Ok::<_, String>(7) }, Some(Duration::from_secs(1))).unwrap();
        assert_eq!(value, 7);
    }

    /// Single-threaded Tokio runtime should not nest synchronous waits internally to avoid triggering Tokio panic.
    #[test]
    fn async_runtime_rejects_current_thread_runtime() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async {
            run_async(
                async { Ok::<_, String>(1) },
                Some(Duration::from_millis(10)),
            )
        });
        assert!(matches!(result, Err(err) if err.contains("single-threaded Tokio runner")));
    }

    /// The asynchronous wait version blocking pool entry should be able to return task results within the Tokio runtime.
    #[test]
    fn blocking_pool_async_wait_returns_value() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let value = runtime
            .block_on(run_blocking_async(Some(Duration::from_secs(1)), || {
                Ok::<_, String>(11)
            }))
            .unwrap();

        assert_eq!(value, 11);
    }

    /// Work whose queue deadline expired must never execute user side effects.
    #[test]
    fn expired_request_is_skipped_before_execution() {
        let pool = BlockingPool::start(test_config(4)).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        pool.submit(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }))
        .unwrap();
        entered_rx.recv().unwrap();
        let ran = Arc::new(AtomicBool::new(false));
        let job_ran = ran.clone();
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(pool.run_async(Duration::from_millis(10), move || {
            job_ran.store(true, Ordering::Relaxed);
            Ok(())
        }));
        assert!(result.unwrap_err().contains("exceeds 10 milliseconds"));
        release_tx.send(()).unwrap();
        assert_eq!(
            runtime
                .block_on(pool.run_async(Duration::from_secs(2), || {
                    assert!(current_execution_budget().is_some());
                    Ok(7)
                }))
                .unwrap(),
            7
        );
        assert!(!ran.load(Ordering::Relaxed));
        assert!(pool.shutdown(Duration::from_secs(2)));
        assert_eq!(pool.snapshot().queued, 0);
    }

    /// Dropping a waiter cancels an active cooperative job and releases the only worker.
    #[test]
    fn dropped_request_releases_worker_and_restores_context() {
        let pool = BlockingPool::start(test_config(4)).unwrap();
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            {
                let future = pool.run_async(Duration::from_secs(5), move || {
                    let budget = current_execution_budget().unwrap();
                    let _ = started_tx.send(());
                    budget.sleep(Duration::from_secs(5))
                });
                tokio::pin!(future);
                tokio::select! {
                    result = &mut future => panic!("job ended before cancellation: {result:?}"),
                    _ = started_rx => {},
                }
            }
            let result = pool
                .run_async(Duration::from_secs(2), || Ok(11))
                .await
                .unwrap();
            assert_eq!(result, 11);
        });
        // A plain job on the same worker must not inherit the cancelled request.
        assert!(pool
            .run(Duration::from_secs(2), || Ok(
                current_execution_budget().is_none()
            ))
            .unwrap());
        assert!(pool.shutdown(Duration::from_secs(2)));
    }

    /// A request deadline limits I/O even when the operation has no separate timeout.
    #[test]
    fn request_deadline_bounds_pending_async_io() {
        let budget = ExecutionBudget::new(Duration::from_millis(20)).unwrap();
        let _scope = budget.enter();
        let result = run_async(std::future::pending::<Result<(), String>>(), None);
        assert!(result.is_err());
    }

    /// Cancellation wakes pending asynchronous I/O without waiting for its long deadline.
    #[test]
    fn request_cancellation_wakes_pending_async_io() {
        let budget = ExecutionBudget::new(Duration::from_secs(5)).unwrap();
        let cancel = budget.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _scope = budget.enter();
            run_async(
                async move {
                    started_tx.send(()).unwrap();
                    std::future::pending::<Result<(), String>>().await
                },
                None,
            )
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        cancel.cancel();
        assert!(worker.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// Extreme configuration values must return errors instead of panicking a server worker.
    #[test]
    fn request_budget_rejects_unrepresentable_deadline() {
        assert!(ExecutionBudget::new(Duration::MAX).is_err());
    }

    /// A result ready after an executor stall must not bypass the absolute request deadline.
    #[test]
    fn delayed_waiter_rejects_result_after_deadline() {
        let pool = BlockingPool::start(test_config(4)).unwrap();
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let future = pool.run_async(Duration::from_millis(50), move || {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                Ok(7)
            });
            tokio::pin!(future);
            tokio::select! {
                result = &mut future => panic!("job ended before release: {result:?}"),
                _ = started_rx => {},
            }
            release_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(80));
            assert!(future
                .await
                .unwrap_err()
                .contains("exceeds 50 milliseconds"));
        });
        assert!(pool.shutdown(Duration::from_secs(2)));
    }
}
