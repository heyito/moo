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

/// The top-level directory of the current checkout, if inside one. For a
/// linked worktree this is the worktree's own root — the right base for
/// reading files (moo.toml, the working tree to sync).
pub fn toplevel() -> Option<std::path::PathBuf> {
    git(&["rev-parse", "--show-toplevel"]).map(std::path::PathBuf::from)
}

/// The main repository's top-level directory, if inside one. All worktrees
/// of a repository resolve to the same path, so machine scope is shared:
/// `moo new feat/x from base` works from any worktree.
pub fn main_root() -> Option<std::path::PathBuf> {
    // The common dir is the main repository's .git directory from every
    // worktree; its parent is the main checkout.
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = std::path::PathBuf::from(common);
    match common.file_name().and_then(|n| n.to_str()) {
        Some(".git") => common.parent().map(std::path::Path::to_path_buf),
        // Bare or unusual layouts: fall back to the checkout's own toplevel.
        _ => toplevel(),
    }
}
