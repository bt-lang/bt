//! Desktop API implementation for `bt.drag`.

use crate::app::api::{absolute_path_text, DRAG_FILES_EVENT};
use crate::permission::{self, Capability};
use serde::Serialize;
use tauri::{DragDropEvent, Runtime, WebviewEvent, WebviewWindow};

/// Payload for a file-drop event.
#[derive(Clone, Debug, Serialize)]
pub struct DragFilesPayload {
    /// Absolute paths of files or directories dropped onto the window.
    pub paths: Vec<String>,
}

/// Registers a file-drop event forwarder for the main window.
pub fn attach_drag_handler<R: Runtime>(window: &WebviewWindow<R>) {
    let event_window = window.clone();
    window.on_webview_event(move |event| {
        let WebviewEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event else {
            return;
        };
        if permission::check(Capability::Desktop).is_err() {
            return;
        }
        let paths = paths
            .iter()
            .cloned()
            .filter_map(|path| absolute_path_text(path).ok())
            .collect::<Vec<_>>();
        dispatch_drag_files_event(&event_window, DragFilesPayload { paths });
    });
}

/// Dispatches a file-drop DOM event to the page.
fn dispatch_drag_files_event<R: Runtime>(window: &WebviewWindow<R>, payload: DragFilesPayload) {
    let Ok(payload) = serde_json::to_string(&payload) else {
        return;
    };
    let Ok(event) = serde_json::to_string(DRAG_FILES_EVENT) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent({}, {{ detail: {} }}));",
        event, payload
    ));
}
