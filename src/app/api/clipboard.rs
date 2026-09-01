//! Desktop API implementation for `bt.clipboard`.

use crate::app::api::map_error;
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

/// Reads plain text from the clipboard.
pub fn read_text(app: AppHandle) -> Result<String, String> {
    match app.clipboard().read_text() {
        Ok(text) => Ok(text),
        Err(err) => {
            let message = err.to_string();
            if message.contains("not available") || message.contains("clipboard is empty") {
                Ok(String::new())
            } else {
                Err(map_error("Read clipboard text", message))
            }
        }
    }
}

/// Writes plain text to the clipboard.
pub fn write_text(app: AppHandle, text: String) -> Result<(), String> {
    app.clipboard()
        .write_text(text)
        .map_err(|err| map_error("Write clipboard text", err))
}

/// Clears the clipboard.
pub fn clear(app: AppHandle) -> Result<(), String> {
    app.clipboard()
        .clear()
        .map_err(|err| map_error("Clear clipboard", err))
}
