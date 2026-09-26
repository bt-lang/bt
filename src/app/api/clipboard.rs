//! Desktop API implementation for `bt.clipboard`.

/// Execute one clipboard operation on GTK's owning context and await its result without blocking it.
#[cfg(target_os = "linux")]
async fn on_clipboard<T: Send + 'static>(
    app: AppHandle,
    operation: impl FnOnce(gtk::Clipboard, tokio::sync::oneshot::Sender<Result<T, String>>)
        + Send
        + 'static,
) -> Result<T, String> {
    let (send, receive) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        operation(gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD), send);
    })
    .map_err(|error| error.to_string())?;
    tokio::time::timeout(std::time::Duration::from_secs(5), receive)
        .await
        .map_err(|_| "Clipboard operation timed out")?
        .map_err(|error| error.to_string())?
}

/// Read native Wayland clipboard text using GTK's asynchronous selection request.
#[cfg(target_os = "linux")]
pub async fn read_text(app: AppHandle) -> Result<String, String> {
    on_clipboard(app, |clipboard, send| {
        clipboard.request_text(move |_, text| {
            let _ = send.send(Ok(text.unwrap_or_default().to_string()));
        });
    })
    .await
}

/// Publish text through the current Wayland input serial, retaining GTK ownership.
#[cfg(target_os = "linux")]
pub async fn write_text(app: AppHandle, text: String) -> Result<(), String> {
    on_clipboard(app, move |clipboard, send| {
        clipboard.set_text(&text);
        let _ = send.send(Ok(()));
    })
    .await
}

/// Clear this application's native selection without creating a compatibility backend.
#[cfg(target_os = "linux")]
pub async fn clear(app: AppHandle) -> Result<(), String> {
    on_clipboard(app, |clipboard, send| {
        clipboard.clear();
        let _ = send.send(Ok(()));
    })
    .await
}

/// Publish an owned RGBA image through the platform's native clipboard.
pub async fn write_image(
    app: AppHandle,
    image: tauri::image::Image<'static>,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        return on_clipboard(app, move |clipboard, send| {
            let bytes = gtk::glib::Bytes::from_owned(image.rgba().to_vec());
            let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_bytes(
                &bytes,
                gtk::gdk_pixbuf::Colorspace::Rgb,
                true,
                8,
                image.width() as i32,
                image.height() as i32,
                image.width() as i32 * 4,
            );
            clipboard.set_image(&pixbuf);
            let _ = send.send(Ok(()));
        })
        .await;
    }
    #[cfg(not(target_os = "linux"))]
    tauri::async_runtime::spawn_blocking(move || {
        app.clipboard()
            .write_image(&image)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(not(target_os = "linux"))]
use crate::app::api::map_error;
use tauri::AppHandle;
#[cfg(not(target_os = "linux"))]
use tauri_plugin_clipboard_manager::ClipboardExt;

/// Reads plain text from the clipboard.
#[cfg(not(target_os = "linux"))]
pub async fn read_text(app: AppHandle) -> Result<String, String> {
    match app.clipboard().read_text() {
        Ok(text) => Ok(text),
        Err(err) => {
            let message = err.to_string();
            if message.contains("not available") || message.contains("clipboard is empty") {
                Ok(String::new())
            } else {
                Err(map_error("Read clipboard text", message))
            }
        }
    }
}

/// Writes plain text to the clipboard.
#[cfg(not(target_os = "linux"))]
pub async fn write_text(app: AppHandle, text: String) -> Result<(), String> {
    app.clipboard()
        .write_text(text)
        .map_err(|err| map_error("Write clipboard text", err))
}

/// Clears the clipboard.
#[cfg(not(target_os = "linux"))]
pub async fn clear(app: AppHandle) -> Result<(), String> {
    app.clipboard()
        .clear()
        .map_err(|err| map_error("Clear clipboard", err))
}
