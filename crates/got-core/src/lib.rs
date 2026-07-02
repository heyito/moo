//! Lifecycle orchestration for the four verbs (plan.md §6).

pub mod git;
pub mod shim;

use anyhow::{bail, Context, Result};
use got_store::{
    cow_clone, images_dir, machines_dir, save_snapshot, timestamp, Machine, Registry, Snapshot,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_CPUS: u8 = 2;
const DEFAULT_RAM_MIB: u32 = 4096;

/// The default base image for machines. The MVP ships prebuilt images;
/// until the fetch pipeline lands, `got doctor` explains how to build one.
pub fn default_base_image() -> PathBuf {
    images_dir().join("default.img")
}

fn overlay_path(handle: &str) -> PathBuf {
    machines_dir().join(format!("{}.img", shim::sanitize(handle)))
}

/// Spawn the detached supervisor for `handle` and wait for its socket.
fn boot(handle: &str, overlay: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locate own binary")?;
    std::process::Command::new(exe)
        .args([
            "__shim",
            handle,
            overlay.to_str().context("bad overlay path")?,
            &DEFAULT_CPUS.to_string(),
            &DEFAULT_RAM_MIB.to_string(),
        ])
        .env(got_vmm::LOADER_PATH_VAR, got_vmm::loader_path_value())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("start machine supervisor")?;

    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if shim::is_running(handle) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("machine '{}' did not come up", handle);
}

/// Stop a running machine gracefully (quiesce + power off) and wait.
fn stop(handle: &str) -> Result<()> {
    if !shim::is_running(handle) {
        return Ok(());
    }
    let _ = shim::request(handle, got_vmm::proto::POWEROFF);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if !shim::socket_path(handle).exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("machine '{}' did not shut down", handle);
}

pub struct NewOutcome {
    pub handle: String,
    /// Set when the machine was rebooted from a saved snapshot.
    pub restored_from: Option<Snapshot>,
    pub created: bool,
}

/// `got new <name> [from <src>]` — idempotent create/restore (plan.md §6.1).
pub fn new_machine(name: &str, from: Option<&str>, detached: bool) -> Result<NewOutcome> {
    let reg = Registry::open()?;
    std::fs::create_dir_all(machines_dir())?;

    if let Some(existing) = reg.get_machine(name)? {
        // Existing handle: prefer the snapshot saved for the current HEAD of
        // the shadowed ref; otherwise reuse the live overlay.
        let head = git::shadowed_head(name);
        let snap = match &head {
            Some(sha) => reg.find_snapshot(name, sha)?,
            None => None,
        };
        let overlay = PathBuf::from(&existing.overlay_path);

        if let Some(snap) = snap {
            let live_hash_matches = overlay.exists()
                && !shim::is_running(name)
                && got_store::content_hash(&overlay)? == snap.content_hash;
            if !live_hash_matches {
                stop(name)?;
                if overlay.exists() {
                    std::fs::remove_file(&overlay)?;
                }
                cow_clone(Path::new(&snap.snapshot_path), &overlay)?;
                boot(name, &overlay)?;
                return Ok(NewOutcome {
                    handle: name.to_string(),
                    restored_from: Some(snap),
                    created: false,
                });
            }
        }
        if !shim::is_running(name) {
            boot(name, &overlay)?;
        }
        return Ok(NewOutcome {
            handle: name.to_string(),
            restored_from: None,
            created: false,
        });
    }

    // New handle: resolve the source into a disk to clone.
    let mut restored_from = None;
    let (source_disk, base_commit, parent): (PathBuf, Option<String>, Option<String>) =
        match from {
            Some(src) if src.starts_with("s_") => {
                let snap = reg
                    .get_snapshot_by_id(src)?
                    .with_context(|| format!("no snapshot '{}'", src))?;
                let path = PathBuf::from(&snap.snapshot_path);
                let sha = snap.head_sha.clone();
                restored_from = Some(snap);
                (path, sha, None)
            }
            Some(src) => {
                if let Some(other) = reg.get_machine(src)? {
                    // Fork another machine: quiesce it first (plan.md §5.1).
                    if shim::is_running(src) {
                        let (code, _) = shim::request(src, got_vmm::proto::QUIESCE)?;
                        anyhow::ensure!(code == 0, "could not quiesce machine '{}'", src);
                    }
                    got_store::full_fsync(Path::new(&other.overlay_path))?;
                    (
                        PathBuf::from(&other.overlay_path),
                        other.base_commit.clone(),
                        Some(other.handle.clone()),
                    )
                } else if let Some(sha) = git::resolve(src) {
                    // A commit-ish: restore this handle's snapshot for that
                    // SHA if one exists, else start from the base image.
                    match reg.find_snapshot(name, &sha)? {
                        Some(snap) => {
                            let path = PathBuf::from(&snap.snapshot_path);
                            restored_from = Some(snap);
                            (path, Some(sha), None)
                        }
                        None => (default_base_image(), Some(sha), None),
                    }
                } else {
                    bail!("'{}' is not a snapshot, machine, or git commit", src);
                }
            }
            None => {
                // Reusing a dropped handle restores the snapshot saved for
                // the SHA it shadows now, if any (plan.md §6.4).
                let head = git::shadowed_head(name);
                match &head {
                    Some(sha) => match reg.find_snapshot(name, sha)? {
                        Some(snap) => {
                            let path = PathBuf::from(&snap.snapshot_path);
                            restored_from = Some(snap);
                            (path, head.clone(), None)
                        }
                        None => (default_base_image(), head.clone(), None),
                    },
                    None => (default_base_image(), None, None),
                }
            }
        };

    if !source_disk.exists() {
        bail!(
            "no base image at {} — run `got doctor` for setup instructions",
            source_disk.display()
        );
    }

    let overlay = overlay_path(name);
    if overlay.exists() {
        std::fs::remove_file(&overlay)?;
    }
    cow_clone(&source_disk, &overlay)?;

    reg.insert_machine(&Machine {
        handle: name.to_string(),
        base_commit,
        recipe_hash: "default".into(),
        parent_machine: parent,
        base_image_path: default_base_image().to_string_lossy().into_owned(),
        overlay_path: overlay.to_string_lossy().into_owned(),
        lifecycle: "live".into(),
        detached,
        created_at: timestamp(),
    })?;

    boot(name, &overlay)?;
    Ok(NewOutcome {
        handle: name.to_string(),
        restored_from,
        created: true,
    })
}

/// `got run <name> -- <cmd>` — execute inside the machine (plan.md §6.2).
pub fn run_in_machine(name: &str, cmd: &str) -> Result<(u8, Vec<u8>)> {
    let reg = Registry::open()?;
    if reg.get_machine(name)?.is_none() {
        bail!("no machine '{}' — create it with `got new {}`", name, name);
    }
    if !shim::is_running(name) {
        // Machines persist between invocations; reboot the live overlay.
        let m = reg.get_machine(name)?.unwrap();
        boot(name, Path::new(&m.overlay_path))?;
    }
    shim::request(name, cmd.as_bytes())
}

pub struct SaveOutcome {
    pub snapshot: Snapshot,
    pub fresh: bool,
}

/// `got save <name>` — quiesce, snapshot, associate with HEAD (plan.md §6.3).
pub fn save_machine(name: &str) -> Result<SaveOutcome> {
    let reg = Registry::open()?;
    let m = reg
        .get_machine(name)?
        .with_context(|| format!("no machine '{}'", name))?;

    if shim::is_running(name) {
        let (code, out) = shim::request(name, got_vmm::proto::QUIESCE)?;
        if code != 0 {
            bail!(
                "could not quiesce machine '{}': {}",
                name,
                String::from_utf8_lossy(&out)
            );
        }
    }

    let head = if m.detached { None } else { git::shadowed_head(name) };
    let (snapshot, fresh) =
        save_snapshot(&reg, name, head.as_deref(), Path::new(&m.overlay_path))?;
    Ok(SaveOutcome { snapshot, fresh })
}

/// `got save` with no name — save every registered machine.
pub fn save_all() -> Result<Vec<(String, SaveOutcome)>> {
    let reg = Registry::open()?;
    let mut results = Vec::new();
    for m in reg.list_machines()? {
        let outcome = save_machine(&m.handle)?;
        results.push((m.handle, outcome));
    }
    Ok(results)
}

/// `got drop <name>` — destroy the live machine; snapshots survive unless
/// `drop_snapshots` (plan.md §6.4). Idempotent.
pub fn drop_machine(name: &str, force: bool, drop_snapshots: bool) -> Result<()> {
    let reg = Registry::open()?;

    if shim::is_running(name) {
        if force {
            if let Ok(pid_str) = std::fs::read_to_string(shim::pid_path(name)) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    // The supervisor leads its own process group (setsid);
                    // this takes the guest down with it.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
            }
            let _ = std::fs::remove_file(shim::socket_path(name));
            let _ = std::fs::remove_file(shim::pid_path(name));
        } else {
            stop(name)?;
        }
    }

    if let Some(m) = reg.get_machine(name)? {
        let overlay = PathBuf::from(&m.overlay_path);
        if overlay.exists() {
            std::fs::remove_file(&overlay)?;
        }
        reg.remove_machine(name)?;
    }

    if drop_snapshots {
        let removed = reg.remove_snapshots(name)?;
        for snap in removed {
            // Content files are shared across handles; only delete when the
            // last reference is gone.
            if !reg.snapshot_content_referenced(&snap.content_hash)? {
                let _ = std::fs::remove_file(&snap.snapshot_path);
            }
        }
    }
    Ok(())
}

pub struct LsRow {
    pub machine: Machine,
    pub running: bool,
    pub snapshots: Vec<Snapshot>,
}

/// `got ls` — read-only listing.
pub fn list() -> Result<Vec<LsRow>> {
    let reg = Registry::open()?;
    let mut rows = Vec::new();
    for m in reg.list_machines()? {
        let running = shim::is_running(&m.handle);
        let snapshots = reg.list_snapshots(Some(&m.handle))?;
        rows.push(LsRow { machine: m, running, snapshots });
    }
    Ok(rows)
}
