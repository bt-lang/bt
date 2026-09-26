//! BT desktop app module.
//!
//! The desktop tools are brought in incrementally by task round: config, Bundle, windowing, protocol, and command bridging.

pub mod api;
pub mod bridge;
pub mod build;
pub mod commands;
pub mod config;
pub mod console;
pub mod dependency;
pub mod dev;
pub mod file_association;
pub mod html;
pub mod icon;
#[cfg(windows)]
pub mod metadata;
pub mod protocol;
pub mod resource;
pub mod runtime;
pub mod server;
#[cfg(any(windows, target_os = "linux"))]
#[cfg_attr(target_os = "linux", path = "single_instance_linux.rs")]
pub mod single_instance;
pub mod starter;
pub mod vm_bridge;
pub mod window;
