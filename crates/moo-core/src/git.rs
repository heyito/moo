//! Read-only git queries. `moo` never writes to git.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Resolve any ref-ish string (branch, tag, SHA, HEAD) to a full SHA.
pub fn resolve(refish: &str) -> Option<String> {
    git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{refish}^{{commit}}"),
    ])
}

/// The SHA a handle shadows right now: the branch tip if a branch with the
/// handle's name exists, otherwise the current HEAD, otherwise none (not in
/// a git repository).
pub fn shadowed_head(handle: &str) -> Option<String> {
    resolve(&format!("refs/heads/{handle}")).or_else(|| resolve("HEAD"))
}

/// The repository's top-level directory, if inside one.
pub fn toplevel() -> Option<std::path::PathBuf> {
    git(&["rev-parse", "--show-toplevel"]).map(std::path::PathBuf::from)
}
