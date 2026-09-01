//! BT URL standard library.
//!
//! `url(text)` provides URL parsing, query-parameter access, percent encoding, and URL construction from objects.

use crate::value::Value;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::rc::Rc;
use url::Url;

/// URL standard library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtUrl {
    /// The current URL or text to be processed.
    text: String,
}

impl BtUrl {
    /// Creates a URL object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let text = match args.first() {
            Some(Value::Object(values)) => build_url_from_object(&values.borrow()),
            Some(value) => value.to_string(),
            None => String::new(),
        };
        Ok(Value::Url(Self { text }))
    }

    /// Dispatches a URL method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "encode" => Ok(Value::Str(percent_encode(&self.text))),
            "decode" => Ok(Value::Str(percent_decode(&self.text))),
            "parse" => self.parse(),
            "query" => self.query(),
            "build" => {
                if let Some(Value::Object(values)) = args.first() {
                    Ok(Value::Str(build_url_from_object(&values.borrow())))
                } else {
                    Ok(Value::Str(self.text.clone()))
                }
            }
            "to_string" => Ok(Value::Str(self.text.clone())),
            _ => Err(format!("url has no method `{}`", method)),
        }
    }

    /// Parses the current URL into an object.
    fn parse(&self) -> Result<Value, String> {
        let parsed =
            Url::parse(&self.text).map_err(|err| format!("url.parse() failed: {}", err))?;
        let mut values = IndexMap::new();
        values.insert(
            "scheme".to_string(),
            Value::Str(parsed.scheme().to_string()),
        );
        values.insert(
            "username".to_string(),
            Value::Str(parsed.username().to_string()),
        );
        values.insert(
            "password".to_string(),
            parsed
                .password()
                .map(|value| Value::Str(value.to_string()))
                .unwrap_or(Value::Empty),
        );
        values.insert(
            "host".to_string(),
            parsed
                .host_str()
                .map(|value| Value::Str(value.to_string()))
                .unwrap_or(Value::Empty),
        );
        values.insert(
            "port".to_string(),
            parsed
                .port()
                .map(|value| Value::Int(value as i64))
                .unwrap_or(Value::Empty),
        );
        values.insert("path".to_string(), Value::Str(parsed.path().to_string()));
        values.insert(
            "query".to_string(),
            parsed
                .query()
                .map(|value| Value::Str(value.to_string()))
                .unwrap_or(Value::Empty),
        );
        values.insert(
            "fragment".to_string(),
            parsed
                .fragment()
                .map(|value| Value::Str(value.to_string()))
                .unwrap_or(Value::Empty),
        );
        values.insert(
            "origin".to_string(),
            Value::Str(parsed.origin().ascii_serialization()),
        );
        values.insert("url".to_string(), Value::Str(parsed.to_string()));
        Ok(Value::Object(Rc::new(RefCell::new(values))))
    }

    /// Read the current URL query parameters as an object.
    fn query(&self) -> Result<Value, String> {
        let parsed =
            Url::parse(&self.text).map_err(|err| format!("url.query() failed: {}", err))?;
        let mut values = IndexMap::new();
        for (key, value) in parsed.query_pairs() {
            values.insert(key.to_string(), Value::Str(value.to_string()));
        }
        Ok(Value::Object(Rc::new(RefCell::new(values))))
    }
}

/// Percent-encode text according to URL component rules.
fn percent_encode(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char)
            }
            value => {
                output.push('%');
                output.push_str(&format!("{:02X}", value));
            }
        }
    }
    output
}

/// Decodes percent-encoded text.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &text[index + 1..index + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                output.push(value);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(output).unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into())
}

/// Constructs a URL string from an object configuration.
fn build_url_from_object(values: &IndexMap<String, Value>) -> String {
    let scheme = object_text(values, "scheme", "http");
    let host = object_text(values, "host", "");
    let path = object_text(values, "path", "/");
    let mut output = String::with_capacity(scheme.len() + host.len() + path.len() + 8);
    output.push_str(&scheme);
    output.push_str("://");
    if let Some(username) = object_non_empty(values, "username") {
        output.push_str(&percent_encode(&username));
        if let Some(password) = object_non_empty(values, "password") {
            output.push(':');
            output.push_str(&percent_encode(&password));
        }
        output.push('@');
    }
    output.push_str(&host);
    if let Some(port) = values.get("port") {
        if !matches!(port, Value::Empty | Value::Null) {
            output.push(':');
            output.push_str(&port.to_string());
        }
    }
    if path.starts_with('/') {
        output.push_str(&path);
    } else {
        output.push('/');
        output.push_str(&path);
    }
    if let Some(query) = values.get("query") {
        let query = query_text(query);
        if !query.is_empty() {
            output.push('?');
            output.push_str(&query);
        }
    }
    if let Some(fragment) = object_non_empty(values, "fragment") {
        output.push('#');
        output.push_str(&percent_encode(&fragment));
    }
    output
}

/// Reads the text field in the object.
fn object_text(values: &IndexMap<String, Value>, key: &str, default: &str) -> String {
    values
        .get(key)
        .filter(|value| !matches!(value, Value::Empty | Value::Null))
        .map(Value::to_string)
        .unwrap_or_else(|| default.to_string())
}

/// Reads a non-null text field in an object.
fn object_non_empty(values: &IndexMap<String, Value>, key: &str) -> Option<String> {
    values
        .get(key)
        .map(Value::to_string)
        .filter(|value| !value.is_empty())
}

/// Convert query configuration to query string.
fn query_text(value: &Value) -> String {
    match value {
        Value::Object(values) => {
            let values = values.borrow();
            let mut output = String::new();
            for (index, (key, value)) in values.iter().enumerate() {
                if index > 0 {
                    output.push('&');
                }
                output.push_str(&percent_encode(key));
                output.push('=');
                output.push_str(&percent_encode(&value.to_string()));
            }
            output
        }
        Value::Str(value) => value.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL library should support component encoding, parsing, and object-wise URL building.
    #[test]
    fn url_methods_cover_common_project_usage() {
        let Value::Url(value) = BtUrl::new(vec![Value::Str(
            "https://btlang.org/docs?q=BT%20Lang".to_string(),
        )])
        .expect("url() should create the URL object") else {
            panic!("url() should return the Url value");
        };

        assert_eq!(
            value.call_method("query", Vec::new()).unwrap().to_string(),
            "{\"q\":\"BT Lang\"}"
        );
        assert_eq!(
            BtUrl::new(vec![Value::Str("BT Lang".to_string())])
                .unwrap()
                .type_name(),
            "Url"
        );
        assert_eq!(percent_encode("BT Lang"), "BT%20Lang");
        assert_eq!(percent_decode("BT%20Lang"), "BT Lang");
    }
}
