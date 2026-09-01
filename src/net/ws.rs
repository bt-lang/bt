//! WebSocket server and client implementation.

use crate::libs::bytes as bt_bytes;
use crate::net::traits::{BtNetConnection, BtNetServer};
use crate::net::{self, NetConfig, NetEvent, NetEventSender};
use crate::value::Value;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc as tokio_mpsc;
use tokio_tungstenite::tungstenite::error::Error as WsError;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::{accept_hdr_async, connect_async, WebSocketStream};

/// Shutdown polling interval for the WebSocket accept loop.
const WS_ACCEPT_POLL: Duration = Duration::from_millis(200);
/// WebSocket accept error backoff time.
const WS_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// WebSocket service handle.
#[derive(Debug, Clone, PartialEq)]
pub struct WsServerHandle {
    /// Server ID.
    id: usize,
    /// The listening address visible to the script.
    addr: String,
}

/// WebSocket connection handle.
#[derive(Debug, Clone, PartialEq)]
pub struct WsSocketHandle {
    /// Connection number.
    id: usize,
    /// The remote address visible to the script.
    addr: String,
}

/// WebSocket server registry entry.
struct WsServerEntry {
    /// Service shutdown flag.
    closed: Arc<AtomicBool>,
}

/// WebSocket connection registry entry.
struct WsSocketEntry {
    /// Write command sender.
    command_tx: tokio_mpsc::Sender<WsCommand>,
    /// Connection close flag.
    closed: Arc<AtomicBool>,
}

/// WebSocket global registry.
struct WsState {
    /// Next server ID.
    next_server_id: usize,
    /// Next connection number.
    next_socket_id: usize,
    /// Active WebSocket servers.
    servers: HashMap<usize, WsServerEntry>,
    /// Active WebSocket connections.
    sockets: HashMap<usize, WsSocketEntry>,
}

/// WebSocket write task command.
enum WsCommand {
    /// Send a text message.
    SendText(String),
    /// Sends a binary message.
    SendBinary(Vec<u8>),
    /// Respond to ping.
    Pong(Bytes),
    /// Closes the connection.
    Close,
}

impl WsServerHandle {
    /// Creates a WebSocket service handle.
    pub fn new(id: usize, addr: String) -> Self {
        Self { id, addr }
    }

    /// Returns the server ID.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Dispatches a WebSocket server method.
    pub fn call_method(&self, method: &str, _args: Vec<Value>) -> Result<Value, String> {
        match method {
            "close" => {
                self.close()?;
                Ok(Value::Bool(true))
            }
            _ => Err(format!("ws server has no method `{}`", method)),
        }
    }
}

impl BtNetServer for WsServerHandle {
    /// Closes the WebSocket server.
    fn close(&self) -> Result<(), String> {
        close_server(self.id)
    }

    /// Returns the WebSocket server's listening address.
    fn addr(&self) -> String {
        self.addr.clone()
    }

    /// Returns the server type name.
    fn kind(&self) -> &'static str {
        "ws"
    }
}

impl WsSocketHandle {
    /// Creates a WebSocket connection handle.
    pub fn new(id: usize, addr: String) -> Self {
        Self { id, addr }
    }

    /// Returns the WebSocket connection number.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Calls the WebSocket connection method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "send" | "write" => {
                match args.first() {
                    Some(Value::Bytes(bytes)) => send_binary_socket(self.id, bytes.as_slice())?,
                    Some(Value::Array(_)) => {
                        let data = bt_bytes::value_to_bytes(
                            args.first().expect("array checked above"),
                            "ws.send",
                        )?;
                        send_binary_socket(self.id, data.as_ref())?;
                    }
                    Some(value) => send_text_socket(self.id, value.to_string())?,
                    None => send_text_socket(self.id, String::new())?,
                }
                Ok(Value::Bool(true))
            }
            "close" => {
                self.close()?;
                Ok(Value::Bool(true))
            }
            _ => Err(format!("ws socket has no method `{}`", method)),
        }
    }
}

impl BtNetConnection for WsSocketHandle {
    /// WebSocket read operations are distributed by background tasks, and synchronous reads are not exposed here.
    fn read(&self) -> Result<Vec<u8>, String> {
        Err("ws.read: receive WebSocket data through on_message".to_string())
    }

    /// Send WebSocket text message.
    fn write(&self, data: &[u8]) -> Result<usize, String> {
        let text = std::str::from_utf8(data)
            .map_err(|_| {
                "ws.write: Data is not valid UTF-8; please use bytes() to send binary".to_string()
            })?
            .to_string();
        send_text_socket(self.id, text)?;
        Ok(data.len())
    }

    /// Close WebSocket connection.
    fn close(&self) -> Result<(), String> {
        close_socket(self.id)
    }

    /// Returns the WebSocket connection's remote address.
    fn addr(&self) -> String {
        self.addr.clone()
    }

    /// Returns the connection type name.
    fn kind(&self) -> &'static str {
        "ws"
    }
}

/// Starts the WebSocket listening service.
pub fn listen(bind: &str, route: &str) -> Result<WsServerHandle, String> {
    let config = net::config()?;
    let bind_text = bind.to_string();
    let listener = crate::io::run_async(
        async move {
            TcpListener::bind(&bind_text)
                .await
                .map_err(|err| net::bind_error("ws", &bind_text, &err))
        },
        Some(crate::io::default_timeout()),
    )?;
    let addr = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| bind.to_string());
    let closed = Arc::new(AtomicBool::new(false));
    let id = insert_server(closed.clone())?;
    let sender = net::event_sender()?;
    net::register_event_service();
    if let Err(err) = crate::io::spawn_async(accept_loop(
        id,
        listener,
        route.to_string(),
        closed,
        sender,
        config.clone(),
    )) {
        remove_server(id);
        net::unregister_event_service();
        return Err(format!(
            "net.listen(ws): failed to start `{}`: {}",
            bind, err
        ));
    }
    Ok(WsServerHandle::new(id, addr))
}

/// Establishing WebSocket client connection.
pub fn connect(url: &str) -> Result<WsSocketHandle, String> {
    let config = net::config()?;
    let parsed = url::Url::parse(url)
        .map_err(|err| format!("net.connect(ws): invalid URL `{}`: {}", url, err))?;
    if parsed.scheme() != "ws" {
        return Err(format!(
            "net.connect(ws): URL scheme `{}` is not supported; only `ws` is currently available",
            parsed.scheme()
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("net.connect(ws): URL `{}` is missing host", url))?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("net.connect(ws): URL `{}` missing port", url))?;
    let addr = format!("{}:{}", host, port);
    let url_text = url.to_string();
    let (socket, _) = crate::io::run_async(
        async move {
            connect_async(&url_text)
                .await
                .map_err(|err| net::handshake_error("ws", &url_text, err))
        },
        Some(crate::io::default_timeout()),
    )?;
    let (command_tx, command_rx) = tokio_mpsc::channel(config.write_queue_limit);
    let closed = Arc::new(AtomicBool::new(false));
    let sender = net::event_sender()?;
    let id = insert_socket(command_tx.clone(), closed.clone(), config.connection_limit)?;
    net::register_event_service();
    if let Err(err) = start_socket_tasks(
        None, id, socket, command_tx, command_rx, closed, addr, sender, &config, true,
    ) {
        remove_socket(id);
        net::unregister_event_service();
        return Err(err);
    }
    Ok(WsSocketHandle::new(id, url.to_string()))
}

/// WebSocket accept loop.
async fn accept_loop(
    server_id: usize,
    listener: TcpListener,
    route: String,
    closed: Arc<AtomicBool>,
    sender: NetEventSender,
    config: NetConfig,
) {
    while !closed.load(Ordering::Relaxed) {
        match tokio::time::timeout(WS_ACCEPT_POLL, listener.accept()).await {
            Err(_) => continue,
            Ok(Ok((stream, addr))) => {
                let addr = addr.to_string();
                let route = route.clone();
                let sender = sender.clone();
                let config = config.clone();
                if let Err(err) = crate::io::spawn_async(handle_socket(
                    server_id,
                    stream,
                    addr,
                    route,
                    sender.clone(),
                    config,
                )) {
                    let _ = sender.send(NetEvent::WsError {
                        server_id: Some(server_id),
                        socket_id: None,
                        message: format!("ws.accept: failed to start connection task: {}", err),
                    });
                }
            }
            Ok(Err(err)) => {
                let _ = sender.send(NetEvent::WsError {
                    server_id: Some(server_id),
                    socket_id: None,
                    message: net::io_error("ws.accept", &err),
                });
                tokio::time::sleep(WS_ERROR_BACKOFF).await;
            }
        }
    }
    remove_server(server_id);
    net::unregister_event_service();
}

/// Handles a single WebSocket server connection.
async fn handle_socket(
    server_id: usize,
    stream: TcpStream,
    addr: String,
    route: String,
    sender: NetEventSender,
    config: NetConfig,
) {
    let route_for_callback = route.clone();
    let callback = move |request: &Request,
                         response: Response|
          -> Result<Response, ErrorResponse> {
        if request.uri().path() == route_for_callback {
            Ok(response)
        } else {
            let mut response =
                ErrorResponse::new(Some("WebSocket route does not exist".to_string()));
            *response.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::NOT_FOUND;
            Err(response)
        }
    };
    let socket = match accept_hdr_async(stream, callback).await {
        Ok(socket) => socket,
        Err(err) => {
            let _ = sender.send(NetEvent::WsError {
                server_id: Some(server_id),
                socket_id: None,
                message: net::protocol_error("ws.accept: Handshake failed", err),
            });
            return;
        }
    };
    let (command_tx, command_rx) = tokio_mpsc::channel(config.write_queue_limit);
    let closed = Arc::new(AtomicBool::new(false));
    let socket_id = match insert_socket(command_tx.clone(), closed.clone(), config.connection_limit)
    {
        Ok(id) => id,
        Err(err) => {
            let _ = sender.send(NetEvent::WsError {
                server_id: Some(server_id),
                socket_id: None,
                message: err,
            });
            return;
        }
    };
    if !sender.send(NetEvent::WsConnect {
        server_id,
        socket_id,
        addr: addr.clone(),
    }) {
        remove_socket(socket_id);
        let _ = sender.send(NetEvent::WsError {
            server_id: Some(server_id),
            socket_id: None,
            message: net::event_queue_full_error("ws.connect"),
        });
        return;
    }
    if let Err(err) = start_socket_tasks(
        Some(server_id),
        socket_id,
        socket,
        command_tx,
        command_rx,
        closed,
        addr,
        sender.clone(),
        &config,
        false,
    ) {
        remove_socket(socket_id);
        let _ = sender.send(NetEvent::WsError {
            server_id: Some(server_id),
            socket_id: Some(socket_id),
            message: err,
        });
    }
}

/// Start WebSocket read and write tasks.
fn start_socket_tasks<S>(
    server_id: Option<usize>,
    socket_id: usize,
    socket: WebSocketStream<S>,
    command_tx: tokio_mpsc::Sender<WsCommand>,
    command_rx: tokio_mpsc::Receiver<WsCommand>,
    closed: Arc<AtomicBool>,
    addr: String,
    sender: NetEventSender,
    config: &NetConfig,
    unregister_on_close: bool,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (writer, reader) = socket.split();
    crate::io::spawn_async(ws_write_loop(
        server_id,
        socket_id,
        writer,
        command_rx,
        closed.clone(),
        sender.clone(),
    ))
    .map_err(|err| format!("ws.socket: failed to start the write task: {}", err))?;
    crate::io::spawn_async(ws_read_loop(
        server_id,
        socket_id,
        reader,
        command_tx,
        closed,
        addr,
        sender,
        config.message_limit,
        (config.idle_ttl_ms > 0).then_some(Duration::from_millis(config.idle_ttl_ms)),
        unregister_on_close,
    ))
    .map_err(|err| format!("ws.socket: failed to start the read task: {}", err))?;
    Ok(())
}

/// WebSocket read loop.
async fn ws_read_loop<S>(
    server_id: Option<usize>,
    socket_id: usize,
    mut reader: SplitStream<WebSocketStream<S>>,
    command_tx: tokio_mpsc::Sender<WsCommand>,
    closed: Arc<AtomicBool>,
    addr: String,
    sender: NetEventSender,
    message_limit: usize,
    idle_ttl: Option<Duration>,
    unregister_on_close: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while !closed.load(Ordering::Relaxed) {
        let message = match idle_ttl {
            Some(timeout) => match tokio::time::timeout(timeout, reader.next()).await {
                Ok(message) => message,
                Err(_) => {
                    let _ = sender.send(NetEvent::WsError {
                        server_id,
                        socket_id: Some(socket_id),
                        message: format!(
                            "ws: Connection idle for more than {} milliseconds",
                            timeout.as_millis()
                        ),
                    });
                    break;
                }
            },
            None => reader.next().await,
        };
        match message {
            Some(Ok(Message::Text(text))) => {
                if text.len() > message_limit {
                    send_ws_size_error(&sender, server_id, socket_id, text.len(), message_limit);
                    break;
                }
                if !sender.send(NetEvent::WsMessage {
                    socket_id,
                    addr: addr.clone(),
                    data: text.to_string().into_bytes(),
                }) {
                    break;
                }
            }
            Some(Ok(Message::Binary(data))) => {
                if data.len() > message_limit {
                    send_ws_size_error(&sender, server_id, socket_id, data.len(), message_limit);
                    break;
                }
                if !sender.send(NetEvent::WsMessage {
                    socket_id,
                    addr: addr.clone(),
                    data: data.to_vec(),
                }) {
                    break;
                }
            }
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(Message::Ping(data))) => {
                let _ = command_tx.try_send(WsCommand::Pong(data));
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Err(WsError::ConnectionClosed)) | Some(Err(WsError::AlreadyClosed)) => break,
            Some(Err(err)) => {
                let _ = sender.send(NetEvent::WsError {
                    server_id,
                    socket_id: Some(socket_id),
                    message: net::protocol_error("ws.read", err),
                });
                break;
            }
        }
    }
    let already_closed = closed.swap(true, Ordering::Relaxed);
    remove_socket(socket_id);
    let _ = command_tx.try_send(WsCommand::Close);
    if !already_closed {
        let _ = sender.send(NetEvent::WsClose { socket_id, addr });
    }
    if unregister_on_close {
        net::unregister_event_service();
    }
}

/// WebSocket write loop.
async fn ws_write_loop<S>(
    server_id: Option<usize>,
    socket_id: usize,
    mut writer: SplitSink<WebSocketStream<S>, Message>,
    mut command_rx: tokio_mpsc::Receiver<WsCommand>,
    closed: Arc<AtomicBool>,
    sender: NetEventSender,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(command) = command_rx.recv().await {
        if closed.load(Ordering::Relaxed) && !matches!(command, WsCommand::Close) {
            break;
        }
        let result = match command {
            WsCommand::SendText(text) => writer.send(Message::Text(text.into())).await,
            WsCommand::SendBinary(data) => writer.send(Message::Binary(data.into())).await,
            WsCommand::Pong(data) => writer.send(Message::Pong(data)).await,
            WsCommand::Close => {
                closed.store(true, Ordering::Relaxed);
                writer.close().await
            }
        };
        if let Err(err) = result {
            closed.store(true, Ordering::Relaxed);
            let _ = sender.send(NetEvent::WsError {
                server_id,
                socket_id: Some(socket_id),
                message: net::protocol_error("ws.write", err),
            });
            break;
        }
    }
}

/// Send WebSocket text message.
fn send_text_socket(id: usize, data: String) -> Result<(), String> {
    let limit = net::message_limit()?;
    if data.len() > limit {
        return Err(net::message_limit_error("ws.send", data.len(), limit));
    }
    let (tx, closed) = {
        let state = ws_state()
            .lock()
            .map_err(|_| "WebSocket state lock is poisoned".to_string())?;
        let entry = state
            .sockets
            .get(&id)
            .ok_or_else(|| net::closed_error("ws.send", "socket"))?;
        (entry.command_tx.clone(), entry.closed.clone())
    };
    if closed.load(Ordering::Relaxed) {
        return Err(net::closed_error("ws.send", "socket"));
    }
    match tx.try_send(WsCommand::SendText(data)) {
        Ok(()) => Ok(()),
        Err(tokio_mpsc::error::TrySendError::Full(_)) => Err(net::queue_full_error("ws.send")),
        Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
            Err(net::closed_error("ws.send", "socket"))
        }
    }
}

/// Sends a WebSocket binary message.
fn send_binary_socket(id: usize, data: &[u8]) -> Result<(), String> {
    let limit = net::message_limit()?;
    if data.len() > limit {
        return Err(net::message_limit_error("ws.send", data.len(), limit));
    }
    let (tx, closed) = {
        let state = ws_state()
            .lock()
            .map_err(|_| "WebSocket state lock is poisoned".to_string())?;
        let entry = state
            .sockets
            .get(&id)
            .ok_or_else(|| net::closed_error("ws.send", "socket"))?;
        (entry.command_tx.clone(), entry.closed.clone())
    };
    if closed.load(Ordering::Relaxed) {
        return Err(net::closed_error("ws.send", "socket"));
    }
    match tx.try_send(WsCommand::SendBinary(data.to_vec())) {
        Ok(()) => Ok(()),
        Err(tokio_mpsc::error::TrySendError::Full(_)) => Err(net::queue_full_error("ws.send")),
        Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
            Err(net::closed_error("ws.send", "socket"))
        }
    }
}

/// Closes a WebSocket server.
fn close_server(id: usize) -> Result<(), String> {
    if let Some(closed) = ws_state()
        .lock()
        .map_err(|_| "WebSocket state lock is poisoned".to_string())?
        .servers
        .get(&id)
        .map(|entry| entry.closed.clone())
    {
        closed.store(true, Ordering::Relaxed);
    }
    net::send_event(NetEvent::Wake);
    Ok(())
}

/// Close WebSocket connection.
fn close_socket(id: usize) -> Result<(), String> {
    let entry = ws_state()
        .lock()
        .map_err(|_| "WebSocket state lock is poisoned".to_string())?
        .sockets
        .remove(&id);
    if let Some(entry) = entry {
        entry.closed.store(true, Ordering::Relaxed);
        let _ = entry.command_tx.try_send(WsCommand::Close);
    }
    net::send_event(NetEvent::Wake);
    Ok(())
}

/// Inserts the WebSocket service registry.
fn insert_server(closed: Arc<AtomicBool>) -> Result<usize, String> {
    let mut state = ws_state()
        .lock()
        .map_err(|_| "WebSocket state lock is poisoned".to_string())?;
    let id = state.next_server_id;
    state.next_server_id = state.next_server_id.saturating_add(1);
    state.servers.insert(id, WsServerEntry { closed });
    Ok(id)
}

/// Remove the WebSocket service registry.
fn remove_server(id: usize) {
    if let Ok(mut state) = ws_state().lock() {
        state.servers.remove(&id);
    }
}

/// Inserts the WebSocket connection registry.
fn insert_socket(
    command_tx: tokio_mpsc::Sender<WsCommand>,
    closed: Arc<AtomicBool>,
    limit: usize,
) -> Result<usize, String> {
    let mut state = ws_state()
        .lock()
        .map_err(|_| "WebSocket state lock is poisoned".to_string())?;
    if state.sockets.len() >= limit {
        return Err(format!(
            "WebSocket connection limit of {} has been reached",
            limit
        ));
    }
    let id = state.next_socket_id;
    state.next_socket_id = state.next_socket_id.saturating_add(1);
    state
        .sockets
        .insert(id, WsSocketEntry { command_tx, closed });
    Ok(id)
}

/// Remove the WebSocket connection registration item.
fn remove_socket(id: usize) {
    if let Ok(mut state) = ws_state().lock() {
        state.sockets.remove(&id);
    }
}

/// Delivers an error event when a WebSocket message exceeds the size limit.
fn send_ws_size_error(
    sender: &NetEventSender,
    server_id: Option<usize>,
    socket_id: usize,
    size: usize,
    limit: usize,
) {
    let _ = sender.send(NetEvent::WsError {
        server_id,
        socket_id: Some(socket_id),
        message: net::message_limit_error("ws.read", size, limit),
    });
}

/// Returns WebSocket global state.
fn ws_state() -> &'static Mutex<WsState> {
    static STATE: OnceLock<Mutex<WsState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(WsState {
            next_server_id: 1,
            next_socket_id: 1,
            servers: HashMap::new(),
            sockets: HashMap::new(),
        })
    })
}
