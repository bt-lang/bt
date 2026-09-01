use crate::error::BtError;

const WEBVIEW2_MESSAGE: &str =
    "Microsoft Edge WebView2 Runtime is missing from this system, so the BT desktop app cannot open a window.\n\n\
Install Microsoft Edge WebView2 Runtime and reopen this app.\n\n\
Download:\n\
https://developer.microsoft.com/microsoft-edge/webview2/";

/// Provide a dependency-missing friendly message for desktop startup errors.
///
/// Returns `true` when a user-facing error has already been emitted, so the caller does not need to print the raw error again.
pub fn show_friendly_startup_error(err: &BtError) -> bool {
    if let Some(friendly) = friendly_startup_message(err) {
        eprintln!("{}", friendly);
        show_message_box(&friendly);
        return true;
    }
    false
}

/// Build a friendly message for a missing desktop dependency.
fn friendly_startup_message(err: &BtError) -> Option<String> {
    if !cfg!(target_os = "windows") {
        return None;
    }

    let msg = err.to_string();
    if looks_like_webview2_error(&msg) {
        return Some(format!("{}\n\nOriginal error:\n{}", WEBVIEW2_MESSAGE, msg));
    }

    None
}

/// Determine whether an error message looks like a WebView2 or WebView initialization failure.
fn looks_like_webview2_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("webview2")
        || lower.contains("webview")
        || lower.contains("webview runtime")
        || lower.contains("webviewruntimenotinstalled")
        || lower.contains("runtime not installed")
        || lower.contains("edge")
        || lower.contains("hresult")
        || lower.contains("0x800")
}

/// On Windows, try to show a system message box; other platforms keep console output only.
#[cfg(windows)]
fn show_message_box(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let title = to_wide("BT desktop app startup failed");
    let message = to_wide(message);
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Do not show a system message box on non-Windows platforms.
#[cfg(not(windows))]
fn show_message_box(_message: &str) {}

/// Convert a string to the NUL-terminated UTF-16 form required by the Windows API.
#[cfg(windows)]
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the actual Tauri message for a missing WebView Runtime hits the friendly fallback.
    #[test]
    fn detects_tauri_webview_runtime_missing_message() {
        assert!(looks_like_webview2_error(
            "runtime error: Could not find the webview runtime, make sure it is installed"
        ));
    }

    /// Verify HRESULT-style WebView initialization failures hit the friendly fallback.
    #[test]
    fn detects_hresult_style_webview_error() {
        assert!(looks_like_webview2_error(
            "failed to create webview: HRESULT 0x80040154"
        ));
    }

    /// Verify a normal configuration error is not misclassified as a missing WebView2 runtime.
    #[test]
    fn ignores_unrelated_config_error() {
        assert!(!looks_like_webview2_error(
            "Config error: the current directory is missing app.json"
        ));
    }
}
