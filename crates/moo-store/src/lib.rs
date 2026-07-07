//! Storage: the SQLite registry, copy-on-write clones, and the
//! content-addressed snapshot store.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::ffi::CString;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

extern "C" {
    // APFS copy-on-write clone. Not exposed by the libc crate.
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> i32;
}

/// Root of all moo state on this host (`~/.moo`).
pub fn moo_home() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    Path::new(&home).join(".moo")
}

pub fn machines_dir() -> PathBuf {
    moo_home().join("machines")
}

pub fn snapshots_dir() -> PathBuf {
    moo_home().join("snapshots")
}

pub fn images_dir() -> PathBuf {
    moo_home().join("images")
}

pub fn runtime_dir() -> PathBuf {
    moo_home().join("run")
}

/// CoW-clone `src` to `dst`. Fails if `dst` exists.
pub fn cow_clone(src: &Path, dst: &Path) -> Result<()> {
    let csrc = CString::new(src.to_str().context("bad path")?)?;
    let cdst = CString::new(dst.to_str().context("bad path")?)?;
    let rc = unsafe { clonefile(csrc.as_ptr(), cdst.as_ptr(), 0) };
    if rc != 0 {
        bail!(
            "copy-on-write clone {} -> {} failed: {}",
            src.display(),
            dst.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Flush a file's data all the way to the physical drive. Snapshots must
/// survive power loss, so this is used before every clone
/// that produces a snapshot.
pub fn full_fsync(path: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {} for flush", path.display()))?;
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc != 0 {
        bail!(
            "flush of {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// BLAKE3 content hash of a file (hex). Decided in WP0: ~10x faster than
/// SHA-256 on overlay-sized files.
pub fn content_hash(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_mmap_rayon(path)
        .with_context(|| format!("hash {}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---- registry ----

#[derive(Debug, Clone)]
pub struct Machine {
    pub handle: String,
    pub base_commit: Option<String>,
    pub recipe_hash: String,
    pub parent_machine: Option<String>,
    pub base_image_path: String,
    pub overlay_path: String,
    /// "live" or "sealed".
    pub lifecycle: String,
    pub detached: bool,
    pub created_at: i64,
    pub cpus: u8,
    pub ram_mib: u32,
    /// "host:guest" pairs, comma-separated. Empty = no ports published.
    pub port_map: String,
    /// Host path of the git repository this machine was created from; the
    /// working tree there is auto-synced into the guest. Empty = no repo.
    pub project_root: String,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub snapshot_id: String,
    pub handle: String,
    /// Repository the owning machine belonged to. Empty = no repo.
    pub project_root: String,
    pub head_sha: Option<String>,
    pub snapshot_path: String,
    pub content_hash: String,
    pub saved_at: i64,
}

pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Open (creating if needed) the registry at `~/.moo/registry.db`.
    pub fn open() -> Result<Self> {
        std::fs::create_dir_all(moo_home()).context("create ~/.moo")?;
        let conn = Connection::open(moo_home().join("registry.db"))?;
        // Many machines are created/saved/dropped in parallel (one CLI
        // process each). WAL lets readers proceed under a writer, and the
        // busy timeout makes concurrent writers queue instead of failing
        // with "database is locked".
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS machines (
                 handle          TEXT NOT NULL,
                 base_commit     TEXT,
                 recipe_hash     TEXT NOT NULL,
                 parent_machine  TEXT,
                 base_image_path TEXT NOT NULL,
                 overlay_path    TEXT NOT NULL,
                 lifecycle       TEXT NOT NULL DEFAULT 'live',
                 detached        INTEGER NOT NULL DEFAULT 0,
                 created_at      INTEGER NOT NULL,
                 cpus            INTEGER NOT NULL DEFAULT 2,
                 ram_mib         INTEGER NOT NULL DEFAULT 4096,
                 port_map        TEXT NOT NULL DEFAULT '',
                 project_root    TEXT NOT NULL DEFAULT '',
                 PRIMARY KEY (project_root, handle)
             );
             CREATE TABLE IF NOT EXISTS snapshots (
                 snapshot_id   TEXT PRIMARY KEY,
                 handle        TEXT NOT NULL,
                 project_root  TEXT NOT NULL DEFAULT '',
                 head_sha      TEXT,
                 snapshot_path TEXT NOT NULL,
                 content_hash  TEXT NOT NULL,
                 saved_at      INTEGER NOT NULL
             );",
        )?;
        // Pre-release schema additions; harmless when the column exists.
        for stmt in [
            "ALTER TABLE machines ADD COLUMN cpus INTEGER NOT NULL DEFAULT 2",
            "ALTER TABLE machines ADD COLUMN ram_mib INTEGER NOT NULL DEFAULT 4096",
            "ALTER TABLE machines ADD COLUMN port_map TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE machines ADD COLUMN project_root TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE snapshots ADD COLUMN project_root TEXT NOT NULL DEFAULT ''",
        ] {
            let _ = conn.execute(stmt, []);
        }
        // The index arrives after the column additions above so legacy
        // registries gain the column first.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_snapshots_scope_handle_sha
                 ON snapshots(project_root, handle, head_sha)",
            [],
        )?;
        // Pre-release: registries created before machines were scoped by
        // repository keyed machines on handle alone. Rebuild once so two
        // repositories can hold the same handle.
        let machines_scoped: bool = {
            let mut stmt = conn.prepare("PRAGMA table_info(machines)")?;
            let pks: Vec<String> = stmt
                .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?
                .filter_map(|r| r.ok())
                .filter(|(_, pk)| *pk > 0)
                .map(|(name, _)| name)
                .collect();
            pks.contains(&"project_root".to_string())
        };
        if !machines_scoped {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE machines RENAME TO machines_legacy;
                 CREATE TABLE machines (
                     handle          TEXT NOT NULL,
                     base_commit     TEXT,
                     recipe_hash     TEXT NOT NULL,
                     parent_machine  TEXT,
                     base_image_path TEXT NOT NULL,
                     overlay_path    TEXT NOT NULL,
                     lifecycle       TEXT NOT NULL DEFAULT 'live',
                     detached        INTEGER NOT NULL DEFAULT 0,
                     created_at      INTEGER NOT NULL,
                     cpus            INTEGER NOT NULL DEFAULT 2,
                     ram_mib         INTEGER NOT NULL DEFAULT 4096,
                     port_map        TEXT NOT NULL DEFAULT '',
                     project_root    TEXT NOT NULL DEFAULT '',
                     PRIMARY KEY (project_root, handle)
                 );
                 INSERT INTO machines
                     SELECT handle, base_commit, recipe_hash, parent_machine,
                            base_image_path, overlay_path, lifecycle, detached,
                            created_at, cpus, ram_mib, port_map, project_root
                     FROM machines_legacy;
                 DROP TABLE machines_legacy;
                 COMMIT;",
            )?;
        }
        // Backfill snapshot scope from the owning machine where possible.
        conn.execute(
            "UPDATE snapshots SET project_root =
                 COALESCE((SELECT m.project_root FROM machines m
                           WHERE m.handle = snapshots.handle), '')
             WHERE project_root = ''",
            [],
        )?;
        Ok(Self { conn })
    }

    pub fn insert_machine(&self, m: &Machine) -> Result<()> {
        self.conn.execute(
            "INSERT INTO machines
             (handle, base_commit, recipe_hash, parent_machine, base_image_path,
              overlay_path, lifecycle, detached, created_at, cpus, ram_mib, port_map,
              project_root)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                m.handle,
                m.base_commit,
                m.recipe_hash,
                m.parent_machine,
                m.base_image_path,
                m.overlay_path,
                m.lifecycle,
                m.detached as i64,
                m.created_at,
                m.cpus as i64,
                m.ram_mib as i64,
                m.port_map,
                m.project_root,
            ],
        )?;
        Ok(())
    }

    const MACHINE_COLS: &'static str =
        "handle, base_commit, recipe_hash, parent_machine, base_image_path,
         overlay_path, lifecycle, detached, created_at, cpus, ram_mib, port_map,
         project_root";

    fn row_to_machine(r: &rusqlite::Row) -> rusqlite::Result<Machine> {
        Ok(Machine {
            handle: r.get(0)?,
            base_commit: r.get(1)?,
            recipe_hash: r.get(2)?,
            parent_machine: r.get(3)?,
            base_image_path: r.get(4)?,
            overlay_path: r.get(5)?,
            lifecycle: r.get(6)?,
            detached: r.get::<_, i64>(7)? != 0,
            created_at: r.get(8)?,
            cpus: r.get::<_, i64>(9)? as u8,
            ram_mib: r.get::<_, i64>(10)? as u32,
            port_map: r.get(11)?,
            project_root: r.get(12)?,
        })
    }

    /// Look up a machine by handle within one repository scope
    /// (`project_root`; empty = created outside any repository).
    pub fn get_machine(&self, project_root: &str, handle: &str) -> Result<Option<Machine>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {} FROM machines WHERE project_root = ?1 AND handle = ?2",
                    Self::MACHINE_COLS
                ),
                params![project_root, handle],
                Self::row_to_machine,
            )
            .optional()?)
    }

    pub fn list_machines(&self) -> Result<Vec<Machine>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM machines ORDER BY created_at",
            Self::MACHINE_COLS
        ))?;
        let rows = stmt.query_map([], Self::row_to_machine)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn remove_machine(&self, project_root: &str, handle: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM machines WHERE project_root = ?1 AND handle = ?2",
            params![project_root, handle],
        )?;
        Ok(())
    }

    pub fn set_lifecycle(&self, project_root: &str, handle: &str, lifecycle: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE machines SET lifecycle = ?3 WHERE project_root = ?1 AND handle = ?2",
            params![project_root, handle, lifecycle],
        )?;
        Ok(())
    }

    const SNAPSHOT_COLS: &'static str =
        "snapshot_id, handle, project_root, head_sha, snapshot_path, content_hash, saved_at";

    /// Latest snapshot for (project_root, handle, head_sha), if any.
    pub fn find_snapshot(
        &self,
        project_root: &str,
        handle: &str,
        head_sha: &str,
    ) -> Result<Option<Snapshot>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {} FROM snapshots
                     WHERE project_root = ?1 AND handle = ?2 AND head_sha = ?3
                     ORDER BY saved_at DESC LIMIT 1",
                    Self::SNAPSHOT_COLS
                ),
                params![project_root, handle, head_sha],
                Self::row_to_snapshot,
            )
            .optional()?)
    }

    pub fn get_snapshot_by_id(&self, snapshot_id: &str) -> Result<Option<Snapshot>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {} FROM snapshots WHERE snapshot_id = ?1",
                    Self::SNAPSHOT_COLS
                ),
                params![snapshot_id],
                Self::row_to_snapshot,
            )
            .optional()?)
    }

    pub fn list_snapshots(&self, project_root: &str, handle: &str) -> Result<Vec<Snapshot>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM snapshots
             WHERE project_root = ?1 AND handle = ?2 ORDER BY saved_at",
            Self::SNAPSHOT_COLS
        ))?;
        let rows = stmt.query_map(params![project_root, handle], Self::row_to_snapshot)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn insert_snapshot(&self, s: &Snapshot) -> Result<()> {
        self.conn.execute(
            "INSERT INTO snapshots
             (snapshot_id, handle, project_root, head_sha, snapshot_path, content_hash, saved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                s.snapshot_id,
                s.handle,
                s.project_root,
                s.head_sha,
                s.snapshot_path,
                s.content_hash,
                s.saved_at,
            ],
        )?;
        Ok(())
    }

    pub fn remove_snapshots(&self, project_root: &str, handle: &str) -> Result<Vec<Snapshot>> {
        let snaps = self.list_snapshots(project_root, handle)?;
        self.conn.execute(
            "DELETE FROM snapshots WHERE project_root = ?1 AND handle = ?2",
            params![project_root, handle],
        )?;
        Ok(snaps)
    }

    /// True if any other snapshot row still references this content hash.
    pub fn snapshot_content_referenced(&self, content_hash: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM snapshots WHERE content_hash = ?1",
            params![content_hash],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    fn row_to_snapshot(r: &rusqlite::Row) -> rusqlite::Result<Snapshot> {
        Ok(Snapshot {
            snapshot_id: r.get(0)?,
            handle: r.get(1)?,
            project_root: r.get(2)?,
            head_sha: r.get(3)?,
            snapshot_path: r.get(4)?,
            content_hash: r.get(5)?,
            saved_at: r.get(6)?,
        })
    }
}

/// Save `overlay` as a content-addressed snapshot and record it.
/// Idempotent: if the latest snapshot for (handle, head_sha) has the same
/// content hash, returns the existing snapshot.
pub fn save_snapshot(
    reg: &Registry,
    project_root: &str,
    handle: &str,
    head_sha: Option<&str>,
    overlay: &Path,
) -> Result<(Snapshot, bool)> {
    full_fsync(overlay)?;
    let hash = content_hash(overlay)?;

    if let Some(sha) = head_sha {
        if let Some(existing) = reg.find_snapshot(project_root, handle, sha)? {
            if existing.content_hash == hash {
                return Ok((existing, false));
            }
        }
    }

    std::fs::create_dir_all(snapshots_dir())?;
    let snap_path = snapshots_dir().join(&hash);
    if !snap_path.exists() {
        cow_clone(overlay, &snap_path)?;
    }

    let snapshot = Snapshot {
        snapshot_id: format!("s_{}", &hash[..8]),
        handle: handle.to_string(),
        project_root: project_root.to_string(),
        head_sha: head_sha.map(str::to_string),
        snapshot_path: snap_path.to_string_lossy().into_owned(),
        content_hash: hash,
        saved_at: now(),
    };
    // Re-saving identical content under a new (handle, sha) pair reuses the
    // snapshot_id; keep the row insert tolerant of that.
    if reg.get_snapshot_by_id(&snapshot.snapshot_id)?.is_none() {
        reg.insert_snapshot(&snapshot)?;
    } else {
        let mut s2 = snapshot.clone();
        s2.snapshot_id = format!("s_{}_{}", &s2.content_hash[..8], now());
        reg.insert_snapshot(&s2)?;
        return Ok((s2, true));
    }
    Ok((snapshot, true))
}

pub fn timestamp() -> i64 {
    now()
}
