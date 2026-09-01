//! BT Base64 standard library.
//!
//! `base64(text)` creates a lightweight object and then selects the encoding table via `encode()` / `decode()`.
//! Legacy standalone constant tables were folded into this module to keep the small library cohesive.

use crate::value::Value;
use base64::{engine::general_purpose::*, Engine};

/// Standard Base64 encoding table.
pub const BASE64_STANDARD: i64 = 0;
/// Standard Base64 encoding table, no complement character is output.
pub const BASE64_STANDARD_NO_PAD: i64 = 1;
/// URL safe Base64 encoding table.
pub const BASE64_URL_SAFE: i64 = 2;
/// URL safe Base64 encoding table, does not output fillers.
pub const BASE64_URL_SAFE_NO_PAD: i64 = 3;

/// Base64 library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtBase64 {
    /// Text to be encoded or decoded.
    text: String,
}

impl BtBase64 {
    /// Create a Base64 object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let text = args.first().map(Value::to_string).unwrap_or_default();
        Ok(Value::Base64(Self { text }))
    }

    /// Dispatches a Base64 method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "encode" => Ok(Value::Str(
                self.engine(args.first()).encode(self.text.as_bytes()),
            )),
            "decode" => {
                let bytes = self
                    .engine(args.first())
                    .decode(self.text.as_bytes())
                    .map_err(|err| format!("base64 decoding failed: {}", err))?;
                String::from_utf8(bytes)
                    .map(Value::Str)
                    .map_err(|_| "base64 decoding result is not a legal UTF-8 string".to_string())
            }
            "to_string" => Ok(Value::Str(self.text.clone())),
            _ => Err(format!("base64 has no method `{}`", method)),
        }
    }

    /// Selects the encoding table based on script parameters.
    fn engine(&self, value: Option<&Value>) -> &'static base64::engine::GeneralPurpose {
        match value.map(Value::to_i64_lossy).unwrap_or(BASE64_STANDARD) {
            BASE64_STANDARD_NO_PAD => &STANDARD_NO_PAD,
            BASE64_URL_SAFE => &URL_SAFE,
            BASE64_URL_SAFE_NO_PAD => &URL_SAFE_NO_PAD,
            _ => &STANDARD,
        }
    }
}
