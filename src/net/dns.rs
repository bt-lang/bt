//! DNS resolution implementation.

use crate::net::traits::value_array;
use crate::value::Value;
use std::net::ToSocketAddrs;

/// Resolves a host name and returns deduplicated IP-address strings.
pub fn resolve(args: Vec<Value>) -> Result<Value, String> {
    let host = args
        .first()
        .map(Value::to_string)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "net.resolve() requires hostname argument".to_string())?;
    let query = format!("{}:0", host);
    let mut values = Vec::new();
    for addr in query
        .to_socket_addrs()
        .map_err(|err| format!("Resolving host `{}` failed: {}", host, err))?
    {
        let text = addr.ip().to_string();
        if !values.iter().any(|value: &Value| value.to_string() == text) {
            values.push(Value::Str(text));
        }
    }
    Ok(value_array(values))
}
