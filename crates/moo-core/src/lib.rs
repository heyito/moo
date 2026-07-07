//! Lifecycle orchestration for the four verbs.

pub mod config;
pub mod git;
pub mod image;
pub mod shim;
pub mod sync;

use anyhow::{bail, Context, Result};
use moo_store::{cow_clone, machines_dir, save_snapshot, timestamp, Machine, Registry, Snapshot};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The current repository scope: machines belong to the repository they
/// were created from, and all worktrees of a repository share its
/// machines. Empty string = created outside any repository.
pub fn scope() -> String {
    git::main_root()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn overlay_path(project_root: &str, handle: &str) -> PathBuf {
    machines_dir().join(format!("{}.img", shim::runtime_name(project_root, handle)))
}

/// Spawn the detached supervisor for `handle` and wait for its socket.
fn boot(project_root: &str, handle: &str) -> Result<()> {
    let exe = std::env::current_exe().context("locate own binary")?;
    // Internal debug: keep the supervisor's stderr when engine logging is on.
    let stderr = if std::env::var("MOO_ENGINE_LOG").is_ok() {
        let f = std::fs::File::create(moo_store::runtime_dir().join(format!(
            "{}.engine.log",
            shim::runtime_name(project_root, handle)
        )))?;
        std::process::Stdio::from(f)
    } else {
        std::process::Stdio::null()
    };
    std::fs::create_dir_all(moo_store::runtime_dir())?;
    std::process::Command::new(exe)
        .args(["__shim", handle, project_root])
        .env(moo_vmm::LOADER_PATH_VAR, moo_vmm::loader_path_value())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
        .context("start machine supervisor")?;

    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if shim::is_running(project_root, handle) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("machine '{handle}' did not come up");
}

/// Deterministic per-handle host ports for the project's guest ports.
/// The candidate derives from a stable hash of
/// (handle, guest port); collisions with ports already in use on the host
/// probe forward until a free one is found.
fn allocate_ports(project_root: &str, handle: &str, guest_ports: &[u16]) -> Vec<(u16, u16)> {
    const RANGE_START: u32 = 20000;
    const RANGE_LEN: u32 = 10000;
    let mut taken: Vec<u16> = Vec::new();
    let mut map = Vec::new();
    for &guest in guest_ports {
        let mut h = blake3::Hasher::new();
        h.update(project_root.as_bytes());
        h.update(&[0]);
        h.update(handle.as_bytes());
        h.update(&guest.to_le_bytes());
        let seed = u32::from_le_bytes(h.finalize().as_bytes()[..4].try_into().unwrap());
        let mut candidate = RANGE_START + (seed % RANGE_LEN);
        for _ in 0..RANGE_LEN {
            let port = candidate as u16;
            let free =
                !taken.contains(&port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
            if free {
                taken.push(port);
                map.push((port, guest));
                break;
            }
            candidate = RANGE_START + ((candidate - RANGE_START + 1) % RANGE_LEN);
        }
    }
    map
}

fn format_port_map(map: &[(u16, u16)]) -> String {
    map.iter()
        .map(|(h, g)| format!("{h}:{g}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Stop a running machine gracefully (quiesce + power off) and wait.
fn stop(project_root: &str, handle: &str) -> Result<()> {
    if !shim::is_running(project_root, handle) {
        return Ok(());
    }
    let _ = shim::request(project_root, handle, moo_vmm::proto::POWEROFF);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10) {
        if !shim::socket_path(project_root, handle).exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("machine '{handle}' did not shut down");
}

pub struct NewOutcome {
    pub handle: String,
    /// Set when the machine was rebooted from a saved snapshot.
    pub restored_from: Option<Snapshot>,
    pub created: bool,
    /// Set when the working tree was synced into the machine.
    pub synced: Option<sync::SyncOutcome>,
}

/// The guest tree may differ from anything the host remembers after a
/// create/restore — force the next sync and run it now if the caller is
/// inside the machine's repository.
fn sync_after_new(reg: &Registry, root: &str, name: &str) -> Result<Option<sync::SyncOutcome>> {
    sync::invalidate(root, name);
    let Some(machine) = reg.get_machine(root, name)? else {
        return Ok(None);
    };
    sync::sync_into(&machine)
}

/// `moo new <name> [from <src>]` — idempotent create/restore. Handles are
/// scoped to the current repository: `base` here and `base` in another
/// repository are different machines.
pub fn new_machine(name: &str, from: Option<&str>, detached: bool) -> Result<NewOutcome> {
    let reg = Registry::open()?;
    let root = scope();
    std::fs::create_dir_all(machines_dir())?;

    if let Some(existing) = reg.get_machine(&root, name)? {
        // Existing handle: prefer the snapshot saved for the current HEAD of
        // the shadowed ref; otherwise reuse the live overlay.
        let head = git::shadowed_head(name);
        let snap = match &head {
            Some(sha) => reg.find_snapshot(&root, name, sha)?,
            None => None,
        };
        let overlay = PathBuf::from(&existing.overlay_path);

        if let Some(snap) = snap {
            let live_hash_matches = overlay.exists()
                && !shim::is_running(&root, name)
                && moo_store::content_hash(&overlay)? == snap.content_hash;
            if !live_hash_matches {
                stop(&root, name)?;
                if overlay.exists() {
                    std::fs::remove_file(&overlay)?;
                }
                cow_clone(Path::new(&snap.snapshot_path), &overlay)?;
                boot(&root, name)?;
                let synced = sync_after_new(&reg, &root, name)?;
                return Ok(NewOutcome {
                    handle: name.to_string(),
                    restored_from: Some(snap),
                    created: false,
                    synced,
                });
            }
        }
        if !shim::is_running(&root, name) {
            boot(&root, name)?;
        }
        let synced = sync_after_new(&reg, &root, name)?;
        return Ok(NewOutcome {
            handle: name.to_string(),
            restored_from: None,
            created: false,
            synced,
        });
    }

    // New handle: resolve the source into a disk to clone. The golden image
    // is built on first use for this project's recipe.
    let (cfg, cfg_root) = config::load()?;
    let base_image = image::ensure(&cfg, &cfg_root)?;
    let mut restored_from = None;
    let (source_disk, base_commit, parent): (PathBuf, Option<String>, Option<String>) = match from {
        Some(src) if src.starts_with("s_") => {
            let snap = reg
                .get_snapshot_by_id(src)?
                .with_context(|| format!("no snapshot '{src}'"))?;
            let path = PathBuf::from(&snap.snapshot_path);
            let sha = snap.head_sha.clone();
            restored_from = Some(snap);
            (path, sha, None)
        }
        Some(src) => {
            if let Some(other) = reg.get_machine(&root, src)? {
                // Fork another machine in this repository: quiesce it first.
                if shim::is_running(&root, src) {
                    let (code, _) = shim::request(&root, src, moo_vmm::proto::QUIESCE)?;
                    anyhow::ensure!(code == 0, "could not quiesce machine '{src}'");
                }
                moo_store::full_fsync(Path::new(&other.overlay_path))?;
                (
                    PathBuf::from(&other.overlay_path),
                    other.base_commit.clone(),
                    Some(other.handle.clone()),
                )
            } else if let Some(sha) = git::resolve(src) {
                // A commit-ish: restore this handle's snapshot for that
                // SHA if one exists, else start from the base image.
                match reg.find_snapshot(&root, name, &sha)? {
                    Some(snap) => {
                        let path = PathBuf::from(&snap.snapshot_path);
                        restored_from = Some(snap);
                        (path, Some(sha), None)
                    }
                    None => (base_image.clone(), Some(sha), None),
                }
            } else {
                bail!("'{src}' is not a snapshot, machine, or git commit");
            }
        }
        None => {
            // Reusing a dropped handle restores the snapshot saved for
            // the SHA it shadows now, if any.
            let head = git::shadowed_head(name);
            match &head {
                Some(sha) => match reg.find_snapshot(&root, name, sha)? {
                    Some(snap) => {
                        let path = PathBuf::from(&snap.snapshot_path);
                        restored_from = Some(snap);
                        (path, head.clone(), None)
                    }
                    None => (base_image.clone(), head.clone(), None),
                },
                None => (base_image.clone(), None, None),
            }
        }
    };

    let overlay = overlay_path(&root, name);
    if overlay.exists() {
        std::fs::remove_file(&overlay)?;
    }
    cow_clone(&source_disk, &overlay)?;

    let port_map = allocate_ports(&root, name, &cfg.network.ports);

    reg.insert_machine(&Machine {
        handle: name.to_string(),
        base_commit,
        recipe_hash: cfg.recipe_hash(&cfg_root),
        parent_machine: parent,
        base_image_path: base_image.to_string_lossy().into_owned(),
        overlay_path: overlay.to_string_lossy().into_owned(),
        lifecycle: "live".into(),
        detached,
        created_at: timestamp(),
        cpus: cfg.cpus(),
        ram_mib: cfg.ram_mib(),
        port_map: format_port_map(&port_map),
        project_root: root.clone(),
    })?;

    boot(&root, name)?;
    let synced = sync_after_new(&reg, &root, name)?;
    Ok(NewOutcome {
        handle: name.to_string(),
        restored_from,
        created: true,
        synced,
    })
}

/// `moo run <name> -- <cmd>` — execute inside the machine.
/// The working tree follows the code: when invoked from the machine's
/// repository, any host-side change is synced in before the command runs.
pub fn run_in_machine(name: &str, cmd: &str) -> Result<(u8, Vec<u8>)> {
    let reg = Registry::open()?;
    let root = scope();
    let Some(machine) = reg.get_machine(&root, name)? else {
        bail!("no machine '{name}' — create it with `moo new {name}`");
    };
    if !shim::is_running(&root, name) {
        // Machines persist between invocations; reboot the live overlay.
        boot(&root, name)?;
    }
    if let Some(s) = sync::sync_into(&machine)? {
        eprintln!(
            "moo: synced {} files ({:.1} MB) to {}",
            s.files,
            s.bytes as f64 / 1e6,
            s.workdir
        );
    }
    shim::request(&root, name, cmd.as_bytes())
}

pub struct SaveOutcome {
    pub snapshot: Snapshot,
    pub fresh: bool,
}

/// `moo save <name>` — quiesce, snapshot, associate with HEAD.
pub fn save_machine(name: &str) -> Result<SaveOutcome> {
    let reg = Registry::open()?;
    let root = scope();
    let m = reg
        .get_machine(&root, name)?
        .with_context(|| format!("no machine '{name}'"))?;
    save_machine_row(&reg, &m)
}

fn save_machine_row(reg: &Registry, m: &Machine) -> Result<SaveOutcome> {
    let root = m.project_root.as_str();
    let name = m.handle.as_str();
    if shim::is_running(root, name) {
        // Project-defined quiesce commands (DB checkpoints etc.) run first,
        // then the built-in filesystem sync.
        let (cfg, _) = config::load()?;
        for cmd in &cfg.quiesce.commands {
            let (code, out) = shim::request(root, name, cmd.as_bytes())?;
            if code != 0 {
                eprintln!(
                    "moo: warning: quiesce command failed in '{}' (exit {}): {}",
                    name,
                    code,
                    String::from_utf8_lossy(&out).trim()
                );
            }
        }
        let (code, out) = shim::request(root, name, moo_vmm::proto::QUIESCE)?;
        if code != 0 {
            bail!(
                "could not quiesce machine '{}': {}",
                name,
                String::from_utf8_lossy(&out)
            );
        }
    }

    let head = if m.detached {
        None
    } else {
        git::shadowed_head(name)
    };
    let (snapshot, fresh) =
        save_snapshot(reg, root, name, head.as_deref(), Path::new(&m.overlay_path))?;
    Ok(SaveOutcome { snapshot, fresh })
}

/// `moo save` with no name — save every machine of the current repository.
pub fn save_all() -> Result<Vec<(String, SaveOutcome)>> {
    let reg = Registry::open()?;
    let root = scope();
    let mut results = Vec::new();
    for m in reg.list_machines()? {
        if m.project_root != root {
            continue;
        }
        let outcome = save_machine_row(&reg, &m)?;
        results.push((m.handle, outcome));
    }
    Ok(results)
}

/// `moo drop <name>` — destroy the live machine; snapshots survive unless
/// `drop_snapshots`. Idempotent.
pub fn drop_machine(name: &str, force: bool, drop_snapshots: bool) -> Result<()> {
    let reg = Registry::open()?;
    let root = scope();

    if shim::is_running(&root, name) {
        if force {
            if let Ok(pid_str) = std::fs::read_to_string(shim::pid_path(&root, name)) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    // The supervisor leads its own process group (setsid);
                    // this takes the guest down with it.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
            }
            let _ = std::fs::remove_file(shim::socket_path(&root, name));
            let _ = std::fs::remove_file(shim::pid_path(&root, name));
            let _ = std::fs::remove_file(shim::net_socket_path(&root, name));
            let _ = std::fs::remove_file(shim::net_api_path(&root, name));
        } else {
            stop(&root, name)?;
        }
    }

    if let Some(m) = reg.get_machine(&root, name)? {
        let overlay = PathBuf::from(&m.overlay_path);
        if overlay.exists() {
            std::fs::remove_file(&overlay)?;
        }
        reg.remove_machine(&root, name)?;
    }
    sync::invalidate(&root, name);

    if drop_snapshots {
        let removed = reg.remove_snapshots(&root, name)?;
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

/// `moo ls` — read-only listing of every machine on this host, across all
/// repositories.
pub fn list() -> Result<Vec<LsRow>> {
    let reg = Registry::open()?;
    let mut rows = Vec::new();
    for m in reg.list_machines()? {
        let running = shim::is_running(&m.project_root, &m.handle);
        let snapshots = reg.list_snapshots(&m.project_root, &m.handle)?;
        rows.push(LsRow {
            machine: m,
            running,
            snapshots,
        });
    }
    Ok(rows)
}

/// True if the named machine of the current repository scope is running.
pub fn machine_is_running(name: &str) -> bool {
    shim::is_running(&scope(), name)
}

/// `moo open` — resolve the host port forwarded to a machine's guest port
/// (admin, read-only). With no guest port, resolves only when
/// the machine forwards exactly one port.
pub fn resolve_host_port(name: &str, guest_port: Option<u16>) -> Result<u16> {
    let reg = Registry::open()?;
    let m = reg
        .get_machine(&scope(), name)?
        .with_context(|| format!("no machine '{name}' — create it with `moo new {name}`"))?;

    let map: Vec<(u16, u16)> = m
        .port_map
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (host, guest) = pair.split_once(':')?;
            Some((host.parse().ok()?, guest.parse().ok()?))
        })
        .collect();

    if map.is_empty() {
        bail!(
            "machine '{name}' forwards no ports — declare guest ports in moo.toml under [network] and recreate the machine"
        );
    }
    let forwarded = || {
        map.iter()
            .map(|(h, g)| format!("{h}->{g}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match guest_port {
        Some(g) => map
            .iter()
            .find(|(_, guest)| *guest == g)
            .map(|(host, _)| *host)
            .with_context(|| {
                format!(
                    "machine '{}' does not forward guest port {} (forwarded: {})",
                    name,
                    g,
                    forwarded()
                )
            }),
        None if map.len() == 1 => Ok(map[0].0),
        None => bail!(
            "machine '{}' forwards several ports ({}) — pick one: moo open {} <guest-port>",
            name,
            forwarded(),
            name
        ),
    }
}
