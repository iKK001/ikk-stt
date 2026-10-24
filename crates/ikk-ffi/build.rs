//! Embeds the git commit and build date into the core so every shell can
//! show exactly which build is running. Read-only git access only.
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short=10", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!(
        "cargo:rustc-env=IKK_GIT_SHA={}{}",
        sha,
        if dirty { "-dirty" } else { "" }
    );
    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=IKK_BUILD_DATE={date}");
    // Build number: `[workspace.metadata.ikk] build_number` in the root Cargo.toml.
    let build_number = build_number_from_workspace()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_else(|| "0".into());
    println!("cargo:rustc-env=IKK_BUILD_NUMBER={build_number}");
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=../../LICENSE");
    println!("cargo:rerun-if-changed=../../NOTICE");
    println!("cargo:rerun-if-changed=../../THIRD_PARTY.md");
}

/// Reads `build_number = N` from the `[workspace.metadata.ikk]` table of the
/// root Cargo.toml without a TOML dependency (the table is ours, one key).
fn build_number_from_workspace() -> Option<String> {
    let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")).ok()?;
    let mut in_table = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_table = line == "[workspace.metadata.ikk]";
            continue;
        }
        if in_table {
            if let Some(rest) = line.strip_prefix("build_number") {
                let value = rest.trim_start().strip_prefix('=')?.trim();
                return Some(value.trim_matches('"').to_string());
            }
        }
    }
    None
}
