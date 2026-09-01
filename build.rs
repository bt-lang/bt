// BT language build script
// Embed per-binary file metadata and icons when compiling on Windows.
fn main() {
    write_tauri_acl_files();

    // The Windows interpreter has deep recursion and script-call chains, so it needs a larger main-thread stack.
    // Apply this only to the final binary to avoid affecting dependency build-script linker arguments.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bin=bt=/STACK:268435456");
        println!("cargo:rustc-link-arg-bin=bt_app=/STACK:268435456");
    }
    // Other platforms are unaffected (Linux/macOS skip this logic).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if let Err(err) = compile_windows_resources() {
            panic!("Windows resource compilation failed: {}", err);
        }
    }
}

/// Compile Windows resources for `bt.exe` and `bt_app.exe` and link them to their respective binaries.
///
/// Cargo runs the package-level `build.rs` once rather than once per `[[bin]]`, so global
/// `winresource::compile()` is insufficient. Generate two resource files and attach each
/// precisely with `rustc-link-arg-bin`, keeping the executables' metadata and icons separate.
fn compile_windows_resources() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=src-tauri/icons/bt.ico");
    println!("cargo:rerun-if-changed=src-tauri/icons/app.ico");
    println!("cargo:rerun-if-env-changed=RC_PATH");
    println!("cargo:rerun-if-env-changed=WINDRES");
    println!("cargo:rerun-if-env-changed=AR");

    let out_dir =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let targets = [
        (
            "bt",
            "bt.exe",
            "BT language interpreter",
            "src-tauri/icons/bt.ico",
            false,
        ),
        (
            "bt_app",
            "bt_app.exe",
            "BT desktop application engine",
            "src-tauri/icons/app.ico",
            true,
        ),
    ];

    for (bin_name, original_filename, file_description, icon_path, icon_required) in targets {
        let resource_dir = out_dir.join("winresource").join(bin_name);
        std::fs::create_dir_all(&resource_dir)?;

        let rc_path = resource_dir.join("resource.rc");
        let link_path = compile_one_windows_resource(
            &rc_path,
            &resource_dir,
            original_filename,
            file_description,
            icon_path,
            icon_required,
        )?;
        println!(
            "cargo:rustc-link-arg-bin={}={}",
            bin_name,
            link_path.display()
        );
    }

    Ok(())
}

/// Compiles the Windows resource file for one binary target.
fn compile_one_windows_resource(
    rc_path: &std::path::Path,
    resource_dir: &std::path::Path,
    original_filename: &str,
    file_description: &str,
    icon_path: &str,
    icon_required: bool,
) -> std::io::Result<std::path::PathBuf> {
    let mut res = winresource::WindowsResource::new();
    res.set_manifest(COMMON_CONTROLS_V6_MANIFEST)
        .set("OriginalFilename", original_filename)
        .set("FileDescription", file_description)
        .set("LegalCopyright", "Copyright © 2026")
        .set("InternalName", original_filename)
        .set("ProductName", "BT");

    let icon_path = std::path::Path::new(icon_path);
    if icon_path.exists() {
        res.set_icon(&icon_path.to_string_lossy());
    } else if icon_required {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Required icon file does not exist: {}", icon_path.display()),
        ));
    }
    res.write_resource_file(rc_path)?;

    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => compile_resource_with_msvc(rc_path, resource_dir),
        Ok("gnu") => compile_resource_with_gnu(rc_path, resource_dir),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "Resource files can only be compiled for msvc or gnu Windows targets",
        )),
    }
}

/// Compiles an `.rc` file with MSVC `rc.exe` and returns the `.res` path for the linker.
fn compile_resource_with_msvc(
    rc_path: &std::path::Path,
    resource_dir: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let output_path = resource_dir.join("resource.res");
    let rc_exe = find_rc_exe()?;
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");

    let output = std::process::Command::new(&rc_exe)
        .arg(format!("/I{}", manifest_dir))
        .arg(format!("/fo{}", output_path.display()))
        .arg(rc_path)
        .output()?;

    if output.status.success() {
        Ok(output_path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "rc.exe compilation failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        ))
    }
}

/// Compiles an `.rc` file with GNU `windres` and returns the `.o` path for the linker.
fn compile_resource_with_gnu(
    rc_path: &std::path::Path,
    resource_dir: &std::path::Path,
) -> std::io::Result<std::path::PathBuf> {
    let output_path = resource_dir.join("resource.o");
    let windres = std::env::var("WINDRES").unwrap_or_else(|_| "windres".to_string());
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
    let target_args: &[&str] = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => &["--target", "pe-x86-64"],
        Ok("x86") => &["--target", "pe-i386"],
        _ => &[],
    };

    let status = std::process::Command::new(windres)
        .arg(format!("-I{}", manifest_dir))
        .args(target_args)
        .arg(rc_path)
        .arg(&output_path)
        .status()?;

    if status.success() {
        Ok(output_path)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "windres failed to compile the resource file",
        ))
    }
}

/// Finds an available Windows SDK `rc.exe` on the current machine.
fn find_rc_exe() -> std::io::Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("RC_PATH") {
        return Ok(std::path::PathBuf::from(path));
    }
    if cfg!(unix) {
        return Ok(std::path::PathBuf::from("llvm-rc"));
    }

    if let Some(path) = find_tool_from_path("rc.exe") {
        return Ok(path);
    }

    find_rc_exe_from_windows_sdk()
}

/// Finds the full path of a command in `PATH`.
fn find_tool_from_path(tool: &str) -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("where")
        .arg(tool)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(std::path::PathBuf::from)
}

/// Finds `rc.exe` using Windows SDK registry information.
fn find_rc_exe_from_windows_sdk() -> std::io::Result<std::path::PathBuf> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots",
            "/reg:32",
        ])
        .output()?;

    if !output.status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Windows SDK registry information was not found",
        ));
    }

    let arch_dir = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86") {
        "x86"
    } else {
        "x64"
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if !line.contains("KitsRoot") {
            continue;
        }
        let Some(root) = line.split("REG_SZ").nth(1).map(str::trim) else {
            continue;
        };
        let bin_dir = std::path::Path::new(root).join("bin");
        let Ok(entries) = std::fs::read_dir(bin_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let rc_exe = entry.path().join(arch_dir).join("rc.exe");
            if rc_exe.exists() {
                return Ok(rc_exe);
            }
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Windows SDK rc.exe was not found",
    ))
}

/// Generates the ACL files consumed by Tauri macros.
///
/// The project root is the Cargo package root, while Tauri configuration lives under
/// `src-tauri`. Parse `src-tauri/capabilities` and `src-tauri/permissions` directly and write
/// the results to `OUT_DIR`. These files are only used for compile-time permission resolution;
/// they do not add Tauri/WebView2 linkage to `bt_cli.exe`.
fn write_tauri_acl_files() {
    use std::collections::BTreeMap;
    use std::env;
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=src-tauri/capabilities");
    println!("cargo:rerun-if-changed=src-tauri/permissions");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set"));
    let capabilities = tauri_utils::acl::build::parse_capabilities("src-tauri/capabilities/**/*")
        .expect("Failed to parse Tauri capabilities");
    let permission_files = tauri_utils::acl::build::define_permissions(
        "src-tauri/permissions/**/*",
        "__app-acl__",
        &out_dir,
        |path| path.is_file(),
    )
    .expect("Failed to parse Tauri permissions");

    let mut acl = BTreeMap::new();
    acl.insert(
        tauri_utils::acl::APP_ACL_KEY.to_string(),
        tauri_utils::acl::manifest::Manifest::new(permission_files, None),
    );

    write_json(
        out_dir.join(tauri_utils::acl::ACL_MANIFESTS_FILE_NAME),
        &acl,
    );
    write_json(
        out_dir.join(tauri_utils::acl::CAPABILITIES_FILE_NAME),
        &capabilities,
    );
}

/// Writes a JSON file for `tauri::generate_context!` to read at compile time.
fn write_json<T: serde::Serialize>(path: impl AsRef<std::path::Path>, value: &T) {
    let path = path.as_ref();
    let json = serde_json::to_vec(value).expect("Failed to serialize the Tauri ACL");
    std::fs::write(path, json).expect("Failed to write the Tauri ACL file");
}

/// Windows Common Controls v6 manifest.
///
/// The Tauri/tao window runtime uses comctl32 v6 APIs such as `TaskDialogIndirect`.
/// Without this manifest, Windows may bind an older comctl32 and fail before entering `main`.
const COMMON_CONTROLS_V6_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
        type="win32"
        name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0"
        processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df"
        language="*" />
    </dependentAssembly>
  </dependency>
</assembly>
"#;
