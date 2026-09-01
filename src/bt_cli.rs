//! BT command-line interpreter entry point.
//!
//! This module retains the CLI, script execution, and default-entry behavior. The unified executable entry delegates CLI work here,
//! while scripts start Web services explicitly through `net.listen()`.

use crate::compiler::Compiler;
use crate::lexer::{tokenize, Token};
use crate::libs::net;
use crate::parser::{Parser, Statement};
use crate::path as bt_path;
use crate::source::{analyze_source, SourceMode};
use crate::vm::Vm;
use console::style;
use std::fmt::Display;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;

const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Initializes Windows console encoding.
///
/// BT emits UTF-8 diagnostics and prompts, while older Windows consoles may still default to a legacy code page and display those bytes incorrectly.
/// Switch to UTF-8 at startup so command-line output and Web service logs use the same encoding.
#[cfg(windows)]
fn init_console_encoding() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleCP, SetConsoleMode, SetConsoleOutputCP,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let _ = SetConsoleCP(65001);
        let _ = SetConsoleOutputCP(65001);

        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if !handle.is_null() {
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

/// Non-Windows platforms are naturally output in UTF-8. The empty function with the same name is reserved here to avoid conditional branches at the main entry.
#[cfg(not(windows))]
fn init_console_encoding() {}

/// Reads, lexes and parses a BT file.
fn parse_file(path: &str) -> Result<Vec<Statement>, String> {
    let source =
        fs::read_to_string(path).map_err(|err| format!("Failed to read `{}`: {}", path, err))?;
    let document = analyze_source(path, &source)?;
    if document.mode != SourceMode::Script {
        return Err(format!(
            "{}:1:1: `{}` mode cannot be compiled directly as a normal script",
            path,
            match document.mode {
                SourceMode::Script => "SCRIPT",
                SourceMode::Template => "TPL",
            }
        ));
    }
    let tokens: Vec<Token> = tokenize(&document.body).collect();
    let mut parser = Parser::new(path, &document.body, tokens);
    parser.parse().map_err(|err| err.to_string())
}

/// Compiles and executes a BT file.
fn run_bytecode_file(path: &str) -> Result<(), String> {
    if !Path::new(path).is_file() {
        return Err(format!("Script file not found: {}", path));
    }

    let script_path = Path::new(path)
        .canonicalize()
        .map_err(|err| format!("Parsing script path `{}` failed: {}", path, err))?;
    let display_path = bt_path::path_text(&script_path);
    let statements = parse_file(&display_path)?;
    let base_dir = script_path.parent().unwrap_or_else(|| Path::new("."));
    let chunk = Compiler::with_source_file(display_path, base_dir)
        .compile(&statements)
        .map_err(|err| err.to_string())?;

    let mut vm = Vm::with_project_root(base_dir);
    vm.load_project_extensions()?;
    let output = match vm.run(&chunk) {
        Ok(output) => output,
        Err(err) => return Err(format_error_after_output(vm.output(), err)),
    };
    print!("{}", output);
    vm.clear_output();
    wait_for_background_tasks(&mut vm, &chunk);
    Ok(())
}

/// Put the runtime error after the content that the script has output, so that users can see the debugging print first.
fn format_error_after_output(output: &str, err: impl Display) -> String {
    if output.is_empty() {
        return err.to_string();
    }
    let err = err.to_string();
    let mut text = String::with_capacity(output.len() + 1 + err.len());
    text.push_str(output);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&err);
    text
}

/// Executes the default entry script in the current directory.
fn run_main_bt(main_bt: &Path) {
    let path = main_bt.to_string_lossy();
    if let Err(err) = run_bytecode_file(&path) {
        println!("{}", err);
    }
}

/// If a script initiates a background event, keep the CLI process resident and dispatch event callbacks.
fn wait_for_background_tasks(vm: &mut Vm, chunk: &crate::bytecode::Chunk) {
    if !net::has_background_tasks() && !vm.has_background_events() {
        return;
    }
    if net::has_event_tasks() || vm.has_background_events() {
        if let Err(err) = vm.wait_for_background_events(chunk) {
            println!("{}", err);
        }
        return;
    }
    wait_for_background_tasks_without_vm();
}

/// Waits for background network services that do not require VM callbacks.
fn wait_for_background_tasks_without_vm() {
    if !net::has_background_tasks() {
        return;
    }
    if let Err(err) = net::wait_for_background_tasks() {
        println!("{}", err);
    }
}

/// Prints the welcome interface in command line mode.
fn print_cli_banner() {
    // let title = style("BT Language").true_color(107,155,110).bold();
    // let version = style(VERSION).true_color(81, 94, 110).bold();
    // println!();
    // println!("  {} {}", title, version);
    println!();
    println!("   {}", style(" /\\_/\\`").true_color(107, 155, 110));
    println!("   {}", style("( •.• )").true_color(107, 155, 110));
    println!(
        "   {} {}",
        style(" / >BT Language").true_color(107, 155, 110),
        style(VERSION).true_color(81, 94, 110).bold()
    );
    print_cli_help();
}

/// Prints short help for commands with incorrect parameters or unknown interactive mode.
fn print_cli_help() {
    //   ┌─────────────────────────────────────────────────┐
    //   │ Command Help.                                   │
    //   ├─────────────────────────────────────────────────┤
    //   │  -c <file>     Run a BT script               |
    //   │  -v            Show version                  │
    //   │  -h            Show help                     │
    //   │  -e            Exit                          │
    //   └─────────────────────────────────────────────────┘
    println!(
        "  {}",
        style("┌─────────────────────────────────────────────────┐").true_color(81, 94, 110)
    );
    println!(
        "  {}",
        style("│ Command Help.                                   │").true_color(81, 94, 110)
    );
    println!(
        "  {}",
        style("├─────────────────────────────────────────────────┤").true_color(81, 94, 110)
    );
    println!(
        "  {}  {}      {}",
        style("│").true_color(81, 94, 110),
        style("-c <file>").true_color(107, 155, 110).bold(),
        style("Run script                      │").true_color(81, 94, 110)
    );
    println!(
        "  {}  {}             {}",
        style("│").true_color(81, 94, 110),
        style("-v").true_color(107, 155, 110).bold(),
        style("Show version                    │").true_color(81, 94, 110)
    );
    println!(
        "  {}  {}             {}",
        style("│").true_color(81, 94, 110),
        style("-h").true_color(107, 155, 110).bold(),
        style("Show help                       │").true_color(81, 94, 110)
    );
    println!(
        "  {}  {}      {}",
        style("│").true_color(81, 94, 110),
        style("ext <cmd>").true_color(107, 155, 110).bold(),
        style("Extension tools                 │").true_color(81, 94, 110)
    );
    println!(
        "  {}  {} {}",
        style("│").true_color(81, 94, 110),
        style("install <name>").true_color(107, 155, 110).bold(),
        style("Install official extension      │").true_color(81, 94, 110)
    );
    println!(
        "  {}  {}             {}",
        style("│").true_color(81, 94, 110),
        style("-e").true_color(107, 155, 110).bold(),
        style("Exit                            │").true_color(81, 94, 110)
    );
    println!(
        "  {}",
        style("└─────────────────────────────────────────────────┘").true_color(81, 94, 110)
    );
    println!();
}

/// Execute a command line command and return whether the command was successfully matched.
fn run_cli_command(
    command: &str,
    values: &[String],
    allow_file_shortcut: bool,
) -> Result<bool, String> {
    match command {
        "-c" => {
            let path = values
                .first()
                .map(|value| value.as_str())
                .filter(|path| !path.trim().is_empty())
                .ok_or_else(|| "Missing script path: -c <file>".to_string())?;
            run_bytecode_file(path.trim())?;
            Ok(true)
        }
        "-v" => {
            println!("{}", VERSION);
            Ok(true)
        }
        "-e" => Ok(true),
        "ext" => {
            run_extension_cli(values)?;
            Ok(true)
        }
        "install" => {
            run_extension_install_cli(values)?;
            Ok(true)
        }
        _ if allow_file_shortcut && !command.starts_with('-') => {
            run_bytecode_file(command)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Execute the extended toolchain command after enabling the extended feature.
#[cfg(feature = "extensions")]
fn run_extension_cli(args: &[String]) -> Result<(), String> {
    crate::extensions::cli::run(args)
}

/// Execute the official extension library installation command after enabling the extension feature.
#[cfg(feature = "extensions")]
fn run_extension_install_cli(args: &[String]) -> Result<(), String> {
    crate::extensions::cli::install(args)
}

/// Output an explicit prompt for the extension toolchain in lightweight builds without extension capabilities enabled.
#[cfg(not(feature = "extensions"))]
fn run_extension_cli(_args: &[String]) -> Result<(), String> {
    Err("This BT build does not include extension support, so extension commands are unavailable. Extension support is enabled in default builds; rebuild without --no-default-features.".to_string())
}

/// In the lightweight build that does not enable extension capabilities, output a clear prompt for the official extension library installation.
#[cfg(not(feature = "extensions"))]
fn run_extension_install_cli(_args: &[String]) -> Result<(), String> {
    Err("This BT build does not include extension support, so packages cannot be installed from the official extension registry. Extension support is enabled in default builds; rebuild without --no-default-features.".to_string())
}

/// Parses a line of input in interactive mode.
fn run_interactive_command(input: &str) -> Result<bool, String> {
    if input == "-e" || input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
        return Ok(false);
    }
    if input == "-v" {
        println!("{}", VERSION);
        return Ok(true);
    }
    if let Some(rest) = input.strip_prefix("-c") {
        let path = rest.trim();
        if path.is_empty() {
            return Err("Missing script path: -c <file>".to_string());
        }
        run_bytecode_file(path)?;
        return Ok(true);
    }
    if input == "-h" {
        print_cli_help();
        return Ok(true);
    }

    // Forward install / ext subcommands to command line parsing logic
    let mut parts = input.split_whitespace();
    if let Some(command) = parts.next() {
        let values: Vec<String> = parts.map(|s| s.to_string()).collect();
        match command {
            "install" => {
                run_extension_install_cli(&values)?;
                return Ok(true);
            }
            "ext" => {
                run_extension_cli(&values)?;
                return Ok(true);
            }
            _ => {}
        }
    }

    Err(format!("Unknown command: {}", input))
}

/// A lightweight interactive mode that runs when the current directory has no default entry.
fn run_cli_loop() {
    print_cli_banner();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{}", style("bt> ").true_color(107, 155, 110).bold());
        if let Err(err) = io::stdout().flush() {
            println!("Failed to refresh output: {}", err);
            return;
        }
        let Some(line) = lines.next() else {
            println!();
            return;
        };
        let input = match line {
            Ok(line) => line,
            Err(err) => {
                println!("Failed to read input: {}", err);
                continue;
            }
        };
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        match run_interactive_command(input) {
            Ok(true) => {}
            Ok(false) => {
                println!("{}", style("bye").dim());
                return;
            }
            Err(err) => {
                println!("{}", style(err).red());
                print_cli_help();
            }
        }
    }
}

/// Starting the BT command line interpreter.
pub fn main() {
    init_console_encoding();
    let mut args = std::env::args();
    let _program = args.next();
    if let Some(command) = args.next() {
        let values: Vec<String> = args.collect();
        match run_cli_command(&command, &values, true) {
            Ok(true) => {}
            Ok(false) => {
                println!("{}", style(format!("Unknown command: {}", command)).red());
                print_cli_help();
            }
            Err(err) => {
                println!("{}", style(err).red());
            }
        }
        return;
    }

    let main_bt = Path::new("main.bt");
    if main_bt.exists() {
        run_main_bt(main_bt);
    } else {
        run_cli_loop();
    }
}
