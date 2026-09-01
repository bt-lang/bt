use std::fmt;

/// BT engineering unified error type.
///
/// Purpose: Allow CLI, App, Bundle, and Tauri commands to use the same error format.
#[derive(Debug)]
pub enum BtError {
    Compile(String),
    Runtime(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Config(String),
    Bundle(String),
    #[allow(dead_code)]
    Desktop(String),
    WebView(String),
    Tauri(String),
}

impl fmt::Display for BtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BtError::Compile(msg) => write!(f, "Compilation error: {}", msg),
            BtError::Runtime(msg) => write!(f, "Runtime error: {}", msg),
            BtError::Io(err) => write!(f, "I/O error: {}", err),
            BtError::Json(err) => write!(f, "JSON parse error: {}", err),
            BtError::Config(msg) => write!(f, "Configuration error: {}", msg),
            BtError::Bundle(msg) => write!(f, "Bundle error: {}", msg),
            BtError::Desktop(msg) => write!(f, "Desktop runtime error: {}", msg),
            BtError::WebView(msg) => write!(f, "WebView error: {}", msg),
            BtError::Tauri(msg) => write!(f, "Tauri error: {}", msg),
        }
    }
}

impl std::error::Error for BtError {}

impl From<std::io::Error> for BtError {
    fn from(value: std::io::Error) -> Self {
        BtError::Io(value)
    }
}

impl From<serde_json::Error> for BtError {
    fn from(value: serde_json::Error) -> Self {
        BtError::Json(value)
    }
}
