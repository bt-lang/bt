//! File watching and hot reload for bt-app development mode.

use crate::app::runtime::{load_dev_runtime_for_reload, runtime_navigation_url, AppState};
use crate::bundle::builder::normalize_bundle_name;
use crate::error::BtError;
use glob::Pattern;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, LogicalSize, Manager, Runtime};
use url::Url;

/// Entry point for the development-mode watcher thread.
pub fn start_dev_watcher<R: Runtime>(app_handle: AppHandle<R>) {
    let _ = thread::Builder::new()
        .name("bt-app-dev-watch".to_string())
        .spawn(move || {
            if let Err(err) = watch_loop(app_handle) {
                eprintln!("Development-mode file watcher stopped: {}", err);
            }
        });
}

/// A single watch root.
#[derive(Clone, Debug, Eq, PartialEq)]
struct WatchRoot {
    /// Project path passed to notify for watching.
    path: PathBuf,
    /// Whether to watch this directory recursively.
    recursive: bool,
}

/// Watch plan for the current runtime.
#[derive(Clone, Debug)]
struct WatchPlan {
    /// Canonical project root path.
    project_dir: PathBuf,
    /// Deduplicated watch roots.
    roots: Vec<WatchRoot>,
    /// Resource event matcher.
    matcher: ResourceMatcher,
    /// Whether to watch resources other than app.json.
    watch_resources: bool,
    /// File-change debounce interval.
    delay: Duration,
}

/// Resource event matcher.
#[derive(Clone, Debug)]
struct ResourceMatcher {
    /// Resource rules to watch.
    resources: Vec<ResourceRule>,
    /// Resource rules to exclude.
    excludes: Vec<ResourceRule>,
}

/// A single resource-matching rule.
#[derive(Clone, Debug)]
struct ResourceRule {
    /// Static base path used to catch new-directory events.
    base: String,
    /// Rule match type.
    kind: ResourceRuleKind,
}

/// Resource rule match type.
#[derive(Clone, Debug)]
enum ResourceRuleKind {
    /// Glob resource rule.
    Glob(Pattern),
    /// Plain file or directory resource rule.
    Plain {
        /// Relative path separated by `/`.
        path: String,
        /// Whether this rule matches a directory recursively.
        directory: bool,
    },
}

/// Runs the file-watching main loop.
fn watch_loop<R: Runtime>(app_handle: AppHandle<R>) -> Result<(), BtError> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = sender.send(result);
        },
        Config::default(),
    )
    .map_err(|err| BtError::Runtime(format!("Failed to create file watcher: {}", err)))?;
    let mut roots = Vec::new();
    let mut plan = rebuild_watch_plan(&app_handle, &mut watcher, &mut roots)?;

    loop {
        let result = receiver
            .recv()
            .map_err(|_| BtError::Runtime("File-watch event channel has closed".to_string()))?;
        let event = match result {
            Ok(event) => event,
            Err(err) => {
                eprintln!("File-watch event error: {}", err);
                continue;
            }
        };
        if !event_matches_plan(&plan, &event) {
            continue;
        }
        wait_for_quiet_period(&receiver, &plan);
        if let Err(err) = reload_dev_runtime(&app_handle) {
            eprintln!("Development-mode hot reload failed: {}", err);
        }
        plan = rebuild_watch_plan(&app_handle, &mut watcher, &mut roots)?;
    }
}

/// Waits until no new relevant events arrive during the debounce interval.
fn wait_for_quiet_period(receiver: &Receiver<notify::Result<Event>>, plan: &WatchPlan) {
    if plan.delay.is_zero() {
        return;
    }
    loop {
        match receiver.recv_timeout(plan.delay) {
            Ok(Ok(event)) if event_matches_plan(plan, &event) => {}
            Ok(Ok(_)) => {}
            Ok(Err(err)) => eprintln!("File-watch event error: {}", err),
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Rebuilds the watch plan and applies it to the notify watcher.
fn rebuild_watch_plan<R: Runtime>(
    app_handle: &AppHandle<R>,
    watcher: &mut RecommendedWatcher,
    roots: &mut Vec<WatchRoot>,
) -> Result<WatchPlan, BtError> {
    let plan = build_watch_plan(app_handle)?;
    apply_watch_roots(watcher, roots, &plan.roots)?;
    *roots = plan.roots.clone();
    Ok(plan)
}

/// Builds a watch plan from the current runtime.
fn build_watch_plan<R: Runtime>(app_handle: &AppHandle<R>) -> Result<WatchPlan, BtError> {
    let state = app_handle.state::<AppState>();
    let runtime = state.lock_runtime()?;
    let project_dir = normalize_existing_dir(&runtime.project_dir)?;
    let watch_resources = runtime.resource.is_dev() && runtime.config.dev.watch;
    let delay = Duration::from_millis(runtime.config.dev.delay);
    let resource_patterns = if watch_resources {
        watch_resource_patterns(&runtime)
    } else {
        Vec::new()
    };
    let matcher = ResourceMatcher::new(&project_dir, &resource_patterns, &runtime.config.exclude)?;
    let mut roots = BTreeMap::new();
    insert_watch_root(
        &mut roots,
        WatchRoot {
            path: project_dir.clone(),
            recursive: false,
        },
    );
    if watch_resources {
        for pattern in &resource_patterns {
            if let Some(root) = watch_root_for_pattern(&project_dir, pattern) {
                insert_watch_root(&mut roots, root);
            }
        }
    }

    Ok(WatchPlan {
        project_dir,
        roots: roots.into_values().collect(),
        matcher,
        watch_resources,
        delay,
    })
}

/// Inserts a deduplicated watch root, preferring recursive watches.
fn insert_watch_root(roots: &mut BTreeMap<PathBuf, WatchRoot>, root: WatchRoot) {
    roots
        .entry(root.path.clone())
        .and_modify(|old| old.recursive |= root.recursive)
        .or_insert(root);
}

/// Applies watch roots to the notify watcher.
fn apply_watch_roots(
    watcher: &mut RecommendedWatcher,
    old_roots: &[WatchRoot],
    new_roots: &[WatchRoot],
) -> Result<(), BtError> {
    for root in old_roots {
        let _ = watcher.unwatch(&root.path);
    }
    for root in new_roots {
        let mode = if root.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher.watch(&root.path, mode).map_err(|err| {
            BtError::Runtime(format!(
                "failed to watch `{}`: {}",
                root.path.display(),
                err
            ))
        })?;
    }
    Ok(())
}

/// Determines whether a notify event should trigger a hot reload.
fn event_matches_plan(plan: &WatchPlan, event: &Event) -> bool {
    event.paths.iter().any(|path| {
        let Some(relative) = relative_event_path(&plan.project_dir, path) else {
            return false;
        };
        relative == "app.json" || (plan.watch_resources && plan.matcher.matches_related(&relative))
    })
}

/// Performs one development-mode hot reload.
fn reload_dev_runtime<R: Runtime>(app_handle: &AppHandle<R>) -> Result<(), BtError> {
    let state = app_handle.state::<AppState>();
    let (project_dir, previous_config, previous_app_args) = {
        let runtime = state.lock_runtime()?;
        (
            runtime.project_dir.clone(),
            runtime.config.clone(),
            runtime.app_args.clone(),
        )
    };

    ensure_window_creation_config_unchanged(&project_dir, &previous_config)?;
    crate::net::stop_web_services().map_err(BtError::Runtime)?;
    let mut runtime = load_dev_runtime_for_reload(&project_dir, &previous_config);
    runtime.app_args = previous_app_args;
    let url = runtime_navigation_url(&runtime)?;
    let config = runtime.config.clone();
    let error_message = runtime.startup_error_message.clone();
    {
        let mut current = state.lock_runtime()?;
        *current = runtime;
    }

    crate::app::console::configure_app_console(config.dev.console);
    if let Some(window) = app_handle.get_webview_window("main") {
        apply_window_config(&window, &config)?;
        reload_window(&window, &url)?;
    }
    if let Some(message) = error_message {
        println!(
            "development-mode hot reload opened the error page: {}",
            message
        );
    } else {
        println!("Development-mode hot reload complete");
    }
    Ok(())
}

/// Prevents hot reload from silently accepting settings that only apply during window creation.
///
/// Parse failures still follow the existing error-page path. Only a valid configuration that
/// changes transparency is rejected early, keeping the current runtime and services active while
/// clearly asking the developer to restart the app.
fn ensure_window_creation_config_unchanged(
    project_dir: &Path,
    previous_config: &crate::app::config::AppJson,
) -> Result<(), BtError> {
    let app_json_path = project_dir.join("app.json");
    let Ok(next_config) = crate::app::config::load_app_json_from_path(&app_json_path) else {
        return Ok(());
    };
    if next_config.window.transparent != previous_config.window.transparent {
        return Err(BtError::Config(
            "window.transparent can only be applied when creating the window; restart bt-app after changing it".to_string(),
        ));
    }
    Ok(())
}

/// Triggers a hot reload in the page context.
///
/// WebView2 reuses the current page when `WebviewWindow::navigate()` targets the same custom-protocol
/// URL, so logs report a reload after saving while the DOM remains stale. Reloading or replacing
/// from inside the page matches the behavior of pressing F5 or Ctrl+R.
fn reload_window<R: Runtime>(window: &tauri::WebviewWindow<R>, url: &Url) -> Result<(), BtError> {
    let target = webview_visible_url(url);
    let target_json = serde_json::to_string(&target)
        .map_err(|err| BtError::Runtime(format!("Failed to serialize hot-reload URL: {}", err)))?;
    let script = format!(
        r#"
(() => {{
  const target = {};
  const resolved = new URL(target, window.location.href).href;
  if (resolved === window.location.href) {{
    window.location.reload();
  }} else {{
    window.location.replace(resolved);
  }}
}})();
"#,
        target_json
    );

    match window.eval(&script) {
        Ok(()) => Ok(()),
        Err(err) => window
            .navigate(url.clone())
            .map_err(|_| BtError::WebView(err.to_string())),
    }
}

/// Converts an internal `bt://app/...` URL to the same-origin address visible to the WebView page.
fn webview_visible_url(url: &Url) -> String {
    if url.scheme() != "bt" || url.host_str() != Some("app") {
        return url.as_str().to_string();
    }

    let mut visible = String::from("http://bt.app");
    visible.push_str(url.path());
    if let Some(query) = url.query() {
        visible.push('?');
        visible.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        visible.push('#');
        visible.push_str(fragment);
    }
    visible
}

/// Applies the latest app.json window settings to the existing window.
fn apply_window_config<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    config: &crate::app::config::AppJson,
) -> Result<(), BtError> {
    window
        .set_title(&config.app.title)
        .map_err(|err| BtError::WebView(err.to_string()))?;
    window
        .set_size(LogicalSize::new(
            config.window.width as f64,
            config.window.height as f64,
        ))
        .map_err(|err| BtError::WebView(err.to_string()))?;
    window
        .set_resizable(config.window.resizable)
        .map_err(|err| BtError::WebView(err.to_string()))?;
    window
        .set_fullscreen(config.window.fullscreen)
        .map_err(|err| BtError::WebView(err.to_string()))?;
    window
        .set_decorations(!config.window.hide_titlebar)
        .map_err(|err| BtError::WebView(err.to_string()))?;
    window
        .set_shadow(
            !config.window.transparent
                && (!cfg!(target_os = "windows") || !config.window.hide_titlebar),
        )
        .map_err(|err| BtError::WebView(err.to_string()))?;
    window
        .set_always_on_top(config.window.always_on_top)
        .map_err(|err| BtError::WebView(err.to_string()))?;
    Ok(())
}

impl ResourceMatcher {
    /// Creates a resource event matcher.
    fn new(project_dir: &Path, resources: &[String], excludes: &[String]) -> Result<Self, BtError> {
        Ok(Self {
            resources: build_rules(project_dir, resources)?,
            excludes: build_rules(project_dir, excludes)?,
        })
    }

    /// Checks whether a relative path matches a resource rule or is an ancestor of its base path.
    fn matches_related(&self, relative: &str) -> bool {
        let included = self
            .resources
            .iter()
            .any(|rule| rule.matches(relative) || rule.is_ancestor_event(relative));
        if !included {
            return false;
        }
        !self.excludes.iter().any(|rule| rule.matches(relative))
    }
}

impl ResourceRule {
    /// Checks whether a relative path matches this rule.
    fn matches(&self, relative: &str) -> bool {
        match &self.kind {
            ResourceRuleKind::Glob(pattern) => pattern.matches(relative),
            ResourceRuleKind::Plain { path, directory } => {
                relative == path
                    || (*directory
                        && relative.len() > path.len()
                        && relative.starts_with(path)
                        && relative.as_bytes().get(path.len()) == Some(&b'/'))
            }
        }
    }

    /// Checks whether an event path is an ancestor of the resource base path.
    fn is_ancestor_event(&self, relative: &str) -> bool {
        self.base == "."
            || self.base == relative
            || self
                .base
                .strip_prefix(relative)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// Builds a set of resource rules.
fn build_rules(project_dir: &Path, patterns: &[String]) -> Result<Vec<ResourceRule>, BtError> {
    let mut rules = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let pattern = normalize_rule(pattern);
        if pattern.is_empty() {
            continue;
        }
        let base = static_base_for_pattern(&pattern);
        if contains_glob(&pattern) {
            let glob = Pattern::new(&pattern).map_err(|err| {
                BtError::Config(format!("Invalid resource glob `{}`: {}", pattern, err))
            })?;
            rules.push(ResourceRule {
                base,
                kind: ResourceRuleKind::Glob(glob),
            });
        } else {
            let directory = project_dir.join(&pattern).is_dir() || pattern.ends_with('/');
            rules.push(ResourceRule {
                base: pattern.trim_end_matches('/').to_string(),
                kind: ResourceRuleKind::Plain {
                    path: pattern.trim_end_matches('/').to_string(),
                    directory,
                },
            });
        }
    }
    Ok(rules)
}

/// Returns the resource rules watched by the current runtime.
fn watch_resource_patterns(runtime: &crate::app::runtime::AppRuntime) -> Vec<String> {
    let mut patterns = runtime.config.resources.clone();
    push_unique(&mut patterns, "app.json");
    if runtime.config.app.mode == "static" {
        push_unique(&mut patterns, &runtime.config.app.entry);
    }
    match &runtime.config.app.main {
        crate::app::config::AppMain::Auto => push_unique(&mut patterns, "main.bt"),
        crate::app::config::AppMain::Disabled => {}
        crate::app::config::AppMain::File(main) => push_unique(&mut patterns, main),
    }
    if runtime.config.app.mode == "server"
        || runtime.resource.exists("server.bt")
        || patterns
            .iter()
            .any(|pattern| normalize_rule(pattern) == "server.bt")
    {
        push_unique(&mut patterns, "server.bt");
    }
    if let Some(icon) = &runtime.config.app.icon {
        push_unique(&mut patterns, icon);
    }
    patterns
}

/// Appends a resource rule if it is not already present.
fn push_unique(patterns: &mut Vec<String>, value: &str) {
    if !patterns.iter().any(|pattern| pattern == value) {
        patterns.push(value.to_string());
    }
}

/// Finds the nearest existing directory to watch for a resource rule.
fn watch_root_for_pattern(project_dir: &Path, pattern: &str) -> Option<WatchRoot> {
    let pattern = normalize_rule(pattern);
    if pattern.is_empty() {
        return None;
    }
    let base = static_base_for_pattern(&pattern);
    let target = if base == "." {
        project_dir.to_path_buf()
    } else {
        project_dir.join(&base)
    };
    let recursive = contains_recursive_glob(&pattern) || target.is_dir();
    nearest_existing_watch_root(project_dir, &target, recursive)
}

/// Returns the existing watch directory nearest to the target path.
fn nearest_existing_watch_root(
    project_dir: &Path,
    target: &Path,
    recursive: bool,
) -> Option<WatchRoot> {
    let mut current = target.to_path_buf();
    loop {
        if current.exists() {
            let path = if current.is_file() {
                current.parent()?.to_path_buf()
            } else {
                current
            };
            let recursive = recursive && path != project_dir;
            return Some(WatchRoot { path, recursive });
        }
        if current == project_dir {
            return Some(WatchRoot {
                path: project_dir.to_path_buf(),
                recursive: false,
            });
        }
        current = current.parent()?.to_path_buf();
    }
}

/// Canonicalizes an existing directory to an absolute path.
fn normalize_existing_dir(path: &Path) -> Result<PathBuf, BtError> {
    let full = path.canonicalize()?;
    if full.is_dir() {
        Ok(full)
    } else {
        Err(BtError::Config(format!(
            "project directory does not exist: {}",
            path.display()
        )))
    }
}

/// Converts an event path to a project-relative path separated by `/`.
fn relative_event_path(project_dir: &Path, path: &Path) -> Option<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_dir.join(path)
    };
    let normalized = normalize_absolute_path(&absolute)?;
    if !normalized.starts_with(project_dir) {
        return None;
    }
    let relative = normalized.strip_prefix(project_dir).ok()?;
    normalize_bundle_name(relative).ok()
}

/// Normalizes an absolute path lexically without requiring it to exist.
fn normalize_absolute_path(path: &Path) -> Option<PathBuf> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(value) => output.push(value),
            Component::ParentDir => {
                if !output.pop() {
                    return None;
                }
            }
        }
    }
    Some(output)
}

/// Normalizes resource rule text.
fn normalize_rule(value: &str) -> String {
    value.trim().replace('\\', "/")
}

/// Checks whether a rule contains glob metacharacters.
fn contains_glob(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'['))
}

/// Checks whether a rule contains a recursive glob.
fn contains_recursive_glob(pattern: &str) -> bool {
    pattern.split('/').any(|part| part == "**")
}

/// Extracts the static base path before the first wildcard in a glob rule.
fn static_base_for_pattern(pattern: &str) -> String {
    let mut parts = Vec::new();
    for part in pattern.split('/') {
        if part.is_empty() || part.bytes().any(|byte| matches!(byte, b'*' | b'?' | b'[')) {
            break;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Resource events matched by an exclude rule do not trigger a reload.
    #[test]
    fn matcher_excludes_resource_paths() {
        let dir = fresh_temp_dir("exclude");
        fs::create_dir_all(dir.join("assets/test")).unwrap();
        let matcher = ResourceMatcher::new(
            &dir,
            &["assets/**".to_string()],
            &["assets/test/**".to_string()],
        )
        .unwrap();

        assert!(matcher.matches_related("assets/app.js"));
        assert!(!matcher.matches_related("assets/test/app.js"));

        let _ = fs::remove_dir_all(dir);
    }

    /// Creating a glob's base directory triggers a reload so its recursive watch can be attached.
    #[test]
    fn matcher_accepts_resource_base_ancestor() {
        let dir = fresh_temp_dir("ancestor");
        let matcher = ResourceMatcher::new(&dir, &["assets/img/**".to_string()], &[]).unwrap();

        assert!(matcher.matches_related("assets"));
        assert!(matcher.matches_related("assets/img"));

        let _ = fs::remove_dir_all(dir);
    }

    /// A custom-protocol URL is converted to the same-origin address visible to the WebView page.
    #[test]
    fn webview_visible_url_maps_bt_app_origin() {
        let url = Url::parse("bt://app/index.html?x=1#top").unwrap();

        assert_eq!(
            webview_visible_url(&url),
            "http://bt.app/index.html?x=1#top"
        );
    }

    /// A transparency change requires a restart to avoid a mismatched window/WebView state after hot reload.
    #[test]
    fn transparent_change_requires_app_restart() {
        let dir = fresh_temp_dir("transparent-restart");
        fs::write(
            dir.join("app.json"),
            r#"{"window":{"hide_titlebar":true,"transparent":true}}"#,
        )
        .unwrap();

        let error =
            ensure_window_creation_config_unchanged(&dir, &crate::app::config::AppJson::default())
                .unwrap_err()
                .to_string();
        assert!(error.contains("restart bt-app after changing it"));

        let _ = fs::remove_dir_all(dir);
    }

    /// Creates a unique test directory.
    fn fresh_temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "bt-app-dev-test-{}-{}-{}",
            name,
            std::process::id(),
            stamp
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
