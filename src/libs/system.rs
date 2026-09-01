//! BT system function standard library.
//!
//! System functions are stateless, globally callable basic capabilities, such as `include()`, `type()`, `int()`, and `json()`.
//! Constructors such as `date()`, `fs()`, and `mysql()` belong in their own library modules and are routed by the VM's library constructor.
//! This keeps system functions lightweight and preserves clear boundaries between standard-library objects.

use crate::value::Value;
use indexmap::IndexMap;
use regex::RegexBuilder;
use std::cell::RefCell;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Determines whether the name is a BT system function.
///
/// The VM checks this after variable lookup fails and creates a `NativeFunction`
/// only on a match, keeping the system-function table out of the executor.
pub fn is_system_function(name: &str) -> bool {
    matches!(
        name,
        "envs"
            | "env"
            | "has_envs"
            | "has_env"
            | "pause"
            | "assert"
            | "echo"
            | "eval"
            | "exit"
            | "include"
            | "include_once"
            | "cur_dir"
            | "cur_file"
            | "cur_root"
            | "call"
            | "bool"
            | "type"
            | "number"
            | "string"
            | "float"
            | "int"
            | "array"
            | "object"
            | "json"
            | "regex"
            | "is_empty"
            | "is_null"
            | "sleep"
            | "rand"
    )
}

/// Dispatches a system function.
///
/// Arguments have already been evaluated by the VM, so this layer performs only
/// runtime conversion. Errors contain the operation detail; the VM attaches source
/// locations. New system functions should normally be added to this dispatch table.
pub fn call(name: &str, args: Vec<Value>) -> Result<Value, String> {
    let value = match name {
        "include" | "include_once" => args.first().cloned().unwrap_or(Value::Empty),
        "assert" => {
            let Some(condition) = args.first() else {
                return Err("assert requires at least 1 argument".to_string());
            };
            if condition.is_truthy() {
                Value::Bool(true)
            } else {
                let message = args
                    .get(1)
                    .map(|value| format!("Assertion failed: {}", value.to_string()))
                    .unwrap_or_else(|| "Assertion failed".to_string());
                return Err(message);
            }
        }
        "bool" => Value::Bool(args.first().map(Value::is_truthy).unwrap_or(false)),
        "type" => Value::Str(
            args.first()
                .map(Value::type_name)
                .unwrap_or("Empty")
                .to_string(),
        ),
        "number" => args
            .first()
            .map(Value::to_number_value)
            .unwrap_or(Value::Null),
        "string" => Value::Str(args.first().map(Value::to_string).unwrap_or_default()),
        "float" => Value::Float(args.first().map(Value::to_f64_lossy).unwrap_or(0.0)),
        "int" => Value::Int(args.first().map(Value::to_i64_lossy).unwrap_or(0)),
        "array" => args
            .first()
            .cloned()
            .map(to_array)
            .unwrap_or_else(|| Value::Array(Rc::new(RefCell::new(Vec::new())))),
        "object" => args
            .first()
            .cloned()
            .map(to_object)
            .unwrap_or_else(|| Value::Object(Rc::new(RefCell::new(IndexMap::new())))),
        "json" => Value::Str(args.first().unwrap_or(&Value::Null).to_json_string()),
        "regex" => {
            let pattern = args.first().map(Value::to_string).unwrap_or_default();
            let flags = args.get(1).map(Value::to_string).unwrap_or_default();
            let regex = RegexBuilder::new(&pattern)
                .case_insensitive(flags.contains('i'))
                .multi_line(flags.contains('m'))
                .dot_matches_new_line(flags.contains('s'))
                .build()
                .map_err(|err| format!("Regular expression compilation failed: {}", err))?;
            Value::Regex(Rc::new(regex), pattern, flags)
        }
        "is_empty" => Value::Bool(match args.first() {
            None | Some(Value::Empty) | Some(Value::Null) => true,
            Some(Value::Str(value)) => value.is_empty(),
            Some(Value::Array(values)) => values.borrow().is_empty(),
            Some(Value::Object(values)) => values.borrow().is_empty(),
            Some(Value::Instance(value)) => value.borrow().members.is_empty(),
            _ => false,
        }),
        "is_null" => Value::Bool(matches!(args.first(), Some(Value::Null))),
        "sleep" => {
            let millis = args.first().map(Value::to_i64_lossy).unwrap_or(0).max(0) as u64;
            thread::sleep(Duration::from_millis(millis));
            Value::Empty
        }
        "rand" => {
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|time| time.as_nanos() as u64)
                .unwrap_or(0);
            // The system function remains stateless, so SplitMix64 is used here to single-step to mix the current high-precision time.
            // It is not a cryptographic random number, but the distribution is more stable than taking the nanosecond low bit directly, and is suitable for the general purpose of script layer rand().
            let mut x = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            x ^= x >> 31;
            let unit = (x as f64) / (u64::MAX as f64);
            if args.is_empty() {
                Value::Float(unit)
            } else {
                let (min, max) = if args.len() == 1 {
                    let end = args[0].to_f64_lossy();
                    if end < 0.0 {
                        (end, 0.0)
                    } else {
                        (0.0, end)
                    }
                } else {
                    (args[0].to_f64_lossy(), args[1].to_f64_lossy())
                };
                let low = min.min(max);
                let high = min.max(max);
                let value = low + unit * (high - low);
                if args.iter().any(|value| matches!(value, Value::Float(_))) {
                    Value::Float(value)
                } else {
                    Value::Int(value.floor() as i64)
                }
            }
        }
        _ => return Err(format!("Unknown system function `{}`", name)),
    };
    Ok(value)
}

/// Format integers according to the specified base.
pub fn format_radix(value: i64, radix: i64) -> String {
    match radix {
        2 => format!("{:b}", value),
        8 => format!("{:o}", value),
        16 => format!("{:x}", value),
        _ => value.to_string(),
    }
}

/// Parses JSON string into BT values.
pub fn parse_json_text(text: &str) -> Value {
    serde_json::from_str::<serde_json::Value>(text)
        .map(from_json_value)
        .unwrap_or(Value::Null)
}

/// Parses integer text according to the specified base.
pub fn parse_radix_int_text(text: &str, radix: i64) -> Value {
    i64::from_str_radix(text, radix as u32)
        .map(Value::Int)
        .unwrap_or(Value::Null)
}

/// Parses whitespace-delimited byte text into a UTF-8 string in the specified base.
pub fn parse_radix_str_text(text: &str, radix: i64) -> Value {
    let bytes = text
        .split_whitespace()
        .filter_map(|item| u8::from_str_radix(item, radix as u32).ok())
        .collect::<Vec<_>>();
    String::from_utf8(bytes)
        .map(Value::Str)
        .unwrap_or(Value::Null)
}

/// Generates a single-character string in Unicode code points.
pub fn char_from_code(code: i64) -> Value {
    char::from_u32(code as u32)
        .map(|ch| Value::Str(ch.to_string()))
        .unwrap_or(Value::Null)
}

/// Convert any value to an array.
fn to_array(value: Value) -> Value {
    match value {
        Value::Array(_) => value,
        Value::Object(values) => Value::Array(Rc::new(RefCell::new(
            values.borrow().values().cloned().collect(),
        ))),
        Value::Str(value) => Value::Array(Rc::new(RefCell::new(
            value.chars().map(|ch| Value::Str(ch.to_string())).collect(),
        ))),
        Value::Null | Value::Empty => Value::Array(Rc::new(RefCell::new(Vec::new()))),
        other => Value::Array(Rc::new(RefCell::new(vec![other]))),
    }
}

/// Converts any value to an object.
fn to_object(value: Value) -> Value {
    match value {
        Value::Object(_) => value,
        Value::Array(values) => {
            let mut object = IndexMap::new();
            for (index, value) in values.borrow().iter().enumerate() {
                object.insert(index.to_string(), value.clone());
            }
            Value::Object(Rc::new(RefCell::new(object)))
        }
        other => {
            let mut object = IndexMap::new();
            object.insert("value".to_string(), other);
            Value::Object(Rc::new(RefCell::new(object)))
        }
    }
}

/// Converts serde_json values to VM values.
fn from_json_value(value: serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(Value::Int)
            .or_else(|| value.as_f64().map(Value::Float))
            .unwrap_or(Value::Null),
        serde_json::Value::String(value) => Value::Str(value),
        serde_json::Value::Array(values) => Value::Array(Rc::new(RefCell::new(
            values.into_iter().map(from_json_value).collect(),
        ))),
        serde_json::Value::Object(values) => Value::Object(Rc::new(RefCell::new(
            values
                .into_iter()
                .map(|(key, value)| (key, from_json_value(value)))
                .collect(),
        ))),
    }
}
