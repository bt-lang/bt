//! Shared asynchronous Wayland portal transport with bounded consent and cleanup.

use glib::variant::ToVariant;
use gtk::{gio, glib};
use std::{cell::RefCell, collections::BTreeMap, time::Duration};
use tauri::Manager;
use tokio::sync::oneshot;

/// Desktop portal bus name.
pub(super) const SERVICE: &str = "org.freedesktop.portal.Desktop";
/// Desktop portal object path.
pub(super) const PATH: &str = "/org/freedesktop/portal/desktop";
/// D-Bus dictionaries for options and responses.
pub(super) type Properties = BTreeMap<String, glib::Variant>;

/// Keep at most one consent request alive even if its invoking WebView is destroyed.
static CAPTURE_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Capture the compositor through its Screenshot portal without an X11 connection.
pub(super) async fn capture(
    app: tauri::AppHandle,
    pixel_limit: u64,
) -> Result<Vec<super::screen::ScreenFrame>, String> {
    let permit = CAPTURE_SLOT
        .try_acquire()
        .map_err(|_| "Another screenshot request is pending")?;
    let (send, receive) = oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        glib::MainContext::default().spawn_local(async move {
            let _permit = permit;
            let result = screenshot_request(&handle).await;
            if let Err(Ok((path, _))) = send.send(result) {
                let _ = std::fs::remove_file(path);
            }
        });
    })
    .map_err(|error| error.to_string())?;
    let (path, monitors) = receive.await.map_err(|error| error.to_string())??;
    tauri::async_runtime::spawn_blocking(move || decode_screenshot(path, monitors, pixel_limit))
        .await
        .map_err(|error| error.to_string())?
}

/// Monitor rectangles in compositor logical coordinates, plus the primary flag.
type Monitors = Vec<(i32, i32, u32, u32, bool)>;

/// Ask once per capture and gather monitor geometry from the native Wayland display.
async fn screenshot_request(
    app: &tauri::AppHandle,
) -> Result<(std::path::PathBuf, Monitors), String> {
    use gtk::prelude::*;
    let display = gtk::gdk::Display::default().ok_or("Wayland display is unavailable")?;
    let primary = display.primary_monitor();
    let mut monitors = Vec::new();
    for index in 0..display.n_monitors() {
        let monitor = display.monitor(index).ok_or("Monitor disappeared")?;
        let rect = monitor.geometry();
        monitors.push((
            rect.x(),
            rect.y(),
            rect.width() as u32,
            rect.height() as u32,
            primary.as_ref() == Some(&monitor),
        ));
    }
    if monitors.is_empty() || monitors.len() > 16 {
        return Err("Expected 1-16 Wayland monitors".into());
    }
    let connection = connect(app).await?;
    let token = format!("bt_capture_{}", uuid::Uuid::new_v4().simple());
    let options = Properties::from([
        ("handle_token".into(), token.to_variant()),
        ("interactive".into(), false.to_variant()),
        ("modal".into(), true.to_variant()),
    ]);
    let values = request(
        &connection,
        "org.freedesktop.portal.Screenshot",
        "Screenshot",
        &token,
        ("", options).to_variant(),
    )
    .await?;
    let uri = values
        .get("uri")
        .and_then(|value| value.get::<String>())
        .ok_or("Screenshot portal omitted the image URI")?;
    let path = url::Url::parse(&uri)
        .map_err(|error| error.to_string())?
        .to_file_path()
        .map_err(|_| "Screenshot portal returned a non-local image")?;
    Ok((path, monitors))
}

/// Decode under the existing image budget, crop monitor frames, and delete the portal temporary file.
fn decode_screenshot(
    path: std::path::PathBuf,
    monitors: Monitors,
    pixel_limit: u64,
) -> Result<Vec<super::screen::ScreenFrame>, String> {
    let result = (|| {
        let mut reader = image::ImageReader::open(&path)
            .map_err(|error| error.to_string())?
            .with_guessed_format()
            .map_err(|error| error.to_string())?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(pixel_limit * 4);
        limits.max_image_width = Some(32768);
        limits.max_image_height = Some(32768);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| error.to_string())?
            .to_rgba8();
        if u64::from(image.width()) * u64::from(image.height()) > pixel_limit {
            return Err("Screenshot exceeds the image budget".into());
        }
        let left = monitors.iter().map(|m| m.0).min().unwrap();
        let top = monitors.iter().map(|m| m.1).min().unwrap();
        let right = monitors
            .iter()
            .map(|m| i64::from(m.0) + i64::from(m.2))
            .max()
            .unwrap();
        let bottom = monitors
            .iter()
            .map(|m| i64::from(m.1) + i64::from(m.3))
            .max()
            .unwrap();
        let scale_x = image.width() as f64 / (right - i64::from(left)) as f64;
        let scale_y = image.height() as f64 / (bottom - i64::from(top)) as f64;
        if !scale_x.is_finite() || !scale_y.is_finite() || (scale_x - scale_y).abs() > 0.02 {
            return Err("Screenshot dimensions do not match the Wayland desktop layout".into());
        }
        let mut frames = Vec::with_capacity(monitors.len());
        let mut pixels = 0u64;
        for (index, (x, y, width, height, primary)) in monitors.into_iter().enumerate() {
            let px = ((x - left) as f64 * scale_x).round() as u32;
            let py = ((y - top) as f64 * scale_y).round() as u32;
            let w = (width as f64 * scale_x).round() as u32;
            let h = (height as f64 * scale_y).round() as u32;
            pixels += u64::from(w) * u64::from(h);
            if pixels > pixel_limit
                || px.saturating_add(w) > image.width()
                || py.saturating_add(h) > image.height()
            {
                return Err("Monitor image exceeds capture bounds".into());
            }
            frames.push(super::screen::ScreenFrame {
                monitor_index: index,
                primary,
                x: (x as f64 * scale_x).round() as i32,
                y: (y as f64 * scale_y).round() as i32,
                overlay_x: x,
                overlay_y: y,
                overlay_width: width,
                overlay_height: height,
                width: w,
                height: h,
                rgba: image::imageops::crop_imm(&image, px, py, w, h)
                    .to_image()
                    .into_raw(),
            });
        }
        Ok(frames)
    })();
    let _ = std::fs::remove_file(path);
    result
}

/// Owns a subscription and releases it on errors, cancellation, or session replacement.
pub(super) struct Subscription {
    /// Connection that owns this signal match.
    pub(super) connection: gio::DBusConnection,
    /// Exactly one live subscription, consumed during cleanup.
    pub(super) id: Option<gio::SignalSubscriptionId>,
}

impl Drop for Subscription {
    /// Removes the signal match before its captured state can outlive the session.
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.connection.signal_unsubscribe(id);
        }
    }
}

/// Calls a portal method with a bounded transport timeout.
pub(super) async fn call(
    connection: &gio::DBusConnection,
    path: &str,
    interface: &str,
    method: &str,
    args: glib::Variant,
) -> Result<glib::Variant, String> {
    connection
        .call_future(
            Some(SERVICE),
            path,
            interface,
            method,
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            5000,
        )
        .await
        .map_err(|error| error.to_string())
}

/// Subscribes before dispatch so a fast Response signal cannot race the method reply.
pub(super) async fn request(
    connection: &gio::DBusConnection,
    interface: &str,
    method: &str,
    token: &str,
    args: glib::Variant,
) -> Result<Properties, String> {
    let sender = connection
        .unique_name()
        .ok_or("Portal connection has no unique name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let (send, receive) = oneshot::channel();
    let send = RefCell::new(Some(send));
    let subscription = connection.signal_subscribe(
        Some(SERVICE),
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&path),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, value| {
            if let Some(send) = send.borrow_mut().take() {
                let _ = send.send(
                    value
                        .get::<(u32, Properties)>()
                        .ok_or_else(|| "Invalid portal response".to_string()),
                );
            }
        },
    );
    let _subscription = Subscription {
        connection: connection.clone(),
        id: Some(subscription),
    };
    call(connection, PATH, interface, method, args).await?;
    let response = glib::future_with_timeout(Duration::from_secs(90), receive).await;
    let result = match response {
        Ok(Ok(result)) => result,
        _ => Err("Desktop authorization timed out".into()),
    };
    connection.call(
        Some(SERVICE),
        &path,
        "org.freedesktop.portal.Request",
        "Close",
        None,
        None,
        gio::DBusCallFlags::NONE,
        3000,
        gio::Cancellable::NONE,
        |_| {},
    );
    let (status, values) = result?;
    if status != 0 {
        return Err("Desktop permission was denied or cancelled".into());
    }
    Ok(values)
}

/// Uses a private D-Bus peer so registration cannot conflict with file-picker portal calls.
pub(super) async fn connect(app: &tauri::AppHandle) -> Result<gio::DBusConnection, String> {
    let app_id = {
        let state = app.state::<crate::app::runtime::AppState>();
        let runtime = state.lock_runtime().map_err(|error| error.to_string())?;
        runtime.config.app.id.clone()
    };
    if gio::DesktopAppInfo::new(&format!("{app_id}.desktop")).is_none() {
        return Err(format!(
            "Desktop portals require an installed {app_id}.desktop launcher"
        ));
    }
    let address = gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|error| error.to_string())?;
    let connection = gio::DBusConnection::for_address_future(
        &address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
    )
    .await
    .map_err(|error| error.to_string())?;
    call(
        &connection,
        PATH,
        "org.freedesktop.host.portal.Registry",
        "Register",
        (app_id, Properties::new()).to_variant(),
    )
    .await?;
    Ok(connection)
}
