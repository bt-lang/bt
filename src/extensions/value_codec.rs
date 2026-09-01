//! Binary value encoding and decoding for the BT extension ABI.
//!
//! `BtValueBinary` transfers ordinary BT values between the WASM backend and host. It avoids JSON
//! to preserve the `empty` / `null` distinction and prevent the CPU and memory overhead of Base64-encoding binary data.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::extensions::manager::ExtObject;
use crate::extensions::registry::ExtensionModuleId;
use crate::libs::bytes::BtBytes;
use crate::value::Value;

/// Tag for an `empty` value.
const TAG_EMPTY: u8 = 0x00;
/// Tag for a `null` value.
const TAG_NULL: u8 = 0x01;
/// Tag for a Boolean value.
const TAG_BOOL: u8 = 0x02;
/// Tag for an integer value.
const TAG_INT: u8 = 0x03;
/// Tag for a floating-point value.
const TAG_FLOAT: u8 = 0x04;
/// Tag for a string value.
const TAG_STRING: u8 = 0x05;
/// Tag for a Bytes value.
const TAG_BYTES: u8 = 0x06;
/// Tag for an array value.
const TAG_ARRAY: u8 = 0x07;
/// Tag for an ordinary object value.
const TAG_OBJECT: u8 = 0x08;
/// Tag for an extension object handle.
const TAG_EXT_OBJECT: u8 = 0x09;

/// Success marker in a WASM call result envelope.
const CALL_RESULT_OK: u8 = 0x00;
/// Extension error marker in a WASM call result envelope.
const CALL_RESULT_ERR: u8 = 0x01;

/// Default limit on total bytes produced by a single encoding operation.
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
/// Default UTF-8 byte limit for a single string.
pub const DEFAULT_MAX_STRING_BYTES: usize = 4 * 1024 * 1024;
/// Default byte limit for a single Bytes buffer.
pub const DEFAULT_MAX_BYTES_BYTES: usize = 16 * 1024 * 1024;
/// Default array item limit.
pub const DEFAULT_MAX_ARRAY_ITEMS: usize = 65_536;
/// Default object field limit.
pub const DEFAULT_MAX_OBJECT_FIELDS: usize = 65_536;
/// Default maximum nesting depth.
pub const DEFAULT_MAX_DEPTH: usize = 64;

/// BtValueBinary encoding and decoding limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueCodecLimits {
    /// Maximum total bytes allowed for one encoding or decoding operation.
    pub max_total_bytes: usize,
    /// Maximum UTF-8 bytes allowed in a single string.
    pub max_string_bytes: usize,
    /// Maximum bytes allowed in a single Bytes value.
    pub max_bytes_bytes: usize,
    /// Maximum items allowed in a single array.
    pub max_array_items: usize,
    /// Maximum fields allowed in a single object.
    pub max_object_fields: usize,
    /// Maximum nesting depth allowed during recursion.
    pub max_depth: usize,
}

impl Default for ValueCodecLimits {
    /// Returns conservative default limits for the extension ABI.
    fn default() -> Self {
        Self {
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_string_bytes: DEFAULT_MAX_STRING_BYTES,
            max_bytes_bytes: DEFAULT_MAX_BYTES_BYTES,
            max_array_items: DEFAULT_MAX_ARRAY_ITEMS,
            max_object_fields: DEFAULT_MAX_OBJECT_FIELDS,
            max_depth: DEFAULT_MAX_DEPTH,
        }
    }
}

impl ValueCodecLimits {
    /// Creates limits with the specified total byte limit and all other defaults unchanged.
    pub fn with_total_bytes(max_total_bytes: usize) -> Self {
        Self {
            max_total_bytes,
            ..Self::default()
        }
    }
}

/// Decoded extension call result envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionCallOutput {
    /// A successful extension call with its return value.
    Value(Value),
    /// An extension error with its message.
    Error(String),
}

/// Encodes an ordinary BT value.
pub fn encode_value(value: &Value, limits: ValueCodecLimits) -> Result<Vec<u8>, String> {
    let mut encoder = Encoder::new(limits);
    encoder.encode_value(value, 0)?;
    Ok(encoder.into_output())
}

/// Decodes an ordinary BT value.
pub fn decode_value(data: &[u8], limits: ValueCodecLimits) -> Result<Value, String> {
    let mut decoder = Decoder::new(data, limits)?;
    let value = decoder.decode_value(0)?;
    decoder.finish()?;
    Ok(value)
}

/// Encodes a successful extension call envelope.
pub fn encode_call_success(value: &Value, limits: ValueCodecLimits) -> Result<Vec<u8>, String> {
    let mut encoder = Encoder::new(limits);
    encoder.write_u8(CALL_RESULT_OK)?;
    encoder.encode_value(value, 0)?;
    Ok(encoder.into_output())
}

/// Encodes an extension call error envelope.
pub fn encode_call_error(message: &str, limits: ValueCodecLimits) -> Result<Vec<u8>, String> {
    let mut encoder = Encoder::new(limits);
    encoder.write_u8(CALL_RESULT_ERR)?;
    encoder.encode_value(&Value::Str(message.to_string()), 0)?;
    Ok(encoder.into_output())
}

/// Decodes an extension call result envelope.
pub fn decode_call_output(
    data: &[u8],
    limits: ValueCodecLimits,
) -> Result<ExtensionCallOutput, String> {
    let mut decoder = Decoder::new(data, limits)?;
    let marker = decoder.read_u8()?;
    let value = decoder.decode_value(0)?;
    decoder.finish()?;
    match marker {
        CALL_RESULT_OK => Ok(ExtensionCallOutput::Value(value)),
        CALL_RESULT_ERR => match value {
            Value::Str(message) => Ok(ExtensionCallOutput::Error(message)),
            other => Err(format!(
                "BtValueBinary call error envelope must contain a string, found {}",
                other.type_name()
            )),
        },
        other => Err(format!(
            "Unsupported BtValueBinary call result marker 0x{other:02x}"
        )),
    }
}

/// Reference visitation stack used during recursive encoding.
#[derive(Default)]
struct EncodeVisit {
    /// Array pointers in the current recursion stack.
    arrays: HashSet<usize>,
    /// Object pointers in the current recursion stack.
    objects: HashSet<usize>,
}

/// BtValueBinary encoder.
struct Encoder {
    /// Encoded output buffer.
    output: Vec<u8>,
    /// Encoding limits.
    limits: ValueCodecLimits,
    /// Reference visitation state for the current recursion stack.
    visit: EncodeVisit,
}

impl Encoder {
    /// Creates a new encoder.
    fn new(limits: ValueCodecLimits) -> Self {
        Self {
            output: Vec::new(),
            limits,
            visit: EncodeVisit::default(),
        }
    }

    /// Returns the encoded output.
    fn into_output(self) -> Vec<u8> {
        self.output
    }

    /// Recursively encodes a BT value.
    fn encode_value(&mut self, value: &Value, depth: usize) -> Result<(), String> {
        self.ensure_depth(depth)?;
        match value {
            Value::Empty => self.write_u8(TAG_EMPTY),
            Value::Null => self.write_u8(TAG_NULL),
            Value::Bool(value) => {
                self.write_u8(TAG_BOOL)?;
                self.write_u8(u8::from(*value))
            }
            Value::Int(value) => {
                self.write_u8(TAG_INT)?;
                self.write_i64(*value)
            }
            Value::Float(value) => {
                if !value.is_finite() {
                    return Err(
                        "BtValueBinary does not support transporting NaN or Infinity".to_string(),
                    );
                }
                self.write_u8(TAG_FLOAT)?;
                self.write_f64(*value)
            }
            Value::Str(value) => {
                self.write_u8(TAG_STRING)?;
                self.write_len_prefixed_bytes(
                    value.as_bytes(),
                    self.limits.max_string_bytes,
                    "string",
                )
            }
            Value::Bytes(value) => {
                self.write_u8(TAG_BYTES)?;
                self.write_len_prefixed_bytes(
                    value.as_slice(),
                    self.limits.max_bytes_bytes,
                    "Bytes",
                )
            }
            Value::Array(values) => self.encode_array(values, depth),
            Value::Object(values) => self.encode_object(values, depth),
            Value::ExtObject(object) => self.encode_ext_object(object),
            other => Err(format!(
                "BtValueBinary does not support encoding values of type `{}`",
                other.type_name()
            )),
        }
    }

    /// Encodes an array value.
    fn encode_array(
        &mut self,
        values: &Rc<RefCell<Vec<Value>>>,
        depth: usize,
    ) -> Result<(), String> {
        let pointer = Rc::as_ptr(values) as usize;
        if !self.visit.arrays.insert(pointer) {
            return Err("BtValueBinary does not support cyclic array references".to_string());
        }
        let result = (|| {
            let values = values.borrow();
            self.ensure_count(values.len(), self.limits.max_array_items, "array item")?;
            self.write_u8(TAG_ARRAY)?;
            self.write_u32(usize_to_u32(values.len(), "array item count")?)?;
            for value in values.iter() {
                self.encode_value(value, depth + 1)?;
            }
            Ok(())
        })();
        self.visit.arrays.remove(&pointer);
        result
    }

    /// Encodes an ordinary object value.
    fn encode_object(
        &mut self,
        values: &Rc<RefCell<IndexMap<String, Value>>>,
        depth: usize,
    ) -> Result<(), String> {
        let pointer = Rc::as_ptr(values) as usize;
        if !self.visit.objects.insert(pointer) {
            return Err("BtValueBinary does not support cyclic object references".to_string());
        }
        let result = (|| {
            let values = values.borrow();
            self.ensure_count(values.len(), self.limits.max_object_fields, "object field")?;
            self.write_u8(TAG_OBJECT)?;
            self.write_u32(usize_to_u32(values.len(), "object field count")?)?;
            for (key, value) in values.iter() {
                self.write_len_prefixed_bytes(
                    key.as_bytes(),
                    self.limits.max_string_bytes,
                    "object field name",
                )?;
                self.encode_value(value, depth + 1)?;
            }
            Ok(())
        })();
        self.visit.objects.remove(&pointer);
        result
    }

    /// Encodes an extension object handle.
    fn encode_ext_object(&mut self, object: &ExtObject) -> Result<(), String> {
        self.write_u8(TAG_EXT_OBJECT)?;
        self.write_u64(u64::try_from(object.module_id).map_err(|_| {
            "BtValueBinary extension object module_id exceeds the u64 limit".to_string()
        })?)?;
        self.write_u32(object.type_id)?;
        self.write_u64(object.object_id)?;
        self.write_len_prefixed_bytes(
            object.type_name.as_bytes(),
            self.limits.max_string_bytes,
            "extension object type name",
        )
    }

    /// Writes one byte.
    fn write_u8(&mut self, value: u8) -> Result<(), String> {
        self.reserve_output(1)?;
        self.output.push(value);
        Ok(())
    }

    /// Writes a little-endian u32 integer.
    fn write_u32(&mut self, value: u32) -> Result<(), String> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Writes a little-endian u64 integer.
    fn write_u64(&mut self, value: u64) -> Result<(), String> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Writes a little-endian i64 integer.
    fn write_i64(&mut self, value: i64) -> Result<(), String> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Writes a little-endian f64 value.
    fn write_f64(&mut self, value: f64) -> Result<(), String> {
        self.write_raw_bytes(&value.to_le_bytes())
    }

    /// Writes a byte slice prefixed with its length as a u32.
    fn write_len_prefixed_bytes(
        &mut self,
        bytes: &[u8],
        limit: usize,
        label: &str,
    ) -> Result<(), String> {
        if bytes.len() > limit {
            return Err(format!(
                "BtValueBinary {} length {} exceeds the limit of {}",
                label,
                bytes.len(),
                limit
            ));
        }
        self.write_u32(usize_to_u32(bytes.len(), label)?)?;
        self.write_raw_bytes(bytes)
    }

    /// Writes raw bytes.
    fn write_raw_bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.reserve_output(bytes.len())?;
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    /// Ensures that another write will not exceed the total byte limit.
    fn reserve_output(&mut self, additional: usize) -> Result<(), String> {
        let next_len = self
            .output
            .len()
            .checked_add(additional)
            .ok_or_else(|| "BtValueBinary encoded length overflow".to_string())?;
        if next_len > self.limits.max_total_bytes {
            return Err(format!(
                "BtValueBinary encoded size {} exceeds the limit of {} bytes",
                next_len, self.limits.max_total_bytes
            ));
        }
        self.output
            .try_reserve(additional)
            .map_err(|_| "Failed to allocate BtValueBinary encoding buffer".to_string())
    }

    /// Validates the current nesting depth.
    fn ensure_depth(&self, depth: usize) -> Result<(), String> {
        if depth > self.limits.max_depth {
            Err(format!(
                "BtValueBinary nesting depth {} exceeds the limit of {}",
                depth, self.limits.max_depth
            ))
        } else {
            Ok(())
        }
    }

    /// Validates the number of collection elements.
    fn ensure_count(&self, count: usize, limit: usize, label: &str) -> Result<(), String> {
        if count > limit {
            Err(format!(
                "BtValueBinary {} count {} exceeds the limit of {}",
                label, count, limit
            ))
        } else {
            Ok(())
        }
    }
}

/// BtValueBinary decoder.
struct Decoder<'a> {
    /// Input byte slice.
    input: &'a [u8],
    /// Current read position.
    offset: usize,
    /// Decoding limits.
    limits: ValueCodecLimits,
}

impl<'a> Decoder<'a> {
    /// Creates a new decoder.
    fn new(input: &'a [u8], limits: ValueCodecLimits) -> Result<Self, String> {
        if input.len() > limits.max_total_bytes {
            return Err(format!(
                "BtValueBinary input size {} exceeds the limit of {} bytes",
                input.len(),
                limits.max_total_bytes
            ));
        }
        Ok(Self {
            input,
            offset: 0,
            limits,
        })
    }

    /// Recursively decodes a BT value.
    fn decode_value(&mut self, depth: usize) -> Result<Value, String> {
        self.ensure_depth(depth)?;
        let tag = self.read_u8()?;
        match tag {
            TAG_EMPTY => Ok(Value::Empty),
            TAG_NULL => Ok(Value::Null),
            TAG_BOOL => self.decode_bool(),
            TAG_INT => Ok(Value::Int(self.read_i64()?)),
            TAG_FLOAT => self.decode_float(),
            TAG_STRING => self.decode_string_value(),
            TAG_BYTES => self.decode_bytes_value(),
            TAG_ARRAY => self.decode_array(depth),
            TAG_OBJECT => self.decode_object(depth),
            TAG_EXT_OBJECT => self.decode_ext_object(),
            other => Err(format!("Unsupported BtValueBinary value tag 0x{other:02x}")),
        }
    }

    /// Decodes a Boolean value.
    fn decode_bool(&mut self) -> Result<Value, String> {
        match self.read_u8()? {
            0 => Ok(Value::Bool(false)),
            1 => Ok(Value::Bool(true)),
            other => Err(format!("Invalid BtValueBinary bool byte {}", other)),
        }
    }

    /// Decodes a finite floating-point value.
    fn decode_float(&mut self) -> Result<Value, String> {
        let value = self.read_f64()?;
        if value.is_finite() {
            Ok(Value::Float(value))
        } else {
            Err("BtValueBinary does not support decoding NaN or Infinity".to_string())
        }
    }

    /// Decodes a string value.
    fn decode_string_value(&mut self) -> Result<Value, String> {
        Ok(Value::Str(self.read_string("string")?))
    }

    /// Decodes a Bytes value.
    fn decode_bytes_value(&mut self) -> Result<Value, String> {
        let bytes = self.read_len_prefixed_bytes(self.limits.max_bytes_bytes, "Bytes")?;
        let bytes = copy_slice(bytes)?;
        Ok(Value::Bytes(BtBytes::unchecked(bytes)))
    }

    /// Decodes an array value.
    fn decode_array(&mut self, depth: usize) -> Result<Value, String> {
        let count = self.read_count(self.limits.max_array_items, "array item")?;
        let mut values = Vec::new();
        values
            .try_reserve(count)
            .map_err(|_| "Failed to allocate BtValueBinary array".to_string())?;
        for _ in 0..count {
            values.push(self.decode_value(depth + 1)?);
        }
        Ok(Value::Array(Rc::new(RefCell::new(values))))
    }

    /// Decodes an ordinary object value.
    fn decode_object(&mut self, depth: usize) -> Result<Value, String> {
        let count = self.read_count(self.limits.max_object_fields, "object field")?;
        let mut values = IndexMap::new();
        values
            .try_reserve(count)
            .map_err(|_| "Failed to allocate BtValueBinary object".to_string())?;
        for _ in 0..count {
            let key = self.read_string("object field name")?;
            if values.contains_key(&key) {
                return Err(format!(
                    "BtValueBinary object contains duplicate field `{}`",
                    key
                ));
            }
            let value = self.decode_value(depth + 1)?;
            values.insert(key, value);
        }
        Ok(Value::Object(Rc::new(RefCell::new(values))))
    }

    /// Decodes an extension object handle.
    fn decode_ext_object(&mut self) -> Result<Value, String> {
        let module_id = self.read_u64()?;
        let module_id = ExtensionModuleId::try_from(module_id).map_err(|_| {
            "BtValueBinary extension object module_id exceeds the platform usize limit".to_string()
        })?;
        let type_id = self.read_u32()?;
        if type_id == 0 {
            return Err("BtValueBinary extension object type_id must not be 0".to_string());
        }
        let object_id = self.read_u64()?;
        let type_name = self.read_string("extension object type name")?;
        if type_name.is_empty() {
            return Err("BtValueBinary extension object type name must not be empty".to_string());
        }
        Ok(Value::ExtObject(ExtObject {
            module_id,
            type_id,
            type_name,
            object_id,
        }))
    }

    /// Reads one byte.
    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.read_raw_bytes(1)?[0])
    }

    /// Reads a little-endian u32 integer.
    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self.read_raw_bytes(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect(
            "a fixed-length slice must convert to a u32 byte array",
        )))
    }

    /// Reads a little-endian u64 integer.
    fn read_u64(&mut self) -> Result<u64, String> {
        let bytes = self.read_raw_bytes(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().expect(
            "a fixed-length slice must convert to a u64 byte array",
        )))
    }

    /// Reads a little-endian i64 integer.
    fn read_i64(&mut self) -> Result<i64, String> {
        let bytes = self.read_raw_bytes(8)?;
        Ok(i64::from_le_bytes(bytes.try_into().expect(
            "a fixed-length slice must convert to an i64 byte array",
        )))
    }

    /// Reads a little-endian f64 value.
    fn read_f64(&mut self) -> Result<f64, String> {
        let bytes = self.read_raw_bytes(8)?;
        Ok(f64::from_le_bytes(bytes.try_into().expect(
            "a fixed-length slice must convert to an f64 byte array",
        )))
    }

    /// Reads a byte slice prefixed with its length as a u32.
    fn read_len_prefixed_bytes(&mut self, limit: usize, label: &str) -> Result<&'a [u8], String> {
        let len = self.read_u32()? as usize;
        if len > limit {
            return Err(format!(
                "BtValueBinary {} length {} exceeds the limit of {}",
                label, len, limit
            ));
        }
        self.read_raw_bytes(len)
    }

    /// Reads a UTF-8 string prefixed with its length as a u32.
    fn read_string(&mut self, label: &str) -> Result<String, String> {
        let bytes = self.read_len_prefixed_bytes(self.limits.max_string_bytes, label)?;
        let bytes = copy_slice(bytes)?;
        String::from_utf8(bytes)
            .map_err(|err| format!("BtValueBinary {} is not valid UTF-8: {}", label, err))
    }

    /// Reads the number of collection elements.
    fn read_count(&mut self, limit: usize, label: &str) -> Result<usize, String> {
        let count = self.read_u32()? as usize;
        if count > limit {
            return Err(format!(
                "BtValueBinary {} count {} exceeds the limit of {}",
                label, count, limit
            ));
        }
        Ok(count)
    }

    /// Reads the specified number of raw bytes.
    fn read_raw_bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| "BtValueBinary read position overflow".to_string())?;
        if end > self.input.len() {
            return Err(format!(
                "Truncated BtValueBinary data: need {} bytes, {} bytes remain",
                len,
                self.input.len().saturating_sub(self.offset)
            ));
        }
        let start = self.offset;
        self.offset = end;
        Ok(&self.input[start..end])
    }

    /// Validates the current nesting depth.
    fn ensure_depth(&self, depth: usize) -> Result<(), String> {
        if depth > self.limits.max_depth {
            Err(format!(
                "BtValueBinary nesting depth {} exceeds the limit of {}",
                depth, self.limits.max_depth
            ))
        } else {
            Ok(())
        }
    }

    /// Verifies that the input has been fully consumed.
    fn finish(&self) -> Result<(), String> {
        if self.offset == self.input.len() {
            Ok(())
        } else {
            Err(format!(
                "BtValueBinary contains {} trailing bytes",
                self.input.len() - self.offset
            ))
        }
    }
}

/// Converts a length to the u32 representation used by BtValueBinary.
fn usize_to_u32(value: usize, label: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| format!("BtValueBinary {} exceeds the u32 limit", label))
}

/// Copies an input slice while handling allocation failure explicitly.
fn copy_slice(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    output
        .try_reserve(bytes.len())
        .map_err(|_| "Failed to allocate BtValueBinary decoding buffer".to_string())?;
    output.extend_from_slice(bytes);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the default limits used by tests.
    fn limits() -> ValueCodecLimits {
        ValueCodecLimits::default()
    }

    /// Builds an ordinary object containing scalars, Bytes, an array, an object, empty, and null.
    fn mixed_object_value() -> Value {
        let mut object = IndexMap::new();
        object.insert("empty".to_string(), Value::Empty);
        object.insert("null".to_string(), Value::Null);
        object.insert("bool".to_string(), Value::Bool(true));
        object.insert("int".to_string(), Value::Int(-7));
        object.insert("float".to_string(), Value::Float(3.5));
        object.insert("string".to_string(), Value::Str("BT".to_string()));
        object.insert(
            "bytes".to_string(),
            Value::Bytes(BtBytes::unchecked(vec![0x42, 0x54])),
        );
        object.insert(
            "array".to_string(),
            Value::Array(Rc::new(RefCell::new(vec![Value::Int(1), Value::Null]))),
        );
        Value::Object(Rc::new(RefCell::new(object)))
    }

    /// Encoding and decoding preserve ordinary value structure and distinguish empty from null.
    #[test]
    fn round_trips_plain_values() {
        let encoded =
            encode_value(&mixed_object_value(), limits()).expect("encoding should succeed");
        let decoded = decode_value(&encoded, limits()).expect("decoding should succeed");
        let Value::Object(values) = decoded else {
            panic!("value should decode to an object");
        };
        let values = values.borrow();
        assert_eq!(values.get("empty"), Some(&Value::Empty));
        assert_eq!(values.get("null"), Some(&Value::Null));
        assert_eq!(values.get("bool"), Some(&Value::Bool(true)));
        assert_eq!(values.get("int"), Some(&Value::Int(-7)));
        assert_eq!(values.get("float"), Some(&Value::Float(3.5)));
        assert_eq!(values.get("string"), Some(&Value::Str("BT".to_string())));
        let Some(Value::Bytes(bytes)) = values.get("bytes") else {
            panic!("bytes field should remain a Bytes value");
        };
        assert_eq!(bytes.as_slice(), b"BT");
        let Some(Value::Array(array)) = values.get("array") else {
            panic!("array field should remain an array");
        };
        assert_eq!(array.borrow().as_slice(), &[Value::Int(1), Value::Null]);
    }

    /// The tags for empty and null must differ.
    #[test]
    fn empty_and_null_use_distinct_tags() {
        assert_eq!(
            encode_value(&Value::Empty, limits()).unwrap(),
            vec![TAG_EMPTY]
        );
        assert_eq!(
            encode_value(&Value::Null, limits()).unwrap(),
            vec![TAG_NULL]
        );
    }

    /// Extension object handles round-trip without loss.
    #[test]
    fn round_trips_ext_object() {
        let value = Value::ExtObject(ExtObject {
            module_id: 7,
            type_id: 3,
            type_name: "Calc".to_string(),
            object_id: 42,
        });
        let encoded = encode_value(&value, limits()).expect("encoding should succeed");
        let decoded = decode_value(&encoded, limits()).expect("decoding should succeed");
        assert_eq!(decoded, value);
    }

    /// Success and error envelopes decode to distinct results.
    #[test]
    fn call_output_envelope_round_trips() {
        let ok =
            encode_call_success(&Value::Int(3), limits()).expect("success envelope should encode");
        assert_eq!(
            decode_call_output(&ok, limits()).expect("success envelope should decode"),
            ExtensionCallOutput::Value(Value::Int(3))
        );

        let err =
            encode_call_error("Invalid argument", limits()).expect("error envelope should encode");
        assert_eq!(
            decode_call_output(&err, limits()).expect("error envelope should decode"),
            ExtensionCallOutput::Error("Invalid argument".to_string())
        );
    }

    /// Encoding rejects non-finite floating-point values.
    #[test]
    fn rejects_non_finite_float_on_encode() {
        let err = encode_value(&Value::Float(f64::NAN), limits()).unwrap_err();
        assert!(err.contains("NaN"));
    }

    /// Decoding rejects non-finite floating-point values.
    #[test]
    fn rejects_non_finite_float_on_decode() {
        let mut data = vec![TAG_FLOAT];
        data.extend_from_slice(&f64::INFINITY.to_le_bytes());
        let err = decode_value(&data, limits()).unwrap_err();
        assert!(err.contains("Infinity"));
    }

    /// Encoding rejects VM-internal runtime objects.
    #[test]
    fn rejects_unsupported_runtime_values() {
        let err = encode_value(&Value::Function(0), limits()).unwrap_err();
        assert!(err.contains("does not support encoding"));
    }

    /// BtValueBinary rejects thread-local runtime values using their exact FFI type names.
    #[cfg(feature = "ffi")]
    #[test]
    fn rejects_ffi_runtime_values() {
        let _resource_guard = crate::libs::ffi::lock_test_resources();
        let value = Value::Ffi(crate::libs::ffi::BtFfiValue::static_value());
        let err = encode_value(&value, limits()).unwrap_err();

        assert!(err.contains("does not support encoding values of type `Ffi`"));

        let buffer = crate::libs::ffi::BtFfiValue::buffer(vec![Value::Int(16)]).unwrap();
        let err = encode_value(&buffer, limits()).unwrap_err();
        assert!(err.contains("does not support encoding values of type `FfiBuffer`"));
    }

    /// Encoding rejects cyclic array references.
    #[test]
    fn rejects_array_cycles() {
        let values = Rc::new(RefCell::new(Vec::new()));
        let value = Value::Array(values.clone());
        values.borrow_mut().push(value.clone());
        let err = encode_value(&value, limits()).unwrap_err();
        assert!(err.contains("cyclic array references"));
    }

    /// The string length limit applies to both encoding and decoding.
    #[test]
    fn enforces_string_limit() {
        let limited = ValueCodecLimits {
            max_string_bytes: 1,
            ..limits()
        };
        let err = encode_value(&Value::Str("BT".to_string()), limited).unwrap_err();
        assert!(err.contains("string length"));

        let encoded = encode_value(&Value::Str("BT".to_string()), limits()).unwrap();
        let err = decode_value(&encoded, limited).unwrap_err();
        assert!(err.contains("string length"));
    }

    /// The total byte limit includes tags and length fields.
    #[test]
    fn enforces_total_byte_limit() {
        let limited = ValueCodecLimits::with_total_bytes(1);
        let err = encode_value(&Value::Int(1), limited).unwrap_err();
        assert!(err.contains("encoded size"));

        let encoded = encode_value(&Value::Int(1), limits()).unwrap();
        let err = decode_value(&encoded, limited).unwrap_err();
        assert!(err.contains("input size"));
    }

    /// The array item limit applies to both encoding and decoding.
    #[test]
    fn enforces_array_limit() {
        let limited = ValueCodecLimits {
            max_array_items: 1,
            ..limits()
        };
        let value = Value::Array(Rc::new(RefCell::new(vec![Value::Int(1), Value::Int(2)])));
        let err = encode_value(&value, limited).unwrap_err();
        assert!(err.contains("array item count"));

        let encoded = encode_value(&value, limits()).unwrap();
        let err = decode_value(&encoded, limited).unwrap_err();
        assert!(err.contains("array item count"));
    }

    /// The nesting depth limit rejects overly deep arrays.
    #[test]
    fn enforces_depth_limit() {
        let value = Value::Array(Rc::new(RefCell::new(vec![Value::Array(Rc::new(
            RefCell::new(vec![Value::Int(1)]),
        ))])));
        let limited = ValueCodecLimits {
            max_depth: 1,
            ..limits()
        };
        let err = encode_value(&value, limited).unwrap_err();
        assert!(err.contains("nesting depth"));
    }

    /// Decoding rejects unknown tags.
    #[test]
    fn rejects_unknown_tag() {
        let err = decode_value(&[0xff], limits()).unwrap_err();
        assert!(err.contains("Unsupported"));
    }

    /// Decoding rejects trailing bytes.
    #[test]
    fn rejects_trailing_bytes() {
        let err = decode_value(&[TAG_EMPTY, TAG_NULL], limits()).unwrap_err();
        assert!(err.contains("trailing bytes"));
    }

    /// Decoding rejects invalid bool bytes.
    #[test]
    fn rejects_invalid_bool_byte() {
        let err = decode_value(&[TAG_BOOL, 2], limits()).unwrap_err();
        assert!(err.contains("bool"));
    }
}
