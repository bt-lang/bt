//! BT internal device traits and data models.

/// BT internal device port abstraction.
///
/// The initial backend uses synchronous serial ports and can later be replaced by an asynchronous implementation without changing the BT language API.
pub trait BtDevicePort: Send {
    /// Read a segment of bytes from the device.
    fn read(&mut self) -> Result<Vec<u8>, String>;

    /// Writes bytes to the device and returns the number of bytes written successfully.
    fn write(&mut self, data: &[u8]) -> Result<usize, String>;

    /// Flushes the device output buffer.
    fn flush(&mut self) -> Result<(), String>;

    /// Closes the device connection.
    fn close(&mut self) -> Result<(), String>;

    /// Determines whether the device connection has been closed.
    fn is_closed(&self) -> bool;
}

/// Serial port open configuration.
pub struct BtSerialConfig {
    /// Serial port name, such as `COM3` or `/dev/ttyUSB0`.
    pub port: String,
    /// Baud rate.
    pub baud_rate: u32,
    /// Data bits.
    pub data_bits: u8,
    /// Stop bits.
    pub stop_bits: u8,
    /// Parity mode.
    pub parity: String,
    /// Read timeout, in milliseconds.
    pub timeout_ms: u64,
}

/// System serial port information.
pub struct BtSerialPortInfo {
    /// Serial port name.
    pub port: String,
    /// Friendly name.
    pub name: String,
    /// Serial port source type.
    pub kind: String,
    /// USB vendor ID.
    pub vid: Option<u16>,
    /// USB product ID.
    pub pid: Option<u16>,
    /// USB serial number.
    pub serial_number: Option<String>,
    /// USB manufacturer name.
    pub manufacturer: Option<String>,
    /// USB product name.
    pub product: Option<String>,
}
