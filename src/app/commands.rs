use crate::app::api::{self, ApiState};
use crate::app::resource::ResourceSource;
use crate::app::runtime::{AppRuntime, AppState};
use crate::app::starter::CreateProjectInput;
use crate::permission::{self, Capability};
use serde_json::{json, Value as JsonValue};
use std::path::PathBuf;
use tauri::{AppHandle, State, WebviewWindow};

/// Call a BT function registered in `main.bt`.
#[tauri::command]
pub fn bt_call(
    state: State<AppState>,
    window: WebviewWindow,
    name: String,
    args: JsonValue,
) -> Result<JsonValue, String> {
    let vm = {
        let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
        runtime
            .bt_vm
            .as_ref()
            .cloned()
            .ok_or_else(|| "The current application has no app.main configured, so BT functions cannot be called".to_string())?
    };
    let result = vm.call(name.clone(), args);
    api::production::dispatch_backend_event(&window, &name, result.is_ok());
    result
}

/// Save an API key in a plaintext JSON file under the current user's hidden application data directory.
#[tauri::command]
pub fn credential_store(
    app_state: State<AppState>,
    credential_id: String,
    secret: String,
) -> Result<(), String> {
    require_desktop_permission()?;
    let app_name = current_app_identity(&app_state)?;
    api::production::store_credential(&app_name, &credential_id, secret)
}

/// Check whether an application credential JSON file exists and is valid without returning its contents.
#[tauri::command]
pub fn credential_has(app_state: State<AppState>, credential_id: String) -> Result<bool, String> {
    require_desktop_permission()?;
    let app_name = current_app_identity(&app_state)?;
    api::production::has_credential(&app_name, &credential_id)
}

/// Delete an application credential JSON file.
#[tauri::command]
pub fn credential_delete(
    app_state: State<AppState>,
    credential_id: String,
) -> Result<bool, String> {
    require_desktop_permission()?;
    let app_name = current_app_identity(&app_state)?;
    api::production::delete_credential(&app_name, &credential_id)
}

/// Start a bounded streaming HTTP request.
#[tauri::command]
pub fn http_stream_start(
    app_state: State<AppState>,
    production: State<api::production::ProductionState>,
    window: WebviewWindow,
    options: api::production::HttpStreamOptions,
) -> Result<api::production::HttpStreamStart, String> {
    require_desktop_permission()?;
    let app_name = current_app_identity(&app_state)?;
    production.start_stream(window, &app_name, options)
}

/// Cancel a streaming HTTP request and block subsequent events.
#[tauri::command]
pub fn http_stream_cancel(
    production: State<api::production::ProductionState>,
    window: WebviewWindow,
    request_id: String,
) -> Result<bool, String> {
    require_desktop_permission()?;
    production.cancel_stream(&window, &request_id)
}

/// Register a normalized workspace root.
#[tauri::command]
pub fn workspace_open(
    production: State<api::production::ProductionState>,
    root: String,
) -> Result<api::production::WorkspaceOpenResult, String> {
    require_desktop_permission()?;
    production.open_workspace(root)
}

/// Close a workspace registration without deleting files.
#[tauri::command]
pub fn workspace_close(
    production: State<api::production::ProductionState>,
    workspace_id: String,
) -> Result<bool, String> {
    require_desktop_permission()?;
    production.close_workspace(&workspace_id)
}

/// List a workspace directory with bounded output.
#[tauri::command]
pub fn workspace_list(
    production: State<api::production::ProductionState>,
    workspace_id: String,
    relative: String,
    recursive: bool,
) -> Result<Vec<api::production::WorkspaceEntry>, String> {
    require_desktop_permission()?;
    production.list_workspace(&workspace_id, &relative, recursive)
}

/// Read a UTF-8 workspace file and its SHA-256 digest.
#[tauri::command]
pub fn workspace_read(
    production: State<api::production::ProductionState>,
    workspace_id: String,
    relative: String,
    max_bytes: usize,
) -> Result<api::production::WorkspaceReadResult, String> {
    require_desktop_permission()?;
    production.read_workspace(&workspace_id, &relative, max_bytes)
}

/// Atomically write within a workspace, guarded by a digest.
#[tauri::command]
pub fn workspace_atomic_write(
    production: State<api::production::ProductionState>,
    workspace_id: String,
    relative: String,
    content: String,
    expected_sha256: Option<String>,
) -> Result<api::production::WorkspaceWriteResult, String> {
    require_desktop_permission()?;
    production.atomic_write_workspace(&workspace_id, &relative, content, expected_sha256)
}

/// Start a bounded native process without invoking a shell.
#[tauri::command]
pub fn process_start(
    production: State<api::production::ProductionState>,
    window: WebviewWindow,
    options: api::production::ProcessStartOptions,
) -> Result<api::production::ProcessSnapshot, String> {
    require_desktop_permission()?;
    production.start_process(window, options)
}

/// Query and verify a native process identity.
#[tauri::command]
pub fn process_status(
    production: State<api::production::ProductionState>,
    process_id: String,
    task_id: String,
    identity: String,
) -> Result<api::production::ProcessSnapshot, String> {
    require_desktop_permission()?;
    production.process_status(&process_id, &task_id, &identity)
}

/// Stop a specific native process tree.
#[tauri::command]
pub fn process_stop(
    production: State<api::production::ProductionState>,
    window: WebviewWindow,
    process_id: String,
    task_id: String,
    identity: String,
) -> Result<api::production::ProcessSnapshot, String> {
    require_desktop_permission()?;
    production.stop_process(&window, &process_id, &task_id, &identity)
}

/// Stop only the native processes registered to a specific task.
#[tauri::command]
pub fn process_stop_task(
    production: State<api::production::ProductionState>,
    window: WebviewWindow,
    task_id: String,
) -> Result<Vec<api::production::ProcessSnapshot>, String> {
    require_desktop_permission()?;
    production.stop_task_processes(&window, &task_id)
}

/// Generate an application-owned data cleanup preview and confirmation token.
#[tauri::command]
pub fn data_cleanup_prepare(
    app_state: State<AppState>,
    production: State<api::production::ProductionState>,
) -> Result<api::production::CleanupPreview, String> {
    require_desktop_permission()?;
    production.prepare_cleanup(&current_app_identity(&app_state)?)
}

/// Clean up application-owned data using a single-use token.
#[tauri::command]
pub fn data_cleanup_confirm(
    app_state: State<AppState>,
    production: State<api::production::ProductionState>,
    confirm_token: String,
) -> Result<api::production::CleanupResult, String> {
    require_desktop_permission()?;
    production.confirm_cleanup(&current_app_identity(&app_state)?, &confirm_token)
}

/// Atomically save bounded JSON application state.
#[tauri::command]
pub fn data_store(
    app_state: State<AppState>,
    production: State<api::production::ProductionState>,
    key: String,
    value: JsonValue,
) -> Result<(), String> {
    require_desktop_permission()?;
    production.store_data(&current_app_identity(&app_state)?, &key, value)
}

/// Read bounded JSON application state.
#[tauri::command]
pub fn data_load(
    app_state: State<AppState>,
    production: State<api::production::ProductionState>,
    key: String,
) -> Result<Option<JsonValue>, String> {
    require_desktop_permission()?;
    production.load_data(&current_app_identity(&app_state)?, &key)
}

/// Set the current window title.
#[tauri::command]
pub fn window_set_title(window: WebviewWindow, title: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_title(window, title)
}

/// Set the current window size.
#[tauri::command]
pub fn window_set_size(window: WebviewWindow, width: u32, height: u32) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_size(window, width, height)
}

/// Set the top-left position of the current window frame in logical pixels.
#[tauri::command]
pub fn window_set_position(window: WebviewWindow, x: i32, y: i32) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_position(window, x, y)
}

/// Read the current window and monitor work-area layout in logical pixels.
#[tauri::command]
pub fn window_placement(window: WebviewWindow) -> Result<api::window::WindowPlacement, String> {
    require_desktop_permission()?;
    api::window::placement(window)
}

/// Set the current window and WebView background color.
#[tauri::command]
pub fn window_set_background_color(window: WebviewWindow, color: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_background_color(window, color)
}

/// Set whether the current window is resizable.
#[tauri::command]
pub fn window_set_resizable(window: WebviewWindow, resizable: bool) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_resizable(window, resizable)
}

/// Minimize the current window.
#[tauri::command]
pub fn window_minimize(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::minimize(window)
}

/// Maximize the current window.
#[tauri::command]
pub fn window_maximize(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::maximize(window)
}

/// Restore the current window.
#[tauri::command]
pub fn window_restore(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::restore(window)
}

/// Close the current window.
#[tauri::command]
pub fn window_close(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::close(window)
}

/// Allow and retrigger window closing after the page finishes asynchronous cleanup.
#[tauri::command]
pub fn window_close_now(window: WebviewWindow, state: State<ApiState>) -> Result<(), String> {
    require_desktop_permission()?;
    state.allow_close_once()?;
    api::window::close(window)
}

/// Hide the current window.
#[tauri::command]
pub fn window_hide(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::hide(window)
}

/// Show the current window.
#[tauri::command]
pub fn window_show(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::show(window)
}

/// Focus the current window.
#[tauri::command]
pub fn window_focus(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::focus(window)
}

/// Center the current window.
#[tauri::command]
pub fn window_center(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::center(window)
}

/// Set whether the current window is fullscreen.
#[tauri::command]
pub fn window_set_fullscreen(window: WebviewWindow, fullscreen: bool) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_fullscreen(window, fullscreen)
}

/// Return whether the current window is fullscreen.
#[tauri::command]
pub fn window_is_fullscreen(window: WebviewWindow) -> Result<bool, String> {
    require_desktop_permission()?;
    api::window::is_fullscreen(window)
}

/// Return whether the current window is maximized.
#[tauri::command]
pub fn window_is_maximized(window: WebviewWindow) -> Result<bool, String> {
    require_desktop_permission()?;
    api::window::is_maximized(window)
}

/// Return whether the current window is minimized.
#[tauri::command]
pub fn window_is_minimized(window: WebviewWindow) -> Result<bool, String> {
    require_desktop_permission()?;
    api::window::is_minimized(window)
}

/// Return whether the current window is visible.
#[tauri::command]
pub fn window_is_visible(window: WebviewWindow) -> Result<bool, String> {
    require_desktop_permission()?;
    api::window::is_visible(window)
}

/// Set whether the current window is always on top.
#[tauri::command]
pub fn window_set_always_on_top(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_always_on_top(window, enabled)
}

/// Return whether the current window is always on top.
#[tauri::command]
pub fn window_is_always_on_top(window: WebviewWindow) -> Result<bool, String> {
    require_desktop_permission()?;
    api::window::is_always_on_top(window)
}

/// Set whether the current window displays system decorations.
#[tauri::command]
pub fn window_set_decorations(window: WebviewWindow, visible: bool) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_decorations(window, visible)
}

/// Set whether the current window is omitted from the taskbar.
#[tauri::command]
pub fn window_set_skip_taskbar(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_skip_taskbar(window, enabled)
}

/// Set the behavior of the current window's close button.
#[tauri::command]
pub fn window_set_close_mode(
    app: AppHandle,
    state: State<ApiState>,
    mode: String,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::set_close_mode(app, &state, mode)
}

/// Set whether the page intercepts close requests to await snapshots or other asynchronous cleanup.
#[tauri::command]
pub fn window_set_close_intercept(state: State<ApiState>, enabled: bool) -> Result<(), String> {
    require_desktop_permission()?;
    state.set_close_intercept(enabled)
}

/// Start dragging the current window.
#[tauri::command]
pub async fn window_drag(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::drag(window).await
}

/// Start resizing the current window.
#[tauri::command]
pub fn window_start_resize(window: WebviewWindow, edge: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::start_resize(window, edge)
}

/// Ask the system to draw the user's attention to the current window.
#[tauri::command]
pub fn window_flash(window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    api::window::flash(window)
}

/// Open developer tools for the current window.
#[tauri::command]
pub fn window_open_devtools(state: State<AppState>, window: WebviewWindow) -> Result<(), String> {
    require_desktop_permission()?;
    {
        let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
        if !runtime.config.dev.devtools {
            return Err("dev.devtools is disabled; developer tools cannot be opened".to_string());
        }
    }
    api::window::open_devtools(window);
    Ok(())
}

/// Select one file.
#[tauri::command]
pub fn dialog_open_file(
    app: AppHandle,
    options: Option<api::dialog::FileDialogOptions>,
) -> Result<Option<String>, String> {
    require_desktop_permission()?;
    api::dialog::open_file(app, options)
}

/// Select multiple files.
#[tauri::command]
pub fn dialog_open_files(
    app: AppHandle,
    options: Option<api::dialog::FileDialogOptions>,
) -> Result<Vec<String>, String> {
    require_desktop_permission()?;
    api::dialog::open_files(app, options)
}

/// Select one directory.
#[tauri::command]
pub fn dialog_open_dir(
    app: AppHandle,
    options: Option<api::dialog::FileDialogOptions>,
) -> Result<Option<String>, String> {
    require_desktop_permission()?;
    api::dialog::open_dir(app, options)
}

/// Select a path for saving a file.
#[tauri::command]
pub fn dialog_save_file(
    app: AppHandle,
    options: Option<api::dialog::FileDialogOptions>,
) -> Result<Option<String>, String> {
    require_desktop_permission()?;
    api::dialog::save_file(app, options)
}

/// Display a system message dialog.
#[tauri::command]
pub fn dialog_message(
    app: AppHandle,
    message: String,
    options: Option<api::dialog::MessageDialogOptions>,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::dialog::message(app, message, options)
}

/// Display a system confirmation dialog.
#[tauri::command]
pub fn dialog_confirm(
    app: AppHandle,
    message: String,
    options: Option<api::dialog::MessageDialogOptions>,
) -> Result<bool, String> {
    require_desktop_permission()?;
    api::dialog::confirm(app, message, options)
}

/// Enable the system tray.
#[tauri::command]
pub fn tray_enable(
    app: AppHandle,
    state: State<ApiState>,
    options: Option<api::tray::TrayEnableOptions>,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::tray::enable(app, &state, options)
}

/// Disable the system tray.
#[tauri::command]
pub fn tray_disable(app: AppHandle, state: State<ApiState>) -> Result<(), String> {
    require_desktop_permission()?;
    api::tray::disable(app, &state)
}

/// Set the tray icon.
#[tauri::command]
pub fn tray_set_icon(app: AppHandle, state: State<ApiState>, icon: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::tray::set_icon(app, &state, icon)
}

/// Set the tray tooltip.
#[tauri::command]
pub fn tray_set_tooltip(state: State<ApiState>, text: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::tray::set_tooltip(&state, text)
}

/// Set the tray menu.
#[tauri::command]
pub fn tray_set_menu(
    app: AppHandle,
    state: State<ApiState>,
    menu: Vec<api::tray::TrayMenuItem>,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::tray::set_menu(app, &state, menu)
}

/// Read clipboard text.
#[tauri::command]
pub fn clipboard_read_text(app: AppHandle) -> Result<String, String> {
    require_desktop_permission()?;
    api::clipboard::read_text(app)
}

/// Write clipboard text.
#[tauri::command]
pub fn clipboard_write_text(app: AppHandle, text: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::clipboard::write_text(app, text)
}

/// Clear the clipboard.
#[tauri::command]
pub fn clipboard_clear(app: AppHandle) -> Result<(), String> {
    require_desktop_permission()?;
    api::clipboard::clear(app)
}

/// Open the fullscreen color picker and return the selected color, or `null` if canceled.
#[tauri::command]
pub async fn screen_pick_color(
    app: AppHandle,
    state: State<'_, api::screen::ScreenState>,
    options: Option<api::screen::PickColorOptions>,
) -> Result<Option<api::screen::ScreenColor>, String> {
    require_screen_permission()?;
    api::screen::pick_color(app, &state, options).await
}

/// Open the fullscreen area-selection overlay and copy the capture to the system image clipboard, or return `null` if canceled.
#[tauri::command]
pub async fn screen_capture_area(
    app: AppHandle,
    state: State<'_, api::screen::ScreenState>,
    options: Option<api::screen::CaptureAreaOptions>,
) -> Result<Option<api::screen::ScreenCaptureResult>, String> {
    require_screen_permission()?;
    api::screen::capture_area(app, &state, options).await
}

/// Register or replace a global shortcut.
#[tauri::command]
pub fn shortcut_register(
    app: AppHandle,
    state: State<api::shortcut::ShortcutState>,
    shortcut_id: String,
    accelerator: String,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::shortcut::register(app, &state, shortcut_id, accelerator)
}

/// Unregister the global shortcut with the specified ID.
#[tauri::command]
pub fn shortcut_unregister(
    app: AppHandle,
    state: State<api::shortcut::ShortcutState>,
    shortcut_id: String,
) -> Result<bool, String> {
    require_desktop_permission()?;
    api::shortcut::unregister(app, &state, shortcut_id)
}

/// Unregister every global shortcut registered by the current application through the BT bridge.
#[tauri::command]
pub fn shortcut_unregister_all(
    app: AppHandle,
    state: State<api::shortcut::ShortcutState>,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::shortcut::unregister_all(app, &state)
}

/// Return the frozen pixel color at the current pointer position in the color-picker overlay.
#[tauri::command]
pub fn screen_overlay_sample(
    state: State<api::screen::ScreenState>,
    session_id: u64,
    monitor_index: usize,
    x: u32,
    y: u32,
) -> Result<api::screen::ScreenColor, String> {
    permission::check(Capability::Screen)?;
    api::screen::overlay_sample(&state, session_id, monitor_index, x, y)
}

/// Complete pixel selection in the color-picker overlay.
#[tauri::command]
pub fn screen_overlay_pick(
    state: State<api::screen::ScreenState>,
    session_id: u64,
    monitor_index: usize,
    x: u32,
    y: u32,
) -> Result<(), String> {
    permission::check(Capability::Screen)?;
    api::screen::overlay_pick(&state, session_id, monitor_index, x, y)
}

/// Complete a single-monitor rectangular selection in the capture overlay.
#[tauri::command]
pub fn screen_overlay_capture(
    state: State<api::screen::ScreenState>,
    session_id: u64,
    monitor_index: usize,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    permission::check(Capability::Screen)?;
    api::screen::overlay_capture(&state, session_id, monitor_index, x, y, width, height)
}

/// Cancel a color-picker or capture-overlay session.
#[tauri::command]
pub fn screen_overlay_cancel(
    state: State<api::screen::ScreenState>,
    session_id: u64,
) -> Result<bool, String> {
    permission::check(Capability::Screen)?;
    api::screen::overlay_cancel(&state, session_id)
}

/// Read the notification permission state.
#[tauri::command]
pub fn notify_permission_state(app: AppHandle) -> Result<String, String> {
    require_desktop_permission()?;
    api::notify::permission_state(app)
}

/// Request notification permission.
#[tauri::command]
pub fn notify_request_permission(app: AppHandle) -> Result<String, String> {
    require_desktop_permission()?;
    api::notify::request_permission(app)
}

/// Display a system notification.
#[tauri::command]
pub fn notify_show(
    app: AppHandle,
    options: Option<api::notify::NotifyShowOptions>,
) -> Result<(), String> {
    require_desktop_permission()?;
    api::notify::show(app, options)
}

/// Return the application version.
#[tauri::command]
pub fn app_version(state: State<AppState>) -> Result<String, String> {
    require_desktop_permission()?;
    api::app::version(&state)
}

/// Read BTR application metadata and its default icon without executing application code.
#[tauri::command]
pub fn app_info(path: String) -> Result<api::app::BtrAppInfo, String> {
    require_desktop_permission()?;
    api::app::info(path)
}

/// Launch an independent BTR application process with the current bt_app runtime.
#[tauri::command]
pub fn app_run(path: String, args: Vec<String>) -> Result<api::app::BtrRunResult, String> {
    require_desktop_permission()?;
    api::app::run(path, args)
}

/// Return the bt_app engine version.
#[tauri::command]
pub fn app_engine_version() -> Result<String, String> {
    require_desktop_permission()?;
    Ok(api::app::engine_version())
}

/// Return the current system platform.
#[tauri::command]
pub fn app_platform() -> Result<String, String> {
    require_desktop_permission()?;
    Ok(api::app::platform())
}

/// Open a URL in the system default browser.
#[tauri::command]
pub fn app_open_url(app: AppHandle, url: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::app::open_url(app, url)
}

/// Open a file or directory with the system default application.
#[tauri::command]
pub fn app_open_path(app: AppHandle, path: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::app::open_path(app, path)
}

/// Reveal a file or directory in the system file manager.
#[tauri::command]
pub fn app_reveal_path(app: AppHandle, path: String) -> Result<(), String> {
    require_desktop_permission()?;
    api::app::reveal_path(app, path)
}

/// Exit the entire program.
#[tauri::command]
pub fn app_quit(app: AppHandle) -> Result<(), String> {
    require_desktop_permission()?;
    api::app::quit(app)
}

/// Return the launch arguments.
#[tauri::command]
pub fn app_args(state: State<AppState>) -> Result<Vec<String>, String> {
    require_desktop_permission()?;
    api::app::args(&state)
}

/// Return the user's Documents known folder as resolved by the operating system.
#[tauri::command]
pub fn app_documents_dir(app: AppHandle) -> Result<String, String> {
    require_desktop_permission()?;
    api::app::documents_dir(&app)
}

/// Start watching a specified directory for file changes.
#[tauri::command]
pub fn app_watch_path(
    window: WebviewWindow,
    state: State<ApiState>,
    watch_id: String,
    path: String,
    recursive: bool,
) -> Result<(), String> {
    require_desktop_permission()?;
    let watcher =
        api::watch::PathWatcher::new(window, watch_id.clone(), PathBuf::from(path), recursive)?;
    state.set_watch(watch_id, watcher)
}

/// Stop the specified file watcher, or all watchers for the current window if no ID is provided.
#[tauri::command]
pub fn app_unwatch_path(state: State<ApiState>, watch_id: Option<String>) -> Result<(), String> {
    require_desktop_permission()?;
    let _ = state.take_watch(watch_id.as_deref())?;
    Ok(())
}

/// Create a new BT desktop project in the current directory.
#[tauri::command]
pub fn project_create(state: State<AppState>, input: CreateProjectInput) -> JsonValue {
    if let Err(message) = require_desktop_permission() {
        return project_error(message);
    }
    let runtime = match state.lock_runtime() {
        Ok(runtime) => runtime,
        Err(err) => return project_error(err.to_string()),
    };
    if let Err(message) = ensure_starter_runtime(&runtime) {
        return project_error(message);
    }

    match crate::app::starter::create_project(&runtime.project_dir, input) {
        Ok(result) => json!({
            "error": false,
            "message": "Created successfully",
            "data": {
                "files": result.files
            }
        }),
        Err(err) => project_error(err.to_string()),
    }
}

/// Restart the current bt_app program.
#[tauri::command]
pub fn project_restart() -> Result<(), String> {
    require_desktop_permission()?;
    crate::app::starter::restart_current_app().map_err(|err| err.to_string())
}

/// Check whether the current configuration permits desktop bridge API calls.
fn require_desktop_permission() -> Result<(), String> {
    permission::check(Capability::Desktop)
}

/// Check whether the current configuration permits both the desktop bridge and sensitive screen capture.
fn require_screen_permission() -> Result<(), String> {
    require_desktop_permission()?;
    permission::check(Capability::Screen)
}

/// Return a persistent application identity key compatible with legacy projects while isolating explicit app.id values.
fn current_app_identity(state: &State<AppState>) -> Result<String, String> {
    state
        .lock_runtime()
        .map(|runtime| runtime.config.app.identity_key().to_string())
        .map_err(|err| err.to_string())
}

/// Build a consistent JSON error response for project setup commands.
fn project_error(message: impl Into<String>) -> JsonValue {
    json!({
        "error": true,
        "message": message.into(),
        "data": null
    })
}

/// Restrict the project creation command to the built-in setup page shown when app.json is missing.
fn ensure_starter_runtime(runtime: &AppRuntime) -> Result<(), String> {
    if matches!(&runtime.resource, ResourceSource::Embedded(resources)
        if resources.iter().any(|resource| resource.path == crate::app::starter::STARTER_ENTRY))
    {
        Ok(())
    } else {
        Err("The project creation command is only available on the setup page".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Desktop commands are rejected when desktop permission is disabled.
    #[test]
    fn desktop_permission_denies_desktop_command() {
        crate::permission::with_test_config(None, Some("desktop"), || {
            let err = app_engine_version().unwrap_err();
            assert!(err.contains("desktop"));
            assert!(err.contains("Permission denied"));
        });
    }

    /// Public screen-capture APIs are rejected when screen permission is disabled.
    #[test]
    fn screen_permission_denies_screen_command() {
        crate::permission::with_test_config(None, Some("screen"), || {
            let err = require_screen_permission().unwrap_err();
            assert!(err.contains("screen"));
            assert!(err.contains("Permission denied"));
        });
    }
}
