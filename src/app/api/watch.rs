//! Desktop file-watching API implementation for `bt.app.watch_path`.

use crate::app::api::{absolute_path_text, WATCH_PATH_EVENT};
use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::path::PathBuf;
use tauri::{Runtime, WebviewWindow};

/// Desktop file watcher handle.
pub struct PathWatcher {
    /// notify watcher; keeping this value alive keeps the watch active.
    _watcher: RecommendedWatcher,
    /// Current watched root directory.
    root: PathBuf,
}

/// File watcher event payload.
#[derive(Clone, Debug, Serialize)]
struct WatchPathPayload {
    /// Stable ID distinguishing concurrent watchers in the same window.
    watch_id: String,
    /// Current watched root directory.
    root: String,
    /// Event kind.
    kind: String,
    /// Paths involved in the event.
    paths: Vec<String>,
}

impl PathWatcher {
    /// Creates a new directory watcher.
    pub fn new<R: Runtime>(
        window: WebviewWindow<R>,
        watch_id: String,
        root: PathBuf,
        recursive: bool,
    ) -> Result<Self, String> {
        if !root.exists() {
            return Err("Watch path does not exist".to_string());
        }
        if !root.is_dir() {
            return Err("Only directory paths can be watched".to_string());
        }

        let root_text = absolute_path_text(root.clone())?;
        let event_window = window.clone();
        let mut watcher = RecommendedWatcher::new(
            move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                let payload = WatchPathPayload {
                    watch_id: watch_id.clone(),
                    root: root_text.clone(),
                    kind: event_kind_text(&event.kind).to_string(),
                    paths: event
                        .paths
                        .into_iter()
                        .filter_map(|path| absolute_path_text(path).ok())
                        .collect(),
                };
                dispatch_watch_path_event(&event_window, payload);
            },
            Config::default(),
        )
        .map_err(|err| format!("Failed to create file watcher: {}", err))?;

        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher
            .watch(&root, mode)
            .map_err(|err| format!("Failed to watch directory `{}`: {}", root.display(), err))?;

        Ok(Self {
            _watcher: watcher,
            root,
        })
    }
}

impl Drop for PathWatcher {
    /// Explicitly stops watching the root directory when the watcher is dropped.
    fn drop(&mut self) {
        let _ = self._watcher.unwatch(&self.root);
    }
}

/// Maps notify event kinds to stable frontend strings.
fn event_kind_text(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder) => "create",
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Any | RenameMode::Both | RenameMode::From | RenameMode::To,
        )) => "rename",
        EventKind::Modify(_) => "modify",
        EventKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder) => "remove",
        _ => "other",
    }
}

/// Dispatches a file watcher DOM event to the page.
fn dispatch_watch_path_event<R: Runtime>(window: &WebviewWindow<R>, payload: WatchPathPayload) {
    let Ok(payload) = serde_json::to_string(&payload) else {
        return;
    };
    let Ok(event) = serde_json::to_string(WATCH_PATH_EVENT) else {
        return;
    };
    let _ = window.eval(format!(
        "window.dispatchEvent(new CustomEvent({}, {{ detail: {} }}));",
        event, payload
    ));
}
