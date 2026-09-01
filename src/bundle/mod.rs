//! Resource container support for BT desktop applications.
//!
//! BTR files can be distributed independently or appended to an executable. Legacy uncompressed
//! bundle footers and VFS reads remain supported. A unified runtime resource source avoids
//! temporary extraction in static mode and keeps real disk paths private.

pub mod builder;
pub mod footer;
pub mod package;
pub mod reader;
pub mod vfs;
