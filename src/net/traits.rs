//! Shared network traits and lightweight argument-parsing helpers.

use crate::value::Value;
use std::cell::RefCell;
use std::rc::Rc;

/// Internal abstraction for BT network server objects.
pub trait BtNetServer: Send {
    /// Close the service.
    fn close(&self) -> Result<(), String>;
    /// Returns the server's listening address.
    fn addr(&self) -> String;
    /// Returns the server type name.
    fn kind(&self) -> &'static str;
}

/// Internal abstraction for BT network connection objects.
pub trait BtNetConnection: Send {
    /// Reads a chunk of byte data.
    fn read(&self) -> Result<Vec<u8>, String>;
    /// Writes byte data and returns the number of bytes written.
    fn write(&self, data: &[u8]) -> Result<usize, String>;
    /// Close the connection.
    fn close(&self) -> Result<(), String>;
    /// Returns the remote address.
    fn addr(&self) -> String;
    /// Returns the connection type name.
    fn kind(&self) -> &'static str;
}

/// Read object fields.
pub fn object_get(value: &Value, key: &str) -> Option<Value> {
    let Value::Object(values) = value else {
        return None;
    };
    values.borrow().get(key).cloned()
}

/// Reads the object string field.
pub fn object_string(value: &Value, key: &str, default: &str) -> String {
    object_get(value, key)
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Reads required string fields.
pub fn required_string(value: &Value, key: &str, message: &str) -> Result<String, String> {
    object_get(value, key)
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| message.to_string())
}

/// Reads an object Boolean field.
pub fn object_bool(value: &Value, key: &str, default: bool) -> bool {
    object_get(value, key)
        .map(|value| value.is_truthy())
        .unwrap_or(default)
}

/// Read object array field.
pub fn object_array(value: &Value, key: &str) -> Vec<Value> {
    match object_get(value, key) {
        Some(Value::Array(values)) => values.borrow().clone(),
        _ => Vec::new(),
    }
}

/// Reads an object integer field.
pub fn object_i64(value: &Value, key: &str, default: i64) -> i64 {
    object_get(value, key)
        .map(|value| value.to_i64_lossy())
        .unwrap_or(default)
}

/// Reads the required port field.
pub fn required_port(value: &Value, key: &str, message: &str) -> Result<u16, String> {
    let value = object_get(value, key).ok_or_else(|| message.to_string())?;
    let port = value.to_i64_lossy();
    if !(0..=u16::MAX as i64).contains(&port) {
        return Err(format!(
            "net.connect(): invalid port `{}`",
            value.to_string()
        ));
    }
    Ok(port as u16)
}

/// Parses the host and port from a listening address.
pub fn parse_bind(bind: &str) -> Result<(String, u16), String> {
    let (host, port) = bind
        .rsplit_once(':')
        .ok_or_else(|| format!("Listening address `{}` is missing a port", bind))?;
    let port = port
        .parse::<u16>()
        .map_err(|err| format!("Invalid port for listening address `{}`: {}", bind, err))?;
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    Ok((host.to_string(), port))
}

/// Create BT array value.
pub fn value_array(values: Vec<Value>) -> Value {
    Value::Array(Rc::new(RefCell::new(values)))
}
