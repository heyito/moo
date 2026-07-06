//! moo — git for the machine. Four verbs: new, run, save, drop.
//! Admin: ls, open, doctor. (plan.md §10)
//!
//! Argument parsing is hand-rolled: the surface is five commands and the
//! `from` keyword; a parser dependency would outweigh it.

use anyhow::{bail, Result};
use std::io::Write;
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("moo: {:#}", e);
            1
        }
    };
    exit(code);
}

fn dispatch(args: &[String]) -> Result<i32> {
    match args.first().map(String::as_str) {
        Some("new") => cmd_new(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some("save") => cmd_save(&args[1..]),
        Some("drop") => cmd_drop(&args[1..]),
        Some("ls") => cmd_ls(),
        Some("open") => cmd_open(&args[1..]),
        Some("doctor") => cmd_doctor(),
        Some("__shim") => cmd_shim(&args[1..]),
        _ => {
            print_usage();
            Ok(2)
        }
    }
}

fn print_usage() {
    eprintln!(
        "moo — git for the machine

usage:
  moo new <name> [from <src>] [--detached]   create a machine (restores the
                                             snapshot for the current commit
                                             if one was saved)
  moo run <name> -- <cmd> [args...]          execute inside the machine
  moo save [<name>]                          snapshot state, tag with the
                                             current commit
  moo drop <name> [--force] [--snapshots]    destroy the machine (saved
                                             snapshots survive by default)

  moo ls                                     list machines and snapshots
  moo open <name> [guest-port] [/path]       open a forwarded port in the
                                             browser (port optional when the
                                             machine forwards exactly one)
  moo doctor                                 check this host can run machines"
    );
}

fn cmd_new(args: &[String]) -> Result<i32> {
    let mut name = None;
    let mut from = None;
    let mut detached = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "from" => {
                i += 1;
                from = args.get(i).cloned();
                if from.is_none() {
                    bail!("'from' needs a source (ref, commit, snapshot, or machine)");
                }
            }
            "--detached" => detached = true,
            other if name.is_none() => name = Some(other.to_string()),
            other => bail!("unexpected argument '{}'", other),
        }
        i += 1;
    }
    let name = match (name, detached) {
        (Some(n), _) => n,
        (None, true) => format!("m_{:08x}", std::process::id() as u64 * 2654435761 % 0xFFFF_FFFF),
        (None, false) => bail!("machine name required (or --detached)"),
    };

    let outcome = moo_core::new_machine(&name, from.as_deref(), detached)?;
    if let Some(snap) = &outcome.restored_from {
        let sha = snap.head_sha.as_deref().unwrap_or("(detached)");
        println!(
            "machine '{}' restored from snapshot {} (commit {})",
            outcome.handle,
            snap.snapshot_id,
            &sha[..sha.len().min(12)]
        );
        if !outcome.created {
            eprintln!(
                "note: the previous live state of '{}' was replaced; `moo save` before switching commits to keep it",
                outcome.handle
            );
        }
    } else if outcome.created {
        println!("machine '{}' created", outcome.handle);
    } else {
        println!("machine '{}' ready", outcome.handle);
    }
    if let Some(s) = &outcome.synced {
        println!(
            "working tree synced: {} files ({:.1} MB) at {}",
            s.files,
            s.bytes as f64 / 1e6,
            s.workdir
        );
    }
    Ok(0)
}

fn cmd_run(args: &[String]) -> Result<i32> {
    let Some(name) = args.first() else {
        bail!("usage: moo run <name> -- <cmd> [args...]");
    };
    let Some(sep) = args.iter().position(|a| a == "--") else {
        bail!("usage: moo run <name> -- <cmd> [args...]");
    };
    let cmd_parts = &args[sep + 1..];
    if cmd_parts.is_empty() {
        bail!("no command given after --");
    }
    // Each argv token is shell-quoted so the guest's `sh -c` sees exactly
    // what the caller's shell passed. A single token is left untouched so
    // `moo run m -- 'echo a && echo b'` still composes as a shell command.
    let cmd = if cmd_parts.len() == 1 {
        cmd_parts[0].clone()
    } else {
        cmd_parts.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
    };
    let (code, out) = moo_core::run_in_machine(name, &cmd)?;
    std::io::stdout().write_all(&out)?;
    Ok(code as i32)
}

fn cmd_save(args: &[String]) -> Result<i32> {
    match args.first() {
        Some(name) => {
            let o = moo_core::save_machine(name)?;
            print_save(name, &o);
        }
        None => {
            let all = moo_core::save_all()?;
            if all.is_empty() {
                println!("no machines to save");
            }
            for (name, o) in all {
                print_save(&name, &o);
            }
        }
    }
    Ok(0)
}

/// Quote one token for POSIX sh unless it is already safe.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | ',' | '@' | '+' | '%')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn print_save(name: &str, o: &moo_core::SaveOutcome) {
    let sha = o.snapshot.head_sha.as_deref().unwrap_or("(no commit)");
    let sha_short = &sha[..sha.len().min(12)];
    if o.fresh {
        println!("saved '{}' as {} (commit {})", name, o.snapshot.snapshot_id, sha_short);
    } else {
        println!(
            "'{}' unchanged since {} (commit {})",
            name, o.snapshot.snapshot_id, sha_short
        );
    }
}

fn cmd_drop(args: &[String]) -> Result<i32> {
    let mut name = None;
    let mut force = false;
    let mut snapshots = false;
    for a in args {
        match a.as_str() {
            "--force" => force = true,
            "--snapshots" => snapshots = true,
            other if name.is_none() => name = Some(other.to_string()),
            other => bail!("unexpected argument '{}'", other),
        }
    }
    let Some(name) = name else { bail!("usage: moo drop <name> [--force] [--snapshots]") };
    moo_core::drop_machine(&name, force, snapshots)?;
    println!("machine '{}' dropped{}", name, if snapshots { " (snapshots too)" } else { "" });
    Ok(0)
}

fn cmd_ls() -> Result<i32> {
    let rows = moo_core::list()?;
    if rows.is_empty() {
        println!("no machines");
        return Ok(0);
    }
    println!(
        "{:<24} {:<8} {:<14} {:<22} snapshots",
        "HANDLE", "STATE", "BASE COMMIT", "PORTS (host->guest)"
    );
    for row in rows {
        let state = if row.running { "running" } else { &row.machine.lifecycle };
        let base = row
            .machine
            .base_commit
            .as_deref()
            .map(|s| &s[..s.len().min(12)])
            .unwrap_or("-");
        let ports = if row.machine.port_map.is_empty() {
            "(none)".to_string()
        } else {
            row.machine
                .port_map
                .split(',')
                .map(|p| p.replacen(':', "->", 1))
                .collect::<Vec<_>>()
                .join(" ")
        };
        println!(
            "{:<24} {:<8} {:<14} {:<22} {}",
            row.machine.handle,
            state,
            base,
            ports,
            row.snapshots.len()
        );
        for s in &row.snapshots {
            let sha = s.head_sha.as_deref().unwrap_or("(no commit)");
            println!("  {} @ {}", s.snapshot_id, &sha[..sha.len().min(12)]);
        }
    }
    Ok(0)
}

/// `moo open <name> [guest-port] [/path]` — admin, read-only: resolve the
/// host port for a forwarded guest port, print the URL, and open it in the
/// default browser. Never touches machine state.
fn cmd_open(args: &[String]) -> Result<i32> {
    let mut name: Option<&str> = None;
    let mut guest: Option<u16> = None;
    let mut path: Option<&str> = None;
    for a in args {
        if name.is_none() {
            name = Some(a);
        } else if a.starts_with('/') && path.is_none() {
            path = Some(a);
        } else if guest.is_none() && !a.starts_with('/') {
            guest = Some(
                a.parse()
                    .map_err(|_| anyhow::anyhow!("'{}' is not a guest port number", a))?,
            );
        } else {
            bail!("unexpected argument '{}'", a);
        }
    }
    let Some(name) = name else {
        bail!("usage: moo open <name> [guest-port] [/path]");
    };

    let host_port = moo_core::resolve_host_port(name, guest)?;
    let url = format!("http://localhost:{}{}", host_port, path.unwrap_or("/"));
    println!("{}", url);

    if !moo_core::shim::is_running(name) {
        eprintln!(
            "note: machine '{}' is not running — nothing answers yet; `moo run {} -- true` boots it",
            name, name
        );
        return Ok(0);
    }
    // Best-effort browser launch; the printed URL is the contract.
    let _ = std::process::Command::new("open").arg(&url).status();
    Ok(0)
}

fn cmd_doctor() -> Result<i32> {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, hint: &str| {
        println!("{} {}", if pass { "ok " } else { "FAIL" }, name);
        if !pass {
            println!("     {}", hint);
            ok = false;
        }
    };

    check(
        "machine firmware installed",
        moo_vmm::firmware_installed(),
        "machine firmware is missing — see the install section of the README",
    );

    check(
        "machine network runtime installed",
        moo_vmm::net_proxy_installed(),
        "the network runtime is missing — re-run scripts/install.sh",
    );

    let store_on_apfs = {
        // clonefile requires APFS; probe by cloning a scratch file.
        let dir = moo_store::moo_home();
        std::fs::create_dir_all(&dir).ok();
        let probe = dir.join(".doctor-probe");
        let clone = dir.join(".doctor-probe-clone");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&clone);
        std::fs::write(&probe, b"probe").is_ok()
            && moo_store::cow_clone(&probe, &clone).is_ok()
            && {
                let _ = std::fs::remove_file(&probe);
                let _ = std::fs::remove_file(&clone);
                true
            }
    };
    check(
        "storage supports copy-on-write clones",
        store_on_apfs,
        "~/.moo must live on an APFS volume",
    );

    check(
        "filesystem tools installed",
        moo_core::image::tools_installed(),
        "install with: brew install e2fsprogs",
    );

    let entitled = {
        // Verify our own signature carries the hypervisor entitlement.
        let exe = std::env::current_exe().ok();
        exe.map(|e| {
            std::process::Command::new("codesign")
                .args(["-d", "--entitlements", "-", e.to_str().unwrap()])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout).contains("hypervisor")
                        || String::from_utf8_lossy(&o.stderr).contains("hypervisor")
                })
                .unwrap_or(false)
        })
        .unwrap_or(false)
    };
    check(
        "binary is signed for machine isolation",
        entitled,
        "re-install moo, or run: codesign --force --sign - --entitlements entitlements.plist $(which moo)",
    );

    Ok(if ok { 0 } else { 1 })
}

fn cmd_shim(args: &[String]) -> Result<i32> {
    let [handle] = args else {
        bail!("internal: bad supervisor invocation");
    };
    moo_core::shim::run(handle)?;
    Ok(0)
}
