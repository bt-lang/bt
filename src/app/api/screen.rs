//! Desktop API implementation for the `bt.screen` color picker and area capture.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tauri::window::Color;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tauri::{LogicalPosition, LogicalSize};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use tauri::{PhysicalPosition, PhysicalSize};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tokio::sync::oneshot;
use xcap::Monitor;

/// Reserved path used by the built-in screen selector under `bt://app/`.
pub const OVERLAY_ENTRY: &str = "__bt_screen_overlay.html";

/// Maximum number of monitors allowed in one screen-selection session.
const MAX_MONITORS: usize = 16;

/// Maximum total pixels frozen per selection, equivalent to 256 MiB of RGBA data.
const MAX_CAPTURE_PIXELS: u64 = 64 * 1024 * 1024;

/// Minimum side length of a valid selection in physical pixels.
const MIN_SELECTION_SIZE: u32 = 2;

/// Screen color-picker options.
#[derive(Debug, Deserialize)]
pub struct PickColorOptions {
    /// Whether to write `#RRGGBB` to the system text clipboard after a successful pick.
    #[serde(default = "default_true")]
    pub copy_to_clipboard: bool,
}

/// Area-capture options.
#[derive(Debug, Deserialize)]
pub struct CaptureAreaOptions {
    /// Whether to write the RGBA image to the system image clipboard after a successful selection.
    #[serde(default = "default_true")]
    pub copy_to_clipboard: bool,
}

/// Color information returned by the screen color picker.
#[derive(Clone, Debug, Serialize)]
pub struct ScreenColor {
    /// Physical-pixel X coordinate on the virtual desktop; may be negative on a monitor to the left.
    pub x: i32,
    /// Physical-pixel Y coordinate on the virtual desktop; may be negative on a monitor above.
    pub y: i32,
    /// Red channel, from 0 to 255.
    pub r: u8,
    /// Green channel, from 0 to 255.
    pub g: u8,
    /// Blue channel, from 0 to 255.
    pub b: u8,
    /// Alpha channel, normally 255 for screen captures.
    pub a: u8,
    /// 24-bit `0xRRGGBB` value.
    pub rgb: u32,
    /// Uppercase `#RRGGBB` text.
    pub hex: String,
    /// `[r, g, b, a]` bytes in fixed order.
    pub rgba: [u8; 4],
    /// Whether this result was written to the system text clipboard.
    pub clipboard: bool,
}

/// Area-capture result.
#[derive(Clone, Debug, Serialize)]
pub struct ScreenCaptureResult {
    /// Physical-pixel X coordinate of the selection's top-left corner on the virtual desktop.
    pub x: i32,
    /// Physical-pixel Y coordinate of the selection's top-left corner on the virtual desktop.
    pub y: i32,
    /// Selection width in physical pixels.
    pub width: u32,
    /// Selection height in physical pixels.
    pub height: u32,
    /// Whether this capture was written to the system image clipboard.
    pub clipboard: bool,
}

/// Shared runtime state for the screen selector.
pub struct ScreenState {
    /// The sole active session and next session ID.
    inner: Mutex<ScreenStateInner>,
}

/// Ensures the exclusive screen session is released if an async command is canceled or returns early.
struct ScreenSessionGuard<'a> {
    /// Shared screen state to clean up.
    state: &'a ScreenState,
    /// Only cleans up the session reserved by this guard, leaving later sessions untouched.
    session_id: u64,
}

impl Drop for ScreenSessionGuard<'_> {
    /// Releases captured frames and the busy state on every public-command exit path.
    fn drop(&mut self) {
        let _ = self.state.end(self.session_id);
    }
}

/// Contents of the screen selector's shared runtime state.
struct ScreenStateInner {
    /// Nonzero, increasing ID for the next session.
    next_session_id: u64,
    /// The sole active screen-selection session.
    session: Option<ScreenSession>,
}

/// A single screen-selection session.
struct ScreenSession {
    /// Nonzero, increasing session ID used to reject late events from closed overlays.
    session_id: u64,
    /// Current selector mode.
    mode: ScreenMode,
    /// Per-monitor RGBA frames frozen before overlays appear; installed after the reserved session finishes capture.
    frames: Option<Arc<Vec<ScreenFrame>>>,
    /// One-shot channel that returns completion or cancellation to the public async command.
    sender: Option<oneshot::Sender<Option<ScreenSelection>>>,
}

/// Frozen frame for one monitor.
struct ScreenFrame {
    /// Zero-based monitor index within the session.
    monitor_index: usize,
    /// Whether this is the system's primary monitor.
    primary: bool,
    /// Physical-pixel X coordinate of the monitor's top-left corner on the virtual desktop.
    x: i32,
    /// Physical-pixel Y coordinate of the monitor's top-left corner on the virtual desktop.
    y: i32,
    /// Native monitor X coordinate used by the Tauri overlay.
    overlay_x: i32,
    /// Native monitor Y coordinate used by the Tauri overlay.
    overlay_y: i32,
    /// Native monitor width used by the Tauri overlay.
    overlay_width: u32,
    /// Native monitor height used by the Tauri overlay.
    overlay_height: u32,
    /// Captured frame width in physical pixels.
    width: u32,
    /// Captured frame height in physical pixels.
    height: u32,
    /// Row-major RGBA8 pixel data.
    rgba: Vec<u8>,
}

/// Screen selector mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenMode {
    /// Select one pixel and return its color.
    PickColor,
    /// Drag a rectangle and return its capture.
    CaptureArea,
}

/// Completed screen selection.
#[derive(Clone, Copy, Debug)]
enum ScreenSelection {
    /// A physical-pixel point within one monitor.
    Point {
        /// Monitor index within the session.
        monitor_index: usize,
        /// Physical-pixel X coordinate within the monitor.
        x: u32,
        /// Physical-pixel Y coordinate within the monitor.
        y: u32,
    },
    /// A physical-pixel rectangle within one monitor.
    Area {
        /// Monitor index within the session.
        monitor_index: usize,
        /// X coordinate of the selection's top-left corner within the monitor.
        x: u32,
        /// Y coordinate of the selection's top-left corner within the monitor.
        y: u32,
        /// Selection width in physical pixels.
        width: u32,
        /// Selection height in physical pixels.
        height: u32,
    },
}

/// Cropped RGBA area not yet written to the clipboard.
struct CapturedArea {
    /// Physical-pixel X coordinate of the selection's top-left corner on the virtual desktop.
    x: i32,
    /// Physical-pixel Y coordinate of the selection's top-left corner on the virtual desktop.
    y: i32,
    /// Selection width in physical pixels.
    width: u32,
    /// Selection height in physical pixels.
    height: u32,
    /// Tightly packed row-major RGBA8 pixel data.
    rgba: Vec<u8>,
}

impl Default for PickColorOptions {
    /// Writes successful color picks directly to the text clipboard by default.
    fn default() -> Self {
        Self {
            copy_to_clipboard: true,
        }
    }
}

impl Default for CaptureAreaOptions {
    /// Writes successful area captures directly to the image clipboard by default.
    fn default() -> Self {
        Self {
            copy_to_clipboard: true,
        }
    }
}

impl ScreenState {
    /// Creates empty screen-selector state.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ScreenStateInner {
                next_session_id: 1,
                session: None,
            }),
        }
    }

    /// Reserves the exclusive screen-selection session before capture and returns its ID and completion channel.
    fn begin(
        &self,
        mode: ScreenMode,
    ) -> Result<(u64, oneshot::Receiver<Option<ScreenSelection>>), String> {
        let mut inner = self.lock()?;
        if inner.session.is_some() {
            return Err(
                "A screen color-picker or capture session is already in progress".to_string(),
            );
        }
        let session_id = inner.next_session_id.max(1);
        inner.next_session_id = session_id.wrapping_add(1).max(1);
        let (sender, receiver) = oneshot::channel();
        inner.session = Some(ScreenSession {
            session_id,
            mode,
            frames: None,
            sender: Some(sender),
        });
        Ok((session_id, receiver))
    }

    /// Installs captured frames into the specified session while it remains reserved.
    fn install_frames(&self, session_id: u64, frames: Arc<Vec<ScreenFrame>>) -> Result<(), String> {
        let mut inner = self.lock()?;
        let session = current_session_mut(&mut inner, session_id)?;
        if session.frames.is_some() {
            return Err(
                "Captured frames are already installed for the screen-selection session"
                    .to_string(),
            );
        }
        session.frames = Some(frames);
        Ok(())
    }

    /// Returns the color of a pixel in the current session for the picker hover preview.
    fn sample(
        &self,
        session_id: u64,
        monitor_index: usize,
        x: u32,
        y: u32,
    ) -> Result<ScreenColor, String> {
        let inner = self.lock()?;
        let session = current_session(&inner, session_id)?;
        if session.mode != ScreenMode::PickColor {
            return Err("The current screen session is not in color-picker mode".to_string());
        }
        let frame = frame_by_index(session_frames(session)?, monitor_index)?;
        frame.color_at(x, y, false)
    }

    /// Completes the current color-picker session.
    fn finish_point(
        &self,
        session_id: u64,
        monitor_index: usize,
        x: u32,
        y: u32,
    ) -> Result<(), String> {
        let mut inner = self.lock()?;
        {
            let session = current_session(&inner, session_id)?;
            if session.mode != ScreenMode::PickColor {
                return Err("The current screen session is not in color-picker mode".to_string());
            }
            frame_by_index(session_frames(session)?, monitor_index)?.validate_point(x, y)?;
        }
        finish_session(
            &mut inner,
            ScreenSelection::Point {
                monitor_index,
                x,
                y,
            },
        )
    }

    /// Completes the current area-capture session.
    fn finish_area(
        &self,
        session_id: u64,
        monitor_index: usize,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let mut inner = self.lock()?;
        {
            let session = current_session(&inner, session_id)?;
            if session.mode != ScreenMode::CaptureArea {
                return Err("The current screen session is not in area-capture mode".to_string());
            }
            frame_by_index(session_frames(session)?, monitor_index)?
                .validate_area(x, y, width, height)?;
        }
        finish_session(
            &mut inner,
            ScreenSelection::Area {
                monitor_index,
                x,
                y,
                width,
                height,
            },
        )
    }

    /// Cancels the specified session; duplicate cancellations from stale windows are ignored safely.
    fn cancel(&self, session_id: u64) -> Result<bool, String> {
        let mut inner = self.lock()?;
        if inner
            .session
            .as_ref()
            .map(|session| session.session_id != session_id)
            .unwrap_or(true)
        {
            return Ok(false);
        }
        if let Some(session) = inner.session.as_mut() {
            if let Some(sender) = session.sender.take() {
                let _ = sender.send(None);
            }
        }
        Ok(true)
    }

    /// Releases the specified session and its bounded captured frames after all overlays close.
    fn end(&self, session_id: u64) -> Result<bool, String> {
        let mut inner = self.lock()?;
        if inner
            .session
            .as_ref()
            .map(|session| session.session_id != session_id)
            .unwrap_or(true)
        {
            return Ok(false);
        }
        inner.session.take();
        Ok(true)
    }

    /// Locks the screen state and converts lock poisoning into an error.
    fn lock(&self) -> Result<MutexGuard<'_, ScreenStateInner>, String> {
        self.inner
            .lock()
            .map_err(|_| "Screen-selector state is corrupted".to_string())
    }
}

impl ScreenMode {
    /// Returns the stable mode name passed to the built-in overlay page.
    fn as_str(self) -> &'static str {
        match self {
            ScreenMode::PickColor => "pick_color",
            ScreenMode::CaptureArea => "capture_area",
        }
    }
}

impl ScreenFrame {
    /// Validates a point within the monitor and returns its RGBA byte offset.
    fn pixel_offset(&self, x: u32, y: u32) -> Result<usize, String> {
        self.validate_point(x, y)?;
        let pixel = (y as usize)
            .checked_mul(self.width as usize)
            .and_then(|row| row.checked_add(x as usize))
            .ok_or_else(|| "Screen pixel coordinates overflowed".to_string())?;
        pixel
            .checked_mul(4)
            .filter(|offset| offset + 4 <= self.rgba.len())
            .ok_or_else(|| "Screen pixel buffer is incomplete".to_string())
    }

    /// Validates a point within the monitor.
    fn validate_point(&self, x: u32, y: u32) -> Result<(), String> {
        if x >= self.width || y >= self.height {
            return Err("Color-picker coordinates are outside the monitor bounds".to_string());
        }
        Ok(())
    }

    /// Validates a rectangle within the monitor and its minimum side length.
    fn validate_area(&self, x: u32, y: u32, width: u32, height: u32) -> Result<(), String> {
        if width < MIN_SELECTION_SIZE || height < MIN_SELECTION_SIZE {
            return Err(format!(
                "Capture selection width and height must be at least {} physical pixels",
                MIN_SELECTION_SIZE
            ));
        }
        let right = x
            .checked_add(width)
            .ok_or_else(|| "Capture selection X coordinate overflowed".to_string())?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| "Capture selection Y coordinate overflowed".to_string())?;
        if right > self.width || bottom > self.height {
            return Err("Capture selection is outside the monitor bounds".to_string());
        }
        Ok(())
    }

    /// Reads the color of the specified physical pixel.
    fn color_at(&self, x: u32, y: u32, clipboard: bool) -> Result<ScreenColor, String> {
        let offset = self.pixel_offset(x, y)?;
        let r = self.rgba[offset];
        let g = self.rgba[offset + 1];
        let b = self.rgba[offset + 2];
        let a = self.rgba[offset + 3];
        let rgb = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
        Ok(ScreenColor {
            x: self.x.saturating_add(x as i32),
            y: self.y.saturating_add(y as i32),
            r,
            g,
            b,
            a,
            rgb,
            hex: format!("#{:02X}{:02X}{:02X}", r, g, b),
            rgba: [r, g, b, a],
            clipboard,
        })
    }

    /// Copies a tightly packed RGBA rectangle from the frozen frame.
    fn crop(&self, x: u32, y: u32, width: u32, height: u32) -> Result<CapturedArea, String> {
        self.validate_area(x, y, width, height)?;
        let row_bytes = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| "Capture row byte count overflowed".to_string())?;
        let total_bytes = row_bytes
            .checked_mul(height as usize)
            .ok_or_else(|| "Capture byte count overflowed".to_string())?;
        let source_stride = (self.width as usize)
            .checked_mul(4)
            .ok_or_else(|| "Screen frame stride overflowed".to_string())?;
        let mut rgba = Vec::with_capacity(total_bytes);
        for row in y..y + height {
            let start = (row as usize)
                .checked_mul(source_stride)
                .and_then(|offset| offset.checked_add(x as usize * 4))
                .ok_or_else(|| "Capture crop coordinates overflowed".to_string())?;
            let end = start
                .checked_add(row_bytes)
                .ok_or_else(|| "Capture crop row boundary overflowed".to_string())?;
            let bytes = self
                .rgba
                .get(start..end)
                .ok_or_else(|| "Screen frame data is incomplete".to_string())?;
            rgba.extend_from_slice(bytes);
        }
        Ok(CapturedArea {
            x: self.x.saturating_add(x as i32),
            y: self.y.saturating_add(y as i32),
            width,
            height,
            rgba,
        })
    }
}

/// Opens the color-picker overlay and optionally writes a successful result to the text clipboard.
pub async fn pick_color(
    app: AppHandle,
    state: &ScreenState,
    options: Option<PickColorOptions>,
) -> Result<Option<ScreenColor>, String> {
    let options = options.unwrap_or_default();
    let (frames, selection) = run_selection(app.clone(), state, ScreenMode::PickColor).await?;
    let Some(ScreenSelection::Point {
        monitor_index,
        x,
        y,
    }) = selection
    else {
        return Ok(None);
    };
    let frame = frame_by_index(&frames, monitor_index)?;
    let mut color = frame.color_at(x, y, false)?;
    if options.copy_to_clipboard {
        crate::app::api::clipboard::write_text(app, color.hex.clone())?;
        color.clipboard = true;
    }
    Ok(Some(color))
}

/// Opens the area-selection overlay and optionally writes the raw RGBA image to the image clipboard.
pub async fn capture_area(
    app: AppHandle,
    state: &ScreenState,
    options: Option<CaptureAreaOptions>,
) -> Result<Option<ScreenCaptureResult>, String> {
    let options = options.unwrap_or_default();
    let (frames, selection) = run_selection(app.clone(), state, ScreenMode::CaptureArea).await?;
    let Some(ScreenSelection::Area {
        monitor_index,
        x,
        y,
        width,
        height,
    }) = selection
    else {
        return Ok(None);
    };
    let frame = frame_by_index(&frames, monitor_index)?;
    let area = frame.crop(x, y, width, height)?;
    let result = ScreenCaptureResult {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
        clipboard: options.copy_to_clipboard,
    };
    if options.copy_to_clipboard {
        let image = tauri::image::Image::new_owned(area.rgba, area.width, area.height);
        app.clipboard()
            .write_image(&image)
            .map_err(|err| format!("Failed to write to the image clipboard: {}", err))?;
    }
    Ok(Some(result))
}

/// Returns a color preview for the current picker hover position.
pub fn overlay_sample(
    state: &ScreenState,
    session_id: u64,
    monitor_index: usize,
    x: u32,
    y: u32,
) -> Result<ScreenColor, String> {
    state.sample(session_id, monitor_index, x, y)
}

/// Receives a color-picker completion event from the built-in overlay page.
pub fn overlay_pick(
    state: &ScreenState,
    session_id: u64,
    monitor_index: usize,
    x: u32,
    y: u32,
) -> Result<(), String> {
    state.finish_point(session_id, monitor_index, x, y)
}

/// Receives an area-capture completion event from the built-in overlay page.
pub fn overlay_capture(
    state: &ScreenState,
    session_id: u64,
    monitor_index: usize,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    state.finish_area(session_id, monitor_index, x, y, width, height)
}

/// Receives a cancellation event from the built-in overlay page.
pub fn overlay_cancel(state: &ScreenState, session_id: u64) -> Result<bool, String> {
    state.cancel(session_id)
}

/// Returns the built-in screen-selector HTML, which communicates with the host only through restricted internal commands.
pub fn overlay_html() -> &'static str {
    OVERLAY_HTML
}

/// Returns the overlay's minimal initialization bridge, allowing only four internal screen-session commands.
pub fn overlay_initialization_script() -> &'static str {
    OVERLAY_INITIALIZATION_SCRIPT
}

/// Captures the screen, creates an overlay per monitor, and waits for completion or cancellation.
async fn run_selection(
    app: AppHandle,
    state: &ScreenState,
    mode: ScreenMode,
) -> Result<(Arc<Vec<ScreenFrame>>, Option<ScreenSelection>), String> {
    let (session_id, receiver) = state.begin(mode)?;
    let _session_guard = ScreenSessionGuard { state, session_id };
    let frames = match tauri::async_runtime::spawn_blocking(capture_frames).await {
        Ok(Ok(frames)) => frames,
        Ok(Err(err)) => return Err(err),
        Err(err) => return Err(format!("Screen capture task ended unexpectedly: {}", err)),
    };
    let frames = Arc::new(frames);
    state.install_frames(session_id, frames.clone())?;
    let windows = match create_overlay_windows(&app, session_id, mode, &frames) {
        Ok(windows) => windows,
        Err(err) => {
            let labels = overlay_labels(&windows_from_app(&app, session_id));
            close_overlay_windows_and_wait(&app, &labels).await;
            return Err(err);
        }
    };
    let labels = windows
        .iter()
        .map(|window| window.label().to_string())
        .collect::<Vec<_>>();
    for window in &windows {
        if let Err(err) = window.show() {
            close_overlay_windows_and_wait(&app, &labels).await;
            return Err(format!(
                "Failed to show the screen-selection overlay: {}",
                err
            ));
        }
    }
    if let Some(window) = windows.iter().find(|window| {
        frames
            .iter()
            .find(|frame| {
                window
                    .label()
                    .ends_with(&format!("-{}", frame.monitor_index))
            })
            .map(|frame| frame.primary)
            .unwrap_or(false)
    }) {
        let _ = window.set_focus();
    }

    let selection = match receiver.await {
        Ok(selection) => selection,
        Err(_) => {
            close_overlay_windows_and_wait(&app, &labels).await;
            return Err("Screen-selection session was interrupted unexpectedly".to_string());
        }
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    close_overlay_windows_and_wait(&app, &labels).await;
    Ok((frames, selection))
}

/// Enumerates and freezes every monitor frame; overlays must appear only after this function returns.
fn capture_frames() -> Result<Vec<ScreenFrame>, String> {
    ensure_overlay_platform()?;
    let monitors =
        Monitor::all().map_err(|err| format!("Failed to enumerate monitors: {}", err))?;
    if monitors.is_empty() {
        return Err("No capturable monitors were found".to_string());
    }
    if monitors.len() > MAX_MONITORS {
        return Err(format!("Monitor count cannot exceed {}", MAX_MONITORS));
    }

    let mut declared_pixels = 0u64;
    for monitor in &monitors {
        let width = monitor
            .width()
            .map_err(|err| format!("Failed to read monitor width: {}", err))?;
        let height = monitor
            .height()
            .map_err(|err| format!("Failed to read monitor height: {}", err))?;
        declared_pixels = declared_pixels
            .checked_add(width as u64 * height as u64)
            .ok_or_else(|| "Total monitor pixel count overflowed".to_string())?;
    }
    if declared_pixels > MAX_CAPTURE_PIXELS {
        return Err(format!(
            "Total monitor pixel count exceeds the limit of {}",
            MAX_CAPTURE_PIXELS
        ));
    }

    let mut frames = Vec::with_capacity(monitors.len());
    let mut captured_pixels = 0u64;
    for (monitor_index, monitor) in monitors.into_iter().enumerate() {
        let overlay_x = monitor
            .x()
            .map_err(|err| format!("Failed to read monitor X coordinate: {}", err))?;
        let overlay_y = monitor
            .y()
            .map_err(|err| format!("Failed to read monitor Y coordinate: {}", err))?;
        let overlay_width = monitor
            .width()
            .map_err(|err| format!("Failed to read monitor width: {}", err))?;
        let overlay_height = monitor
            .height()
            .map_err(|err| format!("Failed to read monitor height: {}", err))?;
        let scale_factor = monitor
            .scale_factor()
            .map_err(|err| format!("Failed to read monitor scale factor: {}", err))?;
        let (x, y) = physical_frame_origin(overlay_x, overlay_y, scale_factor)?;
        let primary = monitor
            .is_primary()
            .map_err(|err| format!("Failed to read primary-monitor state: {}", err))?;
        let image = monitor.capture_image().map_err(capture_error)?;
        let width = image.width();
        let height = image.height();
        captured_pixels = captured_pixels
            .checked_add(width as u64 * height as u64)
            .ok_or_else(|| "Total captured-frame pixel count overflowed".to_string())?;
        if captured_pixels > MAX_CAPTURE_PIXELS {
            return Err(format!(
                "Total captured-frame pixel count exceeds the limit of {}",
                MAX_CAPTURE_PIXELS
            ));
        }
        let rgba = image.into_raw();
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "Screen capture byte count overflowed".to_string())?;
        if rgba.len() != expected {
            return Err("Screen capture returned incomplete RGBA data".to_string());
        }
        frames.push(ScreenFrame {
            monitor_index,
            primary,
            x,
            y,
            overlay_x,
            overlay_y,
            overlay_width,
            overlay_height,
            width,
            height,
            rgba,
        });
    }
    Ok(frames)
}

/// Creates a transparent, borderless, always-on-top Tauri overlay for each monitor.
fn create_overlay_windows(
    app: &AppHandle,
    session_id: u64,
    mode: ScreenMode,
    frames: &[ScreenFrame],
) -> Result<Vec<WebviewWindow>, String> {
    let mut windows = Vec::with_capacity(frames.len());
    for frame in frames {
        let label = format!("bt-screen-overlay-{}-{}", session_id, frame.monitor_index);
        let url = format!(
            "bt://app/{}#session_id={}&monitor_index={}&mode={}",
            OVERLAY_ENTRY,
            session_id,
            frame.monitor_index,
            mode.as_str()
        )
        .parse()
        .map_err(|err| format!("Failed to construct the screen-selector URL: {}", err))?;
        let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(url))
            .title("BT Screen Selector")
            .initialization_script(overlay_initialization_script())
            .decorations(false)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .shadow(false)
            .transparent(true)
            .background_color(Color(0, 0, 0, 0))
            .visible(false)
            .focused(false);
        if let Some(storage) = app.try_state::<crate::app::window::WebviewStorageState>() {
            if let Some(path) = storage.path.as_ref() {
                builder = builder.data_directory(path.clone());
            }
        }
        let window = match builder.build() {
            Ok(window) => window,
            Err(err) => {
                let labels = windows
                    .iter()
                    .map(|window: &WebviewWindow| window.label().to_string())
                    .collect::<Vec<_>>();
                close_overlay_windows(app, &labels);
                return Err(format!(
                    "Failed to create the screen-selection overlay: {}",
                    err
                ));
            }
        };
        if let Err(err) = set_overlay_bounds(&window, frame) {
            let _ = window.close();
            close_overlay_windows(app, &overlay_labels(&windows));
            return Err(err);
        }
        let app_handle = app.clone();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed) {
                if let Some(state) = app_handle.try_state::<ScreenState>() {
                    let _ = state.cancel(session_id);
                }
            }
        });
        windows.push(window);
    }
    Ok(windows)
}

/// Configures a monitor overlay using platform coordinate semantics to avoid mixing logical positions with physical capture sizes.
fn set_overlay_bounds(window: &WebviewWindow, frame: &ScreenFrame) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        window
            .set_position(LogicalPosition::new(
                frame.overlay_x as f64,
                frame.overlay_y as f64,
            ))
            .map_err(|err| {
                format!(
                    "Failed to set the screen-selection overlay position: {}",
                    err
                )
            })?;
        window
            .set_size(LogicalSize::new(
                frame.overlay_width as f64,
                frame.overlay_height as f64,
            ))
            .map_err(|err| format!("Failed to set the screen-selection overlay size: {}", err))?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        window
            .set_position(PhysicalPosition::new(frame.overlay_x, frame.overlay_y))
            .map_err(|err| {
                format!(
                    "Failed to set the screen-selection overlay position: {}",
                    err
                )
            })?;
        window
            .set_size(PhysicalSize::new(frame.overlay_width, frame.overlay_height))
            .map_err(|err| format!("Failed to set the screen-selection overlay size: {}", err))?;
    }
    Ok(())
}

/// Returns overlay windows for the specified session that remain in the Tauri manager.
fn windows_from_app(app: &AppHandle, session_id: u64) -> Vec<WebviewWindow> {
    let prefix = format!("bt-screen-overlay-{}-", session_id);
    app.webview_windows()
        .into_values()
        .filter(|window| window.label().starts_with(&prefix))
        .collect()
}

/// Extracts labels from a window list.
fn overlay_labels(windows: &[WebviewWindow]) -> Vec<String> {
    windows
        .iter()
        .map(|window| window.label().to_string())
        .collect()
}

/// Closes every overlay window for the current session.
fn close_overlay_windows(app: &AppHandle, labels: &[String]) {
    for label in labels {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.close();
        }
    }
}

/// Closes all overlays and waits a bounded time for the Tauri manager to remove stale windows.
async fn close_overlay_windows_and_wait(app: &AppHandle, labels: &[String]) {
    close_overlay_windows(app, labels);
    for _ in 0..20 {
        if labels
            .iter()
            .all(|label| app.get_webview_window(label).is_none())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Returns an explicit platform error for native Wayland sessions where fullscreen overlays cannot be positioned reliably.
fn ensure_overlay_platform() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let gtk_backend = std::env::var("GDK_BACKEND").unwrap_or_default();
        if session.eq_ignore_ascii_case("wayland") && !gtk_backend.eq_ignore_ascii_case("x11") {
            return Err(
                "Native Wayland screen overlays are not supported in this version; run under an X11/XWayland session".to_string(),
            );
        }
    }
    Ok(())
}

/// Adds actionable guidance for macOS screen-recording permission failures and preserves the underlying error elsewhere.
fn capture_error(err: impl std::fmt::Display) -> String {
    #[cfg(target_os = "macos")]
    {
        return format!(
            "Failed to capture the monitor: {}; allow this application to record the screen in System Settings",
            err
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        format!("Failed to capture the monitor: {}", err)
    }
}

/// Converts xcap's logical monitor origin to the physical-pixel origin used by frozen frames.
fn physical_frame_origin(x: i32, y: i32, scale_factor: f32) -> Result<(i32, i32), String> {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return Err("Monitor scale factor is invalid".to_string());
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let scale = scale_factor as f64;
        let scaled_x = (x as f64 * scale).round();
        let scaled_y = (y as f64 * scale).round();
        if scaled_x < i32::MIN as f64
            || scaled_x > i32::MAX as f64
            || scaled_y < i32::MIN as f64
            || scaled_y > i32::MAX as f64
        {
            return Err("Physical monitor coordinates overflowed".to_string());
        }
        return Ok((scaled_x as i32, scaled_y as i32));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok((x, y))
    }
}

/// Validates and returns the current session.
fn current_session(inner: &ScreenStateInner, session_id: u64) -> Result<&ScreenSession, String> {
    let session = inner
        .session
        .as_ref()
        .ok_or_else(|| "Screen-selection session has ended".to_string())?;
    if session.session_id != session_id {
        return Err("Screen-selection session has expired".to_string());
    }
    Ok(session)
}

/// Validates and returns the current session mutably.
fn current_session_mut(
    inner: &mut ScreenStateInner,
    session_id: u64,
) -> Result<&mut ScreenSession, String> {
    let session = inner
        .session
        .as_mut()
        .ok_or_else(|| "Screen-selection session has ended".to_string())?;
    if session.session_id != session_id {
        return Err("Screen-selection session has expired".to_string());
    }
    Ok(session)
}

/// Returns the captured frames installed in the session.
fn session_frames(session: &ScreenSession) -> Result<&Arc<Vec<ScreenFrame>>, String> {
    session
        .frames
        .as_ref()
        .ok_or_else(|| "Screen capture is not complete".to_string())
}

/// Returns a frozen frame by its session-local index.
fn frame_by_index(frames: &[ScreenFrame], monitor_index: usize) -> Result<&ScreenFrame, String> {
    frames
        .get(monitor_index)
        .filter(|frame| frame.monitor_index == monitor_index)
        .ok_or_else(|| "Monitor index is invalid".to_string())
}

/// Sends the completed result but retains the session until every overlay window closes.
fn finish_session(inner: &mut ScreenStateInner, selection: ScreenSelection) -> Result<(), String> {
    let session = inner
        .session
        .as_mut()
        .ok_or_else(|| "Screen-selection session has ended".to_string())?;
    let sender = session
        .sender
        .take()
        .ok_or_else(|| "Screen-selection completion channel is unavailable".to_string())?;
    sender
        .send(Some(selection))
        .map_err(|_| "Screen-selection result receiver is closed".to_string())
}

/// Default value for Serde boolean parameters.
fn default_true() -> bool {
    true
}

/// Built-in transparent screen-selector page.
/// Minimal restricted invocation bridge captured before Tauri hides its internal objects.
const OVERLAY_INITIALIZATION_SCRIPT: &str = r#"
(() => {
  const internals = window.__TAURI_INTERNALS__;
  if (!internals?.invoke) return;
  const invoke = internals.invoke.bind(internals);
  const allowed = new Set([
    "screen_overlay_sample",
    "screen_overlay_pick",
    "screen_overlay_capture",
    "screen_overlay_cancel"
  ]);
  Object.defineProperty(window, "__BT_SCREEN_OVERLAY__", {
    value: Object.freeze({
      invoke(command, args = {}) {
        if (!allowed.has(command)) throw new Error("Screen overlay command is not allowed");
        return invoke(command, args);
      }
    }),
    configurable: false,
    enumerable: false,
    writable: false
  });
})();
"#;

const OVERLAY_HTML: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<style>
*{box-sizing:border-box}html,body{width:100%;height:100%;margin:0;overflow:hidden;user-select:none;font-family:"Microsoft YaHei","Segoe UI",system-ui,sans-serif}body{cursor:crosshair;background:rgba(3,8,20,.12)}body.capture_area{background:rgba(3,8,20,.32)}
#hint{position:fixed;top:18px;left:50%;transform:translateX(-50%);z-index:5;padding:9px 14px;border:1px solid rgba(255,255,255,.18);border-radius:9px;background:rgba(12,18,31,.86);box-shadow:0 8px 28px rgba(0,0,0,.26);color:#fff;font-size:13px;letter-spacing:.2px;pointer-events:none;white-space:nowrap;backdrop-filter:blur(10px)}
#cross{position:fixed;display:none;z-index:4;width:22px;height:22px;transform:translate(-11px,-11px);border:1px solid rgba(255,255,255,.95);border-radius:50%;box-shadow:0 0 0 1px rgba(0,0,0,.8),0 3px 12px rgba(0,0,0,.35);pointer-events:none}#cross:before,#cross:after{content:"";position:absolute;background:#fff;box-shadow:0 0 0 1px rgba(0,0,0,.7)}#cross:before{left:10px;top:-8px;width:1px;height:38px}#cross:after{left:-8px;top:10px;width:38px;height:1px}
#badge{position:fixed;display:none;z-index:6;min-width:142px;padding:9px 10px;border-radius:9px;background:rgba(12,18,31,.9);box-shadow:0 8px 28px rgba(0,0,0,.3);color:#fff;font:600 13px/1.25 ui-monospace,SFMono-Regular,Consolas,monospace;pointer-events:none}.badge-row{display:flex;align-items:center;gap:8px}.swatch{width:24px;height:24px;border:2px solid #fff;border-radius:6px;box-shadow:0 0 0 1px rgba(0,0,0,.45)}#coords{margin-top:4px;color:#aeb9ca;font-size:11px;font-weight:400}
#selection{position:fixed;display:none;z-index:3;border:1px solid #7dd3fc;background:rgba(125,211,252,.08);box-shadow:0 0 0 99999px rgba(3,8,20,.46),0 0 0 1px rgba(2,132,199,.8),0 8px 22px rgba(0,0,0,.25);pointer-events:none}#size{position:absolute;right:0;bottom:-30px;padding:5px 8px;border-radius:6px;background:rgba(12,18,31,.9);color:#fff;font:12px/1 ui-monospace,SFMono-Regular,Consolas,monospace;white-space:nowrap}
</style>
</head>
<body>
<div id="hint"></div><div id="cross"></div><div id="badge"><div class="badge-row"><span class="swatch"></span><span id="hex">#000000</span></div><div id="coords"></div></div><div id="selection"><span id="size"></span></div>
<script>
(()=>{
  const params=new URLSearchParams(location.hash.slice(1));
  const sessionId=Number(params.get("session_id"));
  const monitorIndex=Number(params.get("monitor_index"));
  const mode=params.get("mode")==="capture_area"?"capture_area":"pick_color";
  const invoke=(command,args={})=>window.__BT_SCREEN_OVERLAY__.invoke(command,args);
  const hint=document.querySelector("#hint");
  const cross=document.querySelector("#cross");
  const badge=document.querySelector("#badge");
  const swatch=document.querySelector(".swatch");
  const hex=document.querySelector("#hex");
  const coords=document.querySelector("#coords");
  const selection=document.querySelector("#selection");
  const size=document.querySelector("#size");
  document.body.classList.add(mode);
  hint.textContent=mode==="pick_color"?"Click to pick a color · Right-click or press Esc to cancel":"Drag to capture an area · Right-click or press Esc to cancel";
  let busy=false;
  let dragStart=null;
  let pendingSample=null;
  let sampling=false;

  const ratio=()=>window.devicePixelRatio||1;
  const clamp=(value,min,max)=>Math.min(max,Math.max(min,value));
  const physicalPoint=(event)=>({
    x:clamp(Math.floor(event.clientX*ratio()),0,Math.max(0,Math.floor(innerWidth*ratio())-1)),
    y:clamp(Math.floor(event.clientY*ratio()),0,Math.max(0,Math.floor(innerHeight*ratio())-1))
  });
  const placeBadge=(event)=>{
    const left=event.clientX+18;
    const top=event.clientY+22;
    badge.style.left=`${Math.min(left,innerWidth-158)}px`;
    badge.style.top=`${Math.min(top,innerHeight-76)}px`;
  };
  const cancel=async()=>{
    if(busy)return;
    busy=true;
    try{await invoke("screen_overlay_cancel",{sessionId});}catch(error){hint.textContent=String(error);busy=false;}
  };
  const scheduleSample=(event)=>{
    pendingSample={point:physicalPoint(event),clientX:event.clientX,clientY:event.clientY};
    if(sampling)return;
    sampling=true;
    requestAnimationFrame(async()=>{
      const current=pendingSample;
      pendingSample=null;
      try{
        const color=await invoke("screen_overlay_sample",{sessionId,monitorIndex,x:current.point.x,y:current.point.y});
        swatch.style.background=color.hex;
        hex.textContent=color.hex;
        coords.textContent=`${color.x}, ${color.y} · ${color.rgb}`;
      }catch(error){hint.textContent=String(error);}
      sampling=false;
      if(pendingSample)scheduleSample({clientX:pendingSample.clientX,clientY:pendingSample.clientY});
    });
  };
  const drawSelection=(endX,endY)=>{
    const left=Math.min(dragStart.x,endX);
    const top=Math.min(dragStart.y,endY);
    const width=Math.abs(endX-dragStart.x);
    const height=Math.abs(endY-dragStart.y);
    selection.style.display="block";
    selection.style.left=`${left}px`;selection.style.top=`${top}px`;selection.style.width=`${width}px`;selection.style.height=`${height}px`;
    size.textContent=`${Math.round(width*ratio())} × ${Math.round(height*ratio())}`;
  };

  addEventListener("pointermove",event=>{
    cross.style.display="block";cross.style.left=`${event.clientX}px`;cross.style.top=`${event.clientY}px`;
    if(mode==="pick_color"){
      badge.style.display="block";placeBadge(event);scheduleSample(event);
    }else if(dragStart){drawSelection(event.clientX,event.clientY);}
  });
  addEventListener("pointerdown",event=>{
    if(event.button!==0||busy)return;
    if(mode==="capture_area"){
      dragStart={x:event.clientX,y:event.clientY};
      drawSelection(event.clientX,event.clientY);
      event.target.setPointerCapture?.(event.pointerId);
    }
  });
  addEventListener("pointerup",async event=>{
    if(event.button!==0||busy)return;
    if(mode==="pick_color"){
      busy=true;
      const point=physicalPoint(event);
      try{await invoke("screen_overlay_pick",{sessionId,monitorIndex,x:point.x,y:point.y});}catch(error){hint.textContent=String(error);busy=false;}
      return;
    }
    if(!dragStart)return;
    const scale=ratio();
    const endX=clamp(event.clientX,0,innerWidth);
    const endY=clamp(event.clientY,0,innerHeight);
    const left=Math.min(dragStart.x,endX);
    const top=Math.min(dragStart.y,endY);
    const x=clamp(Math.floor(left*scale),0,Math.max(0,Math.floor(innerWidth*scale)-1));
    const y=clamp(Math.floor(top*scale),0,Math.max(0,Math.floor(innerHeight*scale)-1));
    const width=Math.min(Math.floor(innerWidth*scale)-x,Math.round(Math.abs(endX-dragStart.x)*scale));
    const height=Math.min(Math.floor(innerHeight*scale)-y,Math.round(Math.abs(endY-dragStart.y)*scale));
    if(width<2||height<2){dragStart=null;selection.style.display="none";return;}
    busy=true;
    try{await invoke("screen_overlay_capture",{sessionId,monitorIndex,x,y,width,height});}catch(error){hint.textContent=String(error);busy=false;}
  });
  addEventListener("keydown",event=>{if(event.key==="Escape"){event.preventDefault();cancel();}},true);
  addEventListener("contextmenu",event=>{event.preventDefault();cancel();},true);
})();
</script>
</body>
</html>"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Color results keep the RGB value, RGBA bytes, and uppercase hexadecimal text consistent.
    #[test]
    fn color_result_formats_are_consistent() {
        let frame = ScreenFrame {
            monitor_index: 0,
            primary: true,
            x: -20,
            y: 10,
            overlay_x: -20,
            overlay_y: 10,
            overlay_width: 2,
            overlay_height: 1,
            width: 2,
            height: 1,
            rgba: vec![0x12, 0x34, 0x56, 0xff, 1, 2, 3, 4],
        };

        let color = frame.color_at(0, 0, false).unwrap();
        assert_eq!(color.x, -20);
        assert_eq!(color.y, 10);
        assert_eq!(color.rgb, 0x123456);
        assert_eq!(color.hex, "#123456");
        assert_eq!(color.rgba, [0x12, 0x34, 0x56, 0xff]);
    }

    /// Cropping copies tightly packed RGBA rows and preserves negative virtual-desktop coordinates.
    #[test]
    fn crop_copies_exact_rgba_rows() {
        let frame = ScreenFrame {
            monitor_index: 0,
            primary: true,
            x: -100,
            y: -50,
            overlay_x: -100,
            overlay_y: -50,
            overlay_width: 3,
            overlay_height: 2,
            width: 3,
            height: 2,
            rgba: vec![
                1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255,
            ],
        };

        let area = frame.crop(1, 0, 2, 2).unwrap();
        assert_eq!((area.x, area.y, area.width, area.height), (-99, -50, 2, 2));
        assert_eq!(
            area.rgba,
            vec![2, 0, 0, 255, 3, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255]
        );
    }

    /// Area validation rejects zero-sized, undersized, and out-of-bounds rectangles.
    #[test]
    fn area_validation_rejects_invalid_rectangles() {
        let frame = ScreenFrame {
            monitor_index: 0,
            primary: true,
            x: 0,
            y: 0,
            overlay_x: 0,
            overlay_y: 0,
            overlay_width: 10,
            overlay_height: 10,
            width: 10,
            height: 10,
            rgba: vec![0; 10 * 10 * 4],
        };

        assert!(frame.validate_area(0, 0, 1, 2).is_err());
        assert!(frame.validate_area(9, 9, 2, 2).is_err());
        assert!(frame.validate_area(2, 3, 4, 5).is_ok());
    }

    /// Sessions are reserved before capture and remain busy after cancellation until overlay cleanup completes.
    #[test]
    fn session_reservation_stays_busy_until_end() {
        let state = ScreenState::new();
        let (session_id, receiver) = state.begin(ScreenMode::PickColor).unwrap();

        assert!(state.begin(ScreenMode::CaptureArea).is_err());
        assert!(state.cancel(session_id).unwrap());
        assert!(state.begin(ScreenMode::CaptureArea).is_err());
        assert!(receiver.blocking_recv().unwrap().is_none());
        assert!(state.end(session_id).unwrap());

        let (_, next_receiver) = state.begin(ScreenMode::CaptureArea).unwrap();
        drop(next_receiver);
    }

    /// The built-in overlay page references only restricted internal commands.
    #[test]
    fn overlay_html_contains_screen_commands() {
        assert!(OVERLAY_HTML.contains("screen_overlay_sample"));
        assert!(OVERLAY_HTML.contains("screen_overlay_pick"));
        assert!(OVERLAY_HTML.contains("screen_overlay_capture"));
        assert!(OVERLAY_HTML.contains("screen_overlay_cancel"));
        assert!(!OVERLAY_HTML.contains("window.bt"));
        assert!(!OVERLAY_HTML.contains("window.__TAURI_INTERNALS__"));
        assert!(OVERLAY_INITIALIZATION_SCRIPT.contains("__BT_SCREEN_OVERLAY__"));
        assert!(!OVERLAY_INITIALIZATION_SCRIPT.contains("window.bt"));
    }
}
