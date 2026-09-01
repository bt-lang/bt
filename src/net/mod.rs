//! BT's internal network implementation.
//!
//! The standard-library entry only dispatches arguments. TCP, UDP, WebSocket, and legacy Web service state lives here,
//! keeping socket types out of the VM's top layer and preventing `libs/net.rs` from becoming a grab bag.

pub mod dns;
pub mod interfaces;
pub mod tcp;
pub mod traits;
pub mod udp;
pub mod web;
pub mod ws;

use crate::web as bt_web;
use std::collections::HashMap;
use std::fmt::Display;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

/// Default network event queue length.
const DEFAULT_NET_EVENT_QUEUE_LIMIT: usize = 4096;
/// Hard upper limit on network event queue length.
const MAX_NET_EVENT_QUEUE_LIMIT: usize = 65536;
/// Default single-protocol connection upper limit.
const DEFAULT_NET_CONNECTION_LIMIT: usize = 4096;
/// Hard limit for single-protocol connection upper limit.
const MAX_NET_CONNECTION_LIMIT: usize = 65536;
/// Default byte limit for a single network message.
const DEFAULT_NET_MESSAGE_LIMIT: usize = 1024 * 1024;
/// Hard upper limit on bytes for a single network message.
const MAX_NET_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;
/// Default single connection write queue length.
const DEFAULT_NET_WRITE_QUEUE_LIMIT: usize = 1024;
/// Hard upper limit on single connection write queue length.
const MAX_NET_WRITE_QUEUE_LIMIT: usize = 8192;
/// Default connection idle TTL, 0 means not to actively close due to idleness.
const DEFAULT_NET_IDLE_TTL_MS: u64 = 0;

/// Network background event.
#[derive(Debug, Clone)]
pub enum NetEvent {
    /// TCP client has been connected.
    TcpConnect {
        /// TCP server ID.
        server_id: usize,
        /// TCP client number.
        client_id: usize,
        /// Client remote address.
        addr: String,
    },
    /// The TCP client sends a segment of byte data.
    TcpMessage {
        /// TCP client number.
        client_id: usize,
        /// Client remote address.
        addr: String,
        /// Raw message bytes.
        data: Vec<u8>,
    },
    /// The TCP client connection has been closed.
    TcpClose {
        /// TCP client number.
        client_id: usize,
        /// Client remote address.
        addr: String,
    },
    /// TCP background task encountered an error.
    TcpError {
        /// TCP server ID, when the server is known.
        server_id: Option<usize>,
        /// The TCP client number when the client can be located.
        client_id: Option<usize>,
        /// Error message.
        message: String,
    },
    /// UDP socket received a message.
    UdpMessage {
        /// UDP socket number.
        socket_id: usize,
        /// Sender address.
        addr: String,
        /// Original message bytes.
        data: Vec<u8>,
    },
    /// The UDP background task encountered an error.
    UdpError {
        /// UDP socket number.
        socket_id: usize,
        /// Error message.
        message: String,
    },
    /// WebSocket client has been connected.
    WsConnect {
        /// WebSocket server ID.
        server_id: usize,
        /// WebSocket connection number.
        socket_id: usize,
        /// Client remote address.
        addr: String,
    },
    /// WebSocket client sends text or binary messages.
    WsMessage {
        /// WebSocket connection number.
        socket_id: usize,
        /// Client remote address.
        addr: String,
        /// Raw message bytes.
        data: Vec<u8>,
    },
    /// The WebSocket connection has been closed.
    WsClose {
        /// WebSocket connection number.
        socket_id: usize,
        /// Client remote address.
        addr: String,
    },
    /// The WebSocket background task encountered an error.
    WsError {
        /// WebSocket server ID, when the server is known.
        server_id: Option<usize>,
        /// WebSocket connection ID, when the connection is known.
        socket_id: Option<usize>,
        /// Error message.
        message: String,
    },
    /// Wakes the VM event loop so it can recheck background activity.
    Wake,
}

/// Web background running status.
struct RuntimeState {
    /// Multi-threaded Tokio runner, shared by all `net.listen({type:'web'})` background services.
    runtime: Option<Runtime>,
    /// Running Web service tasks keyed by server ID for precise cancellation.
    web_handles: HashMap<usize, JoinHandle<Result<(), String>>>,
    /// Next Web server ID.
    next_web_id: usize,
    /// The number of event-based background services such as TCP, UDP, and WebSocket.
    event_services: usize,
}

/// Network resource configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetConfig {
    /// VM network event queue length.
    pub event_queue_limit: usize,
    /// The upper limit of single-protocol connections.
    pub connection_limit: usize,
    /// The upper limit of bytes in a single network message.
    pub message_limit: usize,
    /// Single connection write queue length.
    pub write_queue_limit: usize,
    /// Connection idle TTL, in milliseconds; 0 means turning off this policy.
    pub idle_ttl_ms: u64,
}

/// The network event bus consumed by the VM main thread.
struct EventBus {
    /// Channel entry for network threads to send events.
    sender: SyncSender<NetEvent>,
    /// The VM main thread blocks the channel exit for receiving events.
    receiver: Mutex<Receiver<NetEvent>>,
    /// Bounded queue length.
    limit: usize,
    /// The number of events currently queued for consumption by the VM.
    queued: AtomicUsize,
    /// The number of successfully delivered events.
    sent: AtomicUsize,
    /// The number of events that were rejected because the queue was full or the channel was closed.
    rejected: AtomicUsize,
}

/// Network event sender that can be cloned across background tasks.
#[derive(Clone)]
pub struct NetEventSender {
    /// Bounded event queue sender.
    sender: SyncSender<NetEvent>,
}

/// Network runtime statistics snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetStats {
    /// Whether the Web background Tokio runtime has been initialized.
    pub runtime_started: bool,
    /// The number of currently running web services.
    pub web_services: usize,
    /// The current number of event-based services that require the VM to distribute callbacks.
    pub event_services: usize,
    /// Whether the network event queue has been bounded.
    pub event_queue_bounded: bool,
    /// Network event queue upper limit.
    pub event_queue_limit: Option<usize>,
    /// The current number of queued network event queues.
    pub event_queue_queued: usize,
    /// The number of network events that were successfully delivered to the VM.
    pub event_queue_sent: usize,
    /// The number of network events that were rejected because the queue was full or closed.
    pub event_queue_rejected: usize,
    /// The upper limit of single-protocol connections.
    pub connection_limit: usize,
    /// The upper limit of bytes in a single network message.
    pub message_limit: usize,
    /// Single connection write queue length.
    pub write_queue_limit: usize,
    /// Connection idle TTL, in milliseconds; 0 means turning off this policy.
    pub idle_ttl_ms: u64,
}

/// Global Web background running status.
static STATE: OnceLock<Mutex<RuntimeState>> = OnceLock::new();
/// Global network configuration.
static CONFIG: OnceLock<Result<NetConfig, String>> = OnceLock::new();

/// Determines whether there is currently a background network service.
pub fn has_background_tasks() -> bool {
    runtime_state()
        .lock()
        .map(|state| !state.web_handles.is_empty() || state.event_services > 0)
        .unwrap_or(false)
}

/// Determines whether there is currently a background service that requires a VM to distribute callbacks.
pub fn has_event_tasks() -> bool {
    runtime_state()
        .lock()
        .map(|state| state.event_services > 0)
        .unwrap_or(false)
}

/// Returns a snapshot of network runtime statistics.
pub fn stats() -> NetStats {
    let config = config().unwrap_or_else(|_| fallback_config());
    let (runtime_started, web_services, event_services) = STATE
        .get()
        .map(|state| {
            let state = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                state.runtime.is_some(),
                state.web_handles.len(),
                state.event_services,
            )
        })
        .unwrap_or((false, 0, 0));
    let (event_queue_queued, event_queue_sent, event_queue_rejected, event_queue_limit) =
        raw_event_bus()
            .get()
            .and_then(|result| result.as_ref().ok())
            .map(|bus| {
                (
                    bus.queued.load(Ordering::Relaxed),
                    bus.sent.load(Ordering::Relaxed),
                    bus.rejected.load(Ordering::Relaxed),
                    bus.limit,
                )
            })
            .unwrap_or((0, 0, 0, config.event_queue_limit));
    NetStats {
        runtime_started,
        web_services,
        event_services,
        event_queue_bounded: true,
        event_queue_limit: Some(event_queue_limit),
        event_queue_queued,
        event_queue_sent,
        event_queue_rejected,
        connection_limit: config.connection_limit,
        message_limit: config.message_limit,
        write_queue_limit: config.write_queue_limit,
        idle_ttl_ms: config.idle_ttl_ms,
    }
}

/// Returns the network resource configuration.
pub fn config() -> Result<NetConfig, String> {
    configured().cloned()
}

/// Returns the current maximum message size in bytes.
pub fn message_limit() -> Result<usize, String> {
    configured().map(|config| config.message_limit)
}

/// Format listener binding error.
pub fn bind_error(protocol: &str, bind: &str, err: &std::io::Error) -> String {
    format!(
        "net.listen({}): Listening on `{}` failed: {}",
        protocol,
        bind,
        io_error_reason(err)
    )
}

/// Format connection error.
pub fn connect_error(protocol: &str, target: &str, err: &std::io::Error) -> String {
    format!(
        "net.connect({}): failed to connect to `{}`: {}",
        protocol,
        target,
        io_error_reason(err)
    )
}

/// Formatted connection timeout error.
pub fn connect_timeout_error(protocol: &str, target: &str, timeout_ms: u128) -> String {
    format!(
        "net.connect({}): Connection `{}` timed out, waited {} milliseconds",
        protocol, target, timeout_ms
    )
}

/// Format protocol handshake error.
pub fn handshake_error(protocol: &str, target: &str, err: impl Display) -> String {
    format!(
        "net.connect({}): Handshake failed with `{}`: {}",
        protocol, target, err
    )
}

/// Format common I/O error.
pub fn io_error(operation: &str, err: &std::io::Error) -> String {
    format!("{}: {}", operation, io_error_reason(err))
}

/// Format generic protocol error.
pub fn protocol_error(operation: &str, err: impl Display) -> String {
    format!("{}: {}", operation, err)
}

/// Formatting closed resource error.
pub fn closed_error(operation: &str, resource: &str) -> String {
    format!("{}: {} is closed", operation, resource)
}

/// Format write queue full error.
pub fn queue_full_error(operation: &str) -> String {
    format!("{}: write queue is full", operation)
}

/// Format event queue full error.
pub fn event_queue_full_error(operation: &str) -> String {
    format!("{}: network event queue is full", operation)
}

/// Formats an error for a message that exceeds the configured size limit.
pub fn message_limit_error(operation: &str, size: usize, limit: usize) -> String {
    format!(
        "{}: message size of {} bytes exceeds the {}-byte limit",
        operation, size, limit
    )
}

/// Format operation timeout error.
pub fn timeout_error(operation: &str, timeout_ms: u128) -> String {
    format!(
        "{}: operation timed out after {} milliseconds",
        operation, timeout_ms
    )
}

/// Format underlying I/O error reason.
fn io_error_reason(err: &std::io::Error) -> String {
    match err.kind() {
        ErrorKind::AddrInUse => format!("address is already in use ({})", err),
        ErrorKind::PermissionDenied => format!("permission denied ({})", err),
        ErrorKind::AddrNotAvailable => format!("address is not available ({})", err),
        ErrorKind::InvalidInput => format!("invalid address or parameters ({})", err),
        ErrorKind::ConnectionRefused => format!("connection refused ({})", err),
        ErrorKind::ConnectionReset => format!("connection was reset by the peer ({})", err),
        ErrorKind::ConnectionAborted => format!("connection was aborted ({})", err),
        ErrorKind::TimedOut => format!("operation timed out ({})", err),
        ErrorKind::NotConnected => format!("connection was not established ({})", err),
        ErrorKind::NotFound => format!("target does not exist ({})", err),
        _ => err.to_string(),
    }
}

/// Registers an event-based background service.
pub fn register_event_service() {
    if let Ok(mut state) = runtime_state().lock() {
        state.event_services = state.event_services.saturating_add(1);
    }
}

/// Log out of an event-based background service.
pub fn unregister_event_service() {
    if let Ok(mut state) = runtime_state().lock() {
        state.event_services = state.event_services.saturating_sub(1);
    }
    send_event(NetEvent::Wake);
}

/// Sends a network event to the VM main thread.
pub fn send_event(event: NetEvent) -> bool {
    event_bus()
        .map(|bus| bus.try_send(&bus.sender, event))
        .unwrap_or(false)
}

/// Returns the network event sender.
pub fn event_sender() -> Result<NetEventSender, String> {
    Ok(NetEventSender {
        sender: event_bus()?.sender.clone(),
    })
}

/// Waits for a network event, returning `None` on timeout.
pub fn recv_event(timeout: Duration) -> Option<NetEvent> {
    let bus = event_bus().ok()?;
    let receiver = bus.receiver.lock().ok()?;
    match receiver.recv_timeout(timeout) {
        Ok(event) => {
            bus.queued.fetch_sub(1, Ordering::Relaxed);
            Some(event)
        }
        Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

/// Starts a Web background task and returns its server ID.
pub fn spawn_web_service(config: bt_web::WebConfig) -> Result<usize, String> {
    let mut state = runtime_state()
        .lock()
        .map_err(|_| "The network runtime state lock is poisoned".to_string())?;
    if state.runtime.is_none() {
        state.runtime = Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(bt_web::worker_threads())
                .enable_all()
                .build()
                .map_err(|err| format!("failed to create the network runtime: {}", err))?,
        );
    }
    let id = state.next_web_id;
    state.next_web_id = state.next_web_id.saturating_add(1);
    let (started_tx, started_rx) = mpsc::channel();
    let handle = {
        let runtime = state
            .runtime
            .as_ref()
            .ok_or_else(|| "The network runner is not initialized".to_string())?;
        runtime
            .spawn(async move { bt_web::serve_with_start_signal(config, Some(started_tx)).await })
    };
    match started_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            handle.abort();
            return Err(err);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            handle.abort();
            return Err("Timeout waiting for the Web service listener to start".to_string());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            handle.abort();
            return Err("The Web service listener startup failed".to_string());
        }
    }
    state.web_handles.insert(id, handle);
    Ok(id)
}

/// Close the specified Web background task.
pub fn close_web_service(id: usize) -> Result<(), String> {
    let handle = runtime_state()
        .lock()
        .map_err(|_| "The network runtime state lock is poisoned".to_string())?
        .web_handles
        .remove(&id);
    if let Some(handle) = handle {
        handle.abort();
    }
    send_event(NetEvent::Wake);
    Ok(())
}

/// Actively stops all web background tasks.
///
/// CLI resident mode lets the Web service run until the process exits, so it cannot reuse this abort path. Desktop hot reload uses it to release the old port
/// before running `server.bt` or `app.main` again.
#[allow(dead_code)]
pub fn stop_web_services() -> Result<(), String> {
    let (runtime, handles) = {
        let mut state = runtime_state()
            .lock()
            .map_err(|_| "The network runtime state lock is poisoned".to_string())?;
        if state.web_handles.is_empty() {
            return Ok(());
        }
        let runtime = state
            .runtime
            .take()
            .ok_or_else(|| "The network runner is not initialized".to_string())?;
        let handles: Vec<JoinHandle<Result<(), String>>> = state
            .web_handles
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        (runtime, handles)
    };

    for handle in &handles {
        handle.abort();
    }

    runtime.block_on(async move {
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => eprintln!("{}", err),
                Err(err) => {
                    if !err.is_cancelled() {
                        eprintln!(
                            "The network background service exited unexpectedly: {}",
                            err
                        );
                    }
                }
            }
        }
    });
    send_event(NetEvent::Wake);
    Ok(())
}

/// Wait for all web background tasks to end.
pub fn wait_for_background_tasks() -> Result<(), String> {
    let (runtime, handles) = {
        let mut state = runtime_state()
            .lock()
            .map_err(|_| "The network runtime state lock is poisoned".to_string())?;
        if state.web_handles.is_empty() {
            return Ok(());
        }
        let runtime = state
            .runtime
            .take()
            .ok_or_else(|| "The network runner is not initialized".to_string())?;
        let handles: Vec<JoinHandle<Result<(), String>>> = state
            .web_handles
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        (runtime, handles)
    };
    runtime.block_on(async move {
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => eprintln!("{}", err),
                Err(err) => {
                    if !err.is_cancelled() {
                        eprintln!(
                            "The network background service exited unexpectedly: {}",
                            err
                        );
                    }
                }
            }
        }
    });
    Ok(())
}

/// Returns the global web background running status.
fn runtime_state() -> &'static Mutex<RuntimeState> {
    STATE.get_or_init(|| {
        Mutex::new(RuntimeState {
            runtime: None,
            web_handles: HashMap::new(),
            next_web_id: 1,
            event_services: 0,
        })
    })
}

impl NetEventSender {
    /// Attempts to deliver a network event.
    pub fn send(&self, event: NetEvent) -> bool {
        event_bus()
            .map(|bus| bus.try_send(&self.sender, event))
            .unwrap_or(false)
    }
}

impl EventBus {
    /// Creates a bounded network event bus.
    fn new(limit: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(limit);
        Self {
            sender,
            receiver: Mutex::new(receiver),
            limit,
            queued: AtomicUsize::new(0),
            sent: AtomicUsize::new(0),
            rejected: AtomicUsize::new(0),
        }
    }

    /// Attempts to write events to the specified sender.
    fn try_send(&self, sender: &SyncSender<NetEvent>, event: NetEvent) -> bool {
        match sender.try_send(event) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
                self.sent.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
}

/// Returns the global network event bus.
fn event_bus() -> Result<&'static EventBus, String> {
    match raw_event_bus().get_or_init(|| {
        let config = configured()?;
        Ok(EventBus::new(config.event_queue_limit))
    }) {
        Ok(bus) => Ok(bus),
        Err(err) => Err(err.clone()),
    }
}

/// Returns the network event bus initialization slot.
fn raw_event_bus() -> &'static OnceLock<Result<EventBus, String>> {
    static BUS: OnceLock<Result<EventBus, String>> = OnceLock::new();
    &BUS
}

/// Returns the parsed network configuration.
fn configured() -> Result<&'static NetConfig, String> {
    match CONFIG.get_or_init(NetConfig::from_env) {
        Ok(config) => Ok(config),
        Err(err) => Err(err.clone()),
    }
}

impl NetConfig {
    /// Reads network resource configuration from environment variables.
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            event_queue_limit: read_usize_env(
                "BT_NET_EVENT_QUEUE",
                DEFAULT_NET_EVENT_QUEUE_LIMIT,
                1,
                MAX_NET_EVENT_QUEUE_LIMIT,
            )?,
            connection_limit: read_usize_env(
                "BT_NET_CONNECTION_LIMIT",
                DEFAULT_NET_CONNECTION_LIMIT,
                1,
                MAX_NET_CONNECTION_LIMIT,
            )?,
            message_limit: read_usize_env(
                "BT_NET_MESSAGE_LIMIT",
                DEFAULT_NET_MESSAGE_LIMIT,
                1,
                MAX_NET_MESSAGE_LIMIT,
            )?,
            write_queue_limit: read_usize_env(
                "BT_NET_WRITE_QUEUE",
                DEFAULT_NET_WRITE_QUEUE_LIMIT,
                1,
                MAX_NET_WRITE_QUEUE_LIMIT,
            )?,
            idle_ttl_ms: read_u64_env("BT_NET_IDLE_TTL_MS", DEFAULT_NET_IDLE_TTL_MS)?,
        })
    }
}

/// Returns a conservative configuration used for statistics display when environment variables cannot be resolved.
fn fallback_config() -> NetConfig {
    NetConfig {
        event_queue_limit: DEFAULT_NET_EVENT_QUEUE_LIMIT,
        connection_limit: DEFAULT_NET_CONNECTION_LIMIT,
        message_limit: DEFAULT_NET_MESSAGE_LIMIT,
        write_queue_limit: DEFAULT_NET_WRITE_QUEUE_LIMIT,
        idle_ttl_ms: DEFAULT_NET_IDLE_TTL_MS,
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
fn read_u64_env(name: &str, default: u64) -> Result<u64, String> {
    let Some(value) = std::env::var(name).ok() else {
        return Ok(default);
    };
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{} must be an integer not less than 0", name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When the bounded event queue is full, it should be rejected immediately and the number of rejections should be recorded.
    #[test]
    fn event_bus_rejects_when_queue_is_full() {
        let bus = EventBus::new(1);
        assert!(bus.try_send(&bus.sender, NetEvent::Wake));
        assert!(!bus.try_send(&bus.sender, NetEvent::Wake));
        assert_eq!(bus.queued.load(Ordering::Relaxed), 1);
        assert_eq!(bus.sent.load(Ordering::Relaxed), 1);
        assert_eq!(bus.rejected.load(Ordering::Relaxed), 1);
    }

    /// Network error formatting should cover common system error categories.
    #[test]
    fn net_error_messages_are_standardized() {
        let addr_in_use = std::io::Error::from(ErrorKind::AddrInUse);
        let reset = std::io::Error::from(ErrorKind::ConnectionReset);

        assert!(
            bind_error("tcp", "127.0.0.1:9000", &addr_in_use).contains("address is already in use")
        );
        assert!(connect_error("tcp", "127.0.0.1:9000", &reset)
            .contains("connection was reset by the peer"));
        assert_eq!(
            message_limit_error("tcp.write", 2048, 1024),
            "tcp.write: message size of 2048 bytes exceeds the 1024-byte limit"
        );
        assert_eq!(queue_full_error("ws.send"), "ws.send: write queue is full");
    }
}
