//! Desktop API implementation for `bt.tray`.

use crate::app::api::{map_error, required_text, ApiState, TRAY_MENU_CLICK_EVENT};
use crate::app::config::{validate_relative_project_path, AppJson};
use crate::app::resource::ResourceSource;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

/// Default tray icon ID.
const TRAY_ID: &str = "bt-main-tray";

/// Tray enable options.
#[derive(Debug, Default, Deserialize)]
pub struct TrayEnableOptions {
    /// Tray icon path; the application icon is used when empty.
    #[serde(default)]
    pub icon: String,
    /// Tray tooltip; the application title is used when empty.
    #[serde(default)]
    pub tooltip: String,
    /// Tray menu items.
    #[serde(default)]
    pub menu: Vec<TrayMenuItem>,
}

/// Tray menu item.
#[derive(Clone, Debug, Deserialize)]
pub struct TrayMenuItem {
    /// Menu item type; `separator` denotes a separator.
    #[serde(default, rename = "type")]
    pub item_type: String,
    /// Menu item ID.
    #[serde(default)]
    pub id: String,
    /// Menu item display text.
    #[serde(default)]
    pub text: String,
    /// Whether the menu item can be clicked.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Enables the tray icon and menu.
pub fn enable(
    app: AppHandle,
    state: &ApiState,
    options: Option<TrayEnableOptions>,
) -> Result<(), String> {
    enable_with_default_menu_behavior(app, state, options.unwrap_or_default(), false)
}

/// Enables the tray icon and menu, optionally with built-in default menu behavior.
fn enable_with_default_menu_behavior(
    app: AppHandle,
    state: &ApiState,
    options: TrayEnableOptions,
    handle_default_menu: bool,
) -> Result<(), String> {
    let menu = build_menu(&app, &options.menu)?;
    let icon = load_tray_icon(&app, &options.icon)?;
    let tooltip = tooltip_text(&app, options.tooltip)?;
    if state.has_tray()? {
        return state.with_tray(|tray| {
            tray.set_icon(Some(icon))
                .map_err(|err| map_error("Set tray icon", err))?;
            tray.set_tooltip(Some(tooltip))
                .map_err(|err| map_error("Set tray tooltip", err))?;
            tray.set_menu(Some(menu))
                .map_err(|err| map_error("Set tray menu", err))
        });
    }
    remove_all_trays_by_id(&app);
    let tray = TrayIconBuilder::<tauri::Wry>::with_id(TRAY_ID)
        .icon(icon)
        .tooltip(tooltip)
        .menu(&menu)
        .on_menu_event(move |app: &AppHandle, event: MenuEvent| {
            let menu_id = event.id().as_ref().to_string();
            dispatch_menu_click_event(app, &menu_id);
            if handle_default_menu {
                handle_default_menu_click(app, &menu_id);
            }
        })
        .build(&app)
        .map_err(|err| map_error("Enable tray", err))?;
    state.set_tray(tray)
}

/// Disables the tray icon.
pub fn disable(app: AppHandle, state: &ApiState) -> Result<(), String> {
    let _ = state.take_tray()?;
    remove_all_trays_by_id(&app);
    Ok(())
}

/// Sets the tray icon.
pub fn set_icon(app: AppHandle, state: &ApiState, icon: String) -> Result<(), String> {
    let icon = required_text(icon, "Tray icon path")?;
    let image = load_tray_icon(&app, &icon)?;
    state.with_tray(|tray| {
        tray.set_icon(Some(image))
            .map_err(|err| map_error("Set tray icon", err))
    })
}

/// Sets the tray tooltip.
pub fn set_tooltip(state: &ApiState, text: String) -> Result<(), String> {
    let text = required_text(text, "Tray tooltip")?;
    state.with_tray(|tray| {
        tray.set_tooltip(Some(text))
            .map_err(|err| map_error("Set tray tooltip", err))
    })
}

/// Sets the tray menu.
pub fn set_menu(app: AppHandle, state: &ApiState, menu: Vec<TrayMenuItem>) -> Result<(), String> {
    let menu = build_menu(&app, &menu)?;
    state.with_tray(|tray| {
        tray.set_menu(Some(menu))
            .map_err(|err| map_error("Set tray menu", err))
    })
}

/// Ensures that a default tray menu exists.
pub fn ensure_default_tray(app: &AppHandle, state: &ApiState) -> Result<(), String> {
    if state.has_tray()? {
        return Ok(());
    }
    enable_with_default_menu_behavior(
        app.clone(),
        state,
        TrayEnableOptions {
            icon: String::new(),
            tooltip: String::new(),
            menu: default_tray_menu(),
        },
        true,
    )
}

/// Builds a tray menu.
fn build_menu(app: &AppHandle, items: &[TrayMenuItem]) -> Result<Menu<tauri::Wry>, String> {
    let menu = Menu::new(app).map_err(|err| map_error("Create tray menu", err))?;
    for item in items {
        if item.item_type == "separator" {
            let separator = PredefinedMenuItem::separator(app)
                .map_err(|err| map_error("Create tray separator", err))?;
            menu.append(&separator)
                .map_err(|err| map_error("Append tray separator", err))?;
            continue;
        }
        validate_menu_item(item)?;
        let menu_item = MenuItemBuilder::with_id(&item.id, &item.text)
            .enabled(item.enabled)
            .build(app)
            .map_err(|err| map_error("Create tray menu item", err))?;
        menu.append(&menu_item)
            .map_err(|err| map_error("Append tray menu item", err))?;
    }
    Ok(menu)
}

/// Validates a tray menu item.
fn validate_menu_item(item: &TrayMenuItem) -> Result<(), String> {
    if !item.item_type.trim().is_empty() {
        return Err(format!(
            "Unsupported tray menu item type: {}",
            item.item_type
        ));
    }
    if item.id.trim().is_empty() {
        return Err("Tray menu item ID cannot be empty".to_string());
    }
    if item.text.trim().is_empty() {
        return Err(format!(
            "Text for tray menu item `{}` cannot be empty",
            item.id
        ));
    }
    Ok(())
}

/// Loads the tray icon.
fn load_tray_icon(app: &AppHandle, icon: &str) -> Result<Image<'static>, String> {
    if icon.trim().is_empty() {
        let (resource, config) = runtime_resource_config(app)?;
        return crate::app::icon::load_window_icon(&resource, &config)
            .map_err(|err| map_error("Read application icon", err));
    }
    let bytes = read_icon_bytes(app, icon)?;
    Image::from_bytes(&bytes)
        .map(|image| image.to_owned())
        .map_err(|err| map_error("Read tray icon", err))
}

/// Reads custom tray icon bytes.
fn read_icon_bytes(app: &AppHandle, icon: &str) -> Result<Vec<u8>, String> {
    let path = PathBuf::from(icon);
    if path.is_absolute() {
        return fs::read(path).map_err(|err| map_error("Read tray icon file", err));
    }
    let name = icon.trim().trim_start_matches('/').replace('\\', "/");
    validate_relative_project_path(&name, "tray.icon").map_err(|err| err.to_string())?;
    let (project_dir, resource) = runtime_project_resource(app)?;
    if resource.exists(&name) {
        return resource
            .read(&name)
            .map_err(|err| map_error("Read tray icon resource", err));
    }
    fs::read(project_dir.join(Path::new(&name)))
        .map_err(|err| map_error("Read tray icon file", err))
}

/// Returns the tray tooltip text.
fn tooltip_text(app: &AppHandle, tooltip: String) -> Result<String, String> {
    if !tooltip.trim().is_empty() {
        return Ok(tooltip);
    }
    let (_, config) = runtime_resource_config(app)?;
    Ok(config.app.title)
}

/// Clones the runtime resource and configuration.
fn runtime_resource_config(app: &AppHandle) -> Result<(ResourceSource, AppJson), String> {
    let state = app.state::<crate::app::runtime::AppState>();
    let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
    Ok((runtime.resource.clone(), runtime.config.clone()))
}

/// Clones the runtime project directory and resource.
fn runtime_project_resource(app: &AppHandle) -> Result<(PathBuf, ResourceSource), String> {
    let state = app.state::<crate::app::runtime::AppState>();
    let runtime = state.lock_runtime().map_err(|err| err.to_string())?;
    Ok((runtime.project_dir.clone(), runtime.resource.clone()))
}

/// Removes all tray icons with the same ID to prevent duplicate system tray entries after re-enabling.
fn remove_all_trays_by_id(app: &AppHandle) {
    while app.remove_tray_by_id(TRAY_ID).is_some() {}
}

/// Returns the default tray menu.
fn default_tray_menu() -> Vec<TrayMenuItem> {
    vec![
        TrayMenuItem {
            item_type: String::new(),
            id: "show".to_string(),
            text: "Show Window".to_string(),
            enabled: true,
        },
        TrayMenuItem {
            item_type: String::new(),
            id: "quit".to_string(),
            text: "Quit".to_string(),
            enabled: true,
        },
    ]
}

/// Returns the default enabled state for menu items.
fn default_enabled() -> bool {
    true
}

/// Dispatches a tray menu click DOM event to the main window page.
fn dispatch_menu_click_event(app: &AppHandle, menu_id: &str) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let Ok(event) = serde_json::to_string(TRAY_MENU_CLICK_EVENT) else {
        return;
    };
    let Ok(detail) = serde_json::to_string(menu_id) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent({}, {{ detail: {} }}));",
        event, detail
    ));
}

/// Handles built-in automatic tray menu actions so the window can be restored or the application exited.
fn handle_default_menu_click(app: &AppHandle, menu_id: &str) {
    match menu_id {
        "show" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        "quit" => app.exit(0),
        _ => {}
    }
}
