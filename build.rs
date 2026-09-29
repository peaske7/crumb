//! Stamps the build with its commit, so `crumb --version` says which code a
//! bug report came from: `0.1.0 (1a2b3c4)`, or `0.1.0 (1a2b3c4-dirty)` for a
//! build with uncommitted changes. This runs at build time, not in crumb, so
//! it calls git directly rather than through the runner.

use std::path::Path;
use std::process::Command;

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let stamped = match commit() {
        Some(commit) => format!("{version} ({commit})"),
        None => version.to_string(),
    };
    println!("cargo:rustc-env=CRUMB_VERSION={stamped}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    // A commit or a checkout moves HEAD and rewrites the index.
    if let Some(paths) = git(&["rev-parse", "--git-path", "HEAD", "--git-path", "index"]) {
        for path in paths.lines() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// The short commit from the `.cargo_vcs_info.json` that `cargo package`
/// writes (a build from crates.io), else from git.
fn commit() -> Option<String> {
    if let Ok(info) = std::fs::read_to_string(Path::new(".cargo_vcs_info.json")) {
        let rest = &info[info.find("\"sha1\"")? + "\"sha1\"".len()..];
        let sha: String = rest
            .chars()
            .skip_while(|c| !c.is_ascii_hexdigit())
            .take_while(char::is_ascii_hexdigit)
            .take(7)
            .collect();
        let dirty = info.contains("\"dirty\": true") || info.contains("\"dirty\":true");
        return (sha.len() == 7).then(|| if dirty { format!("{sha}-dirty") } else { sha });
    }
    let sha = git(&["rev-parse", "--short=7", "HEAD"])?.trim().to_string();
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|out| !out.trim().is_empty());
    Some(if dirty { format!("{sha}-dirty") } else { sha })
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
