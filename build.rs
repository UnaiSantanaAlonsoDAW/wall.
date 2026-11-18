use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    embed_icons();
    // Ship one executable: static MSVC startup/VCRuntime, Windows-provided UCRT.
    // https://learn.microsoft.com/cpp/porting/upgrade-your-code-to-the-universal-crt
    for library in [
        "libvcruntimed",
        "vcruntime",
        "vcruntimed",
        "libcmtd",
        "msvcrt",
        "msvcrtd",
        "libucrt",
        "libucrtd",
    ] {
        println!("cargo:rustc-link-arg=/NODEFAULTLIB:{library}.lib");
    }
    for library in ["libcmt", "libvcruntime", "ucrt"] {
        println!("cargo:rustc-link-arg=/DEFAULTLIB:{library}.lib");
    }
}

fn embed_icons() {
    for file in ["brand.rc", "wall.ico", "tray-light.ico", "tray-dark.ico"] {
        println!("cargo:rerun-if-changed=assets/{file}");
    }
    for variable in [
        "RC",
        "WindowsSdkDir",
        "WindowsSDKVersion",
        "ProgramFiles(x86)",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let compiler = resource_compiler();
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("brand.res");
    let status = Command::new(&compiler)
        .current_dir("assets")
        .arg("/nologo")
        .arg(format!("/fo{}", output.display()))
        .arg("brand.rc")
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "Cannot run {}: {error}. Install the Windows SDK or set RC.",
                compiler.display()
            )
        });
    assert!(
        status.success(),
        "Cannot compile wall. icon resources: {status}"
    );
    println!("cargo:rustc-link-arg-bin=wall={}", output.display());
}

fn resource_compiler() -> PathBuf {
    if let Some(compiler) = env::var_os("RC") {
        return PathBuf::from(compiler);
    }
    let sdk = env::var_os("WindowsSdkDir").map(PathBuf::from).or_else(|| {
        env::var_os("ProgramFiles(x86)").map(|path| PathBuf::from(path).join("Windows Kits/10"))
    });
    if let Some(sdk) = sdk {
        let host = env::var("HOST").unwrap_or_default();
        let architecture = if host.starts_with("aarch64") {
            "arm64"
        } else if host.starts_with("i686") {
            "x86"
        } else {
            "x64"
        };
        let bin = sdk.join("bin");
        if let Some(version) = env::var_os("WindowsSDKVersion") {
            let selected = bin.join(version).join(architecture).join("rc.exe");
            if selected.is_file() {
                return selected;
            }
        }
        let installed = fs::read_dir(&bin)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let version = entry
                    .file_name()
                    .to_string_lossy()
                    .split('.')
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                let compiler = entry.path().join(architecture).join("rc.exe");
                compiler.is_file().then_some((version, compiler))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b));
        if let Some((_, compiler)) = installed {
            return compiler;
        }
    }
    PathBuf::from("rc.exe")
}
