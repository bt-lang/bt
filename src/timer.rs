//! Lightweight BT timer runtime.
//!
//! The scheduler thread stores only timer IDs, deadlines, and VM event senders. Script functions and VM state stay on their owning VM thread,
//! where callbacks run serially after a `TimerEvent` arrives.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Default single-process activity timer upper limit.
const DEFAULT_TIMER_LIMIT: usize = 4096;
/// Hard limit for active timers in one process.
const MAX_TIMER_LIMIT: usize = 65536;
/// Default single VM timer event queue length.
const DEFAULT_TIMER_EVENT_QUEUE: usize = 1024;
/// Hard limit on single VM timer event queue length.
const MAX_TIMER_EVENT_QUEUE: usize = 8192;
/// Short timeout backoff when the event queue is full.
const TIMEOUT_FULL_BACKOFF_MS: u64 = 10;
/// Active status.
const TIMER_STATUS_ACTIVE: u8 = 0;
/// Canceled status.
const TIMER_STATUS_CANCELLED: u8 = 1;
/// Completed status.
const TIMER_STATUS_FINISHED: u8 = 2;

/// Lazy initialized global timer runtime.
static RUNTIME: OnceLock<Result<Arc<TimerRuntime>, String>> = OnceLock::new();

/// Script-visible timer handle.
#[derive(Clone)]
pub struct BtTimer {
    /// Unique timer ID.
    id: usize,
    /// Shared state, used for debug display and identity comparison.
    shared: Arc<TimerShared>,
}

/// Timer type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerKind {
    /// One-shot delay timer.
    Timeout,
    /// Fixed delay repeat timer.
    Interval,
}

/// Timer event delivered to the owning VM.
#[derive(Debug, Clone, Copy)]
pub struct TimerEvent {
    /// Expired timer ID.
    pub id: usize,
}

/// Timer shared status.
struct TimerShared {
    /// Current timer status.
    status: AtomicU8,
}

/// Global timer runtime.
struct TimerRuntime {
    /// Lock-protected scheduling state.
    state: Mutex<TimerRuntimeState>,
    /// Condition variable that schedules threads to wait for new timers or cancellation events.
    ready: Condvar,
}

/// Global timer scheduling status.
struct TimerRuntimeState {
    /// Next process-wide timer ID.
    next_id: usize,
    /// Active timers, the single source of truth for limits and cancellation.
    active: HashMap<usize, Arc<TimerShared>>,
    /// Min-heap sorted by expiration time.
    heap: BinaryHeap<TimerEntry>,
    /// Process-wide active-timer limit.
    limit: usize,
}

/// Statistics snapshot when the timer is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerStats {
    /// Whether the global timer has been initialized during runtime.
    pub runtime_started: bool,
    /// The number of currently active timers.
    pub active: usize,
    /// The number of entries to be triggered in the current scheduling heap.
    pub queued: usize,
    /// Process-level activity timer upper limit.
    pub limit: usize,
    /// Single VM timer event queue length.
    pub event_queue_limit: usize,
}

/// Timer entry in the scheduling heap.
#[derive(Clone)]
struct TimerEntry {
    /// Timer ID.
    id: usize,
    /// Expiration time.
    due: Instant,
    /// Timer type.
    kind: TimerKind,
    /// Fixed interval delay in milliseconds.
    delay_ms: u64,
    /// Event sender for the owning VM.
    sender: SyncSender<TimerEvent>,
}

impl BtTimer {
    /// Returns the timer ID.
    pub fn id(&self) -> usize {
        self.id
    }
}

impl std::fmt::Debug for BtTimer {
    /// Output timer debugging status.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtTimer")
            .field("id", &self.id)
            .field("active", &self.is_active())
            .finish()
    }
}

impl PartialEq for BtTimer {
    /// Compare by timer object identity.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && Arc::ptr_eq(&self.shared, &other.shared)
    }
}

impl TimerEntry {
    /// Creates a new dispatch heap entry.
    fn new(
        id: usize,
        due: Instant,
        kind: TimerKind,
        delay_ms: u64,
        sender: SyncSender<TimerEvent>,
    ) -> Self {
        Self {
            id,
            due,
            kind,
            delay_ms,
            sender,
        }
    }
}

impl PartialEq for TimerEntry {
    /// Compares two heap entries for the same.
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.due == other.due
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    /// Compares deadlines in reverse so `BinaryHeap` behaves as a min-heap.
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    /// Reverses deadline ordering so the earliest entry is popped first.
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .due
            .cmp(&self.due)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl BtTimer {
    /// Determines whether the timer sharing status is still active.
    fn is_active(&self) -> bool {
        self.shared.status.load(Ordering::Acquire) == TIMER_STATUS_ACTIVE
    }
}

/// Registers a new activity timer.
pub fn register(
    kind: TimerKind,
    delay_ms: u64,
    sender: SyncSender<TimerEvent>,
) -> Result<(BtTimer, Instant), String> {
    let runtime = runtime()?;
    let mut state = runtime
        .state
        .lock()
        .map_err(|_| "The timer runtime state lock is poisoned".to_string())?;
    if state.active.len() >= state.limit {
        return Err(format!(
            "The number of active timers exceeds the limit of {}",
            state.limit
        ));
    }

    let id = state.next_id;
    state.next_id = state
        .next_id
        .checked_add(1)
        .ok_or_else(|| "The timer number has been exhausted".to_string())?;
    let shared = Arc::new(TimerShared {
        status: AtomicU8::new(TIMER_STATUS_ACTIVE),
    });
    let due = Instant::now() + Duration::from_millis(delay_ms);
    state.active.insert(id, shared.clone());
    state
        .heap
        .push(TimerEntry::new(id, due, kind, delay_ms, sender));
    runtime.ready.notify_one();
    Ok((BtTimer { id, shared }, due))
}

/// Schedule the next trigger for the interval that is still active.
pub fn schedule(
    timer: &BtTimer,
    kind: TimerKind,
    delay_ms: u64,
    sender: SyncSender<TimerEvent>,
) -> Option<Instant> {
    let runtime = runtime().ok()?;
    let mut state = runtime.state.lock().ok()?;
    if !state.active.contains_key(&timer.id) || !timer.is_active() {
        return None;
    }
    let due = Instant::now() + Duration::from_millis(delay_ms);
    state
        .heap
        .push(TimerEntry::new(timer.id, due, kind, delay_ms, sender));
    runtime.ready.notify_one();
    Some(due)
}

/// Cancels an active timer.
pub fn cancel(id: usize) -> bool {
    let Ok(runtime) = runtime() else {
        return false;
    };
    let mut state = match runtime.state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(shared) = state.active.remove(&id) else {
        return false;
    };
    shared
        .status
        .store(TIMER_STATUS_CANCELLED, Ordering::Release);
    compact_cancelled_entries(&mut state);
    runtime.ready.notify_one();
    true
}

/// Marks a timeout as having completed naturally.
pub fn finish(id: usize) -> bool {
    let Ok(runtime) = runtime() else {
        return false;
    };
    finish_with_runtime(runtime, id)
}

/// Returns the single VM timer event queue length.
pub fn event_queue_limit() -> usize {
    std::env::var("BT_TIMER_EVENT_QUEUE")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_TIMER_EVENT_QUEUE))
        .unwrap_or(DEFAULT_TIMER_EVENT_QUEUE)
}

/// Returns a timer-runtime statistics snapshot.
pub fn stats() -> TimerStats {
    if let Some(Ok(runtime)) = RUNTIME.get() {
        let state = runtime
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        return TimerStats {
            runtime_started: true,
            active: state.active.len(),
            queued: state.heap.len(),
            limit: state.limit,
            event_queue_limit: event_queue_limit(),
        };
    }
    TimerStats {
        runtime_started: false,
        active: 0,
        queued: 0,
        limit: configured_timer_limit(),
        event_queue_limit: event_queue_limit(),
    }
}

/// Returns the lazy-loaded global timer runtime.
fn runtime() -> Result<&'static Arc<TimerRuntime>, String> {
    match RUNTIME.get_or_init(init_runtime) {
        Ok(runtime) => Ok(runtime),
        Err(message) => Err(message.clone()),
    }
}

/// Initializes the global timer runtime and individual scheduling threads.
fn init_runtime() -> Result<Arc<TimerRuntime>, String> {
    let runtime = Arc::new(TimerRuntime {
        state: Mutex::new(TimerRuntimeState {
            next_id: 1,
            active: HashMap::new(),
            heap: BinaryHeap::new(),
            limit: configured_timer_limit(),
        }),
        ready: Condvar::new(),
    });
    let worker_runtime = runtime.clone();
    thread::Builder::new()
        .name("bt-timer".to_string())
        .spawn(move || worker_loop(worker_runtime))
        .map_err(|err| format!("failed to create timer scheduler thread: {}", err))?;
    Ok(runtime)
}

/// Global scheduling thread loop.
fn worker_loop(runtime: Arc<TimerRuntime>) {
    loop {
        let entry = next_due_entry(&runtime);
        dispatch_entry(&runtime, entry);
    }
}

/// Waits for and fetches the next expired entry.
fn next_due_entry(runtime: &Arc<TimerRuntime>) -> TimerEntry {
    let mut state = runtime
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        let Some(entry) = state.heap.peek() else {
            state = runtime
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            continue;
        };
        let now = Instant::now();
        if entry.due <= now {
            return state
                .heap
                .pop()
                .expect("should exist on the top of the timer heap.");
        }
        let wait = entry.due.saturating_duration_since(now);
        let (next_state, _) = runtime
            .ready
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next_state;
    }
}

/// Delivers an expiration event, applying backoff or completion on failure.
fn dispatch_entry(runtime: &Arc<TimerRuntime>, entry: TimerEntry) {
    if !is_active(runtime, entry.id) {
        return;
    }
    match entry.sender.try_send(TimerEvent { id: entry.id }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => handle_full_queue(runtime, entry),
        Err(TrySendError::Disconnected(_)) => {
            finish_with_runtime(runtime, entry.id);
        }
    }
}

/// Handles a full timer queue for the owning VM.
fn handle_full_queue(runtime: &Arc<TimerRuntime>, entry: TimerEntry) {
    let delay_ms = match entry.kind {
        TimerKind::Timeout => TIMEOUT_FULL_BACKOFF_MS,
        TimerKind::Interval => entry.delay_ms.max(1),
    };
    reschedule_entry(runtime, entry, delay_ms);
}

/// Replaces a still-active heap entry.
fn reschedule_entry(runtime: &Arc<TimerRuntime>, mut entry: TimerEntry, delay_ms: u64) {
    let mut state = runtime
        .state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !state.active.contains_key(&entry.id) {
        return;
    }
    entry.due = Instant::now() + Duration::from_millis(delay_ms);
    state.heap.push(entry);
    runtime.ready.notify_one();
}

/// Determine whether the specified number is still in the active table.
fn is_active(runtime: &Arc<TimerRuntime>, id: usize) -> bool {
    runtime
        .state
        .lock()
        .map(|state| state.active.contains_key(&id))
        .unwrap_or(false)
}

/// Completes using an existing runtime mark timer.
fn finish_with_runtime(runtime: &Arc<TimerRuntime>, id: usize) -> bool {
    let mut state = match runtime.state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(shared) = state.active.remove(&id) else {
        return false;
    };
    shared
        .status
        .store(TIMER_STATUS_FINISHED, Ordering::Release);
    compact_cancelled_entries(&mut state);
    runtime.ready.notify_one();
    true
}

/// Compacts the scheduling heap when too many cancellation entries accumulate.
fn compact_cancelled_entries(state: &mut TimerRuntimeState) {
    let threshold = state
        .active
        .len()
        .saturating_mul(2)
        .saturating_add(state.limit);
    if state.heap.len() <= threshold {
        return;
    }
    let old = std::mem::take(&mut state.heap);
    for entry in old.into_vec() {
        if state.active.contains_key(&entry.id) {
            state.heap.push(entry);
        }
    }
}

/// Reads the process activity timer upper limit configuration.
fn configured_timer_limit() -> usize {
    std::env::var("BT_TIMER_LIMIT")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(MAX_TIMER_LIMIT))
        .unwrap_or(DEFAULT_TIMER_LIMIT)
}
