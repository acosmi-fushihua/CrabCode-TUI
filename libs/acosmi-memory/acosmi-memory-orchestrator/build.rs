use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=CRABCODE_BUILD_ID");

    let build_id = std::env::var("CRABCODE_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(derive_build_id);
    println!("cargo:rustc-env=CRABCODE_BUILD_ID={build_id}");
}

fn derive_build_id() -> String {
    let version = product_version().unwrap_or_else(|| "0.0.0".to_string());
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|raw| raw.trim().to_string())
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{version}+{sha}")
}

fn product_version() -> Option<String> {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let workspace =
        std::fs::read_to_string(manifest_dir.join("../../../crates/Cargo.toml")).ok()?;
    let package_section = workspace.split("[workspace.package]").nth(1)?;
    let version_line = package_section
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("version = "))?;
    version_line
        .split_once('=')?
        .1
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')
        .map(str::to_string)
}
