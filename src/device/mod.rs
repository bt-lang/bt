//! Internal device abstraction for BT.
//!
//! The standard library accesses concrete devices here so the VM and `libs::device` do not bind
//! directly to third-party serial-port types.

pub mod serial_sync;
pub mod traits;

pub use serial_sync::{open_serial, scan_serial_ports};
pub use traits::{BtDevicePort, BtSerialConfig, BtSerialPortInfo};
