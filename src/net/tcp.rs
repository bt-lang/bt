//! TCP server and client implementation.

use crate::libs::bytes;
use crate::net::traits::{BtNetConnection, BtNetServer};
use crate::net::{self, NetConfig, NetEvent, NetEventSender};
use crate::value::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc as tokio_mpsc, Mutex as TokioMutex};

/// Single TCP read buffer size.
const READ_BUFFER_SIZE: usize = 8192;
/// Shutdown polling interval for the TCP accept loop.
const TCP_ACCEPT_POLL: Duration = Duration::from_millis(200);
/// TCP error backoff time.
const TCP_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// TCP service handle.
#[derive(Debug, Clone, PartialEq)]
pub struct TcpServerHandle {
    /// Server ID.
    id: usize,
    /// The listening address visible to the script.
    addr: String,
}

/// TCP connection handle.
#[derive(Debug, Clone, PartialEq)]
pub struct TcpClientHandle {
    /// Connection number.
    id: usize,
    /// The remote address visible to the script.
    addr: String,
}

/// TCP server registry entry.
struct TcpServerEntry {
    /// Service shutdown flag.
    closed: Arc<AtomicBool>,
}

/// TCP connection registry entry.
struct TcpClientEntry {
    /// TCP connection I/O mode.
    io: TcpClientIo,
    /// Connection close flag.
    closed: Arc<AtomicBool>,
    /// Synchronous read and write timeout. The client created by
    timeout: Option<Duration>,
}

/// TCP connection I/O mode.
#[derive(Clone)]
enum TcpClientIo {
    /// `net.connect()` reads and writes Tokio stream directly driven by synchronous methods.
    Direct(Arc<TokioMutex<TcpStream>>),
    /// A client accepted by `net.listen()` writes through a bounded background queue.
    Queued(tokio_mpsc::Sender<TcpCommand>),
}

/// TCP write task command.
enum TcpCommand {
    /// Writes bytes and returns the result to the caller.
    Write(Vec<u8>, SyncSender<Result<usize, String>>),
    /// Closes the write half.
    Close,
}

/// TCP global registry.
struct TcpState {
    /// Next server ID.
    next_server_id: usize,
    /// Next connection number.
    next_client_id: usize,
    /// Active TCP servers.
    servers: HashMap<usize, TcpServerEntry>,
    /// Active TCP connections.
    clients: HashMap<usize, TcpClientEntry>,
}

impl TcpServerHandle {
    /// Creates a TCP service handle.
    pub fn new(id: usize, addr: String) -> Self {
        Self { id, addr }
    }

    /// Returns the server ID.
    pub fn id(&self) -> usize {
        self.id
    }

    /// Calls the TCP service method.
    pub fn call_method(&self, method: &str, _args: Vec<Value>) -> Result<Value, String> {
        match method {
            "close" => {
                self.close()?;
                Ok(Value::Bool(true))
            }
            _ => Err(format!("tcp server has no method `{}`", method)),
        }
    }
}

impl BtNetServer for TcpServerHandle {
    /// Closes the TCP server.
    fn close(&self) -> Result<(), String> {
        close_server(self.id)
    }

    /// Returns the TCP server's listening address.
    fn addr(&self) -> String {
        self.addr.clone()
    }

    /// Returns the server type name.
    fn kind(&self) -> &'static str {
        "tcp"
    }
}

impl TcpClientHandle {
    /// Creates a TCP connection handle.
    pub fn new(id: usize, addr: String) -> Self {
        Self { id, addr }
    }

    /// Calls the TCP connection method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "write" | "send" => {
                let written = if let Some(value) = args.first() {
                    let data = bytes::value_to_bytes(value, "tcp.write")?;
                    self.write(data.as_ref())?
                } else {
                    self.write(&[])?
                };
                Ok(Value::Int(written as i64))
            }
            "read" => {
                let data = self.read()?;
                Ok(Value::Str(String::from_utf8_lossy(&data).to_string()))
            }
            "read_bytes" => {
                let data = self.read()?;
                bytes::from_vec(data)
            }
            "close" => {
                self.close()?;
                Ok(Value::Bool(true))
            }
            _ => Err(format!("tcp client has no method `{}`", method)),
        }
    }
}

impl BtNetConnection for TcpClientHandle {
    /// Reads a chunk of TCP data.
    fn read(&self) -> Result<Vec<u8>, String> {
        read_client(self.id)
    }

    /// Writes a chunk of TCP data.
    fn write(&self, data: &[u8]) -> Result<usize, String> {
        write_client(self.id, data)
    }

    /// Closes the TCP connection.
    fn close(&self) -> Result<(), String> {
        close_client(self.id)
    }

    /// Returns the TCP connection's remote address.
    fn addr(&self) -> String {
        self.addr.clone()
    }

    /// Returns the connection type name.
    fn kind(&self) -> &'static str {
        "tcp"
    }
}

/// Starts the TCP listening service.
pub fn listen(bind: &str) -> Result<TcpServerHandle, String> {
    let config = net::config()?;
    let bind_text = bind.to_string();
    let listener = crate::io::run_async(
        async move {
            TcpListener::bind(&bind_text)
                .await
                .map_err(|err| net::bind_error("tcp", &bind_text, &err))
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
    if let Err(err) =
        crate::io::spawn_async(accept_loop(id, listener, closed, sender, config.clone()))
    {
        remove_server(id);
        net::unregister_event_service();
        return Err(format!(
            "net.listen(tcp): failed to start `{}`: {}",
            bind, err
        ));
    }
    Ok(TcpServerHandle::new(id, addr))
}

/// Establishing TCP client connection.
pub fn connect(host: &str, port: u16, timeout_ms: Option<u64>) -> Result<TcpClientHandle, String> {
    let config = net::config()?;
    let addr = format!("{}:{}", host, port);
    let connect_addr = addr.clone();
    let timeout = timeout_ms.map(Duration::from_millis);
    let stream = crate::io::run_async(
        async move {
            let connect = TcpStream::connect(&connect_addr);
            match timeout {
                Some(timeout) => match tokio::time::timeout(timeout, connect).await {
                    Ok(result) => result,
                    Err(_) => {
                        return Err(net::connect_timeout_error(
                            "tcp",
                            &connect_addr,
                            timeout.as_millis(),
                        ));
                    }
                },
                None => connect.await,
            }
            .map_err(|err| net::connect_error("tcp", &connect_addr, &err))
        },
        timeout,
    )?;
    let id = insert_direct_client(
        Arc::new(TokioMutex::new(stream)),
        Arc::new(AtomicBool::new(false)),
        timeout,
        config.connection_limit,
    )?;
    Ok(TcpClientHandle::new(id, addr))
}

/// TCP accept loop.
async fn accept_loop(
    server_id: usize,
    listener: TcpListener,
    closed: Arc<AtomicBool>,
    sender: NetEventSender,
    config: NetConfig,
) {
    while !closed.load(Ordering::Relaxed) {
        match tokio::time::timeout(TCP_ACCEPT_POLL, listener.accept()).await {
            Err(_) => continue,
            Ok(Ok((stream, addr))) => {
                let addr = addr.to_string();
                if let Err(err) =
                    start_accepted_client(server_id, stream, addr.clone(), sender.clone(), &config)
                {
                    let _ = sender.send(NetEvent::TcpError {
                        server_id: Some(server_id),
                        client_id: None,
                        message: err,
                    });
                }
            }
            Ok(Err(err)) => {
                let _ = sender.send(NetEvent::TcpError {
                    server_id: Some(server_id),
                    client_id: None,
                    message: net::io_error("tcp.accept", &err),
                });
                tokio::time::sleep(TCP_ERROR_BACKOFF).await;
            }
        }
    }
    remove_server(server_id);
    net::unregister_event_service();
}

/// Starts a single TCP client read and write task.
fn start_accepted_client(
    server_id: usize,
    stream: TcpStream,
    addr: String,
    sender: NetEventSender,
    config: &NetConfig,
) -> Result<(), String> {
    let (reader, writer) = stream.into_split();
    let (command_tx, command_rx) = tokio_mpsc::channel(config.write_queue_limit);
    let closed = Arc::new(AtomicBool::new(false));
    let client_id =
        insert_queued_client(command_tx, closed.clone(), None, config.connection_limit)?;
    if !sender.send(NetEvent::TcpConnect {
        server_id,
        client_id,
        addr: addr.clone(),
    }) {
        remove_client(client_id);
        return Err(net::event_queue_full_error("tcp.accept"));
    }
    if let Err(err) = crate::io::spawn_async(tcp_write_loop(
        server_id,
        client_id,
        writer,
        command_rx,
        closed.clone(),
        sender.clone(),
    )) {
        remove_client(client_id);
        return Err(format!(
            "tcp.accept: failed to start client write task: {}",
            err
        ));
    }
    if let Err(err) = crate::io::spawn_async(tcp_read_loop(
        server_id,
        client_id,
        reader,
        addr,
        closed,
        sender,
        config.message_limit,
        (config.idle_ttl_ms > 0).then_some(Duration::from_millis(config.idle_ttl_ms)),
    )) {
        remove_client(client_id);
        return Err(format!(
            "tcp.accept: failed to start client read task: {}",
            err
        ));
    }
    Ok(())
}

/// TCP client read loop.
async fn tcp_read_loop(
    server_id: usize,
    client_id: usize,
    mut reader: OwnedReadHalf,
    addr: String,
    closed: Arc<AtomicBool>,
    sender: NetEventSender,
    message_limit: usize,
    idle_ttl: Option<Duration>,
) {
    let mut buffer = vec![0u8; tcp_read_buffer_size(message_limit)];
    while !closed.load(Ordering::Relaxed) {
        let read_result = match idle_ttl {
            Some(timeout) => match tokio::time::timeout(timeout, reader.read(&mut buffer)).await {
                Ok(result) => result,
                Err(_) => {
                    let _ = sender.send(NetEvent::TcpError {
                        server_id: Some(server_id),
                        client_id: Some(client_id),
                        message: format!(
                            "tcp: Connection idle for more than {} milliseconds",
                            timeout.as_millis()
                        ),
                    });
                    break;
                }
            },
            None => reader.read(&mut buffer).await,
        };
        match read_result {
            Ok(0) => break,
            Ok(size) => {
                let data = buffer[..size].to_vec();
                if !sender.send(NetEvent::TcpMessage {
                    client_id,
                    addr: addr.clone(),
                    data,
                }) {
                    break;
                }
            }
            Err(err) => {
                if !closed.load(Ordering::Relaxed) {
                    let _ = sender.send(NetEvent::TcpError {
                        server_id: Some(server_id),
                        client_id: Some(client_id),
                        message: net::io_error("tcp.read", &err),
                    });
                }
                break;
            }
        }
    }
    let already_closed = closed.swap(true, Ordering::Relaxed);
    remove_client(client_id);
    if !already_closed {
        let _ = sender.send(NetEvent::TcpClose { client_id, addr });
    }
}

/// TCP client background write loop.
async fn tcp_write_loop(
    server_id: usize,
    client_id: usize,
    mut writer: OwnedWriteHalf,
    mut command_rx: tokio_mpsc::Receiver<TcpCommand>,
    closed: Arc<AtomicBool>,
    sender: NetEventSender,
) {
    while let Some(command) = command_rx.recv().await {
        match command {
            TcpCommand::Write(data, reply) => {
                if closed.load(Ordering::Relaxed) {
                    let _ = reply.send(Err(net::closed_error("tcp.write", "connection")));
                    break;
                }
                let result = writer
                    .write(&data)
                    .await
                    .map_err(|err| net::io_error("tcp.write", &err));
                if result.is_err() {
                    closed.store(true, Ordering::Relaxed);
                    let _ = sender.send(NetEvent::TcpError {
                        server_id: Some(server_id),
                        client_id: Some(client_id),
                        message: result.as_ref().err().cloned().unwrap_or_default(),
                    });
                }
                let _ = reply.send(result);
            }
            TcpCommand::Close => {
                closed.store(true, Ordering::Relaxed);
                let _ = writer.shutdown().await;
                break;
            }
        }
    }
}

/// Writes data to a TCP client.
fn write_client(id: usize, data: &[u8]) -> Result<usize, String> {
    let limit = net::message_limit()?;
    if data.len() > limit {
        return Err(net::message_limit_error("tcp.write", data.len(), limit));
    }
    let (io, closed, timeout) = client_entry(id, "tcp.write")?;
    if closed.load(Ordering::Relaxed) {
        return Err(net::closed_error("tcp.write", "connection"));
    }
    match io {
        TcpClientIo::Direct(stream) => {
            let payload = data.to_vec();
            crate::io::run_async(
                async move {
                    let mut stream = stream.lock().await;
                    stream
                        .write(&payload)
                        .await
                        .map_err(|err| net::io_error("tcp.write", &err))
                },
                timeout,
            )
        }
        TcpClientIo::Queued(command_tx) => {
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            match command_tx.try_send(TcpCommand::Write(data.to_vec(), reply_tx)) {
                Ok(()) => {}
                Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                    return Err(net::queue_full_error("tcp.write"));
                }
                Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                    return Err(net::closed_error("tcp.write", "connection"));
                }
            }
            let wait = timeout.unwrap_or_else(crate::io::default_timeout);
            match reply_rx.recv_timeout(wait) {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Err(net::timeout_error("tcp.write", wait.as_millis()))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(net::closed_error("tcp.write", "connection"))
                }
            }
        }
    }
}

/// Read TCP client data.
fn read_client(id: usize) -> Result<Vec<u8>, String> {
    let (io, closed, timeout) = client_entry(id, "tcp.read")?;
    if closed.load(Ordering::Relaxed) {
        return Err(net::closed_error("tcp.read", "connection"));
    }
    match io {
        TcpClientIo::Direct(stream) => {
            let limit = net::message_limit()?;
            crate::io::run_async(
                async move {
                    let mut buffer = vec![0u8; tcp_read_buffer_size(limit)];
                    let mut stream = stream.lock().await;
                    let size = stream
                        .read(&mut buffer)
                        .await
                        .map_err(|err| net::io_error("tcp.read", &err))?;
                    buffer.truncate(size);
                    Ok(buffer)
                },
                timeout,
            )
        }
        TcpClientIo::Queued(_) => Err(
            "tcp.read: use on_message to receive data from a server-accepted connection"
                .to_string(),
        ),
    }
}

/// Closes a TCP server.
fn close_server(id: usize) -> Result<(), String> {
    if let Some(closed) = tcp_state()
        .lock()
        .map_err(|_| "The TCP state lock is poisoned".to_string())?
        .servers
        .get(&id)
        .map(|entry| entry.closed.clone())
    {
        closed.store(true, Ordering::Relaxed);
    }
    net::send_event(NetEvent::Wake);
    Ok(())
}

/// Close the TCP client connection.
fn close_client(id: usize) -> Result<(), String> {
    if let Some(entry) = tcp_state()
        .lock()
        .map_err(|_| "The TCP state lock is poisoned".to_string())?
        .clients
        .remove(&id)
    {
        entry.closed.store(true, Ordering::Relaxed);
        match entry.io {
            TcpClientIo::Direct(stream) => {
                let _ = crate::io::run_async(
                    async move {
                        let mut stream = stream.lock().await;
                        stream
                            .shutdown()
                            .await
                            .map_err(|err| net::io_error("tcp.close", &err))
                    },
                    Some(crate::io::default_timeout()),
                );
            }
            TcpClientIo::Queued(command_tx) => {
                let _ = command_tx.try_send(TcpCommand::Close);
            }
        }
    }
    net::send_event(NetEvent::Wake);
    Ok(())
}

/// Inserts the TCP service registry entry.
fn insert_server(closed: Arc<AtomicBool>) -> Result<usize, String> {
    let mut state = tcp_state()
        .lock()
        .map_err(|_| "The TCP state lock is poisoned".to_string())?;
    let id = state.next_server_id;
    state.next_server_id = state.next_server_id.saturating_add(1);
    state.servers.insert(id, TcpServerEntry { closed });
    Ok(id)
}

/// Remove the TCP service registry.
fn remove_server(id: usize) {
    if let Ok(mut state) = tcp_state().lock() {
        state.servers.remove(&id);
    }
}

/// Inserts a TCP client created by `net.connect()`.
fn insert_direct_client(
    stream: Arc<TokioMutex<TcpStream>>,
    closed: Arc<AtomicBool>,
    timeout: Option<Duration>,
    limit: usize,
) -> Result<usize, String> {
    insert_client(TcpClientIo::Direct(stream), closed, timeout, limit)
}

/// Inserts the TCP client obtained by the server accept.
fn insert_queued_client(
    command_tx: tokio_mpsc::Sender<TcpCommand>,
    closed: Arc<AtomicBool>,
    timeout: Option<Duration>,
    limit: usize,
) -> Result<usize, String> {
    insert_client(TcpClientIo::Queued(command_tx), closed, timeout, limit)
}

/// Inserts the TCP client registry entry.
fn insert_client(
    io: TcpClientIo,
    closed: Arc<AtomicBool>,
    timeout: Option<Duration>,
    limit: usize,
) -> Result<usize, String> {
    let mut state = tcp_state()
        .lock()
        .map_err(|_| "The TCP state lock is poisoned".to_string())?;
    if state.clients.len() >= limit {
        return Err(format!(
            "TCP connection limit of {} has been reached",
            limit
        ));
    }
    let id = state.next_client_id;
    state.next_client_id = state.next_client_id.saturating_add(1);
    state.clients.insert(
        id,
        TcpClientEntry {
            io,
            closed,
            timeout,
        },
    );
    Ok(id)
}

/// Remove the TCP client registry.
fn remove_client(id: usize) {
    if let Ok(mut state) = tcp_state().lock() {
        state.clients.remove(&id);
    }
}

/// Reads the TCP client registry.
fn client_entry(
    id: usize,
    method: &str,
) -> Result<(TcpClientIo, Arc<AtomicBool>, Option<Duration>), String> {
    let state = tcp_state()
        .lock()
        .map_err(|_| "The TCP state lock is poisoned".to_string())?;
    let entry = state
        .clients
        .get(&id)
        .ok_or_else(|| net::closed_error(method, "connection"))?;
    Ok((entry.io.clone(), entry.closed.clone(), entry.timeout))
}

/// Returns the TCP read buffer size.
fn tcp_read_buffer_size(message_limit: usize) -> usize {
    message_limit.clamp(1, READ_BUFFER_SIZE)
}

/// Returns TCP global status.
fn tcp_state() -> &'static StdMutex<TcpState> {
    static STATE: OnceLock<StdMutex<TcpState>> = OnceLock::new();
    STATE.get_or_init(|| {
        StdMutex::new(TcpState {
            next_server_id: 1,
            next_client_id: 1,
            servers: HashMap::new(),
            clients: HashMap::new(),
        })
    })
}
