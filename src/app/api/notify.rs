//! Desktop API implementation for `bt.notify`.

use crate::app::api::map_error;
use serde::Deserialize;
use tauri::plugin::PermissionState;
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// Notification display options.
#[derive(Debug, Default, Deserialize)]
pub struct NotifyShowOptions {
    /// Notification title; the application title is used when empty.
    #[serde(default)]
    pub title: String,
    /// Notification body.
    #[serde(default)]
    pub body: String,
}

/// Returns the notification permission status.
pub fn permission_state(app: AppHandle) -> Result<String, String> {
    app.notification()
        .permission_state()
        .map(permission_state_text)
        .map_err(|err| map_error("Read notification permission", err))
}

/// Requests notification permission.
pub fn request_permission(app: AppHandle) -> Result<String, String> {
    app.notification()
        .request_permission()
        .map(permission_state_text)
        .map_err(|err| map_error("Request notification permission", err))
}

/// Shows a system notification.
pub fn show(app: AppHandle, options: Option<NotifyShowOptions>) -> Result<(), String> {
    let options = options.unwrap_or_default();
    let title = if options.title.trim().is_empty() {
        app_title(&app)?
    } else {
        options.title
    };
    show_system_notification(&app, title, options.body)
}

/// Shows a system notification through the Tauri notification plugin.
#[cfg(not(windows))]
fn show_system_notification(app: &AppHandle, title: String, body: String) -> Result<(), String> {
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|err| map_error("Show system notification", err))
}

/// Shows a system notification using portable Windows toast mode.
///
/// Outside Cargo output directories, `tauri-plugin-notification` uses the application identifier
/// as its `AppUserModelID`. Because bt_app ships as a portable single-file executable without an
/// installer-created Start menu shortcut, Windows may accept the call without displaying a toast.
/// Use notify-rust's portable fallback AppID and return underlying errors synchronously so the
/// frontend cannot report success when the system provided no feedback.
#[cfg(windows)]
fn show_system_notification(app: &AppHandle, title: String, body: String) -> Result<(), String> {
    let app_name = app_title(app)?;
    let mut notification = notify_rust::Notification::new();
    notification.summary(&title);
    notification.body(&body);
    notification.appname(&app_name);
    notification.auto_icon();
    notification
        .show()
        .map(|_| ())
        .map_err(|err| map_error("Show system notification", err))
}

/// Converts a permission status to the string defined by the BT API.
fn permission_state_text(state: PermissionState) -> String {
    match state {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::Prompt | PermissionState::PromptWithRationale => "prompt",
    }
    .to_string()
}

/// Reads the application title.
fn app_title(app: &AppHandle) -> Result<String, String> {
    let state = app.state::<crate::app::runtime::AppState>();
    let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
    Ok(runtime.config.app.title.clone())
}
