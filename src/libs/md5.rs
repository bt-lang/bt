//! BT MD5 standard library.
//!
//! `md5(text)` creates a digest object, `write(text)` returns a new object, and `ok()` returns a hexadecimal digest.
//! Value semantics keep method calls from mutating objects in place, avoiding hidden state during chaining and variable reuse.

use crate::value::Value;
use md5::Context;

/// MD5 digest object.
#[derive(Clone)]
pub struct BtMd5 {
    /// The current summary context.
    context: Context,
}

impl std::fmt::Debug for BtMd5 {
    /// Debug output does not expand internal hash state.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtMd5").field("context", &"...").finish()
    }
}

impl PartialEq for BtMd5 {
    /// The MD5 context does not have stable comparison semantics and library objects are not considered equal according to script value semantics.
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl BtMd5 {
    /// Creates an MD5 object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let mut context = Context::new();
        if let Some(value) = args.first() {
            context.consume(value.to_string().as_bytes());
        }
        Ok(Value::Md5(Self { context }))
    }

    /// Dispatches an MD5 method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "ok" => Ok(Value::Str(format!("{:x}", self.context.clone().finalize()))),
            "write" => {
                let mut next = self.context.clone();
                let text = args.first().map(Value::to_string).unwrap_or_default();
                next.consume(text.as_bytes());
                Ok(Value::Md5(Self { context: next }))
            }
            _ => Err(format!("md5 has no method `{}`", method)),
        }
    }
}
