//! BT device communication standard library.
//!
//! The device layer provides a common entry point for hardware communication such as serial ports and Modbus. The first phase exposes synchronous serial support
//! and isolates the underlying serial-port library behind `crate::device`, keeping driver details out of the VM.

use crate::device::{
    open_serial, scan_serial_ports, BtDevicePort, BtSerialConfig, BtSerialPortInfo,
};
use crate::libs::bytes;
use crate::value::Value;
use indexmap::IndexMap;
use std::cell::RefCell;
use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

/// The next device handle number.
static NEXT_DEVICE_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// An open device handle that is still visible within the current thread.
    static OPENED_DEVICES: RefCell<Vec<Weak<RefCell<OpenedDevice>>>> = RefCell::new(Vec::new());
}

/// Device standard library object or specific device handle.
#[derive(Clone)]
pub struct BtDevice {
    /// Device object type.
    kind: BtDeviceKind,
}

/// BT device value internal form.
#[derive(Clone)]
enum BtDeviceKind {
    /// `device` Standard library namespace.
    Namespace,
    /// The serial port device that has been opened.
    Serial(Rc<RefCell<OpenedDevice>>),
}

/// Device status is turned on.
struct OpenedDevice {
    /// Device handle number.
    id: u64,
    /// Device type.
    device_type: &'static str,
    /// Serial port name.
    port: String,
    /// Baud rate.
    baud_rate: u32,
    /// Low-level device port abstraction.
    handle: Box<dyn BtDevicePort>,
}

impl BtDevice {
    /// Creates a device standard library object.
    pub fn new(_args: Vec<Value>) -> Result<Value, String> {
        Ok(Value::Device(Self {
            kind: BtDeviceKind::Namespace,
        }))
    }

    /// Creates a serial-device object.
    fn serial(port: String, baud_rate: u32, handle: Box<dyn BtDevicePort>) -> Self {
        let device = Rc::new(RefCell::new(OpenedDevice {
            id: NEXT_DEVICE_ID.fetch_add(1, Ordering::Relaxed),
            device_type: "serial",
            port,
            baud_rate,
            handle,
        }));
        register_opened_device(&device);
        Self {
            kind: BtDeviceKind::Serial(device),
        }
    }

    /// Dispatches a device-library or device-handle method.
    pub fn call_method(&self, method: &str, args: Vec<Value>) -> Result<Value, String> {
        match &self.kind {
            BtDeviceKind::Namespace => match method {
                "open" => open(args),
                "scan" => scan(args),
                "list" => list_opened_devices(),
                "exists" => exists(args),
                _ => Err(format!("device has no method `{}`", method)),
            },
            BtDeviceKind::Serial(device) => call_serial_method(device, method, args),
        }
    }
}

impl PartialEq for BtDevice {
    /// Compares device namespaces or handle identity.
    fn eq(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (BtDeviceKind::Namespace, BtDeviceKind::Namespace) => true,
            (BtDeviceKind::Serial(left), BtDeviceKind::Serial(right)) => Rc::ptr_eq(left, right),
            _ => false,
        }
    }
}

impl fmt::Debug for BtDevice {
    /// Outputs device debugging information to avoid expanding the underlying serial port handle.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            BtDeviceKind::Namespace => f.write_str("BtDevice::Namespace"),
            BtDeviceKind::Serial(device) => {
                let device = device.borrow();
                f.debug_struct("BtDevice::Serial")
                    .field("id", &device.id)
                    .field("type", &device.device_type)
                    .field("port", &device.port)
                    .field("baud_rate", &device.baud_rate)
                    .field("closed", &device.handle.is_closed())
                    .finish()
            }
        }
    }
}

/// Opens a device connection.
fn open(args: Vec<Value>) -> Result<Value, String> {
    let config = args
        .first()
        .ok_or_else(|| "device.open() requires the configuration object".to_string())?;
    let device_type = object_string(config, "type", "serial");
    if device_type != "serial" {
        return Err(format!(
            "device.open: unsupported device type `{}`, currently only `serial` is supported",
            device_type
        ));
    }
    let port = object_string(config, "port", "");
    if port.is_empty() {
        return Err("device.open: missing required field `port`".to_string());
    }
    let serial_config = BtSerialConfig {
        port: port.clone(),
        baud_rate: object_u32(config, "baud_rate", 9600)?,
        data_bits: object_u8(config, "data_bits", 8)?,
        stop_bits: object_u8(config, "stop_bits", 1)?,
        parity: object_string(config, "parity", "none"),
        timeout_ms: object_u64(config, "timeout", 1000)?,
    };
    let baud_rate = serial_config.baud_rate;
    let handle = open_serial(serial_config)?;
    Ok(Value::Device(BtDevice::serial(port, baud_rate, handle)))
}

/// Scans system devices.
fn scan(args: Vec<Value>) -> Result<Value, String> {
    let device_type = args
        .first()
        .map(Value::to_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "serial".to_string());
    if device_type != "serial" {
        return Err(format!(
            "device.scan: unsupported device type `{}`, currently only `serial` is supported",
            device_type
        ));
    }
    let ports = scan_serial_ports()?;
    Ok(Value::Array(Rc::new(RefCell::new(
        ports.into_iter().map(serial_port_info_value).collect(),
    ))))
}

/// Determines whether the specified serial port exists in the system.
fn exists(args: Vec<Value>) -> Result<Value, String> {
    let port = args
        .first()
        .map(Value::to_string)
        .filter(|value| !value.is_empty());
    let Some(port) = port else {
        return Ok(Value::Bool(false));
    };
    let exists = scan_serial_ports()?.iter().any(|info| info.port == port);
    Ok(Value::Bool(exists))
}

/// Dispatches a serial-device method.
fn call_serial_method(
    device: &Rc<RefCell<OpenedDevice>>,
    method: &str,
    args: Vec<Value>,
) -> Result<Value, String> {
    let mut device = device.borrow_mut();
    match method {
        "read" => bytes_to_read_value(device.handle.read()?),
        "read_bytes" => bytes::from_vec(device.handle.read()?),
        "read_text" => Ok(String::from_utf8(device.handle.read()?)
            .map(Value::Str)
            .unwrap_or(Value::Null)),
        "write" => {
            let value = args
                .first()
                .ok_or_else(|| "serial.write: missing data".to_string())?;
            let data = bytes::value_to_bytes(value, "serial.write")?;
            device
                .handle
                .write(data.as_ref())
                .map(|count| Value::Int(count as i64))
        }
        "flush" => {
            device.handle.flush()?;
            Ok(Value::Bool(true))
        }
        "close" => {
            device.handle.close()?;
            Ok(Value::Bool(true))
        }
        _ => Err(format!("serial device has no method `{}`", method)),
    }
}

/// Registers an open device.
fn register_opened_device(device: &Rc<RefCell<OpenedDevice>>) {
    OPENED_DEVICES.with(|devices| {
        devices.borrow_mut().push(Rc::downgrade(device));
    });
}

/// Returns devices currently open in this process.
fn list_opened_devices() -> Result<Value, String> {
    OPENED_DEVICES.with(|devices| {
        let mut devices = devices.borrow_mut();
        let mut alive = Vec::with_capacity(devices.len());
        let mut values = Vec::with_capacity(devices.len());
        for weak in devices.iter() {
            let Some(device) = weak.upgrade() else {
                continue;
            };
            values.push(opened_device_value(&device.borrow()));
            alive.push(Rc::downgrade(&device));
        }
        *devices = alive;
        Ok(Value::Array(Rc::new(RefCell::new(values))))
    })
}

/// Converts an opened device to a BT object.
fn opened_device_value(device: &OpenedDevice) -> Value {
    let mut object = IndexMap::new();
    object.insert(
        "type".to_string(),
        Value::Str(device.device_type.to_string()),
    );
    object.insert("port".to_string(), Value::Str(device.port.clone()));
    object.insert("baud_rate".to_string(), Value::Int(device.baud_rate as i64));
    object.insert("closed".to_string(), Value::Bool(device.handle.is_closed()));
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Convert serial port scanning information into BT objects.
fn serial_port_info_value(info: BtSerialPortInfo) -> Value {
    let mut object = IndexMap::new();
    object.insert("type".to_string(), Value::Str("serial".to_string()));
    object.insert("port".to_string(), Value::Str(info.port));
    object.insert("name".to_string(), Value::Str(info.name));
    object.insert("kind".to_string(), Value::Str(info.kind));
    if let Some(vid) = info.vid {
        object.insert("vid".to_string(), Value::Int(vid as i64));
    }
    if let Some(pid) = info.pid {
        object.insert("pid".to_string(), Value::Int(pid as i64));
    }
    object.insert(
        "serial_number".to_string(),
        Value::Str(info.serial_number.unwrap_or_default()),
    );
    object.insert(
        "manufacturer".to_string(),
        Value::Str(info.manufacturer.unwrap_or_default()),
    );
    object.insert(
        "product".to_string(),
        Value::Str(info.product.unwrap_or_default()),
    );
    Value::Object(Rc::new(RefCell::new(object)))
}

/// Convert read bytes to BT return value.
fn bytes_to_read_value(bytes: Vec<u8>) -> Result<Value, String> {
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Value::Str(text)),
        Err(err) => Ok(Value::Array(Rc::new(RefCell::new(
            err.into_bytes()
                .into_iter()
                .map(|byte| Value::Int(byte as i64))
                .collect(),
        )))),
    }
}

/// Read object fields.
fn object_get(value: &Value, key: &str) -> Option<Value> {
    let Value::Object(values) = value else {
        return None;
    };
    values.borrow().get(key).cloned()
}

/// Reads the object string field.
fn object_string(value: &Value, key: &str, default: &str) -> String {
    object_get(value, key)
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Reads the object's unsigned 32-bit integer field.
fn object_u32(value: &Value, key: &str, default: u32) -> Result<u32, String> {
    let number = object_get(value, key);
    match number {
        Some(value) => {
            let value = value.to_i64_lossy();
            if value > 0 && value <= u32::MAX as i64 {
                Ok(value as u32)
            } else {
                Err(format!("device.open: invalid `{}` value `{}`", key, value))
            }
        }
        None => Ok(default),
    }
}

/// Reads the object's unsigned 8-bit integer field.
fn object_u8(value: &Value, key: &str, default: u8) -> Result<u8, String> {
    let number = object_get(value, key);
    match number {
        Some(value) => {
            let value = value.to_i64_lossy();
            if value > 0 && value <= u8::MAX as i64 {
                Ok(value as u8)
            } else {
                Err(format!("device.open: invalid `{}` value `{}`", key, value))
            }
        }
        None => Ok(default),
    }
}

/// Reads the object's unsigned 64-bit integer field.
fn object_u64(value: &Value, key: &str, default: u64) -> Result<u64, String> {
    let number = object_get(value, key);
    match number {
        Some(value) => {
            let value = value.to_i64_lossy();
            if value > 0 {
                Ok(value as u64)
            } else {
                Err(format!("device.open: invalid `{}` value `{}`", key, value))
            }
        }
        None => Ok(default),
    }
}
