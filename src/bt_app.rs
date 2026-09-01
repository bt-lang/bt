//! Entry point for the BT desktop application tool.
//!
//! Distinguishes general runtime commands, external BTR files, embedded BTR/legacy bundles,
//! and development directories, then starts the appropriate application or build workflow.

use crate::error::BtError;
use std::path::PathBuf;

/// Reserved argument used by a packaged application to launch an external BTR internally.
pub const INTERNAL_RUN_BTR_ARG: &str = "--bt-runtime-run-btr-v1";

/// Starts the BT desktop application tool.
pub fn main() {
    if let Err(err) = run() {
        if !crate::app::dependency::show_friendly_startup_error(&err) {
            eprintln!("{}", err);
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), BtError> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(INTERNAL_RUN_BTR_ARG) {
        let path = args.get(2).ok_or_else(|| {
            BtError::Config("Internal BTR launch argument is missing a file path".to_string())
        })?;
        let app_args = trailing_app_args(&args, 3);
        return crate::app::runtime::start_app(Some(PathBuf::from(path)), app_args);
    }
    let is_bundled_app = std::env::current_exe()
        .ok()
        .as_deref()
        .map(crate::bundle::footer::has_bundle_injected)
        .unwrap_or(false);
    if is_bundled_app {
        // Remaining arguments come from file associations or the user and are not bt_app commands.
        return crate::app::runtime::start_app(None, args.into_iter().skip(1).collect());
    }
    match args.get(1).map(|s| s.as_str()) {
        None => crate::app::runtime::start_app(None, Vec::new()),
        Some("run") => {
            let target = args.get(2).filter(|value| value.as_str() != "--");
            let app_args = trailing_app_args(&args, if target.is_some() { 3 } else { 2 });
            crate::app::runtime::start_app(target.map(PathBuf::from), app_args)
        }
        Some("pack") => crate::app::build::pack_project(),
        Some("info") => {
            let path = args
                .get(2)
                .ok_or_else(|| BtError::Config("Usage: bt_app.exe info <app.btr>".to_string()))?;
            crate::app::build::info_btr(PathBuf::from(path).as_path())
        }
        Some("associate") => {
            crate::app::file_association::register_btr_runtime()?;
            println!("Registered the current bt_app as the program for opening .btr applications");
            Ok(())
        }
        Some("build") => crate::app::build::build_project(),
        Some("bundle-check") => crate::app::build::bundle_check(),
        Some("export") => {
            let platform = args.get(2).map(|s| s.as_str()).unwrap_or("current");
            crate::app::build::export_project(platform)
        }
        Some(path) if path.to_ascii_lowercase().ends_with(".btr") => {
            let app_args = trailing_app_args(&args, 2);
            crate::app::runtime::start_app(Some(PathBuf::from(path)), app_args)
        }
        Some(cmd) => Err(BtError::Config(format!(
            "Unknown command: {}\nUsage: bt_app.exe [run|pack|info|associate|build|bundle-check|export]",
            cmd
        ))),
    }
}

/// Returns application arguments after the command arguments, skipping an optional `--` separator.
fn trailing_app_args(args: &[String], start: usize) -> Vec<String> {
    let mut index = start;
    if args.get(index).map(String::as_str) == Some("--") {
        index += 1;
    }
    args[index..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Application arguments omit the optional separator so BTR scripts never see bt_app syntax.
    #[test]
    fn trailing_arguments_skip_optional_separator() {
        let args = ["bt_app", "demo.btr", "--", "open.txt"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();

        assert_eq!(trailing_app_args(&args, 2), vec!["open.txt"]);
        assert_eq!(trailing_app_args(&args, 3), vec!["open.txt"]);
    }
}
