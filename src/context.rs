use crate::app::config::AppJson;
use std::path::PathBuf;

/// BT operating mode.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMode {
    /// Normal CLI/Web/Script mode.
    Cli,

    /// Desktop application mode.
    App,
}

/// BT running context.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RuntimeContext {
    /// Current operating mode.
    pub mode: RuntimeMode,
    /// The current project root directory.
    pub project_dir: PathBuf,
    /// Desktop application configuration; empty for non-desktop mode.
    pub app_config: Option<AppJson>,
}
