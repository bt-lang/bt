//! Standard-library entry point for the BT VM.
//!
//! Libraries are exposed explicitly here, following a one-library-per-file layout. Legacy libraries that depend on the old asynchronous `Env` model
//! must not be wired directly into the register VM. New modules should be declared here and routed from the VM's constructor dispatch.

/// Base64 encoding and decoding standard library.
pub mod base64;
/// BT runtime repository.
pub mod bt;
/// Binary byte standard library.
pub mod bytes;
/// Cryptodigest standard library.
pub mod crypto;
/// Date and time standard library.
pub mod date;
/// Device communication standard library.
pub mod device;
/// Native dynamic library FFI standard library.
#[cfg(feature = "ffi")]
pub mod ffi;
/// File system standard library.
pub mod fs;
/// HTML text processing standard library.
pub mod html;
/// Standard library for mathematical calculations.
pub mod math;
/// MD5 digest standard library.
pub mod md5;
/// Modbus RTU/TCP protocol auxiliary library.
pub mod modbus;
/// MySQL database standard library.
pub mod mysql;
/// Network and service monitoring standard library.
pub mod net;
/// Path text standard library.
pub mod path;
/// Process management standard library.
pub mod process;
/// HTTP request standard library.
pub mod reqwest;
/// Standard library of stateless system functions.
pub mod system;
/// URL text standard library.
pub mod url;
/// Web response control status.
pub mod web;
