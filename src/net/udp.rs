//! UDP server and client implementation.

use crate::libs::bytes;
use crate::net::traits::BtNetServer;
use crate::net::{self, NetEvent, NetEventSender};
use crate::value::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::net::UdpSocket;

/// Maximum payload size of a UDP datagram.
const UDP_MAX_DATAGRAM_SIZE: usize = 65_507;
/// UDP shutdown check interval.
const UDP_CLOSE_POLL: Duration = Duration::from_millis(200);
/// UDP error backoff time.
const UDP_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// UDP socket handle.
#[derive(Debug, Clone, PartialEq)]
pub struct UdpSocketHandle {
    /// UDP socket number.
    id: usize,
    /// Local address visible to scripts.
    addr: String,
}

/// UDP socket registry entry.
struct UdpSocketEntry {
    /// Tokio UDP socket.
    socket: Arc<UdpSocket>,
    /// Default remote address, only available in client mode.
    remote: Option<String>,
    /// Socket shutdown flag.
    closed: Arc<AtomicBool>,
}

/// UDP global registry.
struct UdpState {
    /// Next socket number.
    next_socket_id: usize,
    /// Active UDP sockets.
    sockets: HashMap<usize, UdpSocketEntry>,
}

impl UdpSocketHandle {
    /// Creates a UDP socket handle.
    pub fn new(id: usize, addr: String) -> Self {
        Self { id, addr }
    }

    /// Returns the UDP socket number.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Dispatches a UDP socket method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "send" => {
                let data = args
                    .first()
                    .map(|value| bytes::value_to_bytes(value, "udp.send"))
                    .transpose()?;
                let addr = args
                    .get(1)
                    .map(Value::to_string)
                    .filter(|addr| !addr.is_empty());
                let written =
                    send_socket(self.id, data.as_deref().unwrap_or(&[]), addr.as_deref())?;
                Ok(Value::Int(written as i64))
            }
            "close" => {
                self.close()?;
                Ok(Value::Bool(true))
            }
            _ => Err(format!("udp socket has no method `{}`", method)),
        }
    }
}

impl BtNetServer for UdpSocketHandle {
    /// Closes the UDP socket.
    fn close(&self) -> Result<(), String> {
        close_socket(self.id)
    }

    /// Returns the UDP local address.
    fn addr(&self) -> String {
        self.addr.clone()
    }

    /// Returns the socket type name.
    fn kind(&self) -> &'static str {
        "udp"
    }
}

/// Starts a UDP listening socket.
pub fn listen(bind: &str) -> Result<UdpSocketHandle, String> {
    let config = net::config()?;
    let bind_text = bind.to_string();
    let socket = crate::io::run_async(
        async move {
            UdpSocket::bind(&bind_text)
                .await
                .map_err(|err| net::bind_error("udp", &bind_text, &err))
        },
        Some(crate::io::default_timeout()),
    )?;
    let addr = socket
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| bind.to_string());
    let socket = Arc::new(socket);
    let closed = Arc::new(AtomicBool::new(false));
    let id = insert_socket(
        socket.clone(),
        None,
        closed.clone(),
        config.connection_limit,
    )?;
    let sender = net::event_sender()?;
    net::register_event_service();
    if let Err(err) =
        crate::io::spawn_async(recv_loop(id, socket, closed, sender, config.message_limit))
    {
        remove_socket(id);
        net::unregister_event_service();
        return Err(format!(
            "net.listen(udp): failed to start `{}`: {}",
            bind, err
        ));
    }
    Ok(UdpSocketHandle::new(id, addr))
}

/// Create UDP client socket.
pub fn connect(host: &str, port: u16) -> Result<UdpSocketHandle, String> {
    let config = net::config()?;
    let remote = format!("{}:{}", host, port);
    let remote_for_task = remote.clone();
    let socket = crate::io::run_async(
        async move {
            let socket = UdpSocket::bind("0.0.0.0:0")
                .await
                .map_err(|err| net::io_error("net.connect(udp): create socket", &err))?;
            socket
                .connect(&remote_for_task)
                .await
                .map_err(|err| net::connect_error("udp", &remote_for_task, &err))?;
            Ok(socket)
        },
        Some(crate::io::default_timeout()),
    )?;
    let addr = socket
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "0.0.0.0:0".to_string());
    let id = insert_socket(
        Arc::new(socket),
        Some(remote),
        Arc::new(AtomicBool::new(false)),
        config.connection_limit,
    )?;
    Ok(UdpSocketHandle::new(id, addr))
}

/// UDP receive loop.
async fn recv_loop(
    id: usize,
    socket: Arc<UdpSocket>,
    closed: Arc<AtomicBool>,
    sender: NetEventSender,
    message_limit: usize,
) {
    let mut buffer = vec![0u8; udp_read_buffer_size(message_limit)];
    while !closed.load(Ordering::Relaxed) {
        match tokio::time::timeout(UDP_CLOSE_POLL, socket.recv_from(&mut buffer)).await {
            Err(_) => continue,
            Ok(Ok((size, addr))) => {
                let data = buffer[..size].to_vec();
                let _ = sender.send(NetEvent::UdpMessage {
                    socket_id: id,
                    addr: addr.to_string(),
                    data,
                });
            }
            Ok(Err(err)) => {
                let _ = sender.send(NetEvent::UdpError {
                    socket_id: id,
                    message: net::io_error("udp.recv", &err),
                });
                tokio::time::sleep(UDP_ERROR_BACKOFF).await;
            }
        }
    }
    remove_socket(id);
    net::unregister_event_service();
}

/// Send UDP data.
fn send_socket(id: usize, data: &[u8], addr: Option<&str>) -> Result<usize, String> {
    let (socket, remote, closed) = {
        let state = udp_state()
            .lock()
            .map_err(|_| "UDP state lock is poisoned".to_string())?;
        let entry = state
            .sockets
            .get(&id)
            .ok_or_else(|| net::closed_error("udp.send", "socket"))?;
        (
            entry.socket.clone(),
            entry.remote.clone(),
            entry.closed.clone(),
        )
    };
    if closed.load(Ordering::Relaxed) {
        return Err(net::closed_error("udp.send", "socket"));
    }
    if data.len() > net::message_limit()? {
        return Err(net::message_limit_error(
            "udp.send",
            data.len(),
            net::message_limit()?,
        ));
    }
    let payload = data.to_vec();
    let target = addr.map(str::to_string);
    crate::io::run_async(
        async move {
            if let Some(addr) = target {
                socket
                    .send_to(&payload, &addr)
                    .await
                    .map_err(|err| net::io_error("udp.send", &err))
            } else if remote.is_some() {
                socket
                    .send(&payload)
                    .await
                    .map_err(|err| net::io_error("udp.send", &err))
            } else {
                Err("udp.send: destination address is required".to_string())
            }
        },
        Some(crate::io::default_timeout()),
    )
}

/// Closes the UDP socket.
fn close_socket(id: usize) -> Result<(), String> {
    let entry = udp_state()
        .lock()
        .map_err(|_| "UDP state lock is poisoned".to_string())?
        .sockets
        .remove(&id);
    if let Some(entry) = entry {
        entry.closed.store(true, Ordering::Relaxed);
        net::send_event(NetEvent::Wake);
    }
    Ok(())
}

/// Insert UDP socket registry entry.
fn insert_socket(
    socket: Arc<UdpSocket>,
    remote: Option<String>,
    closed: Arc<AtomicBool>,
    limit: usize,
) -> Result<usize, String> {
    let mut state = udp_state()
        .lock()
        .map_err(|_| "UDP state lock is poisoned".to_string())?;
    if state.sockets.len() >= limit {
        return Err(format!("UDP socket limit of {} has been reached", limit));
    }
    let id = state.next_socket_id;
    state.next_socket_id = state.next_socket_id.saturating_add(1);
    state.sockets.insert(
        id,
        UdpSocketEntry {
            socket,
            remote,
            closed,
        },
    );
    Ok(id)
}

/// Remove UDP socket registry entry.
fn remove_socket(id: usize) {
    if let Ok(mut state) = udp_state().lock() {
        state.sockets.remove(&id);
    }
}

/// Returns the UDP receive buffer size.
fn udp_read_buffer_size(message_limit: usize) -> usize {
    message_limit.clamp(1, UDP_MAX_DATAGRAM_SIZE)
}

/// Returns UDP global status.
fn udp_state() -> &'static Mutex<UdpState> {
    static STATE: OnceLock<Mutex<UdpState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(UdpState {
            next_socket_id: 1,
            sockets: HashMap::new(),
        })
    })
}
