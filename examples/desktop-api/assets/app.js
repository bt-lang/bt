const logBox = document.querySelector("#log");
const runtime = document.querySelector("#runtime");
const actions = document.querySelector(".grid");

let alwaysTop = false;

function log(title, value) {
  const line = `[${new Date().toLocaleTimeString()}] ${title}\n${JSON.stringify(value, null, 2)}\n\n`;
  logBox.textContent = line + logBox.textContent;
}

async function refreshRuntime() {
  const [version, engine, platform] = await Promise.all([
    window.bt.app.version(),
    window.bt.app.engine_version(),
    window.bt.app.platform()
  ]);
  runtime.textContent = `app ${version} / engine ${engine} / ${platform}`;
  log("Global Tauri exposure", { tauri: typeof window.__TAURI__ });
}

async function enableTray() {
  await window.bt.tray.enable({
    tooltip: "BT Desktop API Demo",
    menu: [
      { id: "show", text: "Show Window" },
      { type: "separator" },
      { id: "quit", text: "Quit" }
    ]
  });
  log("Tray", "Enabled");
}

const handlers = {
  "bt-call": async () => {
    const result = await window.bt.call("inspect", {
      name: "BT",
      mode: "desktop_api"
    });
    log("bt.call inspect", result);
  },
  "call-error": async () => {
    try {
      await window.bt.call("missing_function", {});
    } catch (error) {
      log("bt.call rejected", String(error));
    }
  },
  "app-info": async () => {
    const result = {
      version: await window.bt.app.version(),
      engine_version: await window.bt.app.engine_version(),
      platform: await window.bt.app.platform(),
      args: await window.bt.app.args()
    };
    log("app", result);
  },
  "clipboard": async () => {
    await window.bt.clipboard.write_text("BT Desktop API");
    const text = await window.bt.clipboard.read_text();
    log("clipboard", text);
  },
  "window-title": async () => {
    await window.bt.window.set_title("BT Desktop API " + new Date().toLocaleTimeString());
    log("window.set_title", true);
  },
  "window-size": async () => {
    await window.bt.window.set_size(1050, 800);
    await window.bt.window.center();
    log("window.set_size", { width: 980, height: 720 });
  },
  "window-state": async () => {
    const result = {
      fullscreen: await window.bt.window.is_fullscreen(),
      maximized: await window.bt.window.is_maximized(),
      minimized: await window.bt.window.is_minimized(),
      visible: await window.bt.window.is_visible(),
      always_on_top: await window.bt.window.is_always_on_top()
    };
    log("window state", result);
  },
  "always-top": async () => {
    alwaysTop = !alwaysTop;
    await window.bt.window.set_always_on_top(alwaysTop);
    log("window.set_always_on_top", alwaysTop);
  },
  "notify": async () => {
    const permission = await window.bt.notify.request_permission();
    await window.bt.notify.show({
      title: "BT Desktop API",
      body: "The notification API was invoked"
    });
    log("notify.show", {
      permission,
      message: "A system notification was sent; check the notification center or desktop alert"
    });
  },
  "dialog-message": async () => {
    await window.bt.dialog.message("The message-dialog API was invoked", {
      title: "BT Desktop API",
      kind: "info"
    });
    log("dialog.message", true);
  },
  "dialog-confirm": async () => {
    const ok = await window.bt.dialog.confirm("Continue?", {
      title: "BT Desktop API",
      kind: "warning"
    });
    log("dialog.confirm", ok);
  },
  "tray": enableTray,
  "close-tray": async () => {
    await enableTray();
    await window.bt.window.set_close_mode("tray");
    log("window.close_mode", "tray");
    await window.bt.window.close();
  },
  "pick-file": async () => {
    const path = await window.bt.dialog.open_file({
      title: "Choose File"
    });
    log("dialog.open_file", path);
  },
  "pick-dir": async () => {
    const path = await window.bt.dialog.open_dir({
      title: "Choose Directory"
    });
    log("dialog.open_dir", path);
  }
};

actions.addEventListener("click", async (event) => {
  const action = event.target.dataset.action;
  if (!action || !handlers[action]) {
    return;
  }
  try {
    await handlers[action]();
  } catch (error) {
    log(action + " error", String(error));
  }
});

window.bt.tray.on_menu_click(async (id) => {
  log("tray menu", id);
  if (id === "show") {
    await window.bt.window.show();
    await window.bt.window.focus();
  }
  if (id === "quit") {
    await window.bt.app.quit();
  }
});

window.bt.drag.on_files((paths) => {
  log("drag.files", paths);
});

document.querySelector("#clear-log").addEventListener("click", () => {
  logBox.textContent = "";
});

refreshRuntime().catch((error) => log("runtime error", String(error)));
