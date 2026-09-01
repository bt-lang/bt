//! BT network and service listening standard library.
//!
//! This file handles only script API dispatch and argument diagnostics. TCP, UDP, WebSocket, DNS, interface discovery, and legacy Web service adapters
//! live under `src/net/`, keeping protocol implementation details out of the standard-library export layer.

use crate::net::traits::{object_i64, object_string, required_port, required_string};
use crate::net::{dns, interfaces, tcp, udp, web as net_web, ws};
use crate::value::Value;
use std::path::Path;

/// Network standard library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtNet;

impl BtNet {
    /// Creates a network standard library object.
    pub fn new(_args: Vec<Value>) -> Result<Value, String> {
        Ok(Value::Net(Self))
    }

    /// Call the network standard library method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "listen" => listen(args),
            "connect" => connect(args),
            "resolve" => dns::resolve(args),
            "interfaces" => interfaces::interfaces(),
            "local_ip" => interfaces::local_ip(),
            _ => Err(format!("net has no method `{}`", method)),
        }
    }

    /// Dispatches a network method with path context for Web service configuration.
    pub fn call_method_with_paths(
        &self,
        method: &str,
        args: Vec<Value>,
        source_dir: &Path,
        project_root: &Path,
    ) -> Result<Value, String> {
        match method {
            "listen" => listen_with_paths(args, source_dir, project_root),
            _ => self.call_method(method, args),
        }
    }
}

/// Determines whether there is currently a background network service.
pub fn has_background_tasks() -> bool {
    crate::net::has_background_tasks()
}

/// Determines whether there is currently a background network service that requires VM distribution callbacks.
pub fn has_event_tasks() -> bool {
    crate::net::has_event_tasks()
}

/// Wait for all background network services to end.
pub fn wait_for_background_tasks() -> Result<(), String> {
    crate::net::wait_for_background_tasks()
}

/// Starts a listening server.
fn listen(args: Vec<Value>) -> Result<Value, String> {
    listen_with_paths(args, Path::new("."), Path::new("."))
}

/// Starts a listening server with explicit script path context.
fn listen_with_paths(
    args: Vec<Value>,
    source_dir: &Path,
    project_root: &Path,
) -> Result<Value, String> {
    let config = args
        .first()
        .ok_or_else(|| "net.listen() requires a configuration object".to_string())?;
    let config_type = required_string(config, "type", "net.listen() missing `type` field")?;
    match config_type.as_str() {
        "web" => net_web::listen(config, source_dir, project_root),
        "tcp" => {
            let bind = required_string(config, "bind", "net.listen() missing `bind` field")?;
            tcp::listen(&bind).map(Value::NetTcpServer)
        }
        "udp" => {
            let bind = required_string(config, "bind", "net.listen() missing `bind` field")?;
            udp::listen(&bind).map(Value::NetUdpSocket)
        }
        "ws" => {
            let bind = required_string(config, "bind", "net.listen() missing `bind` field")?;
            let route = object_string(config, "route", "/ws");
            ws::listen(&bind, &route).map(Value::NetWsServer)
        }
        _ => Err(format!(
            "net.listen() does not support protocol type `{}`",
            config_type
        )),
    }
}

/// Establish a client connection.
fn connect(args: Vec<Value>) -> Result<Value, String> {
    let config = args
        .first()
        .ok_or_else(|| "net.connect() requires a configuration object".to_string())?;
    let config_type = required_string(config, "type", "net.connect() missing `type` field")?;
    match config_type.as_str() {
        "tcp" => {
            let host = required_string(config, "host", "net.connect() missing `host` field")?;
            let port = required_port(config, "port", "net.connect() missing `port` field")?;
            let timeout = object_i64(config, "timeout", 0);
            let timeout = (timeout > 0).then_some(timeout as u64);
            tcp::connect(&host, port, timeout).map(Value::NetTcpClient)
        }
        "udp" => {
            let host = required_string(config, "host", "net.connect() missing `host` field")?;
            let port = required_port(config, "port", "net.connect() missing `port` field")?;
            udp::connect(&host, port).map(Value::NetUdpSocket)
        }
        "ws" => {
            let url = required_string(config, "url", "net.connect() missing `url` field")?;
            ws::connect(&url).map(Value::NetWsSocket)
        }
        _ => Err(format!(
            "net.connect() does not support protocol type `{}`",
            config_type
        )),
    }
}
