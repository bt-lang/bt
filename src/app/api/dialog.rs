//! Desktop API implementation for `bt.dialog`.

use crate::app::api::{absolute_path_text, map_error, required_text};
use serde::Deserialize;
use std::path::PathBuf;
use tauri::AppHandle;
use tauri_plugin_dialog::{
    DialogExt, FileDialogBuilder, FilePath, MessageDialogButtons, MessageDialogKind,
};

/// File dialog options.
#[derive(Debug, Default, Deserialize)]
pub struct FileDialogOptions {
    /// System dialog title.
    #[serde(default)]
    pub title: String,
    /// Default path to open or save.
    #[serde(default)]
    pub default_path: String,
    /// File extension filters.
    #[serde(default)]
    pub filters: Vec<FileDialogFilter>,
}

/// File extension filter for a file dialog.
#[derive(Debug, Deserialize)]
pub struct FileDialogFilter {
    /// Display name of the filter.
    pub name: String,
    /// Extensions accepted by the filter, without leading dots.
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// Message and confirmation dialog options.
#[derive(Debug, Default, Deserialize)]
pub struct MessageDialogOptions {
    /// System dialog title.
    #[serde(default)]
    pub title: String,
    /// Message kind: `info`, `warning`, or `error`.
    #[serde(default)]
    pub kind: String,
}

/// Selects a single file.
pub fn open_file(
    app: AppHandle,
    options: Option<FileDialogOptions>,
) -> Result<Option<String>, String> {
    file_path_to_text(build_file_dialog(&app, options)?.blocking_pick_file())
}

/// Selects multiple files.
pub fn open_files(
    app: AppHandle,
    options: Option<FileDialogOptions>,
) -> Result<Vec<String>, String> {
    file_paths_to_text(
        build_file_dialog(&app, options)?
            .blocking_pick_files()
            .unwrap_or_default(),
    )
}

/// Selects a single directory.
pub fn open_dir(
    app: AppHandle,
    options: Option<FileDialogOptions>,
) -> Result<Option<String>, String> {
    file_path_to_text(build_file_dialog(&app, options)?.blocking_pick_folder())
}

/// Selects a path for saving a file.
pub fn save_file(
    app: AppHandle,
    options: Option<FileDialogOptions>,
) -> Result<Option<String>, String> {
    file_path_to_text(build_file_dialog(&app, options)?.blocking_save_file())
}

/// Shows a system message dialog.
pub fn message(
    app: AppHandle,
    message: String,
    options: Option<MessageDialogOptions>,
) -> Result<(), String> {
    let message = required_text(message, "Message")?;
    let options = options.unwrap_or_default();
    let title = default_title(options.title, "Notice");
    app.dialog()
        .message(message)
        .title(title)
        .kind(dialog_kind(&options.kind, MessageDialogKind::Info)?)
        .buttons(MessageDialogButtons::Ok)
        .blocking_show();
    Ok(())
}

/// Shows a system confirmation dialog.
pub fn confirm(
    app: AppHandle,
    message: String,
    options: Option<MessageDialogOptions>,
) -> Result<bool, String> {
    let message = required_text(message, "Confirmation message")?;
    let options = options.unwrap_or_default();
    let title = default_title(options.title, "Confirm");
    Ok(app
        .dialog()
        .message(message)
        .title(title)
        .kind(dialog_kind(&options.kind, MessageDialogKind::Warning)?)
        .buttons(MessageDialogButtons::OkCancel)
        .blocking_show())
}

/// Builds a file selection dialog.
fn build_file_dialog(
    app: &AppHandle,
    options: Option<FileDialogOptions>,
) -> Result<FileDialogBuilder<tauri::Wry>, String> {
    let options = options.unwrap_or_default();
    let mut builder = app.dialog().file();
    if !options.title.trim().is_empty() {
        builder = builder.set_title(options.title);
    }
    if !options.default_path.trim().is_empty() {
        builder = apply_default_path(builder, PathBuf::from(options.default_path));
    }
    for filter in options.filters {
        validate_filter(&filter)?;
        let extensions = filter.extensions;
        let refs: Vec<&str> = extensions.iter().map(String::as_str).collect();
        builder = builder.add_filter(filter.name, &refs);
    }
    Ok(builder)
}

/// Applies the default path, using a directory as the starting directory or splitting a file path into its parent and default file name.
fn apply_default_path(
    mut builder: FileDialogBuilder<tauri::Wry>,
    path: PathBuf,
) -> FileDialogBuilder<tauri::Wry> {
    if path.is_dir() {
        return builder.set_directory(path);
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        builder = builder.set_directory(parent);
    }
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        builder = builder.set_file_name(name);
    }
    builder
}

/// Validates a file filter.
fn validate_filter(filter: &FileDialogFilter) -> Result<(), String> {
    if filter.name.trim().is_empty() {
        return Err("File filter name cannot be empty".to_string());
    }
    if filter.extensions.is_empty() {
        return Err(format!(
            "File filter `{}` requires at least one extension",
            filter.name
        ));
    }
    for extension in &filter.extensions {
        if extension.trim().is_empty() || extension.contains('.') {
            return Err(format!(
                "File filter `{}` contains an invalid extension",
                filter.name
            ));
        }
    }
    Ok(())
}

/// Converts one file path to absolute path text.
fn file_path_to_text(path: Option<FilePath>) -> Result<Option<String>, String> {
    path.map(file_path_into_text).transpose()
}

/// Converts multiple file paths to absolute path text.
fn file_paths_to_text(paths: Vec<FilePath>) -> Result<Vec<String>, String> {
    paths.into_iter().map(file_path_into_text).collect()
}

/// Converts a Tauri file path to absolute path text.
fn file_path_into_text(path: FilePath) -> Result<String, String> {
    let path = path
        .into_path()
        .map_err(|err| map_error("Convert file path", err))?;
    absolute_path_text(path)
}

/// Returns the default message dialog title when needed.
fn default_title(title: String, fallback: &str) -> String {
    if title.trim().is_empty() {
        fallback.to_string()
    } else {
        title
    }
}

/// Parses a message kind.
fn dialog_kind(value: &str, fallback: MessageDialogKind) -> Result<MessageDialogKind, String> {
    match value.trim() {
        "" => Ok(fallback),
        "info" => Ok(MessageDialogKind::Info),
        "warning" => Ok(MessageDialogKind::Warning),
        "error" => Ok(MessageDialogKind::Error),
        other => Err(format!("Unsupported message kind: {}", other)),
    }
}
