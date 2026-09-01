//! BT cryptographic digest standard library.
//!
//! `crypto(text)` creates a lightweight text object and provides common SHA, HMAC, and UUID operations.
//! It retains no cache or random-generator state, avoiding unbounded growth in resident processes.

use crate::value::Value;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};
use uuid::Uuid;

/// HMAC-SHA256 type alias.
type HmacSha256 = Hmac<Sha256>;
/// HMAC-SHA512 type alias.
type HmacSha512 = Hmac<Sha512>;

/// Cryptographic digest standard library object.
#[derive(Debug, Clone, PartialEq)]
pub struct BtCrypto {
    /// The current input text.
    text: String,
}

impl BtCrypto {
    /// Creates a cryptographic digest object.
    pub fn new(args: Vec<Value>) -> Result<Value, String> {
        let text = args.first().map(Value::to_string).unwrap_or_default();
        Ok(Value::Crypto(Self { text }))
    }

    /// Dispatches a cryptographic digest method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match method {
            "sha256" => Ok(Value::Str(hex_digest(Sha256::digest(self.text.as_bytes())))),
            "sha512" => Ok(Value::Str(hex_digest(Sha512::digest(self.text.as_bytes())))),
            "hmac_sha256" => {
                let key = required_text(&args, 0, method)?;
                let mut mac = HmacSha256::new_from_slice(key.as_bytes()).map_err(|err| {
                    format!("crypto.hmac_sha256(): initialization failed: {}", err)
                })?;
                mac.update(self.text.as_bytes());
                Ok(Value::Str(hex_digest(mac.finalize().into_bytes())))
            }
            "hmac_sha512" => {
                let key = required_text(&args, 0, method)?;
                let mut mac = HmacSha512::new_from_slice(key.as_bytes()).map_err(|err| {
                    format!("crypto.hmac_sha512(): initialization failed: {}", err)
                })?;
                mac.update(self.text.as_bytes());
                Ok(Value::Str(hex_digest(mac.finalize().into_bytes())))
            }
            "uuid" => Ok(Value::Str(Uuid::new_v4().to_string())),
            "to_string" => Ok(Value::Str(self.text.clone())),
            _ => Err(format!("crypto has no method `{}`", method)),
        }
    }
}

/// Encodes a sequence of bytes into a lowercase hexadecimal string.
fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Reads a required text argument.
fn required_text(args: &[Value], index: usize, method: &str) -> Result<String, String> {
    args.get(index)
        .map(Value::to_string)
        .ok_or_else(|| format!("crypto.{}() missing {} argument", method, index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA and HMAC should output a stable lowercase hexadecimal digest.
    #[test]
    fn crypto_hashes_return_hex_text() {
        let Value::Crypto(crypto) = BtCrypto::new(vec![Value::Str("BT".to_string())])
            .expect("crypto() should create objects")
        else {
            panic!("crypto() should return the Crypto value");
        };

        assert_eq!(
            crypto.call_method("sha256", Vec::new()),
            Ok(Value::Str(
                "4ea3d68e3581fa27f86acaa247b297686a8e1a8fecd5523cd8314f14b6a28c31".to_string()
            ))
        );
        assert_eq!(
            crypto
                .call_method("hmac_sha256", vec![Value::Str("key".to_string())])
                .expect("hmac_sha256 should succeed")
                .to_string()
                .len(),
            64
        );
    }
}
