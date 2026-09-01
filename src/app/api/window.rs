//! Desktop API implementation for `bt.window`.

use crate::app::api::{
    map_error, required_text, ApiState, CloseMode, WINDOW_CLOSE_REQUESTED_EVENT,
};
use serde::Serialize;
use tauri::{
    window::Color, AppHandle, LogicalPosition, LogicalSize, Manager, UserAttentionType,
    WebviewWindow, WindowEvent,
};

/// Available work area of the monitor containing the current window, in logical pixels.
#[derive(Clone, Debug, Serialize)]
pub struct WindowWorkArea {
    /// X coordinate of the work area's top-left corner; may be negative with multiple monitors.
    pub x: i32,
    /// Y coordinate of the work area's top-left corner; may be negative with multiple monitors.
    pub y: i32,
    /// Work area width.
    pub width: u32,
    /// Work area height.
    pub height: u32,
}

/// Current WebView content rectangle on the desktop, in logical pixels.
#[derive(Clone, Debug, Serialize)]
pub struct WindowContentArea {
    /// X coordinate of the content area's top-left corner; may be negative with multiple monitors.
    pub x: i32,
    /// Y coordinate of the content area's top-left corner; may be negative with multiple monitors.
    pub y: i32,
    /// Content area width.
    pub width: u32,
    /// Content area height.
    pub height: u32,
}

/// Logical-pixel layout of the current window and its monitor.
#[derive(Clone, Debug, Serialize)]
pub struct WindowPlacement {
    /// X coordinate of the window frame's top-left corner; may be negative with multiple monitors.
    pub x: i32,
    /// Y coordinate of the window frame's top-left corner; may be negative with multiple monitors.
    pub y: i32,
    /// Window frame width.
    pub width: u32,
    /// Window frame height.
    pub height: u32,
    /// Scale factor from logical to physical pixels on the current monitor.
    pub scale_factor: f64,
    /// WebView content area excluding system borders and shadows.
    pub content_area: WindowContentArea,
    /// Current monitor's available work area excluding the taskbar and other system regions.
    pub work_area: WindowWorkArea,
}

/// Sets the window title.
pub fn set_title(window: WebviewWindow, title: String) -> Result<(), String> {
    let title = required_text(title, "window title")?;
    window
        .set_title(&title)
        .map_err(|err| map_error("Set window title", err))
}

/// Sets the window size.
pub fn set_size(window: WebviewWindow, width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("Window width and height must be greater than 0".to_string());
    }
    window
        .set_size(LogicalSize::new(width as f64, height as f64))
        .map_err(|err| map_error("Set window size", err))
}

/// Sets the window frame's top-left position in logical pixels, allowing negative multi-monitor coordinates.
pub fn set_position(window: WebviewWindow, x: i32, y: i32) -> Result<(), String> {
    window
        .set_position(LogicalPosition::new(x as f64, y as f64))
        .map_err(|err| map_error("Set window position", err))
}

/// Reads the logical-pixel layout of the window frame and current monitor's work area.
pub fn placement(window: WebviewWindow) -> Result<WindowPlacement, String> {
    let scale_factor = window
        .scale_factor()
        .map_err(|err| map_error("Read window scale factor", err))?;
    let position = window
        .outer_position()
        .map_err(|err| map_error("Read window position", err))?
        .to_logical::<f64>(scale_factor);
    let size = window
        .outer_size()
        .map_err(|err| map_error("Read window size", err))?
        .to_logical::<f64>(scale_factor);
    let content_position = window
        .inner_position()
        .map_err(|err| map_error("Read window content position", err))?
        .to_logical::<f64>(scale_factor);
    let content_size = window
        .inner_size()
        .map_err(|err| map_error("Read window content size", err))?
        .to_logical::<f64>(scale_factor);
    let monitor = window
        .current_monitor()
        .map_err(|err| map_error("Read current monitor", err))?
        .ok_or_else(|| "No monitor is available for the current window".to_string())?;
    let work_area = monitor.work_area();
    let work_position = work_area.position.to_logical::<f64>(scale_factor);
    let work_size = work_area.size.to_logical::<f64>(scale_factor);

    Ok(WindowPlacement {
        x: position.x.round() as i32,
        y: position.y.round() as i32,
        width: size.width.round().max(0.0) as u32,
        height: size.height.round().max(0.0) as u32,
        scale_factor,
        content_area: WindowContentArea {
            x: content_position.x.round() as i32,
            y: content_position.y.round() as i32,
            width: content_size.width.round().max(0.0) as u32,
            height: content_size.height.round().max(0.0) as u32,
        },
        work_area: WindowWorkArea {
            x: work_position.x.round() as i32,
            y: work_position.y.round() as i32,
            width: work_size.width.round().max(0.0) as u32,
            height: work_size.height.round().max(0.0) as u32,
        },
    })
}

/// Sets the window and WebView background color.
pub fn set_background_color(window: WebviewWindow, color: String) -> Result<(), String> {
    let color = parse_hex_color(&color)?;
    window
        .set_background_color(Some(color))
        .map_err(|err| map_error("Set window background color", err))
}

/// Sets whether the window is resizable.
pub fn set_resizable(window: WebviewWindow, resizable: bool) -> Result<(), String> {
    window
        .set_resizable(resizable)
        .map_err(|err| map_error("Set window resizable state", err))
}

/// Minimizes the window.
pub fn minimize(window: WebviewWindow) -> Result<(), String> {
    window
        .minimize()
        .map_err(|err| map_error("Minimize window", err))
}

/// Maximizes the window.
pub fn maximize(window: WebviewWindow) -> Result<(), String> {
    window
        .maximize()
        .map_err(|err| map_error("Maximize window", err))
}

/// Restores the window and attempts to focus it.
pub fn restore(window: WebviewWindow) -> Result<(), String> {
    window.show().map_err(|err| map_error("Show window", err))?;
    window
        .unminimize()
        .map_err(|err| map_error("Unminimize window", err))?;
    window
        .unmaximize()
        .map_err(|err| map_error("Unmaximize window", err))?;
    window
        .set_focus()
        .map_err(|err| map_error("Focus window", err))
}

/// Closes the window.
pub fn close(window: WebviewWindow) -> Result<(), String> {
    window.close().map_err(|err| map_error("Close window", err))
}

/// Hides the window.
pub fn hide(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|err| map_error("Hide window", err))
}

/// Shows the window.
pub fn show(window: WebviewWindow) -> Result<(), String> {
    window.show().map_err(|err| map_error("Show window", err))
}

/// Focuses the window.
pub fn focus(window: WebviewWindow) -> Result<(), String> {
    window
        .set_focus()
        .map_err(|err| map_error("Focus window", err))
}

/// Centers the window.
pub fn center(window: WebviewWindow) -> Result<(), String> {
    window
        .center()
        .map_err(|err| map_error("Center window", err))
}

/// Sets the window's fullscreen state.
pub fn set_fullscreen(window: WebviewWindow, fullscreen: bool) -> Result<(), String> {
    window
        .set_fullscreen(fullscreen)
        .map_err(|err| map_error("Set window fullscreen state", err))
}

/// Returns whether the window is fullscreen.
pub fn is_fullscreen(window: WebviewWindow) -> Result<bool, String> {
    window
        .is_fullscreen()
        .map_err(|err| map_error("Read window fullscreen state", err))
}

/// Returns whether the window is maximized.
pub fn is_maximized(window: WebviewWindow) -> Result<bool, String> {
    window
        .is_maximized()
        .map_err(|err| map_error("Read window maximized state", err))
}

/// Returns whether the window is minimized.
pub fn is_minimized(window: WebviewWindow) -> Result<bool, String> {
    window
        .is_minimized()
        .map_err(|err| map_error("Read window minimized state", err))
}

/// Returns whether the window is visible.
pub fn is_visible(window: WebviewWindow) -> Result<bool, String> {
    window
        .is_visible()
        .map_err(|err| map_error("Read window visibility state", err))
}

/// Sets whether the window stays on top.
pub fn set_always_on_top(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    window
        .set_always_on_top(enabled)
        .map_err(|err| map_error("Set window always-on-top state", err))
}

/// Returns whether the window stays on top.
pub fn is_always_on_top(window: WebviewWindow) -> Result<bool, String> {
    window
        .is_always_on_top()
        .map_err(|err| map_error("Read window always-on-top state", err))
}

/// Sets whether the system title bar and borders are visible.
pub fn set_decorations(window: WebviewWindow, visible: bool) -> Result<(), String> {
    window
        .set_decorations(visible)
        .map_err(|err| map_error("Set window decoration state", err))
}

/// Sets whether the window is omitted from the taskbar.
pub fn set_skip_taskbar(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    window
        .set_skip_taskbar(enabled)
        .map_err(|err| map_error("Set window taskbar state", err))
}

/// Sets the behavior of the window's close button.
pub fn set_close_mode(app: AppHandle, state: &ApiState, mode: String) -> Result<(), String> {
    let close_mode = parse_close_mode(&mode)?;
    if close_mode == CloseMode::Tray {
        crate::app::api::tray::ensure_default_tray(&app, state)?;
    }
    state.set_close_mode(close_mode)
}

/// Starts a custom title-bar drag and waits for the primary mouse button to be released on Windows.
///
/// The native Windows drag API returns immediately after posting the start message. Waiting for
/// button release ensures callers read the final window position after `await`. Asynchronous polling
/// is limited to 60 seconds and does not block the Tauri event thread.
pub async fn drag(window: WebviewWindow) -> Result<(), String> {
    window
        .start_dragging()
        .map_err(|err| map_error("Start window drag", err))?;
    wait_for_primary_button_release().await;
    Ok(())
}

/// Waits asynchronously for the primary mouse button to be released on Windows, with a bounded task lifetime.
#[cfg(target_os = "windows")]
async fn wait_for_primary_button_release() {
    use tokio::time::{sleep, Duration, Instant};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let pressed = unsafe { GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000 != 0 };
        if !pressed {
            break;
        }
        sleep(Duration::from_millis(16)).await;
    }
}

/// Preserves the native drag API's completion timing on non-Windows platforms.
#[cfg(not(target_os = "windows"))]
async fn wait_for_primary_button_release() {}

/// Starts a custom window resize.
pub fn start_resize(window: WebviewWindow, edge: String) -> Result<(), String> {
    let label = window.label().to_string();
    let native_window = window
        .app_handle()
        .get_window(&label)
        .ok_or_else(|| "The current window handle does not exist".to_string())?;
    native_window
        .start_resize_dragging(parse_resize_direction(&edge)?)
        .map_err(|err| map_error("Start window resize", err))
}

/// Requests system attention for the window.
pub fn flash(window: WebviewWindow) -> Result<(), String> {
    window
        .request_user_attention(Some(UserAttentionType::Informational))
        .map_err(|err| map_error("Request window attention", err))
}

/// Opens the WebView developer tools for the current window.
pub fn open_devtools(window: WebviewWindow) {
    window.open_devtools();
}

/// Registers the close-mode handler for the main window.
pub fn attach_close_handler(window: &WebviewWindow) {
    let window_handle = window.clone();
    let app = window.app_handle().clone();
    window.on_window_event(move |event| {
        let WindowEvent::CloseRequested { api, .. } = event else {
            return;
        };
        let state = app.state::<ApiState>();
        let mode = state.close_mode().unwrap_or(CloseMode::Exit);
        match mode {
            CloseMode::Exit => {
                if !state.take_close_allowance().unwrap_or(false)
                    && state.close_intercept().unwrap_or(false)
                {
                    api.prevent_close();
                    dispatch_close_requested(&window_handle);
                    return;
                }
                let app_state = app.state::<crate::app::runtime::AppState>();
                let _ = app_state.shutdown_runtime();
            }
            CloseMode::Hide => {
                api.prevent_close();
                let _ = window_handle.hide();
            }
            CloseMode::Tray => {
                api.prevent_close();
                let _ = crate::app::api::tray::ensure_default_tray(&app, &state);
                let _ = window_handle.hide();
            }
        }
    });
}

/// Dispatches a close request to the page, which should call `close_now()` after asynchronous cleanup.
fn dispatch_close_requested(window: &WebviewWindow) {
    let Ok(event) = serde_json::to_string(WINDOW_CLOSE_REQUESTED_EVENT) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent({}, {{ detail: {{}} }}));",
        event
    ));
}

/// Parses the close mode.
fn parse_close_mode(mode: &str) -> Result<CloseMode, String> {
    match mode.trim() {
        "exit" => Ok(CloseMode::Exit),
        "hide" => Ok(CloseMode::Hide),
        "tray" => Ok(CloseMode::Tray),
        other => Err(format!("Unsupported close mode: {}", other)),
    }
}

/// Parses the window resize direction.
fn parse_resize_direction(edge: &str) -> Result<tauri_runtime::ResizeDirection, String> {
    match edge.trim() {
        "top" => Ok(tauri_runtime::ResizeDirection::North),
        "bottom" => Ok(tauri_runtime::ResizeDirection::South),
        "left" => Ok(tauri_runtime::ResizeDirection::West),
        "right" => Ok(tauri_runtime::ResizeDirection::East),
        "top_left" => Ok(tauri_runtime::ResizeDirection::NorthWest),
        "top_right" => Ok(tauri_runtime::ResizeDirection::NorthEast),
        "bottom_left" => Ok(tauri_runtime::ResizeDirection::SouthWest),
        "bottom_right" => Ok(tauri_runtime::ResizeDirection::SouthEast),
        other => Err(format!("Unsupported resize direction: {}", other)),
    }
}

/// Parses a `#RRGGBB` window color supplied by the page.
fn parse_hex_color(value: &str) -> Result<Color, String> {
    let value = value.trim();
    if value.len() != 7 || !value.starts_with('#') {
        return Err("Window background color must use #RRGGBB format".to_string());
    }
    let parse_component = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&value[range], 16)
            .map_err(|_| "Window background color must use #RRGGBB format".to_string())
    };
    Ok(Color(
        parse_component(1..3)?,
        parse_component(3..5)?,
        parse_component(5..7)?,
        255,
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_hex_color;
    use tauri::window::Color;

    /// Window background colors accept mixed-case hexadecimal and preserve every RGB component.
    #[test]
    fn parses_window_background_color() {
        assert_eq!(
            parse_hex_color("#1eA2F0").unwrap(),
            Color(30, 162, 240, 255)
        );
    }

    /// Window background colors reject missing hashes, shorthand forms, and non-hexadecimal characters.
    #[test]
    fn rejects_invalid_window_background_color() {
        assert!(parse_hex_color("1e1e1e").is_err());
        assert!(parse_hex_color("#fff").is_err());
        assert!(parse_hex_color("#gggggg").is_err());
    }
}
