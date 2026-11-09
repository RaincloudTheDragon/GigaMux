//! Embed short git SHA for pre-release encoder metadata labels.

use std::process::Command;

fn main() {
    // Rebuild when HEAD moves (best-effort; packed-refs / detached still usually touch HEAD).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");

    let sha = git_sha_short8().unwrap_or_default();
    println!("cargo:rustc-env=GIGAMUX_GIT_SHA={sha}");
}

/// First 8 hex chars of `git rev-parse HEAD`, if git and a repo are available.
fn git_sha_short8() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let full = String::from_utf8(out.stdout).ok()?;
    let hex: String = full
        .trim()
        .chars()
        .take(8)
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.len() == 8 {
        Some(hex)
    } else {
        None
    }
}
