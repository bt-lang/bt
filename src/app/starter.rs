//! Desktop setup wizard and new-project generation.

use crate::app::config::{AppInfo, AppJson, AppMain, DevConfig, WindowConfig};
use crate::app::resource::EmbeddedResource;
use crate::error::BtError;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use url::Url;

/// Resource path for the setup wizard.
pub const STARTER_ENTRY: &str = "__bt_starter__/index.html";

/// Built-in resource table used by the setup wizard.
pub const STARTER_RESOURCES: &[EmbeddedResource] = &[EmbeddedResource {
    path: STARTER_ENTRY,
    content: STARTER_INDEX_HTML,
}];

/// Desktop project creation parameters submitted by the frontend.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProjectInput {
    /// Application name, also used as the packaged executable name.
    pub app_name: String,
    /// Main window title.
    pub title: String,
    /// Application version.
    pub version: String,
    /// Run mode: `static`, `server`, or `remote`.
    pub mode: String,
    /// Application entry; fixed by the backend for static mode, otherwise the window URL.
    pub entry: String,
    /// Whether to watch resources and refresh the page automatically.
    pub watch: bool,
    /// Whether developer tools may be opened.
    pub devtools: bool,
    /// Whether to keep the debug console.
    pub console: bool,
    /// Main window width.
    pub width: u32,
    /// Main window height.
    pub height: u32,
    /// Whether the main window is resizable.
    pub resizable: bool,
    /// Whether the main window starts in fullscreen mode.
    pub fullscreen: bool,
    /// Whether the main window hides the system title bar.
    pub hide_titlebar: bool,
    /// Whether the main window and WebView use transparent backgrounds.
    pub transparent: bool,
    /// Whether the main window stays on top.
    pub always_on_top: bool,
}

/// Result of creating a desktop project.
#[derive(Debug, Clone)]
pub struct CreateProjectResult {
    /// Relative paths created by this operation.
    pub files: Vec<String>,
}

/// Run mode for a new project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectMode {
    /// Static mode, which loads local HTML directly.
    Static,
    /// Server mode, which starts a local BT web service.
    Server,
    /// Remote mode, which loads a remote URL.
    Remote,
}

impl ProjectMode {
    /// Parses a run mode from its form value.
    fn parse(value: &str) -> Result<Self, BtError> {
        match value.trim() {
            "static" => Ok(ProjectMode::Static),
            "server" => Ok(ProjectMode::Server),
            "remote" => Ok(ProjectMode::Remote),
            other => Err(BtError::Config(format!(
                "Run mode must be static, server, or remote; received: {}",
                other
            ))),
        }
    }

    /// Returns the mode string written to app.json.
    fn as_str(self) -> &'static str {
        match self {
            ProjectMode::Static => "static",
            ProjectMode::Server => "server",
            ProjectMode::Remote => "remote",
        }
    }
}

/// Returns the default desktop configuration used when app.json is missing.
pub fn default_starter_config() -> AppJson {
    AppJson {
        app: AppInfo {
            id: "org.btlang.starter".to_string(),
            id_generated: false,
            name: "BTStarter".to_string(),
            title: "Hi, Hello".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            copyright: None,
            mode: "static".to_string(),
            entry: STARTER_ENTRY.to_string(),
            icon: None,
            storage: "app".to_string(),
            file_associations: Vec::new(),
            main: AppMain::Disabled,
        },
        window: WindowConfig {
            width: 800,
            height: 500,
            resizable: true,
            fullscreen: false,
            hide_titlebar: false,
            transparent: false,
            always_on_top: false,
        },
        dev: DevConfig {
            watch: false,
            delay: 500,
            devtools: false,
            console: false,
        },
        resources: Vec::new(),
        exclude: Vec::new(),
    }
}

/// Creates a new BT desktop project in the specified directory.
pub fn create_project(
    project_dir: &Path,
    input: CreateProjectInput,
) -> Result<CreateProjectResult, BtError> {
    let mode = ProjectMode::parse(&input.mode)?;
    let app_name = normalize_required(&input.app_name, "Application name")?;
    validate_app_name(&app_name)?;
    let title = normalize_required(&input.title, "Window title")?;
    let version = normalize_required(&input.version, "Application version")?;
    let entry = normalized_entry(mode, &input.entry)?;
    validate_window_size(input.width, input.height)?;
    if input.transparent && !input.hide_titlebar {
        return Err(BtError::Config(
            "Transparent windows must hide the system title bar".to_string(),
        ));
    }

    if project_dir.join("app.json").exists() {
        return Err(BtError::Config(format!(
            "app.json already exists in the current directory: {}",
            project_dir.join("app.json").display()
        )));
    }

    let config = AppJson {
        app: AppInfo {
            id: crate::app::config::default_app_id_for_name(&app_name),
            id_generated: false,
            name: app_name,
            title,
            version,
            description: None,
            copyright: None,
            mode: mode.as_str().to_string(),
            entry,
            icon: None,
            storage: "app".to_string(),
            file_associations: Vec::new(),
            main: AppMain::File("main.bt".to_string()),
        },
        window: WindowConfig {
            width: input.width,
            height: input.height,
            resizable: input.resizable,
            fullscreen: input.fullscreen,
            hide_titlebar: input.hide_titlebar,
            transparent: input.transparent,
            always_on_top: input.always_on_top,
        },
        dev: DevConfig {
            watch: input.watch,
            delay: 500,
            devtools: input.devtools,
            console: input.console,
        },
        resources: resources_for_mode(mode),
        exclude: Vec::new(),
    };

    let files = files_for_mode(mode);
    ensure_targets_available(project_dir, &files)?;
    let app_json = serde_json::to_string_pretty(&config)?;

    for file in &files {
        let content = match file.as_str() {
            "app.json" => app_json.as_str(),
            "main.bt" => MAIN_BT_TEMPLATE,
            "index.html" => STATIC_INDEX_TEMPLATE,
            "server.bt" => SERVER_BT_TEMPLATE,
            "www/main.bt" => SERVER_WWW_MAIN_TEMPLATE,
            _ => continue,
        };
        write_new_file(project_dir, file, content)?;
    }

    Ok(CreateProjectResult { files })
}

/// Restarts the current bt-app process.
pub fn restart_current_app() -> Result<(), BtError> {
    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    match spawn_restart_process(&exe, &cwd, false) {
        Ok(()) => {
            std::process::exit(0);
        }
        Err(err) if err.raw_os_error() == Some(50) => {
            spawn_restart_process(&exe, &cwd, true)?;
            std::process::exit(0);
        }
        Err(err) => return Err(BtError::Io(err)),
    }
}

/// Starts a new instance of the current program.
///
/// Inherits standard handles by default to preserve development console output. If Windows returns
/// `ERROR_NOT_SUPPORTED(50)` from a console-free wizard, the caller retries with null stdio.
fn spawn_restart_process(exe: &Path, cwd: &Path, null_stdio: bool) -> io::Result<()> {
    let mut command = Command::new(exe);
    command.current_dir(cwd);
    if null_stdio {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    command.spawn().map(|_| ())
}

/// Returns the app.json resource rules for the specified mode.
fn resources_for_mode(mode: ProjectMode) -> Vec<String> {
    match mode {
        ProjectMode::Static => vec![
            "app.json".to_string(),
            "index.html".to_string(),
            "main.bt".to_string(),
        ],
        ProjectMode::Server => vec![
            "app.json".to_string(),
            "server.bt".to_string(),
            "main.bt".to_string(),
            "www/**".to_string(),
        ],
        ProjectMode::Remote => vec!["app.json".to_string(), "main.bt".to_string()],
    }
}

/// Returns the files to create for the specified mode.
fn files_for_mode(mode: ProjectMode) -> Vec<String> {
    match mode {
        ProjectMode::Static => vec![
            "app.json".to_string(),
            "main.bt".to_string(),
            "index.html".to_string(),
        ],
        ProjectMode::Server => vec![
            "app.json".to_string(),
            "main.bt".to_string(),
            "server.bt".to_string(),
            "www/main.bt".to_string(),
        ],
        ProjectMode::Remote => vec!["app.json".to_string(), "main.bt".to_string()],
    }
}

/// Normalizes the entry field for the selected run mode.
fn normalized_entry(mode: ProjectMode, entry: &str) -> Result<String, BtError> {
    match mode {
        ProjectMode::Static => Ok("index.html".to_string()),
        ProjectMode::Server => normalized_url_entry(entry, "http://127.0.0.1:18280", "server"),
        ProjectMode::Remote => normalized_url_entry(entry, "https://example.com", "remote"),
    }
}

/// Normalizes an entry field that must contain a window URL.
fn normalized_url_entry(entry: &str, default: &str, mode: &str) -> Result<String, BtError> {
    let entry = entry.trim();
    let entry = if entry.is_empty() { default } else { entry };
    let url = Url::parse(entry)
        .map_err(|err| BtError::Config(format!("Invalid {} entry URL: {}", mode, err)))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(BtError::Config(format!(
            "{} mode entry must begin with http:// or https://",
            mode
        )));
    }
    Ok(entry.to_string())
}

/// Trims a required text field and verifies that it is not empty.
fn normalize_required(value: &str, name: &str) -> Result<String, BtError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BtError::Config(format!("{} cannot be empty", name)));
    }
    Ok(value.to_string())
}

/// Validates that the application name is a valid Windows executable name.
fn validate_app_name(name: &str) -> Result<(), BtError> {
    let path = Path::new(name);
    if path.components().count() != 1 || path.is_absolute() {
        return Err(BtError::Config(format!(
            "Application name must be a file name and cannot contain a path: {}",
            name
        )));
    }
    if name.chars().any(|ch| {
        matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control()
    }) {
        return Err(BtError::Config(format!(
            "Application name contains characters that are invalid in Windows file names: {}",
            name
        )));
    }
    Ok(())
}

/// Validates the window dimensions.
fn validate_window_size(width: u32, height: u32) -> Result<(), BtError> {
    if width == 0 || height == 0 {
        return Err(BtError::Config(
            "Window width and height must be greater than 0".to_string(),
        ));
    }
    Ok(())
}

/// Checks target files before writing to avoid overwriting existing content.
fn ensure_targets_available(project_dir: &Path, files: &[String]) -> Result<(), BtError> {
    for file in files {
        let path = project_dir.join(file);
        if path.exists() {
            return Err(BtError::Config(format!(
                "Target file already exists; creation stopped to avoid overwriting it: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Creates a new file and writes UTF-8 content to it.
fn write_new_file(project_dir: &Path, relative: &str, content: &str) -> Result<(), BtError> {
    let path = project_dir.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

/// Setup wizard HTML.
const STARTER_INDEX_HTML: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>BT App - Create Project</title>
  <style>
    :root {
      color-scheme: light;
      font-family: "Microsoft YaHei", "Segoe UI", system-ui, -apple-system, BlinkMacSystemFont, sans-serif;
      color: #172033;
      background: #eef3fb;
    }
    *{box-sizing:border-box;}
    html,body{width:100%;height:100%;}
    body{margin:0;min-width:320px;background:radial-gradient(circle at 16% 12%,rgba(89,137,255,0.28),transparent 34%),radial-gradient(circle at 88% 8%,rgba(44,203,170,0.18),transparent 28%),linear-gradient(135deg,#f8fbff 0%,#edf3fb 48%,#e8eef8 100%);}
    main{position:relative;margin:0 auto;padding:60px;height:100vh;overflow-y:auto;scrollbar-width:thin;scrollbar-color:#3c3c3c transparent;}
    .page{display:none;width:100%;}
    .page.active{display:block;}
    .page::-webkit-scrollbar{width:8px;}
    .page::-webkit-scrollbar-thumb{border-radius:999px;background:rgba(37,99,235,0.32);}
    .intro{display:none;align-content:center;justify-items:center;text-align:center;gap:24px;}
    .intro.active{display:grid;}
    .hero-card,form,.success-card{position:relative;overflow:hidden;border:1px solid rgba(255,255,255,0.7);background:rgba(255,255,255,0.88);box-shadow:0 9px 30x rgba(36,56,93,0.15);backdrop-filter:blur(18px);}
    .hero-card{width:min(100%,650px);padding:54px;}
    .hero-card::before,form::before,.success-card::before{position:absolute;content:"";inset:0 0 auto 0;height:5px;background:linear-gradient(90deg,#2563eb,#14b8a6,#8b5cf6);}
    .brand{display:inline-flex;align-items:center;gap:10px;margin-bottom:22px;padding:8px 13px;border:1px solid rgba(37,99,235,0.15);border-radius:999px;color:#1d4ed8;background:rgba(37,99,235,0.08);font-size:13px;font-weight:700;letter-spacing:0.04em;}
    .brand-mark{display:grid;place-items:center;width:24px;height:24px;border-radius:8px;color:#ffffff;background:linear-gradient(135deg,#2563eb,#14b8a6);box-shadow:0 8px 20px rgba(37,99,235,0.3);}
    h1{margin:0;font-size:clamp(30px,5vw,48px);line-height:1.12;font-weight:800;letter-spacing:-0.04em;color:#172033;}
    h2{display:flex;align-items:center;gap:10px;margin:0 0 18px;font-size:18px;line-height:1.35;font-weight:760;color:#1d2939;}
    h2::before{content:"";width:9px;height:9px;border-radius:999px;background:#2563eb;box-shadow:0 0 0 5px rgba(37,99,235,0.11);}
    p{margin:0;line-height:1.8;color:#647083;}
    .subtitle{max-width:560px;margin:18px auto 0;font-size:15px;}
    .actions{display:flex;gap:12px;flex-wrap:wrap;}
    .intro .actions,.success .actions{justify-content:center;margin-top:10px;}
    button{min-width:108px;height:44px;border:1px solid rgba(101,116,139,0.25);border-radius:12px;padding:0 20px;background:rgba(255,255,255,0.82);color:#263244;font:inherit;font-weight:700;cursor:pointer;transition:transform 0.16s ease,box-shadow 0.16s ease,border-color 0.16s ease,background 0.16s ease;}
    button:hover{transform:translateY(-1px);border-color:rgba(37,99,235,0.38);box-shadow:0 14px 28px rgba(36,56,93,0.12);}
    button.primary{border-color:transparent;background:linear-gradient(135deg,#2563eb,#1d4ed8);color:#ffffff;box-shadow:0 16px 34px rgba(37,99,235,0.32);}
    button:disabled{cursor:wait;opacity:0.7;transform:none;}
    form{display:grid;gap:18px;padding:34px;}
    .form-header{display:flex;align-items:flex-start;justify-content:space-between;gap:20px;padding-bottom:8px;}
    .form-title h1{font-size:clamp(24px,4vw,34px);}
    .form-title p{margin-top:8px;font-size:14px;}
    .section-card{padding:22px;border:1px solid rgba(104,121,146,0.2);border-radius:9px;background:rgba(255,255,255,0.66);}
    .grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:16px;}
    label{display:flex;gap:8px;font-size:13px;font-weight:700;color:#3f4a5d;white-space:nowrap;align-items:center;}
    input[type="text"],input[type="url"],input[type="number"]{width:100%;height:37px;border:1px solid rgba(103,119,142,0.28);border-radius:6px;padding:0 13px;background:#ffffff;color:#172033;font:inherit;outline:none;box-shadow:0 1px 0 rgba(255,255,255,0.8) inset;transition:border-color 0.16s ease,box-shadow 0.16s ease,background 0.16s ease;}
    input[type="text"]:focus,input[type="url"]:focus,input[type="number"]:focus{border-color:rgba(37,99,235,0.72);box-shadow:0 0 0 4px rgba(37,99,235,0.12);}
    input:disabled{color:#667085;background:#eef2f7;cursor:not-allowed;}
    input::placeholder{color:#999;font-weight:normal;}
    input[type="radio"],input[type="checkbox"]{width:16px;height:16px;accent-color:#2563eb;}
    .modes,.checks{display:flex;gap:12px;flex-wrap:wrap;}
    .modes label,.checks label{display:inline-flex;grid-template-columns:none;align-items:center;min-height:37px;gap:8px;padding:0 13px;border:1px solid rgba(103,119,142,0.22);border-radius:6px;background:rgba(255,255,255,0.72);font-size:13px;font-weight:650;cursor:pointer;}
    .message{color:#673AB7;font-size:13px;}
    .form-actions{justify-content:flex-end;padding-top:2px;}
    .success{display:none;align-content:center;justify-items:center;text-align:center;gap:12px;}
    .success.active{display:grid;}
    .success-card{width:min(100%,720px);padding:50px;}
    .success-icon{display:grid;place-items:center;width:68px;height:68px;margin:0 auto 18px;border-radius:22px;color:#ffffff;background:linear-gradient(135deg,#16a34a,#14b8a6);box-shadow:0 20px 42px rgba(20,184,166,0.26);font-size:32px;font-weight:900;}
    #created-files{margin-top:14px;padding:12px 16px;border-radius:14px;background:rgba(37,99,235,0.08);word-break:break-all;}
    @media (max-width:720px){main{padding:18px;}
    .page{height:calc(100vh - 36px);}
    .hero-card,form,.success-card{padding:24px;}
    .form-header{display:block;}
    .grid{grid-template-columns:1fr;}
    .form-actions{justify-content:stretch;}
    .form-actions button{flex:1;}
    }
  </style>
</head>
<body>
  <main>
    <section class="page active intro" data-page="intro">
      <div class="hero-card">
        <div class="brand"><span class="brand-mark">B</span> BT APP PROJECT</div>
        <h1>Welcome to BT App</h1>
        <p class="subtitle">This directory does not contain a runnable project yet. Create a basic project to get started with development, debugging, and packaging.</p>
        <div class="actions">
          <button type="button" id="close">Close</button>
          <button type="button" class="primary" id="go-create">Create Project</button>
        </div>
      </div>
    </section>

    <section class="page" data-page="form">
      <form id="project-form">
        <div class="form-header">
          <div class="form-title">
            <h1>Create a New Project</h1>
            <p>Configure the basics, run mode, and window settings. BT App will generate the project files.</p>
          </div>
          <div class="brand"><span class="brand-mark">B</span> BT APP</div>
        </div>

        <div class="section-card">
          <h2>Application</h2>
          <div class="grid">
            <label>Application Name
              <input id="app-name" type="text" placeholder="Enter an application name" autocomplete="off" />
            </label>
            <label>Window Title
              <input id="title" type="text" placeholder="Enter a window title" autocomplete="off" />
            </label>
            <label>Application Version
              <input id="version" type="text" placeholder="Enter an application version" value="1.0.0" autocomplete="off" />
            </label>
            <label>Application Entry
              <input id="entry" type="text" placeholder="Enter an application entry" value="index.html" autocomplete="off" />
            </label>
          </div>
        </div>

        <div class="section-card">
          <h2>Run Mode</h2>
          <div class="modes">
            <label><input type="radio" name="mode" value="static" checked /> Static HTML</label>
            <label><input type="radio" name="mode" value="server" /> Local Web Service</label>
            <label><input type="radio" name="mode" value="remote" /> Remote URL</label>
          </div>
        </div>

        <div class="section-card">
          <h2>Window</h2>
          <div class="grid">
            <label>Window Width
              <input id="width" type="number" placeholder="Enter the window width" min="1" value="900" />
            </label>
            <label>Window Height
              <input id="height" type="number" placeholder="Enter the window height" min="1" value="700" />
            </label>
          </div>
          <div class="checks" style="margin-top: 14px;">
            <label><input id="resizable" type="checkbox" checked /> Resizable</label>
            <label><input id="fullscreen" type="checkbox" /> Fullscreen</label>
            <label><input id="hide-titlebar" type="checkbox" /> Hide Title Bar</label>
            <label><input id="transparent" type="checkbox" /> Transparent Window</label>
            <label><input id="always-on-top" type="checkbox" /> Always on Top</label>
          </div>
        </div>

        <div class="section-card">
          <h2>Developer Options</h2>
          <div class="checks" style="margin-top: 14px;">
            <label><input id="watch" type="checkbox" checked /> Enable live reload</label>
            <label><input id="devtools" type="checkbox" checked /> Enable Developer Tools</label>
            <label><input id="console" type="checkbox" /> Enable Console Logging</label>
          </div>
        </div>

        <div class="message" id="message"></div>
        <div class="actions form-actions">
          <button type="button" id="back">Back</button>
          <button type="submit" class="primary" id="create">Create Project</button>
        </div>
      </form>
    </section>

    <section class="page success" data-page="success">
      <div class="success-card">
        <div class="success-icon">✓</div>
        <h1>Project Created</h1>
        <p class="subtitle">The project files have been generated. The application will restart shortly.</p>
        <p id="created-files"></p>
      </div>
    </section>
  </main>

  <script>
    (() => {
      const pages = [...document.querySelectorAll(".page")];
      const entry = document.querySelector("#entry");
      const hideTitlebar = document.querySelector("#hide-titlebar");
      const transparent = document.querySelector("#transparent");
      const entryByMode = {
        static: "index.html",
        server: "http://127.0.0.1:18280",
        remote: "https://example.com"
      };

      const show = (name) => {
        for (const page of pages) {
          const active = page.dataset.page === name;
          page.classList.toggle("active", active);
          page.hidden = !active;
          if (active) {
            page.scrollTop = 0;
          }
        }
        window.scrollTo(0, 0);
      };

      const currentMode = () => document.querySelector("input[name='mode']:checked").value;
      const setModeEntry = () => {
        const mode = currentMode();
        entry.value = entryByMode[mode];
        entry.type = mode === "remote" ? "url" : "text";
        entry.disabled = mode !== "remote";
      };

      document.querySelector("#close").addEventListener("click", () => {
        window.bt.window.close();
      });
      document.querySelector("#go-create").addEventListener("click", () => show("form"));
      document.querySelector("#back").addEventListener("click", () => show("intro"));
      for (const item of document.querySelectorAll("input[name='mode']")) {
        item.addEventListener("change", setModeEntry);
      }
      transparent.addEventListener("change", () => {
        if (transparent.checked) {
          hideTitlebar.checked = true;
        }
        hideTitlebar.disabled = transparent.checked;
      });

      document.querySelector("#project-form").addEventListener("submit", async (event) => {
        event.preventDefault();
        const create = document.querySelector("#create");
        const message = document.querySelector("#message");
        message.textContent = "";
        create.disabled = true;
        const input = {
          appName: document.querySelector("#app-name").value,
          title: document.querySelector("#title").value,
          version: document.querySelector("#version").value,
          mode: currentMode(),
          entry: entry.value,
          width: Number(document.querySelector("#width").value),
          height: Number(document.querySelector("#height").value),
          resizable: document.querySelector("#resizable").checked,
          fullscreen: document.querySelector("#fullscreen").checked,
          hideTitlebar: hideTitlebar.checked,
          transparent: transparent.checked,
          alwaysOnTop: document.querySelector("#always-on-top").checked,
          watch: document.querySelector("#watch").checked,
          devtools: document.querySelector("#devtools").checked,
          console: document.querySelector("#console").checked,
        };

        if (!input.appName) {
          message.textContent = "Enter an application name";
          create.disabled = false;
          return;
        }
        if (!input.title) {
          message.textContent = "Enter a window title";
          create.disabled = false;
          return;
        }
        if (!input.version) {
          message.textContent = "Enter an application version";
          create.disabled = false;
          return;
        }
        if (!input.entry) {
          message.textContent = "Enter an application entry";
          create.disabled = false;
          return;
        }
        if (input.mode === "remote" && !input.entry.startsWith("http")) {
          message.textContent = "Enter a valid remote URL";
          create.disabled = false;
          return;
        }
        if (input.width < 1 || input.width > 9999){
          message.textContent = "Window width must be between 1 and 9999";
          create.disabled = false;
          return;
        }
        if (input.height < 1 || input.height > 9999){
          message.textContent = "Window height must be between 1 and 9999";
          create.disabled = false;
          return;
        }

        try {
          const result = await window.bt.project.create(input);
          if (result.error) {
            message.textContent = result.message;
            create.disabled = false;
            return;
          }
          const files = result.data && result.data.files ? result.data.files : [];
          document.querySelector("#created-files").textContent = files.join(", ");
          show("success");
          setTimeout(async () => {
            try {
              await window.bt.project.restart();
            } catch (error) {
              message.textContent = error && error.message ? error.message : String(error);
              show("form");
              create.disabled = false;
            }
          }, 3000);
        } catch (error) {
          message.textContent = error && error.message ? error.message : String(error);
          create.disabled = false;
        }
      });

      show("intro");
      setModeEntry();
    })();
  </script>
</body>
</html>
"##;

/// Main desktop bridge script template.
const MAIN_BT_TEMPLATE: &str = r#"/**
 * Frontend bridge test function.
 *
 * @param data JSON object provided by the frontend.
 * @return The desktop bridge test result.
 */
fn hello(data) {
    {
        error: false,
        message: 'message received',
        data: data
    }
}
"#;

/// Default HTML template for static mode.
const STATIC_INDEX_TEMPLATE: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>BT Desktop App</title>
  <style>
    body {
      margin: 0;
      font-family: "Microsoft YaHei", "Segoe UI", system-ui, sans-serif;
      background: #f6f7f9;
      color: #20242c;
    }

    main {
      width: min(100% - 32px, 760px);
      margin: 64px auto;
    }

    h1 {
      margin: 0 0 20px;
      font-size: 28px;
    }

    button {
      height: 38px;
      border: 1px solid #1f6feb;
      border-radius: 6px;
      padding: 0 16px;
      background: #1f6feb;
      color: #fff;
      font: inherit;
      cursor: pointer;
    }

    pre {
      min-height: 140px;
      margin-top: 18px;
      padding: 14px;
      overflow: auto;
      border: 1px solid #d0d5dd;
      border-radius: 6px;
      background: #ffffff;
    }
  </style>
</head>
<body>
  <main>
    <h1>BT Desktop App</h1>
    <button id="hello" type="button">Call hello</button>
    <pre id="output">ready</pre>
  </main>
  <script>
    const output = document.querySelector("#output");
    document.querySelector("#hello").addEventListener("click", async () => {
      const result = await window.bt.call("hello", {
        name: "BT App",
        time: new Date().toISOString()
      });
      output.textContent = JSON.stringify(result, null, 2);
    });
  </script>
</body>
</html>
"##;

/// Service script template for server mode.
const SERVER_BT_TEMPLATE: &str = r#"net.listen({
    type:'web',
    bind:'127.0.0.1:18280',
    sites:[
        {
            domains:['127.0.0.1'],
            root:'www/',
            entry:'main.bt',
            upload:{
                temp:'temp/'
            }
        }
    ]
})
"#;

/// Web entry script template for server mode.
const SERVER_WWW_MAIN_TEMPLATE: &str = r##"print `<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>BT Server App</title>
  <style>
    body {
      margin: 0;
      font-family: "Microsoft YaHei", "Segoe UI", system-ui, sans-serif;
      background: #f6f7f9;
      color: #20242c;
    }

    main {
      width: min(100% - 32px, 760px);
      margin: 64px auto;
    }

    h1 {
      margin: 0 0 20px;
      font-size: 28px;
    }

    button {
      height: 38px;
      border: 1px solid #1f6feb;
      border-radius: 6px;
      padding: 0 16px;
      background: #1f6feb;
      color: #fff;
      font: inherit;
      cursor: pointer;
    }

    pre {
      min-height: 140px;
      margin-top: 18px;
      padding: 14px;
      overflow: auto;
      border: 1px solid #d0d5dd;
      border-radius: 6px;
      background: #ffffff;
    }
  </style>
</head>
<body>
  <main>
    <h1>BT Server App</h1>
    <button id="hello" type="button">Call hello</button>
    <pre id="output">ready</pre>
  </main>
  <script>
    const output = document.querySelector("#output");
    document.querySelector("#hello").addEventListener("click", async () => {
      const result = await window.bt.call("hello", {
        name: "BT Server App",
        time: new Date().toISOString()
      });
      output.textContent = JSON.stringify(result, null, 2);
    });
  </script>
</body>
</html>`
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// Creating a static project writes app.json, main.bt, and index.html.
    #[test]
    fn creates_static_project_files() {
        let dir = fresh_temp_dir("static");
        let result = create_project(&dir, sample_input("static")).unwrap();

        assert_eq!(result.files, vec!["app.json", "main.bt", "index.html"]);
        assert!(dir.join("app.json").is_file());
        assert!(dir.join("main.bt").is_file());
        assert!(dir.join("index.html").is_file());

        let raw = fs::read_to_string(dir.join("app.json")).unwrap();
        let config: AppJson = serde_json::from_str(&raw).unwrap();
        assert_eq!(config.app.mode, "static");
        assert_eq!(config.app.entry, "index.html");
        assert_eq!(config.app.main, AppMain::File("main.bt".to_string()));
        assert!(!config.window.transparent);
        assert_eq!(config.resources, vec!["app.json", "index.html", "main.bt"]);

        let _ = fs::remove_dir_all(dir);
    }

    /// Creating a server project includes a runnable web entry script.
    #[test]
    fn creates_server_project_files() {
        let dir = fresh_temp_dir("server");
        let mut input = sample_input("server");
        input.entry.clear();
        let result = create_project(&dir, input).unwrap();

        assert_eq!(
            result.files,
            vec!["app.json", "main.bt", "server.bt", "www/main.bt"]
        );
        assert!(dir.join("server.bt").is_file());
        assert!(dir.join("www").join("main.bt").is_file());

        let raw = fs::read_to_string(dir.join("app.json")).unwrap();
        let config: AppJson = serde_json::from_str(&raw).unwrap();
        assert_eq!(config.app.mode, "server");
        assert_eq!(config.app.entry, "http://127.0.0.1:18280");
        assert_eq!(
            config.resources,
            vec!["app.json", "server.bt", "main.bt", "www/**"]
        );
        let server_bt = fs::read_to_string(dir.join("server.bt")).unwrap();
        assert!(server_bt.contains("net.listen"));
        assert!(!server_bt.contains("PROJECT_"));

        let _ = fs::remove_dir_all(dir);
    }

    /// Existing target files are not overwritten.
    #[test]
    fn refuses_existing_file() {
        let dir = fresh_temp_dir("existing");
        fs::write(dir.join("main.bt"), "").unwrap();

        let error = create_project(&dir, sample_input("static"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Target file already exists"));

        let _ = fs::remove_dir_all(dir);
    }

    /// Remote mode accepts only HTTP or HTTPS entries.
    #[test]
    fn rejects_invalid_remote_entry() {
        let dir = fresh_temp_dir("remote");
        let mut input = sample_input("remote");
        input.entry = "file:///tmp/index.html".to_string();

        let error = create_project(&dir, input).unwrap_err().to_string();
        assert!(error.contains("remote mode entry must begin with http:// or https://"));

        let _ = fs::remove_dir_all(dir);
    }

    /// The creation API rejects transparent windows that retain the system title bar.
    #[test]
    fn rejects_transparent_window_with_titlebar() {
        let dir = fresh_temp_dir("transparent-titlebar");
        let mut input = sample_input("static");
        input.transparent = true;

        let error = create_project(&dir, input).unwrap_err().to_string();
        assert!(error.contains("Transparent windows must hide the system title bar"));

        let _ = fs::remove_dir_all(dir);
    }

    /// Creates test input.
    fn sample_input(mode: &str) -> CreateProjectInput {
        CreateProjectInput {
            app_name: "Diary".to_string(),
            title: "My Journal".to_string(),
            version: "1.0.0".to_string(),
            mode: mode.to_string(),
            entry: "https://example.com".to_string(),
            watch: true,
            devtools: true,
            console: true,
            width: 900,
            height: 700,
            resizable: true,
            fullscreen: false,
            hide_titlebar: false,
            transparent: false,
            always_on_top: false,
        }
    }

    /// Creates a unique test directory.
    fn fresh_temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bt-app-starter-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
