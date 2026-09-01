//! Synchronous serial-port implementation based on `serialport`.
//!
//! This is the initial blocking I/O backend; the standard library and VM never touch `serialport` types directly.

use crate::device::traits::{BtDevicePort, BtSerialConfig, BtSerialPortInfo};
use serialport::{DataBits, Parity, SerialPort, SerialPortInfo, SerialPortType, StopBits};
use std::io::ErrorKind;
use std::time::Duration;

/// Synchronous serial port.
pub struct SyncSerialPort {
    /// Serial port name, used to generate clear error messages.
    port_name: String,
    /// Underlying serial-port handle; clearing it closes the port and releases system resources.
    port: Option<Box<dyn SerialPort>>,
}

impl SyncSerialPort {
    /// Opens the synchronous serial port.
    fn open(config: BtSerialConfig) -> Result<Self, String> {
        let data_bits = parse_data_bits(config.data_bits)?;
        let stop_bits = parse_stop_bits(config.stop_bits)?;
        let parity = parse_parity(&config.parity)?;
        let port = serialport::new(&config.port, config.baud_rate)
            .data_bits(data_bits)
            .stop_bits(stop_bits)
            .parity(parity)
            .timeout(Duration::from_millis(config.timeout_ms))
            .open()
            .map_err(|err| {
                format!(
                    "device.open: failed to open serial port `{}`: {}",
                    config.port, err
                )
            })?;
        Ok(Self {
            port_name: config.port,
            port: Some(port),
        })
    }

    /// Reads the available underlying serial port handle.
    fn port_mut(&mut self, operation: &str) -> Result<&mut dyn SerialPort, String> {
        match self.port.as_deref_mut() {
            Some(port) => Ok(port),
            None => Err(format!(
                "serial.{}: serial port `{}` is already closed",
                operation, self.port_name
            )),
        }
    }
}

impl BtDevicePort for SyncSerialPort {
    /// Reads the currently available bytes from the serial port.
    fn read(&mut self) -> Result<Vec<u8>, String> {
        let port_name = self.port_name.clone();
        let port = self.port_mut("read")?;
        let size = port
            .bytes_to_read()
            .ok()
            .filter(|size| *size > 0)
            .map(|size| size.min(64 * 1024) as usize)
            .unwrap_or(1024);
        let mut buffer = vec![0; size];
        match port.read(&mut buffer) {
            Ok(count) => {
                buffer.truncate(count);
                Ok(buffer)
            }
            Err(err) if err.kind() == ErrorKind::TimedOut => Ok(Vec::new()),
            Err(err) => Err(format!(
                "serial.read: failed to read from `{}`: {}",
                port_name, err
            )),
        }
    }

    /// Writes bytes to the serial port.
    fn write(&mut self, data: &[u8]) -> Result<usize, String> {
        let port_name = self.port_name.clone();
        self.port_mut("write")?
            .write(data)
            .map_err(|err| format!("serial.write: failed to write to `{}`: {}", port_name, err))
    }

    /// Flushes the serial-port output buffer.
    fn flush(&mut self) -> Result<(), String> {
        let port_name = self.port_name.clone();
        self.port_mut("flush")?
            .flush()
            .map_err(|err| format!("serial.flush: failed to flush `{}`: {}", port_name, err))
    }

    /// Closes the serial port and releases the underlying handle.
    fn close(&mut self) -> Result<(), String> {
        self.port = None;
        Ok(())
    }

    /// Determines whether the serial port has been closed.
    fn is_closed(&self) -> bool {
        self.port.is_none()
    }
}

/// Scans the system for serial ports.
pub fn scan_serial_ports() -> Result<Vec<BtSerialPortInfo>, String> {
    serialport::available_ports()
        .map_err(|err| format!("device.scan: failed to scan serial ports: {}", err))
        .map(|ports| ports.into_iter().map(convert_port_info).collect())
}

/// Opens a synchronous serial port and returns the BT device port abstraction.
pub fn open_serial(config: BtSerialConfig) -> Result<Box<dyn BtDevicePort>, String> {
    SyncSerialPort::open(config).map(|port| Box::new(port) as Box<dyn BtDevicePort>)
}

/// Converts third-party serial-port information to BT's internal data model.
fn convert_port_info(info: SerialPortInfo) -> BtSerialPortInfo {
    match info.port_type {
        SerialPortType::UsbPort(usb) => {
            let name = usb
                .product
                .clone()
                .or_else(|| usb.manufacturer.clone())
                .unwrap_or_else(|| info.port_name.clone());
            BtSerialPortInfo {
                port: info.port_name,
                name,
                kind: "usb".to_string(),
                vid: Some(usb.vid),
                pid: Some(usb.pid),
                serial_number: usb.serial_number,
                manufacturer: usb.manufacturer,
                product: usb.product,
            }
        }
        SerialPortType::BluetoothPort => simple_port_info(info.port_name, "bluetooth"),
        SerialPortType::PciPort => simple_port_info(info.port_name, "pci"),
        SerialPortType::Unknown => simple_port_info(info.port_name, "unknown"),
    }
}

/// Constructs non-USB serial port information.
fn simple_port_info(port: String, kind: &str) -> BtSerialPortInfo {
    BtSerialPortInfo {
        name: port.clone(),
        port,
        kind: kind.to_string(),
        vid: None,
        pid: None,
        serial_number: None,
        manufacturer: None,
        product: None,
    }
}

/// Parses the data-bit configuration.
fn parse_data_bits(value: u8) -> Result<DataBits, String> {
    match value {
        5 => Ok(DataBits::Five),
        6 => Ok(DataBits::Six),
        7 => Ok(DataBits::Seven),
        8 => Ok(DataBits::Eight),
        _ => Err(format!(
            "device.open: unsupported serial data_bits `{}`",
            value
        )),
    }
}

/// Parses the stop-bit configuration.
fn parse_stop_bits(value: u8) -> Result<StopBits, String> {
    match value {
        1 => Ok(StopBits::One),
        2 => Ok(StopBits::Two),
        _ => Err(format!(
            "device.open: unsupported serial stop_bits `{}`",
            value
        )),
    }
}

/// Parses the parity configuration.
fn parse_parity(value: &str) -> Result<Parity, String> {
    match value.to_ascii_lowercase().as_str() {
        "none" | "" => Ok(Parity::None),
        "odd" => Ok(Parity::Odd),
        "even" => Ok(Parity::Even),
        _ => Err(format!(
            "device.open: unsupported serial parity `{}`",
            value
        )),
    }
}
