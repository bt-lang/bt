//! Bounded application-owned images and local WebView windows for screenshot editors and utility panels.

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_clipboard_manager::ClipboardExt;
use xcap::image::{ImageFormat, ImageReader, RgbaImage};

/// Aggregate raw image budget, including frozen monitors and pinned images.
const IMAGE_BUDGET: usize = 256 * 1024 * 1024;
/// Maximum encoded image payload accepted from application canvases.
const ENCODED_LIMIT: usize = 48 * 1024 * 1024;

/// Shared image store. Native image ownership never escapes the application process.
#[derive(Default)]
pub struct SurfaceState {
    /// Stored immutable images, bounded by bytes and item count.
    images: Mutex<HashMap<String, Arc<RgbaImage>>>,
    /// Serializes capture and import allocations, including transient decoding work.
    operation: tokio::sync::Mutex<()>,
}

/// Metadata for one frozen display; desktop positions follow the host's window coordinate conventions.
#[derive(Serialize)]
pub struct FrozenDisplay {
    /// Opaque application-local image identifier.
    pub image_id: String,
    /// Captured width in physical pixels.
    pub width: u32,
    /// Captured height in physical pixels.
    pub height: u32,
    /// Native monitor origin before conversion to WebView logical coordinates.
    pub x: i32,
    /// Native monitor vertical origin.
    pub y: i32,
    /// Native monitor width used for positioning.
    pub display_width: u32,
    /// Native monitor height used for positioning.
    pub display_height: u32,
    /// Whether this is the primary monitor.
    pub primary: bool,
    /// Whether the compositor permits the editor's explicit desktop placement.
    pub overlay_supported: bool,
}

impl SurfaceState {
    /// Stores a validated image while enforcing the aggregate allocation budget.
    fn insert(&self, image: RgbaImage) -> Result<String, String> {
        let mut images = self.images.lock().map_err(|_| "Image store unavailable")?;
        let used: usize = images.values().map(|image| image.as_raw().len()).sum();
        if images.len() >= 32 || image.as_raw().len() > IMAGE_BUDGET.saturating_sub(used) {
            return Err("Application image budget exceeded; close unused images first".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        images.insert(id.clone(), Arc::new(image));
        Ok(id)
    }

    /// Acquires an immutable image reference, rejecting released or foreign identifiers.
    fn get(&self, id: &str) -> Result<Arc<RgbaImage>, String> {
        self.images
            .lock()
            .map_err(|_| "Image store unavailable")?
            .get(id)
            .cloned()
            .ok_or_else(|| "Image is no longer available".into())
    }

    /// Releases a stored image; repeated release is harmless.
    pub fn release(&self, id: &str) -> Result<bool, String> {
        Ok(self
            .images
            .lock()
            .map_err(|_| "Image store unavailable")?
            .remove(id)
            .is_some())
    }
}

/// Enforces the existing screen capability for pixel acquisition and access.
fn require_screen() -> Result<(), String> {
    require_desktop()?;
    crate::permission::check(crate::permission::Capability::Screen)
}

/// Enforces the existing desktop capability for managed local windows.
fn require_desktop() -> Result<(), String> {
    crate::permission::check(crate::permission::Capability::Desktop)
}

/// Freezes display pixels without creating the runtime's built-in selection overlay.
#[tauri::command]
pub async fn surface_freeze(
    state: tauri::State<'_, SurfaceState>,
) -> Result<Vec<FrozenDisplay>, String> {
    require_screen()?;
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| "Another image operation is in progress")?;
    let used: usize = state
        .images
        .lock()
        .map_err(|_| "Image store unavailable")?
        .values()
        .map(|image| image.as_raw().len())
        .sum();
    let pixel_limit = IMAGE_BUDGET.saturating_sub(used) as u64 / 4;
    let frames = tauri::async_runtime::spawn_blocking(move || {
        super::screen::capture_frames_with_limit(pixel_limit)
    })
    .await
    .map_err(|error| error.to_string())??;
    let overlay_supported = !(cfg!(target_os = "linux")
        && std::env::var("XDG_SESSION_TYPE")
            .is_ok_and(|value| value.eq_ignore_ascii_case("wayland")));
    let mut result: Vec<FrozenDisplay> = Vec::with_capacity(frames.len());
    for frame in frames {
        let image = RgbaImage::from_raw(frame.width, frame.height, frame.rgba)
            .ok_or("Invalid screen image")?;
        let image_id = match state.insert(image) {
            Ok(id) => id,
            Err(error) => {
                for display in &result {
                    let _ = state.release(&display.image_id);
                }
                return Err(error);
            }
        };
        result.push(FrozenDisplay {
            image_id,
            width: frame.width,
            height: frame.height,
            x: frame.overlay_x,
            y: frame.overlay_y,
            display_width: frame.overlay_width,
            display_height: frame.overlay_height,
            primary: frame.primary,
            overlay_supported,
        });
    }
    Ok(result)
}

/// Encodes an immutable image once per request and returns binary PNG rather than a JSON pixel array.
#[tauri::command]
pub async fn surface_read(
    state: tauri::State<'_, SurfaceState>,
    image_id: String,
) -> Result<tauri::ipc::Response, String> {
    require_screen()?;
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| "Another image operation is in progress")?;
    let image = state.get(&image_id)?;
    let bytes = tauri::async_runtime::spawn_blocking(move || {
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>(bytes.into_inner())
    })
    .await
    .map_err(|error| error.to_string())??;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Decodes a Canvas PNG with an explicit input and decoded allocation limit.
fn decode_png(data: &str) -> Result<RgbaImage, String> {
    let encoded = data
        .strip_prefix("data:image/png;base64,")
        .ok_or("Expected a PNG data URL")?;
    if encoded.len() > ENCODED_LIMIT {
        return Err("Encoded image exceeds the size limit".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| error.to_string())?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = xcap::image::Limits::default();
    limits.max_alloc = Some(IMAGE_BUDGET as u64);
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    reader.limits(limits);
    Ok(reader
        .decode()
        .map_err(|error| error.to_string())?
        .to_rgba8())
}

/// Imports an edited Canvas image for clipboard output or independent pin ownership.
#[tauri::command]
pub async fn surface_import(
    state: tauri::State<'_, SurfaceState>,
    data: String,
) -> Result<String, String> {
    require_screen()?;
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| "Another image operation is in progress")?;
    let image = tauri::async_runtime::spawn_blocking(move || decode_png(&data))
        .await
        .map_err(|error| error.to_string())??;
    state.insert(image)
}

/// Releases an application image after editing or closing the last pin.
#[tauri::command]
pub fn surface_release(
    state: tauri::State<'_, SurfaceState>,
    image_id: String,
) -> Result<bool, String> {
    require_screen()?;
    state.release(&image_id)
}

/// Copies an application-owned image to the system image clipboard.
#[tauri::command]
pub async fn surface_copy(
    app: tauri::AppHandle,
    state: tauri::State<'_, SurfaceState>,
    image_id: String,
) -> Result<(), String> {
    require_screen()?;
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| "Another image operation is in progress")?;
    let image = state.get(&image_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        app.clipboard()
            .write_image(&tauri::image::Image::new(
                image.as_raw(),
                image.width(),
                image.height(),
            ))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Writes a validated Canvas PNG/JPEG to a selected path without blocking the UI thread.
#[tauri::command]
pub async fn surface_save(
    state: tauri::State<'_, SurfaceState>,
    data: String,
    path: String,
) -> Result<(), String> {
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| "Another image operation is in progress")?;
    require_screen()?;
    crate::permission::check(crate::permission::Capability::Fs)?;
    if data.len() > ENCODED_LIMIT {
        return Err("Encoded image exceeds the size limit".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let encoded = data
            .strip_prefix("data:image/png;base64,")
            .or_else(|| data.strip_prefix("data:image/jpeg;base64,"))
            .ok_or("Expected PNG or JPEG data")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| error.to_string())?;
        if bytes.len() < 8
            || !(bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(b"\xff\xd8\xff"))
        {
            return Err("Invalid image signature".into());
        }
        std::fs::write(path, bytes).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Options for a same-application child window. Coordinates refer to its content, not its caption.
#[derive(Deserialize)]
pub struct SurfaceWindowOptions {
    /// Unique short application-defined identifier.
    pub id: String,
    /// Safe local resource path, optionally followed by a query string.
    pub entry: String,
    /// Initial title.
    pub title: String,
    /// Content width; physical when `physical` is true, logical otherwise.
    pub width: f64,
    /// Content height.
    pub height: f64,
    /// Content origin X.
    pub x: f64,
    /// Content origin Y.
    pub y: f64,
    /// Use physical desktop pixels for content geometry.
    #[serde(default)]
    pub physical: bool,
    /// Whether to display native caption and borders.
    #[serde(default)]
    pub decorations: bool,
    /// Keep the child above normal windows where supported.
    #[serde(default)]
    pub always_on_top: bool,
    /// Transparent WebView background for popup cards.
    #[serde(default)]
    pub transparent: bool,
    /// Close on focus loss, intended for settings popups.
    #[serde(default)]
    pub close_on_blur: bool,
    /// Image references owned by this child and released when it is destroyed.
    #[serde(default)]
    pub release_images: Vec<String>,
    /// Small application bootstrap object; large image data must use the image store.
    #[serde(default)]
    pub data: Value,
}

/// Validates identifiers and local resource paths without accepting schemes or traversal.
fn validate_window(options: &SurfaceWindowOptions) -> Result<(), String> {
    if options.id.is_empty()
        || options.id.len() > 64
        || !options
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid child window ID".into());
    }
    let path = options.entry.split('?').next().unwrap_or("");
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', ':', '%', '#'])
        || path.split('/').any(|part| part == ".." || part.is_empty())
    {
        return Err("Child entry must be a safe application resource".into());
    }
    if ![options.width, options.height, options.x, options.y]
        .iter()
        .all(|value| value.is_finite())
        || options.width < 1.0
        || options.height < 1.0
        || options.width > 32768.0
        || options.height > 32768.0
    {
        return Err("Invalid child window bounds".into());
    }
    if options.data.to_string().len() > 65536 || options.release_images.len() > 16 {
        return Err("Child bootstrap data exceeds its limit".into());
    }
    Ok(())
}

/// Creates a bounded local child window with the public BT bridge and shared application storage.
#[tauri::command]
pub async fn surface_window_create(
    app: tauri::AppHandle,
    options: SurfaceWindowOptions,
) -> Result<String, String> {
    require_desktop()?;
    validate_window(&options)?;
    if app.webview_windows().len() >= 20 {
        return Err("Application window limit exceeded".into());
    }
    let label = format!("bt-child-{}", options.id);
    if app.get_webview_window(&label).is_some() {
        return Err("Child window ID is already in use".into());
    }
    let (icon, devtools) = {
        let state = app.state::<crate::app::runtime::AppState>();
        let runtime = state.lock_runtime().map_err(|error| error.to_string())?;
        runtime
            .resource
            .read(options.entry.split('?').next().unwrap_or(""))
            .map_err(|error| error.to_string())?;
        (
            crate::app::icon::load_window_icon(&runtime.resource, &runtime.config)
                .map_err(|error| error.to_string())?,
            runtime.config.dev.devtools,
        )
    };
    let url = url::Url::parse(&format!("bt://app/{}", options.entry))
        .map_err(|error| error.to_string())?;
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(url))
        .title(&options.title)
        .visible(false)
        .decorations(options.decorations)
        .shadow(options.decorations)
        .always_on_top(options.always_on_top)
        .transparent(options.transparent)
        .resizable(false)
        .skip_taskbar(!options.decorations)
        .devtools(devtools)
        .background_color(tauri::window::Color(
            0,
            0,
            0,
            if options.transparent { 0 } else { 255 },
        ))
        .initialization_script(crate::app::bridge::script(false, devtools))
        .initialization_script(format!("window.__BT_CHILD__ = {};", options.data))
        .icon(icon)
        .map_err(|error| error.to_string())?;
    // Shared WebView2 profiles require identical environment options for every window.
    #[cfg(windows)]
    if let Ok(arguments) = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
        if !arguments.trim().is_empty() {
            builder = builder.additional_browser_args(arguments.trim());
        }
    }
    if let Some(storage) = app
        .try_state::<crate::app::window::WebviewStorageState>()
        .and_then(|state| state.path.clone())
    {
        builder = builder.data_directory(storage);
    }
    let window = builder.build().map_err(|error| error.to_string())?;
    let handle = app.clone();
    let event_label = label.clone();
    let child = window.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Focused(false) if options.close_on_blur => {
            let _ = child.close();
        }
        tauri::WindowEvent::Destroyed => {
            for id in &options.release_images {
                let _ = handle.state::<SurfaceState>().release(id);
            }
            let _ = handle.emit_to("main", "bt://surface/closed", &event_label);
        }
        _ => {}
    });
    let geometry = (|| -> Result<(), String> {
        // Wayland deliberately delegates placement to the compositor while retaining capture/editing.
        let compositor_positioned = cfg!(target_os = "linux")
            && std::env::var("XDG_SESSION_TYPE")
                .is_ok_and(|value| value.eq_ignore_ascii_case("wayland"));
        if !compositor_positioned {
            if options.physical {
                window
                    .set_position(tauri::PhysicalPosition::new(
                        options.x as i32,
                        options.y as i32,
                    ))
                    .map_err(|error| error.to_string())?;
            } else {
                window
                    .set_position(tauri::LogicalPosition::new(options.x, options.y))
                    .map_err(|error| error.to_string())?;
            }
        }
        if options.physical {
            window
                .set_size(tauri::PhysicalSize::new(
                    options.width as u32,
                    options.height as u32,
                ))
                .map_err(|error| error.to_string())?;
        } else {
            window
                .set_size(tauri::LogicalSize::new(options.width, options.height))
                .map_err(|error| error.to_string())?;
        }
        if !compositor_positioned {
            // Use actual native chrome offsets at the target monitor's DPI.
            let outer = window.outer_position().map_err(|error| error.to_string())?;
            let inner = window.inner_position().map_err(|error| error.to_string())?;
            window
                .set_position(tauri::PhysicalPosition::new(
                    outer.x - (inner.x - outer.x),
                    outer.y - (inner.y - outer.y),
                ))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })();
    if let Err(error) = geometry {
        let _ = window.destroy();
        return Err(error);
    }
    Ok(label)
}

/// Closes only an application-created child window; the main window is never a valid target.
#[tauri::command]
pub fn surface_window_close(app: tauri::AppHandle, id: String) -> Result<(), String> {
    require_desktop()?;
    if !id.starts_with("bt-child-") {
        return Err("Expected an application child window".into());
    }
    if let Some(window) = app.get_webview_window(&id) {
        window.close().map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Sends a bounded structured application message to a main or managed child window.
#[tauri::command]
pub fn surface_message(
    app: tauri::AppHandle,
    window: WebviewWindow,
    target: String,
    payload: Value,
) -> Result<(), String> {
    require_desktop()?;
    if target != "main" && !target.starts_with("bt-child-") {
        return Err("Invalid application message target".into());
    }
    if payload.to_string().len() > 65536 {
        return Err("Application message exceeds its limit".into());
    }
    app.emit_to(
        target,
        "bt://surface/message",
        serde_json::json!({"source":window.label(),"payload":payload}),
    )
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reject unsafe resource paths and bounds while accepting normal local entries.
    #[test]
    fn child_window_validation() {
        let mut options: SurfaceWindowOptions = serde_json::from_value(serde_json::json!({
            "id":"capture-1", "entry":"editor.html", "title":"Editor",
            "x":-1920, "y":0, "width":1920, "height":1080
        }))
        .unwrap();
        assert!(validate_window(&options).is_ok());
        for entry in [
            "../editor.html",
            "/editor.html",
            "https://example.com",
            "a/%2e%2e/editor.html",
            "a\\editor.html",
        ] {
            options.entry = entry.into();
            assert!(validate_window(&options).is_err(), "{entry}");
        }
        options.entry = "editor.html?mode=capture".into();
        assert!(validate_window(&options).is_ok());
        options.width = f64::NAN;
        assert!(validate_window(&options).is_err());
        options.width = 100.0;
        options.id = "../main".into();
        assert!(validate_window(&options).is_err());
    }

    /// Enforce the image-count bound and recover capacity after a child releases ownership.
    #[test]
    fn image_store_capacity() {
        let store = SurfaceState::default();
        let mut ids = Vec::new();
        for _ in 0..32 {
            ids.push(store.insert(RgbaImage::new(1, 1)).unwrap());
        }
        assert!(store.insert(RgbaImage::new(1, 1)).is_err());
        store.release(&ids[0]).unwrap();
        assert!(store.insert(RgbaImage::new(1, 1)).is_ok());
    }

    /// Reject malformed, oversized and wrong-format Canvas image payloads before allocation.
    #[test]
    fn image_payload_validation() {
        assert!(decode_png("data:text/plain;base64,AAAA").is_err());
        assert!(decode_png("data:image/png;base64,AAAA").is_err());
        let image = RgbaImage::from_pixel(2, 2, xcap::image::Rgba([12, 34, 56, 255]));
        let mut png = Cursor::new(Vec::new());
        xcap::image::DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let data = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png.into_inner())
        );
        assert_eq!(decode_png(&data).unwrap(), image);
        let store = SurfaceState::default();
        let id = store.insert(image).unwrap();
        assert!(store.release(&id).unwrap());
        assert!(!store.release(&id).unwrap());
        assert!(store.get(&id).is_err());
    }
}
