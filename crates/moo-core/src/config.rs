//! `moo.toml` — the project configuration (plan.md §9). Records the base
//! image reference and the recipe inputs whose hash becomes the golden-image
//! identity, plus resources, exposed ports, and quiesce commands. No
//! services, no health checks, no volumes.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MooToml {
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub recipe: Recipe,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub quiesce: Quiesce,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: Option<String>,
    /// OCI reference or Dockerfile path. The fetch/build pipeline is
    /// post-MVP; this participates in the recipe hash today.
    pub base: Option<String>,
    /// Guest directory the working tree is synced into. Default /srv/app.
    pub workdir: Option<String>,
}

pub const DEFAULT_WORKDIR: &str = "/srv/app";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    #[serde(default)]
    pub lockfiles: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpus: Option<u8>,
    /// e.g. "4GiB", "2048MiB", or a bare MiB count.
    pub memory: Option<String>,
}

impl Default for Resources {
    fn default() -> Self {
        Self { cpus: None, memory: None }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Guest ports the project's runtime listens on. Just numbers — not a
    /// service graph. Each gets a stable per-machine host port.
    #[serde(default)]
    pub ports: Vec<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quiesce {
    /// Extra commands the guest runs during `moo save`, before host flush.
    #[serde(default)]
    pub commands: Vec<String>,
}

pub const DEFAULT_CPUS: u8 = 2;
pub const DEFAULT_RAM_MIB: u32 = 4096;

impl MooToml {
    pub fn cpus(&self) -> u8 {
        self.resources.cpus.unwrap_or(DEFAULT_CPUS)
    }

    pub fn ram_mib(&self) -> u32 {
        match self.resources.memory.as_deref() {
            None => DEFAULT_RAM_MIB,
            Some(s) => parse_memory_mib(s).unwrap_or(DEFAULT_RAM_MIB),
        }
    }

    pub fn workdir(&self) -> &str {
        self.project.workdir.as_deref().unwrap_or(DEFAULT_WORKDIR)
    }

    /// The golden-image identity: hash(base + lockfile contents + resources)
    /// per plan.md §9. Two developers with the same inputs get the same hash.
    pub fn recipe_hash(&self, project_root: &std::path::Path) -> String {
        let mut hasher = blake3::Hasher::new();
        // Guest-agent protocol version: images embed the agent, so a protocol
        // change must produce a new image identity (old cached images carry
        // an agent that can't serve the new ops).
        hasher.update(&[crate::sync::AGENT_PROTO_VERSION]);
        hasher.update(self.project.base.as_deref().unwrap_or("default").as_bytes());
        for lockfile in &self.recipe.lockfiles {
            hasher.update(lockfile.as_bytes());
            if let Ok(content) = std::fs::read(project_root.join(lockfile)) {
                hasher.update(&content);
            }
        }
        hasher.update(&[self.cpus()]);
        hasher.update(&self.ram_mib().to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

fn parse_memory_mib(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("GiB") {
        return n.trim().parse::<u32>().ok().map(|g| g * 1024);
    }
    if let Some(n) = s.strip_suffix("MiB") {
        return n.trim().parse::<u32>().ok();
    }
    s.parse::<u32>().ok()
}

/// The project root: the git toplevel if inside a repository, else the cwd.
pub fn project_root() -> PathBuf {
    crate::git::toplevel().unwrap_or_else(|| PathBuf::from("."))
}

/// Load `moo.toml` from the project root. Absent file = all defaults.
pub fn load() -> Result<(MooToml, PathBuf)> {
    let root = project_root();
    let path = root.join("moo.toml");
    if !path.exists() {
        return Ok((MooToml::default(), root));
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let cfg: MooToml = toml::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok((cfg, root))
}
