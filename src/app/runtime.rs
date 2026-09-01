use crate::app::config::{
    create_default_app_json_for_index, load_app_json_from_path, load_app_json_from_str, AppJson,
    AppMain,
};
use crate::app::resource::ResourceSource;
use crate::app::server::AppServerHandle;
use crate::app::vm_bridge::AppVmHandle;
use crate::bundle::footer::read_bundle_from_exe;
use crate::bundle::package::BtrPackage;
use crate::bundle::vfs::VirtualFileSystem;
use crate::error::BtError;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use tauri::{Manager, WebviewUrl};
use url::Url;

/// Global desktop application state.
///
/// Tauri `State` must be long-lived. Wrapping the runtime in a lock lets development-mode hot
/// reload replace only the inner `AppRuntime` while windows, protocols, and command entry points
/// remain unchanged.
pub struct AppState {
    /// Current runtime shared by protocols, commands, and hot reload.
    pub runtime: Mutex<AppRuntime>,
    /// Whether this process allows development-mode hot reload.
    pub dev_reload: bool,
}

impl AppState {
    /// Creates the global desktop application state.
    pub fn new(runtime: AppRuntime, dev_reload: bool) -> Self {
        Self {
            runtime: Mutex::new(runtime),
            dev_reload,
        }
    }

    /// Locks the current runtime.
    pub fn lock_runtime(&self) -> Result<MutexGuard<'_, AppRuntime>, BtError> {
        self.runtime
            .lock()
            .map_err(|_| BtError::Runtime("desktop runtime state lock is poisoned".to_string()))
    }

    /// Shuts down long-lived resources held by the current runtime.
    pub fn shutdown_runtime(&self) -> Result<(), BtError> {
        let mut runtime = self.lock_runtime()?;
        runtime.shutdown();
        Ok(())
    }
}

/// Shared desktop application runtime state.
#[derive(Debug, Clone)]
pub struct AppRuntime {
    /// Current desktop project root.
    pub project_dir: PathBuf,
    /// Current app source, which defines lifecycle boundaries for file associations, hot reload, and internal startup.
    pub source: AppSource,
    /// App arguments with the bt_app command and BTR path removed.
    pub app_args: Vec<String>,
    /// Parsed and validated app.json configuration.
    pub config: AppJson,
    /// Static resource source: a development directory, BTR, legacy Bundle, or compile-time page.
    pub resource: ResourceSource,
    /// Long-lived BT VM handle for frontend `window.bt.call()` requests.
    pub bt_vm: Option<AppVmHandle>,
    /// Handle to the local web service started in server mode.
    pub server: Option<AppServerHandle>,
    /// Standard error-page HTML returned directly by the protocol layer after startup or hot-reload failure.
    pub startup_error_html: Option<String>,
    /// Original startup or hot-reload error text used in the console summary.
    pub startup_error_message: Option<String>,
}

/// Desktop application startup source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppSource {
    /// App resources embedded at the end of the current executable.
    EmbeddedExe,
    /// Standalone BTR application file specified by the user.
    ExternalBtr,
    /// Development project directory run directly.
    Directory,
    /// Compile-time built-in page, such as the setup or fatal-error page.
    EmbeddedPage,
}

impl AppRuntime {
    /// Releases runtime resources such as the long-lived VM and local service.
    pub fn shutdown(&mut self) {
        if let Some(vm) = self.bt_vm.take() {
            vm.shutdown();
        }
        self.server = None;
    }
}

impl Drop for AppRuntime {
    /// Releases long-lived resources when the runtime is replaced or the app exits.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the BT desktop application.
pub fn start_app(target: Option<PathBuf>, app_args: Vec<String>) -> Result<(), BtError> {
    let (runtime, dev_reload) = match load_initial_runtime(target.as_deref(), app_args) {
        Ok(result) => result,
        Err(err) => {
            let mut runtime = fatal_error_runtime()?;
            attach_runtime_error(&mut runtime, "BT App failed to start", &err.to_string());
            (runtime, false)
        }
    };

    crate::app::console::configure_app_console(runtime.config.dev.console);
    if runtime.source == AppSource::EmbeddedExe {
        crate::app::file_association::register(&runtime.config)?;
    }
    print_start_summary(&runtime);
    let window_url = runtime_window_url(&runtime)?;
    let app_config = runtime.config.clone();
    let title = runtime.config.app.title.clone();
    let state = AppState::new(runtime, dev_reload);

    crate::app::protocol::register_bt_protocol(tauri::Builder::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(crate::app::api::shortcut::dispatch)
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            crate::app::commands::bt_call,
            crate::app::commands::credential_store,
            crate::app::commands::credential_has,
            crate::app::commands::credential_delete,
            crate::app::commands::http_stream_start,
            crate::app::commands::http_stream_cancel,
            crate::app::commands::workspace_open,
            crate::app::commands::workspace_close,
            crate::app::commands::workspace_list,
            crate::app::commands::workspace_read,
            crate::app::commands::workspace_atomic_write,
            crate::app::commands::process_start,
            crate::app::commands::process_status,
            crate::app::commands::process_stop,
            crate::app::commands::process_stop_task,
            crate::app::commands::data_cleanup_prepare,
            crate::app::commands::data_cleanup_confirm,
            crate::app::commands::data_store,
            crate::app::commands::data_load,
            crate::app::commands::window_set_title,
            crate::app::commands::window_set_size,
            crate::app::commands::window_set_position,
            crate::app::commands::window_placement,
            crate::app::commands::window_set_background_color,
            crate::app::commands::window_set_fullscreen,
            crate::app::commands::window_set_resizable,
            crate::app::commands::window_minimize,
            crate::app::commands::window_maximize,
            crate::app::commands::window_restore,
            crate::app::commands::window_close,
            crate::app::commands::window_close_now,
            crate::app::commands::window_hide,
            crate::app::commands::window_show,
            crate::app::commands::window_focus,
            crate::app::commands::window_center,
            crate::app::commands::window_is_fullscreen,
            crate::app::commands::window_is_maximized,
            crate::app::commands::window_is_minimized,
            crate::app::commands::window_is_visible,
            crate::app::commands::window_set_always_on_top,
            crate::app::commands::window_is_always_on_top,
            crate::app::commands::window_set_decorations,
            crate::app::commands::window_set_skip_taskbar,
            crate::app::commands::window_set_close_mode,
            crate::app::commands::window_set_close_intercept,
            crate::app::commands::window_drag,
            crate::app::commands::window_start_resize,
            crate::app::commands::window_flash,
            crate::app::commands::window_open_devtools,
            crate::app::commands::dialog_open_file,
            crate::app::commands::dialog_open_files,
            crate::app::commands::dialog_open_dir,
            crate::app::commands::dialog_save_file,
            crate::app::commands::dialog_message,
            crate::app::commands::dialog_confirm,
            crate::app::commands::tray_enable,
            crate::app::commands::tray_disable,
            crate::app::commands::tray_set_icon,
            crate::app::commands::tray_set_tooltip,
            crate::app::commands::tray_set_menu,
            crate::app::commands::clipboard_read_text,
            crate::app::commands::clipboard_write_text,
            crate::app::commands::clipboard_clear,
            crate::app::commands::screen_pick_color,
            crate::app::commands::screen_capture_area,
            crate::app::commands::shortcut_register,
            crate::app::commands::shortcut_unregister,
            crate::app::commands::shortcut_unregister_all,
            crate::app::commands::screen_overlay_sample,
            crate::app::commands::screen_overlay_pick,
            crate::app::commands::screen_overlay_capture,
            crate::app::commands::screen_overlay_cancel,
            crate::app::commands::notify_permission_state,
            crate::app::commands::notify_request_permission,
            crate::app::commands::notify_show,
            crate::app::commands::app_version,
            crate::app::commands::app_info,
            crate::app::commands::app_run,
            crate::app::commands::app_engine_version,
            crate::app::commands::app_platform,
            crate::app::commands::app_open_url,
            crate::app::commands::app_open_path,
            crate::app::commands::app_reveal_path,
            crate::app::commands::app_quit,
            crate::app::commands::app_args,
            crate::app::commands::app_documents_dir,
            crate::app::commands::app_watch_path,
            crate::app::commands::app_unwatch_path,
            crate::app::commands::project_create,
            crate::app::commands::project_restart
        ])
        .manage(crate::app::api::ApiState::new())
        .manage(crate::app::api::production::ProductionState::new())
        .manage(crate::app::api::screen::ScreenState::new())
        .manage(crate::app::api::shortcut::ShortcutState::new())
        .manage(state)
        .setup(move |app| {
            let dev_reload_enabled = app.state::<AppState>().dev_reload;
            if let Err(err) = crate::app::window::create_main_window(
                app,
                &title,
                &app_config,
                window_url.clone(),
                dev_reload_enabled,
            ) {
                if crate::app::dependency::show_friendly_startup_error(&err) {
                    std::process::exit(1);
                }
                return Err(err.into());
            }
            if dev_reload_enabled {
                crate::app::dev::start_dev_watcher(app.handle().clone());
            }
            Ok(())
        })
        .run(tauri::generate_context!("src-tauri/tauri.conf.json"))
        .map_err(|err| BtError::Tauri(err.to_string()))
}

/// Loads the initial runtime from the current executable and working directory.
fn load_initial_runtime(
    target: Option<&Path>,
    app_args: Vec<String>,
) -> Result<(AppRuntime, bool), BtError> {
    if let Some(target) = target {
        let target = target.canonicalize().map_err(|err| {
            BtError::Config(format!(
                "cannot access run target `{}`: {}",
                target.display(),
                err
            ))
        })?;
        if target.is_file() {
            let package = BtrPackage::open(&target)?;
            let project_dir = target
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let config = package.config().clone();
            let runtime = finish_runtime(resource_runtime(
                project_dir,
                config,
                ResourceSource::Btr(package),
                AppSource::ExternalBtr,
                app_args,
            ));
            return Ok((runtime, false));
        }
        if target.is_dir() {
            let mut runtime = load_dev_runtime_from_app_json(&target);
            runtime.app_args = app_args;
            return Ok((runtime, true));
        }
        return Err(BtError::Config(format!(
            "run target is neither a BTR file nor a project directory: {}",
            target.display()
        )));
    }

    let current_exe = env::current_exe()?;
    if let Some(bytes) = read_bundle_from_exe(&current_exe)? {
        let project_dir = current_exe
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let (config, resource) = if BtrPackage::has_zip_header(&bytes) {
            let package = BtrPackage::from_bytes(bytes, current_exe.clone())?;
            (package.config().clone(), ResourceSource::Btr(package))
        } else {
            let vfs = VirtualFileSystem::from_bundle(&bytes)?;
            let config = if vfs.exists("app.json") {
                let raw = vfs.read_string("app.json")?;
                load_app_json_from_str(&raw)?
            } else {
                return Err(BtError::Bundle(
                    "Bundle is missing app.json; desktop project cannot start".to_string(),
                ));
            };
            (config, ResourceSource::Bundle(vfs))
        };
        let runtime = finish_runtime(resource_runtime(
            project_dir,
            config,
            resource,
            AppSource::EmbeddedExe,
            app_args,
        ));
        return Ok((runtime, false));
    }

    let project_dir = env::current_dir()?;
    let app_json_path = project_dir.join("app.json");
    if app_json_path.is_file() {
        let mut runtime = load_dev_runtime_from_app_json(&project_dir);
        runtime.app_args = app_args;
        return Ok((runtime, true));
    }

    let index_html_path = project_dir.join("index.html");
    if index_html_path.is_file() {
        let mut runtime = match create_default_app_json_for_index(&project_dir) {
            Ok(config) => finish_runtime(resource_runtime(
                project_dir.clone(),
                config,
                ResourceSource::Directory(project_dir),
                AppSource::Directory,
                Vec::new(),
            )),
            Err(err) => dev_error_runtime(project_dir, err.to_string()),
        };
        runtime.app_args = app_args;
        return Ok((runtime, true));
    }

    let mut runtime = starter_runtime(project_dir);
    runtime.app_args = app_args;
    Ok((runtime, false))
}

/// Reloads a development-directory runtime without opening the setup page or creating a default app.json.
///
/// When `app.json` is deleted or malformed, the error-page runtime retains the last valid development
/// and window settings so a hot-reload error does not unexpectedly open the console or resize the window.
pub(crate) fn load_dev_runtime_for_reload(
    project_dir: &Path,
    previous_config: &AppJson,
) -> AppRuntime {
    let app_json_path = project_dir.join("app.json");
    if !app_json_path.is_file() {
        return reload_error_runtime(
            project_dir.to_path_buf(),
            format!("app.json does not exist: {}", app_json_path.display()),
            previous_config,
        );
    }
    load_dev_runtime_from_app_json_with_fallback(project_dir, Some(previous_config))
}

/// Builds a runtime from a development directory's app.json.
fn load_dev_runtime_from_app_json(project_dir: &Path) -> AppRuntime {
    load_dev_runtime_from_app_json_with_fallback(project_dir, None)
}

/// Builds a runtime from a development directory's app.json, retaining previous development settings after a hot-reload parse failure.
fn load_dev_runtime_from_app_json_with_fallback(
    project_dir: &Path,
    previous_config: Option<&AppJson>,
) -> AppRuntime {
    match load_app_json_from_path(&project_dir.join("app.json")) {
        Ok(config) => finish_runtime(resource_runtime(
            project_dir.to_path_buf(),
            config,
            ResourceSource::Directory(project_dir.to_path_buf()),
            AppSource::Directory,
            Vec::new(),
        )),
        Err(err) => match previous_config {
            Some(config) => {
                reload_error_runtime(project_dir.to_path_buf(), err.to_string(), config)
            }
            None => dev_error_runtime(project_dir.to_path_buf(), err.to_string()),
        },
    }
}

/// Builds the runtime skeleton for a regular directory or Bundle.
fn resource_runtime(
    project_dir: PathBuf,
    config: AppJson,
    resource: ResourceSource,
    source: AppSource,
    app_args: Vec<String>,
) -> AppRuntime {
    AppRuntime {
        project_dir,
        source,
        app_args,
        config,
        resource,
        bt_vm: None,
        server: None,
        startup_error_html: None,
        startup_error_message: None,
    }
}

/// Builds a development-mode error runtime.
fn dev_error_runtime(project_dir: PathBuf, message: String) -> AppRuntime {
    let mut runtime = resource_runtime(
        project_dir.clone(),
        error_config(),
        ResourceSource::Directory(project_dir),
        AppSource::Directory,
        Vec::new(),
    );
    attach_runtime_error(&mut runtime, "BT App failed to start", &message);
    runtime
}

/// Builds a hot-reload error runtime while retaining the last valid window and development settings.
fn reload_error_runtime(
    project_dir: PathBuf,
    message: String,
    previous_config: &AppJson,
) -> AppRuntime {
    let mut runtime = dev_error_runtime(project_dir, message);
    runtime.config.window = previous_config.window.clone();
    runtime.config.dev = previous_config.dev.clone();
    runtime
}

/// Builds the minimal runtime used for fatal startup errors.
fn fatal_error_runtime() -> Result<AppRuntime, BtError> {
    Ok(resource_runtime(
        env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        error_config(),
        ResourceSource::Embedded(&[]),
        AppSource::EmbeddedPage,
        Vec::new(),
    ))
}

/// Builds the setup-page runtime.
fn starter_runtime(project_dir: PathBuf) -> AppRuntime {
    AppRuntime {
        project_dir,
        source: AppSource::EmbeddedPage,
        app_args: Vec::new(),
        config: crate::app::starter::default_starter_config(),
        resource: ResourceSource::Embedded(crate::app::starter::STARTER_RESOURCES),
        bt_vm: None,
        server: None,
        startup_error_html: None,
        startup_error_message: None,
    }
}

/// Builds the base error-page configuration.
fn error_config() -> AppJson {
    let mut config = AppJson::default();
    config.app.title = "BT App Error".to_string();
    config.app.entry = "__bt_error__/error.html".to_string();
    config.app.main = AppMain::Disabled;
    config.dev.console = true;
    config
}

/// Initializes server.bt and the long-lived VM, then validates the window entry point.
fn finish_runtime(mut runtime: AppRuntime) -> AppRuntime {
    let mut startup_error = None;
    if runtime.resource.exists("server.bt") {
        match crate::app::server::start_app_server(&runtime) {
            Ok(server) => {
                runtime.server = Some(server);
            }
            Err(err) => {
                startup_error = Some(err.to_string());
            }
        }
    }

    if startup_error.is_none() {
        match resolve_main_script(&runtime) {
            Ok(main) => match crate::app::vm_bridge::start_app_vm(
                &runtime.resource,
                &runtime.project_dir,
                main.as_deref(),
            ) {
                Ok(vm) => runtime.bt_vm = vm,
                Err(err) => startup_error = Some(err.to_string()),
            },
            Err(err) => startup_error = Some(err.to_string()),
        }
    }

    if startup_error.is_none() {
        if let Err(err) = resolve_runtime_external_url(&runtime) {
            startup_error = Some(err.to_string());
        }
    }

    if let Some(message) = startup_error {
        attach_runtime_error(&mut runtime, "BT App failed to start", &message);
    }
    runtime
}

/// Attaches error details to the runtime for the bt:// protocol's standard error page.
fn attach_runtime_error(runtime: &mut AppRuntime, title: &str, message: &str) {
    runtime.startup_error_html = Some(crate::app::html::render_error_html(title, message));
    runtime.startup_error_message = Some(message.to_string());
}

/// Resolves the URL to which the current runtime should navigate.
pub(crate) fn runtime_navigation_url(runtime: &AppRuntime) -> Result<Url, BtError> {
    if runtime.startup_error_html.is_some() {
        return crate::app::html::error_entry_url_value();
    }
    resolve_runtime_external_url(runtime)
}

/// Resolves the window URL the current runtime should load.
fn runtime_window_url(runtime: &AppRuntime) -> Result<WebviewUrl, BtError> {
    if runtime.startup_error_html.is_some() {
        return crate::app::html::error_entry_url();
    }
    runtime_navigation_url(runtime).map(WebviewUrl::External)
}

/// Resolves the app.main script to run.
fn resolve_main_script(runtime: &AppRuntime) -> Result<Option<String>, BtError> {
    match &runtime.config.app.main {
        AppMain::Auto => {
            if runtime.resource.exists("main.bt") {
                Ok(Some("main.bt".to_string()))
            } else {
                Ok(None)
            }
        }
        AppMain::Disabled => Ok(None),
        AppMain::File(path) => {
            if runtime.resource.exists(path) {
                Ok(Some(path.clone()))
            } else {
                Err(BtError::Config(format!(
                    "app.main file does not exist: {}",
                    path
                )))
            }
        }
    }
}

/// Resolves the window URL from runtime state and reports a missing static entry point.
fn resolve_runtime_external_url(runtime: &AppRuntime) -> Result<Url, BtError> {
    if runtime.config.app.mode == "server" {
        let entry = runtime
            .server
            .as_ref()
            .and_then(|server| server.url())
            .unwrap_or(runtime.config.app.entry.as_str());
        return Url::parse(entry)
            .map_err(|err| BtError::Config(format!("Invalid server-mode URL: {}", err)));
    }

    if runtime.config.app.mode == "static" {
        let entry = runtime.config.app.entry.trim_start_matches('/');
        if !runtime.resource.exists(entry) {
            return Err(BtError::Config(format!(
                "entry resource does not exist: {}",
                entry
            )));
        }
    }

    crate::app::window::resolve_window_url(&runtime.config).and_then(webview_url_to_url)
}

/// Extracts an external URL from WebviewUrl.
fn webview_url_to_url(url: WebviewUrl) -> Result<Url, BtError> {
    match url {
        WebviewUrl::External(url) => Ok(url),
        _ => Err(BtError::Config("Window URL is not external".to_string())),
    }
}

/// Prints key startup settings for command-line development and debugging.
fn print_start_summary(runtime: &AppRuntime) {
    if runtime.startup_error_message.is_some() {
        println!("Startup error page opened");
    } else if matches!(&runtime.resource, ResourceSource::Embedded(_)) {
        println!("The current directory is missing app.json and index.html; setup page opened");
    } else {
        println!("Desktop project configuration loaded successfully");
    }
    println!("Application name: {}", runtime.config.app.name);
    println!("Window title: {}", runtime.config.app.title);
    println!("Run mode: {}", runtime.config.app.mode);
    println!("Entry file: {}", runtime.config.app.entry);
    println!(
        "Window size: {}x{}",
        runtime.config.window.width, runtime.config.window.height
    );
    println!(
        "Resource source: {}",
        match runtime.source {
            AppSource::Directory => "development directory",
            AppSource::ExternalBtr => "external BTR",
            AppSource::EmbeddedExe => "BTR/Bundle embedded in executable",
            AppSource::EmbeddedPage => "built-in page",
        }
    );
    println!("BT main script: {}", main_script_display(runtime));
    if let Some(server) = &runtime.server {
        if let Some(url) = server.url() {
            println!("Local service: {}", url);
        } else {
            println!("Local service: server.bt executed");
        }
    }
    if let Some(message) = &runtime.startup_error_message {
        println!("Startup error: {}", message);
    }
}

/// Returns the main-script description shown in the console.
fn main_script_display(runtime: &AppRuntime) -> String {
    match &runtime.config.app.main {
        AppMain::Auto => {
            if runtime.resource.exists("main.bt") {
                "automatic main.bt".to_string()
            } else {
                "automatic (main.bt not found)".to_string()
            }
        }
        AppMain::Disabled => "not executed".to_string(),
        AppMain::File(path) => path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A hot-reload error runtime retains the last valid window and development settings.
    #[test]
    fn reload_error_runtime_preserves_previous_dev_and_window_config() {
        let dir = fresh_temp_dir("reload-error-config");
        let mut previous = AppJson::default();
        previous.window.width = 960;
        previous.window.height = 640;
        previous.dev.console = false;
        previous.dev.devtools = true;

        let runtime = load_dev_runtime_for_reload(&dir, &previous);

        assert!(runtime.startup_error_message.is_some());
        assert_eq!(runtime.config.window.width, 960);
        assert_eq!(runtime.config.window.height, 640);
        assert!(!runtime.config.dev.console);
        assert!(runtime.config.dev.devtools);

        let _ = fs::remove_dir_all(dir);
    }

    /// Creates a unique test directory.
    fn fresh_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "bt-app-runtime-test-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
