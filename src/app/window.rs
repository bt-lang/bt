use crate::app::config::AppJson;
use crate::error::BtError;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{
    webview::PageLoadEvent, window::Color, App, Manager, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder,
};
use url::Url;

/// WebView data directory used by the main window and restricted child windows.
pub(crate) struct WebviewStorageState {
    /// `None` uses Tauri's global default; any other value is the main window's explicit directory.
    pub(crate) path: Option<PathBuf>,
}

/// Creates the main window and applies its app.json appearance after the first page loads.
pub fn create_main_window(
    app: &mut App,
    title: &str,
    config: &AppJson,
    url: WebviewUrl,
    dev_refresh_shortcuts: bool,
) -> Result<(), BtError> {
    let icon = {
        let state = app.state::<crate::app::runtime::AppState>();
        let runtime = state.lock_runtime()?;
        crate::app::icon::load_window_icon(&runtime.resource, config)?
    };
    let storage_dir = webview_storage_dir(app, config)?;
    app.manage(WebviewStorageState {
        path: storage_dir.clone(),
    });
    let configured_decorations = !config.window.hide_titlebar;
    let configured_shadow = native_shadow(config);
    let show_startup_loading = !config.window.transparent;
    let startup_loading_shown = AtomicBool::new(false);
    let startup_finished = AtomicBool::new(false);
    let mut builder = WebviewWindowBuilder::new(app, "main", url)
        .title(title)
        .inner_size(config.window.width as f64, config.window.height as f64)
        .resizable(config.window.resizable)
        .fullscreen(config.window.fullscreen)
        // Create the main window hidden, borderless, and without a shadow so the Windows
        // non-client area and WebView's white background do not flash before the first frame.
        .decorations(false)
        .shadow(false)
        .visible(false)
        .always_on_top(config.window.always_on_top)
        .devtools(config.dev.devtools)
        .initialization_script(startup_loading_script(config.window.transparent))
        .initialization_script(crate::app::bridge::script(
            dev_refresh_shortcuts,
            config.dev.devtools,
        ))
        .on_page_load(move |window, payload| match payload.event() {
            PageLoadEvent::Started
                if show_startup_loading && !startup_loading_shown.swap(true, Ordering::AcqRel) =>
            {
                show_main_window(&window, "Failed to show the main window loading screen");
            }
            PageLoadEvent::Finished if !startup_finished.swap(true, Ordering::AcqRel) => {
                finish_main_window_startup(&window, configured_decorations, configured_shadow);
            }
            _ => {}
        })
        .center();

    builder = apply_window_transparency(builder, config.window.transparent);
    builder = apply_storage_policy(builder, config, storage_dir);
    builder = apply_webview2_browser_arguments(builder);

    builder = builder
        .icon(icon)
        .map_err(|err| BtError::WebView(err.to_string()))?;

    let window = builder
        .build()
        .map_err(|err| BtError::WebView(err.to_string()))?;
    crate::app::api::window::attach_close_handler(&window);
    crate::app::api::drag::attach_drag_handler(&window);
    Ok(())
}

/// Returns the native window shadow state required by the project configuration.
///
/// On Windows, Tauri implements shadows for titleless windows with a 1 px non-client border,
/// which creates the solid edge. Transparent windows must also draw their own shadows.
fn native_shadow(config: &AppJson) -> bool {
    !config.window.transparent && (!cfg!(target_os = "windows") || !config.window.hide_titlebar)
}

/// Returns the built-in startup loading script for an opaque main window.
///
/// A closed Shadow DOM isolates the loading screen from project styles. It remains visible for
/// at least 180 ms after load. Transparent windows stay hidden until the first frame is ready.
fn startup_loading_script(transparent: bool) -> &'static str {
    if transparent {
        ""
    } else {
        r#"
(() => {
  const startedAt = performance.now();
  const mount = () => {
    if (document.getElementById("__bt_startup_loading__")) return;
    const host = document.createElement("div");
    host.id = "__bt_startup_loading__";
    host.setAttribute("aria-hidden", "true");
    const root = host.attachShadow({ mode: "closed" });
    root.innerHTML = `
      <style>
        :host {
          all: initial !important;
          position: fixed !important;
          inset: 0 !important;
          z-index: 2147483647 !important;
          display: grid !important;
          place-items: center !important;
          background: #f5f7fa !important;
          pointer-events: none !important;
        }
        div {
          box-sizing: border-box;
          width: 30px;
          height: 30px;
          border: 3px solid rgba(67, 86, 112, 0.2);
          border-top-color: #5269cf;
          border-radius: 50%;
          animation: bt-startup-spin 0.72s linear infinite;
        }
        @media (prefers-color-scheme: dark) {
          :host { background: #20252b !important; }
          div {
            border-color: rgba(203, 210, 220, 0.2);
            border-top-color: #8ea1ff;
          }
        }
        @keyframes bt-startup-spin { to { transform: rotate(360deg); } }
      </style>
      <div></div>
    `;
    document.documentElement.appendChild(host);
    window.addEventListener("load", () => {
      const delay = Math.max(0, 180 - (performance.now() - startedAt));
      window.setTimeout(() => host.remove(), delay);
    }, { once: true });
  };
  if (document.documentElement) mount();
  else document.addEventListener("DOMContentLoaded", mount, { once: true });
})();
"#
    }
}

/// Restores the configured appearance and shows the main window after its first page loads.
///
/// Appearance errors are logged individually, and the window is still shown afterward.
fn finish_main_window_startup<R: tauri::Runtime>(
    window: &WebviewWindow<R>,
    decorations: bool,
    shadow: bool,
) {
    if let Err(err) = window.set_decorations(decorations) {
        eprintln!("Failed to apply main window border configuration: {}", err);
    }
    if let Err(err) = window.set_shadow(shadow) {
        eprintln!("Failed to apply main window shadow configuration: {}", err);
    }
    show_main_window(window, "Failed to show the main window");
}

/// Shows the main window, logging failures without interrupting the event loop.
fn show_main_window<R: tauri::Runtime>(window: &WebviewWindow<R>, action: &str) {
    if let Err(err) = window.show() {
        eprintln!("{}: {}", action, err);
    }
}

/// Passes the controlled test environment variable to WebView2 on Windows.
///
/// This Wry version stops reading the WebView2 variable after setting its own browser arguments.
/// Passing it explicitly restores WebView2 behavior so CDP automation can enable remote debugging.
#[cfg(windows)]
fn apply_webview2_browser_arguments<'a>(
    builder: WebviewWindowBuilder<'a, tauri::Wry, App>,
) -> WebviewWindowBuilder<'a, tauri::Wry, App> {
    match std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
        Ok(arguments) if !arguments.trim().is_empty() => {
            builder.additional_browser_args(arguments.trim())
        }
        _ => builder,
    }
}

/// Non-Windows platforms have no WebView2 browser arguments.
#[cfg(not(windows))]
fn apply_webview2_browser_arguments<'a>(
    builder: WebviewWindowBuilder<'a, tauri::Wry, App>,
) -> WebviewWindowBuilder<'a, tauri::Wry, App> {
    builder
}

/// Enables transparent backgrounds for both the native window and WebView during creation.
///
/// Transparency cannot be fully toggled on an existing WebView, so it is applied only when the
/// main window is built. An explicit clear background prevents an opaque first frame.
fn apply_window_transparency<'a>(
    builder: WebviewWindowBuilder<'a, tauri::Wry, App>,
    transparent: bool,
) -> WebviewWindowBuilder<'a, tauri::Wry, App> {
    if transparent {
        builder
            .transparent(true)
            .background_color(Color(0, 0, 0, 0))
    } else {
        builder
    }
}

/// Applies the WebView storage policy.
fn apply_storage_policy<'a>(
    builder: WebviewWindowBuilder<'a, tauri::Wry, App>,
    config: &AppJson,
    storage_dir: Option<PathBuf>,
) -> WebviewWindowBuilder<'a, tauri::Wry, App> {
    match config.app.storage.as_str() {
        "global" => builder,
        _ => match storage_dir {
            Some(path) => builder.data_directory(path),
            None => builder,
        },
    }
}

/// Returns the WebView data directory for the current storage policy.
fn webview_storage_dir(app: &App, config: &AppJson) -> Result<Option<PathBuf>, BtError> {
    match config.app.storage.as_str() {
        "private" => Ok(Some(private_storage_dir(config))),
        "global" => Ok(None),
        _ => app_storage_dir(app, config).map(Some),
    }
}

/// Returns the application's persistent WebView data directory.
fn app_storage_dir(app: &App, config: &AppJson) -> Result<PathBuf, BtError> {
    let base = app.path().app_local_data_dir().map_err(|err| {
        BtError::Config(format!(
            "Failed to resolve application data directory: {}",
            err
        ))
    })?;
    Ok(base
        .join("profiles")
        .join(safe_storage_segment(config.app.identity_key())))
}

/// Returns a process-specific temporary WebView data directory.
fn private_storage_dir(config: &AppJson) -> PathBuf {
    static PRIVATE_STORAGE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let counter = PRIVATE_STORAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join("bt_app_private_profiles")
        .join(format!(
            "{}-{}-{}-{}",
            safe_storage_segment(config.app.identity_key()),
            std::process::id(),
            stamp,
            counter
        ))
}

/// Converts `app.name` to a safe profile directory name.
fn safe_storage_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control() {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let out = out.trim_matches([' ', '.']).to_string();
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

/// Resolves the window's initial URL from app.json.
pub fn resolve_window_url(config: &AppJson) -> Result<WebviewUrl, BtError> {
    match config.app.mode.as_str() {
        "static" => {
            let entry = config.app.entry.trim_start_matches('/');
            let url_path = if entry == "index.html" { "" } else { entry };
            let url = Url::parse(&format!("bt://app/{}", url_path))
                .map_err(|err| BtError::Config(format!("Invalid static entry URL: {}", err)))?;
            Ok(WebviewUrl::External(url))
        }
        "remote" | "server" => {
            let url = Url::parse(&config.app.entry).map_err(|err| {
                BtError::Config(format!("Invalid {} entry URL: {}", config.app.mode, err))
            })?;
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(BtError::Config(format!(
                    "{} mode entry must begin with http:// or https://",
                    config.app.mode
                )));
            }
            Ok(WebviewUrl::External(url))
        }
        other => Err(BtError::Config(format!("Unknown run mode: {}", other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native shadows are reserved for opaque Windows windows with system borders.
    #[test]
    fn native_shadow_matches_final_window_appearance() {
        let mut config = AppJson::default();
        assert!(native_shadow(&config));

        config.window.transparent = true;
        assert!(!native_shadow(&config));

        config.window.transparent = false;
        config.window.hide_titlebar = true;
        assert_eq!(native_shadow(&config), !cfg!(target_os = "windows"));
    }

    /// Opaque windows receive a loading animation; transparent windows keep a clear first frame.
    #[test]
    fn startup_loading_script_respects_transparency() {
        assert!(startup_loading_script(false).contains("__bt_startup_loading__"));
        assert!(startup_loading_script(false).contains("bt-startup-spin"));
        assert_eq!(startup_loading_script(true), "");
    }

    /// The default static index.html loads at the root for Vue history routing compatibility.
    #[test]
    fn static_index_entry_uses_protocol_root_url() {
        let mut config = AppJson::default();
        config.app.entry = "index.html".to_string();

        assert_eq!(
            external_url_text(resolve_window_url(&config).unwrap()),
            "bt://app/"
        );
    }

    /// Non-default entries retain their file path to preserve multi-page project semantics.
    #[test]
    fn static_non_index_entry_keeps_entry_path() {
        let mut config = AppJson::default();
        config.app.entry = "pages/start.html".to_string();

        assert_eq!(
            external_url_text(resolve_window_url(&config).unwrap()),
            "bt://app/pages/start.html"
        );
    }

    /// Profile names replace invalid path characters and cannot be empty.
    #[test]
    fn safe_storage_segment_replaces_invalid_path_chars() {
        assert_eq!(safe_storage_segment("A:B*"), "A_B_");
        assert_eq!(safe_storage_segment("..."), "app");
    }

    /// Each private storage directory is unique so concurrent instances do not share sessions.
    #[test]
    fn private_storage_dir_is_unique_per_call() {
        let config = AppJson::default();

        assert_ne!(private_storage_dir(&config), private_storage_dir(&config));
    }

    /// Extracts the WebView external URL string for load-address assertions.
    fn external_url_text(url: WebviewUrl) -> String {
        match url {
            WebviewUrl::External(url) => url.to_string(),
            _ => panic!("window URL should be an external URL"),
        }
    }
}
