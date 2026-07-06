//! Automatic working-tree sync: the machine follows the code.
//!
//! `moo new` and `moo run`, when invoked from inside the git repository a
//! machine was created from, push the working tree (tracked files plus
//! untracked-unignored files — exactly what `git status` considers "your
//! work") into the guest at the project workdir (`/srv/app` by default,
//! `[project] workdir` in moo.toml).
//!
//! Design constraints this satisfies:
//! - Model B is preserved: the code lands *inside* the machine overlay, so
//!   `moo save` snapshots code + packages + services + data together and
//!   restore is bit-exact — including uncommitted work as it stood at save.
//! - Gitignored files are never pushed and never deleted in the guest:
//!   node_modules, build output, and guest-managed .env survive every sync.
//! - Deletions propagate: a file removed (or switched away) on the host is
//!   removed in the guest, tracked via a manifest the guest agent keeps.
//! - Cheap when idle: a host-side (path, size, mtime) fingerprint skips the
//!   transfer when nothing changed since the last sync of this machine.

use crate::{config, git, shim};
use anyhow::{bail, Context, Result};
use moo_store::{runtime_dir, Machine};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bumped whenever the guest agent's protocol grows an op the host relies
/// on. Participates in the golden-image recipe hash, so stale cached images
/// (which embed the old agent) are rebuilt instead of failing at runtime.
/// v3: the agent configures the machine's private network at boot.
/// v4: the agent runs /etc/rc.local at boot (services survive restores).
pub const AGENT_PROTO_VERSION: u8 = 4;

/// Refuse to push absurdly large trees through the exec channel: the frame
/// is buffered in memory on both sides.
const MAX_SYNC_BYTES: usize = 512 * 1024 * 1024;

fn sync_state_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.syncstate", shim::sanitize(handle)))
}

/// Forget the last-synced fingerprint. Called whenever the guest tree may
/// have changed out from under the host (machine created, restored from a
/// snapshot, or dropped) so the next sync is unconditional.
pub fn invalidate(handle: &str) {
    let _ = std::fs::remove_file(sync_state_path(handle));
}

/// The files `git status` would call "your work": tracked + untracked
/// unignored, NUL-separated, relative to the repo root.
fn worktree_files(root: &Path) -> Result<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "-co", "--exclude-standard"])
        .output()
        .context("list working tree files")?;
    if !out.status.success() {
        bail!(
            "could not list the working tree: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out
        .stdout
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

/// Fingerprint of the tree's shape: paths, sizes, and mtimes. Content is
/// deliberately not read — this must be cheap enough to run on every
/// `moo run` of a large repository.
fn fingerprint(root: &Path, files: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for rel in files {
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        if let Ok(meta) = std::fs::symlink_metadata(root.join(rel)) {
            hasher.update(&meta.len().to_le_bytes());
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            hasher.update(&mtime.to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Gzipped tar of the listed files. Symlinks are archived as symlinks;
/// files that vanish mid-walk (editors, builds) are skipped.
fn pack(root: &Path, files: &[String]) -> Result<Vec<u8>> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    for rel in files {
        let path = root.join(rel);
        if std::fs::symlink_metadata(&path).is_err() {
            continue; // vanished since listing — the next sync will settle it
        }
        tar.append_path_with_name(&path, rel)
            .with_context(|| format!("archive {rel}"))?;
    }
    let gz = tar.into_inner().context("finish archive")?;
    let bytes = gz.finish().context("finish compression")?;
    if bytes.len() > MAX_SYNC_BYTES {
        bail!(
            "working tree is too large to sync ({:.0} MB compressed; limit {} MB)",
            bytes.len() as f64 / 1e6,
            MAX_SYNC_BYTES / (1024 * 1024)
        );
    }
    Ok(bytes)
}

pub struct SyncOutcome {
    pub files: usize,
    pub bytes: usize,
    pub workdir: String,
}

/// Sync the caller's working tree into `machine` if it was created from the
/// repository the caller is inside. Returns:
/// - `Ok(None)` — nothing to do (different/no repo, or tree unchanged),
/// - `Ok(Some(_))` — synced,
/// - `Err(_)` — the machine was supposed to receive the tree and didn't.
pub fn sync_into(machine: &Machine) -> Result<Option<SyncOutcome>> {
    if machine.project_root.is_empty() {
        return Ok(None);
    }
    let Some(root) = git::toplevel() else {
        return Ok(None);
    };
    if root != Path::new(&machine.project_root) {
        return Ok(None);
    }

    let files = worktree_files(&root)?;
    let print = fingerprint(&root, &files);
    let state_path = sync_state_path(&machine.handle);
    if std::fs::read_to_string(&state_path).ok().as_deref() == Some(print.as_str()) {
        return Ok(None);
    }

    let (cfg, _) = config::load()?;
    let workdir = cfg.workdir().to_string();
    let payload = pack(&root, &files)?;
    let bytes = payload.len();
    let frame = moo_vmm::proto::synctree_frame(&workdir, &payload);

    let (code, out) = shim::request(&machine.handle, &frame)
        .with_context(|| format!("sync working tree into '{}'", machine.handle))?;
    if code != 0 {
        bail!(
            "machine '{}' could not install the working tree: {} \
             (machines created before working-tree sync need `moo drop {}` and `moo new {}`)",
            machine.handle,
            String::from_utf8_lossy(&out).trim(),
            machine.handle,
            machine.handle,
        );
    }

    std::fs::create_dir_all(runtime_dir())?;
    let mut f = std::fs::File::create(&state_path)?;
    f.write_all(print.as_bytes())?;

    Ok(Some(SyncOutcome {
        files: files.len(),
        bytes,
        workdir,
    }))
}
