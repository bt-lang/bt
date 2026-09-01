/// BT frontend bridge script injected into the WebView.
///
/// Only `window.bt` is exposed; page code neither needs nor should use the Tauri JS API directly.
pub fn script(enable_refresh_shortcuts: bool, enable_devtools_shortcuts: bool) -> String {
    let mut script = String::from(
        r#"
(() => {
  if (window.bt) {
    return;
  }

  /** Internal Tauri IPC object used by the BT desktop bridge. */
  const internals = window.__TAURI_INTERNALS__;

  /** Remove undefined arguments so Tauri deserialization does not receive invalid fields. */
  const cleanArgs = (args) => {
    const output = {};
    for (const key of Object.keys(args || {})) {
      if (args[key] !== undefined) {
        output[key] = args[key];
      }
    }
    return output;
  };

  /** Invoke a BT desktop command on the Rust side. */
  const invoke = async (command, args = {}) => {
    if (!internals || !internals.invoke) {
      throw new Error("BT desktop bridge is unavailable");
    }
    return await internals.invoke(command, cleanArgs(args));
  };

  /** Create a DOM event listener wrapper that synchronously returns an unsubscribe function. */
  const listen = (event, callback, mapPayload) => {
    if (typeof callback !== "function") {
      throw new Error("Event callback must be a function");
    }
    const handler = (message) => {
      callback(mapPayload ? mapPayload(message.detail) : message.detail);
    };
    window.addEventListener(event, handler);
    return () => {
      window.removeEventListener(event, handler);
    };
  };

  /**
   * Subscribe to native `emit_to` events through the Tauri event plugin and synchronously return a cancelable handle.
   * If canceled before registration completes, ready unregisters as soon as it receives the event ID.
   */
  const listenTauri = (event, callback, mapPayload) => {
    if (typeof callback !== "function") {
      throw new Error("Event callback must be a function");
    }
    let active = true;
    let eventId = null;
    const handler = internals.transformCallback((message) => {
      if (active) {
        callback(mapPayload ? mapPayload(message.payload) : message.payload);
      }
    });
    const ready = invoke("plugin:event|listen", {
      event,
      target: { kind: "Any" },
      handler
    }).then((id) => {
      eventId = id;
      if (!active) {
        return invoke("plugin:event|unlisten", { event, eventId: id })
          .finally(() => internals.unregisterCallback(handler));
      }
      return id;
    }).catch((error) => {
      active = false;
      internals.unregisterCallback(handler);
      throw error;
    });
    const off = () => {
      if (!active) {
        return;
      }
      active = false;
      if (eventId !== null) {
        void invoke("plugin:event|unlisten", { event, eventId })
          .catch(() => {})
          .finally(() => internals.unregisterCallback(handler));
      } else {
        void ready.catch(() => {});
      }
    };
    off.ready = ready;
    return off;
  };

  /** Bounded sequence used by this page to generate file watcher IDs. */
  let watchSequence = 0;

  /** Global shortcut listeners stored by stable ID for this page. */
  const shortcutListeners = new Map();

  /** Global BT desktop API object. */
  window.bt = {
    /** Call a BT global function registered in app.main. */
    call(name, ...args) {
      return invoke("bt_call", { name, args });
    },
    /** Project commands for the setup page. */
    project: {
      create(input) {
        return invoke("project_create", { input });
      },
      restart() {
        return invoke("project_restart");
      }
    },
    /** System file picker and message dialog support. */
    dialog: {
      open_file(options = {}) {
        return invoke("dialog_open_file", { options });
      },
      open_files(options = {}) {
        return invoke("dialog_open_files", { options });
      },
      open_dir(options = {}) {
        return invoke("dialog_open_dir", { options });
      },
      save_file(options = {}) {
        return invoke("dialog_save_file", { options });
      },
      message(message, options = {}) {
        return invoke("dialog_message", { message, options });
      },
      confirm(message, options = {}) {
        return invoke("dialog_confirm", { message, options });
      }
    },
    /** Controls for the current main window. */
    window: {
      set_title(title) {
        return invoke("window_set_title", { title });
      },
      minimize() {
        return invoke("window_minimize");
      },
      maximize() {
        return invoke("window_maximize");
      },
      restore() {
        return invoke("window_restore");
      },
      close() {
        return invoke("window_close");
      },
      close_now() {
        return invoke("window_close_now");
      },
      hide() {
        return invoke("window_hide");
      },
      show() {
        return invoke("window_show");
      },
      focus() {
        return invoke("window_focus");
      },
      set_size(width, height) {
        return invoke("window_set_size", { width, height });
      },
      set_position(x, y) {
        return invoke("window_set_position", { x, y });
      },
      placement() {
        return invoke("window_placement");
      },
      set_background_color(color) {
        return invoke("window_set_background_color", { color });
      },
      set_resizable(resizable) {
        return invoke("window_set_resizable", { resizable });
      },
      center() {
        return invoke("window_center");
      },
      set_fullscreen(fullscreen = true) {
        return invoke("window_set_fullscreen", { fullscreen });
      },
      is_fullscreen() {
        return invoke("window_is_fullscreen");
      },
      is_maximized() {
        return invoke("window_is_maximized");
      },
      is_minimized() {
        return invoke("window_is_minimized");
      },
      is_visible() {
        return invoke("window_is_visible");
      },
      set_always_on_top(enabled = true) {
        return invoke("window_set_always_on_top", { enabled });
      },
      is_always_on_top() {
        return invoke("window_is_always_on_top");
      },
      set_decorations(visible = true) {
        return invoke("window_set_decorations", { visible });
      },
      set_skip_taskbar(enabled = true) {
        return invoke("window_set_skip_taskbar", { enabled });
      },
      set_close_mode(mode = "exit") {
        return invoke("window_set_close_mode", { mode });
      },
      async on_close_requested(callback) {
        const off = listen("bt://window/close_requested", callback, (payload) => payload);
        try {
          await invoke("window_set_close_intercept", { enabled: true });
        } catch (error) {
          off();
          throw error;
        }
        return async () => {
          off();
          await invoke("window_set_close_intercept", { enabled: false });
        };
      },
      drag() {
        return invoke("window_drag");
      },
      start_resize(edge) {
        return invoke("window_start_resize", { edge });
      },
      flash() {
        return invoke("window_flash");
      },
      open_devtools() {
        return invoke("window_open_devtools");
      }
    },
    /** System tray icon and menu support. */
    tray: {
      enable(options = {}) {
        return invoke("tray_enable", { options });
      },
      disable() {
        return invoke("tray_disable");
      },
      set_icon(icon) {
        return invoke("tray_set_icon", { icon });
      },
      set_tooltip(text) {
        return invoke("tray_set_tooltip", { text });
      },
      set_menu(menu = []) {
        return invoke("tray_set_menu", { menu });
      },
      on_menu_click(callback) {
        return listen("bt://tray/menu_click", callback, (payload) => payload);
      }
    },
    /** Plain-text clipboard support. */
    clipboard: {
      read_text() {
        return invoke("clipboard_read_text");
      },
      write_text(text) {
        return invoke("clipboard_write_text", { text });
      },
      clear() {
        return invoke("clipboard_clear");
      }
    },
    /** Screen color picker and area capture support. */
    screen: {
      pick_color(options = {}) {
        return invoke("screen_pick_color", { options });
      },
      capture_area(options = {}) {
        return invoke("screen_capture_area", { options });
      }
    },
    /** Operating-system global shortcut support. */
    shortcut: {
      async register(shortcut_id, accelerator, callback) {
        if (typeof callback !== "function") {
          throw new Error("Global shortcut callback must be a function");
        }
        const entry = { off: null };
        entry.off = listenTauri("bt://shortcut/triggered", (payload) => {
          if (shortcutListeners.get(shortcut_id) === entry && payload?.shortcut_id === shortcut_id) {
            callback(payload);
          }
        }, (payload) => payload);
        try {
          await entry.off.ready;
          await invoke("shortcut_register", { shortcutId: shortcut_id, accelerator });
        } catch (error) {
          entry.off();
          throw error;
        }
        const previous = shortcutListeners.get(shortcut_id);
        previous?.off();
        shortcutListeners.set(shortcut_id, entry);
        return async () => {
          if (shortcutListeners.get(shortcut_id) !== entry) {
            return false;
          }
          const removed = await invoke("shortcut_unregister", { shortcutId: shortcut_id });
          entry.off();
          shortcutListeners.delete(shortcut_id);
          return removed;
        };
      },
      async unregister(shortcut_id) {
        const removed = await invoke("shortcut_unregister", { shortcutId: shortcut_id });
        const entry = shortcutListeners.get(shortcut_id);
        entry?.off();
        shortcutListeners.delete(shortcut_id);
        return removed;
      },
      async unregister_all() {
        await invoke("shortcut_unregister_all");
        for (const entry of shortcutListeners.values()) {
          entry.off();
        }
        shortcutListeners.clear();
      }
    },
    /** Basic system notification support. */
    notify: {
      permission_state() {
        return invoke("notify_permission_state");
      },
      request_permission() {
        return invoke("notify_request_permission");
      },
      show(options = {}) {
        return invoke("notify_show", { options });
      }
    },
    /** File and directory drop listener support. */
    drag: {
      on_files(callback) {
        return listen("bt://drag/files", callback, (payload) => {
          return payload && Array.isArray(payload.paths) ? payload.paths : [];
        });
      }
    },
    /** Application-level desktop support. */
    app: {
      info(path) {
        return invoke("app_info", { path });
      },
      run(path, args = []) {
        return invoke("app_run", { path, args });
      },
      version() {
        return invoke("app_version");
      },
      engine_version() {
        return invoke("app_engine_version");
      },
      platform() {
        return invoke("app_platform");
      },
      open_url(url) {
        return invoke("app_open_url", { url });
      },
      open_path(path) {
        return invoke("app_open_path", { path });
      },
      reveal_path(path) {
        return invoke("app_reveal_path", { path });
      },
      quit() {
        return invoke("app_quit");
      },
      args() {
        return invoke("app_args");
      },
      documents_dir() {
        return invoke("app_documents_dir");
      },
      async watch_path(path, callback, options = {}) {
        if (typeof callback !== "function") {
          throw new Error("Event callback must be a function");
        }
        watchSequence = (watchSequence + 1) % 1000000000;
        const watch_id = `watch-${Date.now().toString(36)}-${watchSequence.toString(36)}`;
        const off = listen("bt://app/watch_path", (payload) => {
          if (payload?.watch_id === watch_id) {
            callback(payload);
          }
        }, (payload) => payload);
        try {
          await invoke("app_watch_path", {
            watchId: watch_id,
            path,
            recursive: options.recursive !== false
          });
        } catch (error) {
          off();
          throw error;
        }
        return async () => {
          off();
          await invoke("app_unwatch_path", { watchId: watch_id });
        };
      },
      unwatch_path(watch_id = null) {
        return invoke("app_unwatch_path", { watchId: watch_id });
      }
    },
    /** Plaintext JSON credentials in the current user's hidden app directory; the bridge does not expose plaintext reads to pages. */
    credential: {
      store(credential_id, secret) {
        return invoke("credential_store", { credentialId: credential_id, secret });
      },
      has(credential_id) {
        return invoke("credential_has", { credentialId: credential_id });
      },
      delete(credential_id) {
        return invoke("credential_delete", { credentialId: credential_id });
      }
    },
    /** SSE/chunked HTTP; the listener is installed before startup, and cancel blocks subsequent events. */
    http: {
      async stream(options = {}, callback) {
        if (typeof callback !== "function") {
          throw new Error("Streaming HTTP callback must be a function");
        }
        const request_id = String(options.request_id || (
          globalThis.crypto && crypto.randomUUID
            ? crypto.randomUUID()
            : `request-${Date.now()}-${Math.random().toString(16).slice(2)}`
        ));
        const off = listen("bt://http/stream", (payload) => {
          if (payload && payload.request_id === request_id) {
            callback(payload);
          }
        });
        try {
          const started = await invoke("http_stream_start", {
            options: {...options, request_id}
          });
          return {
            ...started,
            off,
            cancel: () => invoke("http_stream_cancel", { requestId: request_id })
          };
        } catch (error) {
          off();
          throw error;
        }
      },
      cancel(request_id) {
        return invoke("http_stream_cancel", { requestId: request_id });
      }
    },
    /** Normalized workspace file API. */
    workspace: {
      open(root) {
        return invoke("workspace_open", { root });
      },
      close(workspace_id) {
        return invoke("workspace_close", { workspaceId: workspace_id });
      },
      list(workspace_id, relative = "", recursive = false) {
        return invoke("workspace_list", { workspaceId: workspace_id, relative, recursive });
      },
      read(workspace_id, relative, max_bytes = 1048576) {
        return invoke("workspace_read", { workspaceId: workspace_id, relative, maxBytes: max_bytes });
      },
      atomic_write(workspace_id, relative, content, expected_sha256 = null) {
        return invoke("workspace_atomic_write", {
          workspaceId: workspace_id,
          relative,
          content,
          expectedSha256: expected_sha256
        });
      }
    },
    /** Native process management using argument arrays. */
    process: {
      start(options = {}) {
        return invoke("process_start", { options });
      },
      status(process_id, task_id, identity) {
        return invoke("process_status", { processId: process_id, taskId: task_id, identity });
      },
      stop(process_id, task_id, identity) {
        return invoke("process_stop", { processId: process_id, taskId: task_id, identity });
      },
      stop_task(task_id) {
        return invoke("process_stop_task", { taskId: task_id });
      },
      on_event(callback) {
        return listen("bt://process/event", callback, (payload) => payload);
      }
    },
    /** Private application JSON storage and data cleanup with confirmation tokens. */
    data: {
      store(key, value) {
        return invoke("data_store", { key, value });
      },
      load(key) {
        return invoke("data_load", { key });
      },
      prepare_cleanup() {
        return invoke("data_cleanup_prepare");
      },
      confirm_cleanup(confirm_token) {
        return invoke("data_cleanup_confirm", { confirmToken: confirm_token });
      }
    },
    /** BT backend function completion events without function return bodies. */
    events: {
      on_backend(callback) {
        return listen("bt://backend/event", callback, (payload) => payload);
      }
    }
  };
})();
"#,
    );
    if enable_refresh_shortcuts {
        script.push_str(refresh_shortcut_script());
    }
    if enable_devtools_shortcuts {
        script.push_str(devtools_shortcut_script());
    }
    script
}

#[cfg(test)]
mod tests {
    use super::script;

    /// The bridge must expose the known documents directory, multiple listeners, and awaitable closing APIs.
    #[test]
    fn script_contains_session_safety_apis() {
        let bridge = script(false, false);
        assert!(bridge.contains("documents_dir()"));
        assert!(bridge.contains("watchId: watch_id"));
        assert!(bridge.contains("on_close_requested(callback)"));
        assert!(bridge.contains("close_now()"));
        assert!(bridge.contains("set_position(x, y)"));
        assert!(bridge.contains("placement()"));
        assert!(bridge.contains("pick_color(options = {})"));
        assert!(bridge.contains("capture_area(options = {})"));
        assert!(bridge.contains("shortcut_register"));
        assert!(bridge.contains("info(path)"));
        assert!(bridge.contains("run(path, args = [])"));
        assert!(bridge.contains("app_info"));
        assert!(bridge.contains("app_run"));
        assert!(bridge.contains("plugin:event|listen"));
        assert!(bridge.contains("message.payload"));
        assert!(bridge.contains("window.addEventListener(event, handler)"));
        assert!(bridge.contains("listenTauri(\"bt://shortcut/triggered\""));
    }
}

/// Development-mode page refresh shortcut script.
fn refresh_shortcut_script() -> &'static str {
    r#"
(() => {
  /** Listen for development-mode refresh shortcuts. */
  window.addEventListener("keydown", (event) => {
    const key = String(event.key || "").toLowerCase();
    const wantsReload = event.key === "F5" || ((event.ctrlKey || event.metaKey) && key === "r");
    if (!wantsReload) {
      return;
    }
    event.preventDefault();
    window.location.reload();
  }, true);
})();
"#
}

/// Developer tools shortcut script.
fn devtools_shortcut_script() -> &'static str {
    r#"
(() => {
  /** Listen for developer tools shortcuts. */
  window.addEventListener("keydown", (event) => {
    const key = String(event.key || "").toLowerCase();
    const wantsDevtools = event.key === "F12" || ((event.ctrlKey || event.metaKey) && event.shiftKey && key === "i");
    if (!wantsDevtools) {
      return;
    }
    event.preventDefault();
    const internals = window.__TAURI_INTERNALS__;
    if (internals && internals.invoke) {
      internals.invoke("window_open_devtools").catch((err) => {
        console.warn("Failed to open developer tools", err);
      });
    }
  }, true);
})();
"#
}
