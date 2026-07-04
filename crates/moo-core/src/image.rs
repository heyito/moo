//! Golden base images (plan.md §5): content-addressed by recipe hash,
//! built once per hash, cached under `~/.moo/images/`.
//!
//! The WP0-validated pipeline: fetch OCI layers straight from the registry
//! with anonymous auth (no container daemon), unpack them in order with
//! whiteout handling, inject the guest agent, build an ext4 disk with
//! mke2fs -d (no root, no mounting), then restore in-guest file ownership
//! with a debugfs script (unprivileged extraction loses it).

use crate::config::MooToml;
use anyhow::{bail, Context, Result};
use moo_store::images_dir;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The static guest exec agent, cross-compiled at build time.
const AGENT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/moo-agent.bin"));

pub const DEFAULT_BASE: &str = "debian:bookworm";

/// Return the golden image for this project, building it on first use.
pub fn ensure(cfg: &MooToml, project_root: &Path) -> Result<PathBuf> {
    let hash = cfg.recipe_hash(project_root);
    let path = images_dir().join(format!("{}.img", hash));
    if path.exists() {
        return Ok(path);
    }
    let base_ref = cfg.project.base.as_deref().unwrap_or(DEFAULT_BASE);
    eprintln!("moo: building base image from {} (first use for this recipe)", base_ref);
    build(base_ref, &path)?;
    Ok(path)
}

// ---- image reference parsing ----

struct ImageRef {
    registry: String,
    repo: String,
    tag: String,
}

fn parse_ref(s: &str) -> ImageRef {
    let (name, tag) = match s.rsplit_once(':') {
        // A ':' after the last '/' is a tag; otherwise it's a port in the host.
        Some((n, t)) if !t.contains('/') => (n.to_string(), t.to_string()),
        _ => (s.to_string(), "latest".to_string()),
    };
    let first = name.split('/').next().unwrap_or("");
    let has_host = first.contains('.') || first.contains(':') || first == "localhost";
    if has_host {
        let (host, repo) = name.split_once('/').unwrap();
        ImageRef {
            registry: host.to_string(),
            repo: repo.to_string(),
            tag,
        }
    } else {
        let repo = if name.contains('/') { name } else { format!("library/{}", name) };
        ImageRef {
            registry: "registry-1.docker.io".to_string(),
            repo,
            tag,
        }
    }
}

// ---- registry client (anonymous bearer auth) ----

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json";

struct Client {
    agent: ureq::Agent,
    token: Option<String>,
}

impl Client {
    fn new() -> Self {
        Self { agent: ureq::AgentBuilder::new().build(), token: None }
    }

    fn get(&mut self, url: &str, accept: &str) -> Result<ureq::Response> {
        for _ in 0..2 {
            let mut req = self.agent.get(url).set("Accept", accept);
            if let Some(t) = &self.token {
                req = req.set("Authorization", &format!("Bearer {}", t));
            }
            match req.call() {
                Ok(resp) => return Ok(resp),
                Err(ureq::Error::Status(401, resp)) => {
                    let challenge = resp
                        .header("www-authenticate")
                        .context("registry denied access without an auth challenge")?
                        .to_string();
                    self.token = Some(anonymous_token(&self.agent, &challenge)?);
                }
                Err(e) => return Err(e).context("registry request failed"),
            }
        }
        bail!("registry authentication failed for {}", url);
    }
}

/// Parse `Bearer realm="…",service="…",scope="…"` and fetch a pull token.
fn anonymous_token(agent: &ureq::Agent, challenge: &str) -> Result<String> {
    let fields: BTreeMap<String, String> = challenge
        .trim_start_matches("Bearer ")
        .split(',')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        })
        .collect();
    let realm = fields.get("realm").context("auth challenge missing realm")?;
    let mut req = agent.get(realm);
    for key in ["service", "scope"] {
        if let Some(v) = fields.get(key) {
            req = req.query(key, v);
        }
    }
    let body: serde_json::Value = req.call().context("token request failed")?.into_json()?;
    body.get("token")
        .or_else(|| body.get("access_token"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .context("token response had no token")
}

// ---- build pipeline ----

fn build(base_ref: &str, out: &Path) -> Result<()> {
    let img = parse_ref(base_ref);
    let mut client = Client::new();

    // Resolve the platform manifest and its layers.
    let manifest_url = format!(
        "https://{}/v2/{}/manifests/{}",
        img.registry, img.repo, img.tag
    );
    let top: serde_json::Value = client.get(&manifest_url, MANIFEST_ACCEPT)?.into_json()?;
    let manifest = if top.get("manifests").is_some() {
        // Multi-platform index: pick linux/arm64.
        let digest = top["manifests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| {
                m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "arm64"
            })
            .and_then(|m| m["digest"].as_str())
            .with_context(|| format!("{} has no linux/arm64 build", base_ref))?
            .to_string();
        let url = format!("https://{}/v2/{}/manifests/{}", img.registry, img.repo, digest);
        client.get(&url, MANIFEST_ACCEPT)?.into_json()?
    } else {
        top
    };

    let layers = manifest["layers"]
        .as_array()
        .context("manifest has no layers")?
        .iter()
        .map(|l| {
            let digest = l["digest"].as_str().unwrap_or_default().to_string();
            let media = l["mediaType"].as_str().unwrap_or_default().to_string();
            let size = l["size"].as_i64().unwrap_or(0);
            (digest, media, size)
        })
        .collect::<Vec<_>>();

    let work = tempdir()?;
    let rootfs = work.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;
    let mut owners: Vec<(PathBuf, u64, u64)> = Vec::new();

    for (i, (digest, media, size)) in layers.iter().enumerate() {
        if !media.contains("tar") {
            bail!("unsupported layer type in {}: {}", base_ref, media);
        }
        if media.contains("zstd") {
            bail!("zstd-compressed images are not supported yet: {}", base_ref);
        }
        eprintln!(
            "moo:   layer {}/{} ({:.1} MB)",
            i + 1,
            layers.len(),
            *size as f64 / 1e6
        );
        let url = format!("https://{}/v2/{}/blobs/{}", img.registry, img.repo, digest);
        let resp = client.get(&url, "application/octet-stream")?;
        apply_layer(resp.into_reader(), &rootfs, &mut owners)
            .with_context(|| format!("unpack layer {}", digest))?;
    }

    // Inject the guest agent (runs as the machine's init).
    let agent_path = rootfs.join("usr/local/bin/moo-agent");
    std::fs::create_dir_all(agent_path.parent().unwrap())?;
    std::fs::write(&agent_path, AGENT)?;
    set_mode(&agent_path, 0o755)?;
    owners.push((PathBuf::from("usr/local/bin/moo-agent"), 0, 0));

    // Build the filesystem, then restore in-guest ownership.
    let tmp_img = work.join("image.img");
    mkfs(&rootfs, &tmp_img)?;
    fix_ownership(&tmp_img, &rootfs, &owners)?;

    std::fs::create_dir_all(images_dir())?;
    std::fs::rename(&tmp_img, out).or_else(|_| {
        std::fs::copy(&tmp_img, out).map(|_| ())
    })?;
    eprintln!("moo: base image ready");
    Ok(())
}

/// Unpack one gzipped layer tar, honoring OCI whiteouts, recording ownership.
fn apply_layer(
    reader: impl Read,
    rootfs: &Path,
    owners: &mut Vec<(PathBuf, u64, u64)>,
) -> Result<()> {
    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let rel: PathBuf = entry.path()?.components().skip_while(|c| {
            matches!(c, std::path::Component::CurDir)
        }).collect();
        let Some(name) = rel.file_name().and_then(|n| n.to_str()) else { continue };

        // OCI whiteouts: .wh.<name> deletes <name>; .wh..wh..opq empties the dir.
        if name == ".wh..wh..opq" {
            let dir = rootfs.join(rel.parent().unwrap_or(Path::new("")));
            if dir.exists() {
                for child in std::fs::read_dir(&dir)? {
                    let p = child?.path();
                    if p.is_dir() && !p.is_symlink() {
                        std::fs::remove_dir_all(&p)?;
                    } else {
                        std::fs::remove_file(&p)?;
                    }
                }
            }
            continue;
        }
        if let Some(hidden) = name.strip_prefix(".wh.") {
            let target = rootfs.join(rel.parent().unwrap_or(Path::new(""))).join(hidden);
            if target.is_dir() && !target.is_symlink() {
                let _ = std::fs::remove_dir_all(&target);
            } else {
                let _ = std::fs::remove_file(&target);
            }
            continue;
        }

        // Device and fifo nodes need root to create; guests get devtmpfs anyway.
        use tar::EntryType::*;
        if matches!(entry.header().entry_type(), Block | Char | Fifo) {
            continue;
        }

        let uid = entry.header().uid().unwrap_or(0);
        let gid = entry.header().gid().unwrap_or(0);
        if entry.unpack_in(rootfs)? && (uid != 0 || gid != 0) {
            owners.push((rel, uid, gid));
        }
    }
    Ok(())
}

/// True if the tools needed to build images are present (`moo doctor`).
pub fn tools_installed() -> bool {
    find_tool("mkfs.ext4").is_ok() && find_tool("debugfs").is_ok()
}

fn find_tool(name: &str) -> Result<PathBuf> {
    let candidates = [
        format!("/opt/homebrew/opt/e2fsprogs/sbin/{}", name),
        format!("/opt/homebrew/sbin/{}", name),
        format!("/usr/local/sbin/{}", name),
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return Ok(PathBuf::from(c));
        }
    }
    bail!("{} not found — run `moo doctor` for setup instructions", name);
}

fn mkfs(rootfs: &Path, out: &Path) -> Result<()> {
    // Sparse image: size generously, APFS only stores written blocks.
    let used = dir_size(rootfs)?;
    let size_gb = ((used * 2) / 1_000_000_000 + 4).max(8);
    let mkfs = find_tool("mkfs.ext4")?;
    let status = std::process::Command::new(mkfs)
        .arg("-q")
        .arg("-F")
        .arg("-d")
        .arg(rootfs)
        .arg("-L")
        .arg("mooroot")
        .arg(out)
        .arg(format!("{}G", size_gb))
        .status()
        .context("run filesystem build")?;
    anyhow::ensure!(status.success(), "filesystem build failed");
    Ok(())
}

/// Unprivileged extraction leaves every file owned by the build user, and
/// mke2fs -d copies that into the image. Rewrite inode ownership: every
/// path becomes root:root, then the recorded non-root owners are applied.
fn fix_ownership(img: &Path, rootfs: &Path, owners: &[(PathBuf, u64, u64)]) -> Result<()> {
    let debugfs = find_tool("debugfs")?;
    let mut script = String::from("sif / uid 0\nsif / gid 0\n");
    let mut sweep = |path: &Path, uid: u64, gid: u64| {
        let p = format!("/{}", path.display());
        script.push_str(&format!("sif \"{}\" uid {}\nsif \"{}\" gid {}\n", p, uid, p, gid));
    };
    walk_relative(rootfs, Path::new(""), &mut |rel| sweep(rel, 0, 0))?;
    for (path, uid, gid) in owners {
        sweep(path, *uid, *gid);
    }

    let cmdfile = std::env::temp_dir().join(format!("moo-debugfs-{}.cmd", std::process::id()));
    std::fs::write(&cmdfile, script)?;
    let out = std::process::Command::new(debugfs)
        .arg("-w")
        .arg("-f")
        .arg(&cmdfile)
        .arg(img)
        .output()
        .context("run ownership fix")?;
    let _ = std::fs::remove_file(&cmdfile);
    anyhow::ensure!(out.status.success(), "ownership fix failed");
    Ok(())
}

/// Depth-first walk of `dir`, invoking `f` with each path relative to the
/// walk root. Symlinks are visited, not followed.
fn walk_relative(
    root: &Path,
    rel: &Path,
    f: &mut impl FnMut(&Path),
) -> Result<()> {
    for entry in std::fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let child = rel.join(entry.file_name());
        f(&child);
        let meta = entry.metadata()?;
        if meta.is_dir() && !entry.path().is_symlink() {
            walk_relative(root, &child, f)?;
        }
    }
    Ok(())
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() && !entry.path().is_symlink() {
            total += dir_size(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn tempdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("moo-image-build-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
