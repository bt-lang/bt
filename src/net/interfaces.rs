//! Native network interface information implementation.

use crate::net::traits::value_array;
use crate::value::Value;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::net::{IpAddr, UdpSocket};
use std::rc::Rc;

/// Returns local network-interface information.
pub fn interfaces() -> Result<Value, String> {
    let mut values = Vec::new();
    if let Ok(ip) = detect_local_ip() {
        let mut item = IndexMap::new();
        item.insert("name".to_string(), Value::Str("default".to_string()));
        item.insert("ip".to_string(), Value::Str(ip.to_string()));
        item.insert("family".to_string(), Value::Str(ip_family(ip).to_string()));
        item.insert("internal".to_string(), Value::Bool(false));
        values.push(Value::Object(Rc::new(RefCell::new(item))));
    }
    let mut loopback = IndexMap::new();
    loopback.insert("name".to_string(), Value::Str("loopback".to_string()));
    loopback.insert("ip".to_string(), Value::Str("127.0.0.1".to_string()));
    loopback.insert("family".to_string(), Value::Str("IPv4".to_string()));
    loopback.insert("internal".to_string(), Value::Bool(true));
    values.push(Value::Object(Rc::new(RefCell::new(loopback))));
    Ok(value_array(values))
}

/// Returns the default local IP address.
pub fn local_ip() -> Result<Value, String> {
    detect_local_ip()
        .map(|ip| Value::Str(ip.to_string()))
        .or_else(|_| Ok(Value::Str("127.0.0.1".to_string())))
}

/// Detects the default egress IP address.
fn detect_local_ip() -> Result<IpAddr, String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .map_err(|err| format!("failed to create UDP detection socket: {}", err))?;
    socket
        .connect("8.8.8.8:80")
        .map_err(|err| format!("failed to detect local IP address: {}", err))?;
    socket
        .local_addr()
        .map(|addr| addr.ip())
        .map_err(|err| format!("failed to read local IP address: {}", err))
}

/// Return IP address family name.
fn ip_family(ip: IpAddr) -> &'static str {
    if ip.is_ipv4() {
        "IPv4"
    } else {
        "IPv6"
    }
}
