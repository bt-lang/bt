//! BT Binary Bytes Standard Library.
//!
//! `bytes(value, mode)` creates an immutable byte value for binary boundaries such as serial, TCP, UDP, WebSocket, and Modbus.
//! Internally, bytes use a shared read-only buffer. Script methods return new values so object reference semantics cannot turn in-place expansion
//! into unbounded buffer growth in a resident process.

use crate::value::Value;
use base64::{engine::general_purpose::*, Engine};
use indexmap::IndexMap;
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

/// Default single Bytes buffer limit, unit byte.
const DEFAULT_BYTES_LIMIT: usize = 16 * 1024 * 1024;
/// Single Bytes buffer hard upper limit, unit byte.
const MAX_BYTES_LIMIT: usize = 64 * 1024 * 1024;

/// Bytes standard library configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BytesConfig {
    /// The maximum number of bytes allowed in a single Bytes buffer.
    pub limit: usize,
    /// Configuration error text; empty means the current configuration is valid.
    pub config_error: Option<String>,
}

/// Bytes runtime value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtBytes {
    /// Shared read-only byte buffer.
    data: Rc<Vec<u8>>,
}

/// Cached Bytes configuration.
static CONFIG: OnceLock<Result<BytesConfig, String>> = OnceLock::new();

impl BtBytes {
    /// Creates a Bytes value.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let value = args.first();
        let mode = args.get(1).map(Value::to_string).unwrap_or_default();
        let bytes = match (value, mode.as_str()) {
            (None, _) => Vec::new(),
            (Some(Value::Object(values)), "") => bytes_from_object(&values.borrow())?,
            (Some(Value::Str(text)), "hex") => parse_hex(text)?,
            (Some(Value::Str(text)), "base64") => decode_base64_text(text, None)?,
            (Some(Value::Str(text)), "base64_url") => decode_base64_text(text, Some("url_safe"))?,
            (Some(Value::Str(text)), "utf8" | "text" | "") => text.as_bytes().to_vec(),
            (Some(value), "") => value_to_byte_vec(value, "bytes")?,
            (Some(value), "utf8" | "text") => value.to_string().into_bytes(),
            (Some(_), other) => return Err(format!("bytes: unsupported mode `{}`", other)),
        };
        from_vec(bytes)
    }

    /// Creates a Bytes value from a size-checked byte buffer.
    pub fn unchecked(data: Vec<u8>) -> Self {
        Self {
            data: Rc::new(data),
        }
    }

    /// Returns the underlying byte slice.
    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Returns the length in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Returns whether the byte buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Read the byte at the specified index.
    pub fn byte_at(&self, index: usize) -> Option<u8> {
        self.data.get(index).copied()
    }

    /// Returns a JSON representation.
    ///
    /// Standard JSON does not have a native Bytes type, so a Base64 object with type tag is used here to avoid binary data
    /// being implicitly treated as a UTF-8 string.
    pub fn to_json_string(&self) -> String {
        let encoded = encode_base64(self.as_slice(), None);
        let encoded = serde_json::to_string(&encoded).unwrap_or_else(|_| "\"\"".to_string());
        format!(
            "{{\"type\":\"bytes\",\"encoding\":\"base64\",\"data\":{}}}",
            encoded
        )
    }

    /// Returns a hexadecimal string.
    pub fn to_hex_string(&self, separator: &str) -> String {
        bytes_to_hex(self.as_slice(), separator)
    }

    /// Returns whether the name identifies a Bytes method.
    pub fn is_method(name: &str) -> bool {
        matches!(
            name,
            "len"
                | "is_empty"
                | "get"
                | "slice"
                | "to_array"
                | "to_hex"
                | "to_base64"
                | "to_text"
                | "append"
                | "clone"
                | "to_string"
        )
    }

    /// Call the Bytes method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "len" => Ok(Value::Int(self.len() as i64)),
            "is_empty" => Ok(Value::Bool(self.is_empty())),
            "get" => {
                let Some(index) = normalize_existing_index(
                    args.first().map(Value::to_i64_lossy).unwrap_or(0),
                    self.len(),
                ) else {
                    return Ok(Value::Empty);
                };
                Ok(Value::Int(self.data[index] as i64))
            }
            "slice" => {
                let start = normalize_bound(
                    args.first().map(Value::to_i64_lossy).unwrap_or(0),
                    self.len(),
                );
                let end = normalize_bound(
                    args.get(1)
                        .map(Value::to_i64_lossy)
                        .unwrap_or(self.len() as i64),
                    self.len(),
                );
                if start >= end {
                    return from_vec(Vec::new());
                }
                from_vec(self.as_slice()[start..end].to_vec())
            }
            "to_array" => Ok(Value::Array(Rc::new(RefCell::new(
                self.as_slice()
                    .iter()
                    .map(|byte| Value::Int(*byte as i64))
                    .collect(),
            )))),
            "to_hex" | "to_string" => {
                let separator = args.first().map(Value::to_string).unwrap_or_default();
                Ok(Value::Str(self.to_hex_string(&separator)))
            }
            "to_base64" => Ok(Value::Str(encode_base64(self.as_slice(), args.first()))),
            "to_text" => match std::str::from_utf8(self.as_slice()) {
                Ok(text) => Ok(Value::Str(text.to_string())),
                Err(_) => Ok(Value::Null),
            },
            "append" => {
                let value = args
                    .first()
                    .ok_or_else(|| "bytes.append: missing data".to_string())?;
                let extra = value_to_bytes(value, "bytes.append")?;
                let total = self.len().saturating_add(extra.len());
                ensure_len(total, "bytes.append")?;
                let mut output = Vec::with_capacity(total);
                output.extend_from_slice(self.as_slice());
                output.extend_from_slice(extra.as_ref());
                from_vec(output)
            }
            "clone" => Ok(Value::Bytes(self.clone())),
            _ => Err(format!("bytes value has no method `{}`", method)),
        }
    }
}

/// Returns a Bytes configuration snapshot.
pub fn stats() -> BytesConfig {
    match config() {
        Ok(config) => config,
        Err(message) => BytesConfig {
            limit: fallback_limit(),
            config_error: Some(message),
        },
    }
}

/// Creates a script Bytes value from a byte buffer.
pub fn from_vec(data: Vec<u8>) -> Result<Value, String> {
    ensure_len(data.len(), "bytes")?;
    Ok(Value::Bytes(BtBytes::unchecked(data)))
}

/// Converts a script value into a borrowable byte slice.
pub fn value_to_bytes<'a>(value: &'a Value, method: &str) -> Result<Cow<'a, [u8]>, String> {
    match value {
        Value::Bytes(bytes) => Ok(Cow::Borrowed(bytes.as_slice())),
        Value::Str(text) => {
            ensure_len(text.len(), method)?;
            Ok(Cow::Borrowed(text.as_bytes()))
        }
        Value::Array(values) => {
            let values = values.borrow();
            ensure_len(values.len(), method)?;
            let mut output = Vec::with_capacity(values.len());
            for value in values.iter() {
                output.push(byte_from_value(value, method)?);
            }
            Ok(Cow::Owned(output))
        }
        Value::Empty | Value::Null => Ok(Cow::Borrowed(&[])),
        other => {
            let text = other.to_string();
            ensure_len(text.len(), method)?;
            Ok(Cow::Owned(text.into_bytes()))
        }
    }
}

/// Converts a script value to an owned byte buffer.
pub fn value_to_byte_vec(value: &Value, method: &str) -> Result<Vec<u8>, String> {
    value_to_bytes(value, method).map(Cow::into_owned)
}

/// Returns the current Bytes buffer limit.
pub fn limit() -> Result<usize, String> {
    config().map(|config| config.limit)
}

/// Encodes Base64 in the specified pattern.
pub fn encode_base64(data: &[u8], mode: Option<&Value>) -> String {
    base64_engine(mode).encode(data)
}

/// Decodes Base64 in the specified mode.
pub fn decode_base64(data: &str, mode: Option<&Value>) -> Result<Vec<u8>, String> {
    let bytes = base64_engine(mode)
        .decode(data.as_bytes())
        .map_err(|err| format!("bytes: base64 decoding failed: {}", err))?;
    ensure_len(bytes.len(), "bytes")?;
    Ok(bytes)
}

/// Verifies that a byte length does not exceed the configured limit.
fn ensure_len(len: usize, method: &str) -> Result<(), String> {
    let limit = limit()?;
    if len > limit {
        Err(format!(
            "{}: byte length {} exceeds BT_BYTES_LIMIT {}",
            method, len, limit
        ))
    } else {
        Ok(())
    }
}

/// Reads the cached Bytes configuration.
fn config() -> Result<BytesConfig, String> {
    CONFIG.get_or_init(load_config).clone()
}

/// Load Bytes configuration.
fn load_config() -> Result<BytesConfig, String> {
    let limit = match std::env::var("BT_BYTES_LIMIT") {
        Ok(value) => parse_usize_env("BT_BYTES_LIMIT", &value, 1, MAX_BYTES_LIMIT)?,
        Err(std::env::VarError::NotPresent) => DEFAULT_BYTES_LIMIT,
        Err(err) => return Err(format!("failed to read BT_BYTES_LIMIT: {}", err)),
    };
    Ok(BytesConfig {
        limit,
        config_error: None,
    })
}

/// Conservative default value for statistics display when misconfigured.
fn fallback_limit() -> usize {
    DEFAULT_BYTES_LIMIT
}

/// Parses unsigned environment variables and limits scope.
fn parse_usize_env(name: &str, value: &str, min: usize, max: usize) -> Result<usize, String> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("{} must be an integer, currently `{}`", name, value))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{} must be between {}..={}, currently {}",
            name, min, max, parsed
        ));
    }
    Ok(parsed)
}

/// Creates a byte buffer from an object argument.
fn bytes_from_object(values: &IndexMap<String, Value>) -> Result<Vec<u8>, String> {
    if let Some(value) = values.get("hex") {
        return parse_hex(&value.to_string());
    }
    if let Some(value) = values.get("base64") {
        return decode_base64(&value.to_string(), values.get("mode"));
    }
    if let Some(value) = values.get("text") {
        let text = value.to_string();
        ensure_len(text.len(), "bytes")?;
        return Ok(text.into_bytes());
    }
    if let Some(value) = values.get("data") {
        return value_to_byte_vec(value, "bytes");
    }
    Ok(Vec::new())
}

/// Parses hexadecimal text.
fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let mut digits = Vec::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_hexdigit() {
            digits.push(ch);
        } else if ch.is_whitespace() || matches!(ch, '_' | '-' | ':' | ',') {
            continue;
        } else {
            return Err(format!("bytes: invalid hex character `{}`", ch));
        }
    }
    if digits.len() % 2 != 0 {
        return Err("bytes: hex length must be even".to_string());
    }
    ensure_len(digits.len() / 2, "bytes")?;
    let mut output = Vec::with_capacity(digits.len() / 2);
    for chunk in digits.chunks(2) {
        let high = chunk[0]
            .to_digit(16)
            .ok_or_else(|| "bytes: invalid hex digit".to_string())?;
        let low = chunk[1]
            .to_digit(16)
            .ok_or_else(|| "bytes: invalid hex digit".to_string())?;
        output.push(((high << 4) | low) as u8);
    }
    Ok(output)
}

/// Convert bytes to hexadecimal text.
fn bytes_to_hex(data: &[u8], separator: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    if data.is_empty() {
        return String::new();
    }
    let separator_len = separator.len().saturating_mul(data.len().saturating_sub(1));
    let mut output = String::with_capacity(data.len().saturating_mul(2) + separator_len);
    for (index, byte) in data.iter().enumerate() {
        if index > 0 {
            output.push_str(separator);
        }
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Reads a single byte by script value.
fn byte_from_value(value: &Value, method: &str) -> Result<u8, String> {
    let byte = value.to_i64_lossy();
    if (0..=255).contains(&byte) {
        Ok(byte as u8)
    } else {
        Err(format!("{}: invalid byte `{}`", method, byte))
    }
}

/// Standardizes readable subscripts and supports negative numbers to be read from the tail.
fn normalize_existing_index(index: i64, len: usize) -> Option<usize> {
    if index < 0 {
        let offset = (-index) as usize;
        (offset <= len).then_some(len - offset)
    } else {
        let index = index as usize;
        (index < len).then_some(index)
    }
}

/// Normalizes slice boundaries, supports negative numbers calculated from the tail.
fn normalize_bound(index: i64, len: usize) -> usize {
    if index < 0 {
        len.saturating_sub((-index) as usize)
    } else {
        (index as usize).min(len)
    }
}

/// Decode Base64 in string mode.
fn decode_base64_text(text: &str, mode: Option<&str>) -> Result<Vec<u8>, String> {
    let mode_value = mode.map(|mode| Value::Str(mode.to_string()));
    decode_base64(text, mode_value.as_ref())
}

/// Selects the Base64 encoding table based on script parameters.
fn base64_engine(value: Option<&Value>) -> &'static base64::engine::GeneralPurpose {
    match value {
        Some(Value::Str(mode)) => match mode.as_str() {
            "standard_no_pad" | "no_pad" => &STANDARD_NO_PAD,
            "url_safe" => &URL_SAFE,
            "url_safe_no_pad" => &URL_SAFE_NO_PAD,
            _ => &STANDARD,
        },
        Some(value) => match value.to_i64_lossy() {
            1 => &STANDARD_NO_PAD,
            2 => &URL_SAFE,
            3 => &URL_SAFE_NO_PAD,
            _ => &STANDARD,
        },
        None => &STANDARD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_constructor_decodes_hex() {
        let value = BtBytes::new(vec![
            Value::Str("42 54".to_string()),
            Value::Str("hex".to_string()),
        ])
        .expect("should be able to parse hexadecimal");
        let Value::Bytes(bytes) = value else {
            panic!("should return Bytes");
        };
        assert_eq!(bytes.as_slice(), b"BT");
        assert_eq!(bytes.to_hex_string(""), "4254");
    }

    #[test]
    fn bytes_to_text_rejects_invalid_utf8_as_null() {
        let value = from_vec(vec![0xff]).expect("should be able to create Bytes");
        let Value::Bytes(bytes) = value else {
            panic!("should return Bytes");
        };
        assert_eq!(
            bytes
                .call_method("to_text", Vec::new())
                .expect("to_text should succeed"),
            Value::Null
        );
    }

    #[test]
    fn bytes_append_returns_new_value() {
        let left = BtBytes::new(vec![Value::Str("B".to_string())])
            .expect("should be able to create Bytes");
        let Value::Bytes(left) = left else {
            panic!("should return Bytes");
        };
        let value = left
            .call_method("append", vec![Value::Str("T".to_string())])
            .expect("append should succeed");
        let Value::Bytes(bytes) = value else {
            panic!("should return Bytes");
        };
        assert_eq!(bytes.as_slice(), b"BT");
    }
}
