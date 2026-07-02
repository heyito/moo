//! got — git for the machine. Four verbs: new, run, save, drop.
//! Admin: ls, doctor. (plan.md §10)
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
            eprintln!("got: {:#}", e);
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
        "got — git for the machine

usage:
  got new <name> [from <src>] [--detached]   create a machine (restores the
                                             snapshot for the current commit
                                             if one was saved)
  got run <name> -- <cmd> [args...]          execute inside the machine
  got save [<name>]                          snapshot state, tag with the
                                             current commit
  got drop <name> [--force] [--snapshots]    destroy the machine (saved
                                             snapshots survive by default)

  got ls                                     list machines and snapshots
  got doctor                                 check this host can run machines"
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

    let outcome = got_core::new_machine(&name, from.as_deref(), detached)?;
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
                "note: the previous live state of '{}' was replaced; `got save` before switching commits to keep it",
                outcome.handle
            );
        }
    } else if outcome.created {
        println!("machine '{}' created", outcome.handle);
    } else {
        println!("machine '{}' ready", outcome.handle);
    }
    Ok(0)
}

fn cmd_run(args: &[String]) -> Result<i32> {
    let Some(name) = args.first() else {
        bail!("usage: got run <name> -- <cmd> [args...]");
    };
    let Some(sep) = args.iter().position(|a| a == "--") else {
        bail!("usage: got run <name> -- <cmd> [args...]");
    };
    let cmd_parts = &args[sep + 1..];
    if cmd_parts.is_empty() {
        bail!("no command given after --");
    }
    // Single-word commands run as-is via sh -c; multi-word are joined with
    // shell quoting preserved by the caller's shell already.
    let cmd = cmd_parts.join(" ");
    let (code, out) = got_core::run_in_machine(name, &cmd)?;
    std::io::stdout().write_all(&out)?;
    Ok(code as i32)
}

fn cmd_save(args: &[String]) -> Result<i32> {
    match args.first() {
        Some(name) => {
            let o = got_core::save_machine(name)?;
            print_save(name, &o);
        }
        None => {
            let all = got_core::save_all()?;
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

fn print_save(name: &str, o: &got_core::SaveOutcome) {
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
    let Some(name) = name else { bail!("usage: got drop <name> [--force] [--snapshots]") };
    got_core::drop_machine(&name, force, snapshots)?;
    println!("machine '{}' dropped{}", name, if snapshots { " (snapshots too)" } else { "" });
    Ok(0)
}

fn cmd_ls() -> Result<i32> {
    let rows = got_core::list()?;
    if rows.is_empty() {
        println!("no machines");
        return Ok(0);
    }
    println!("{:<24} {:<8} {:<14} snapshots", "HANDLE", "STATE", "BASE COMMIT");
    for row in rows {
        let state = if row.running { "running" } else { &row.machine.lifecycle };
        let base = row
            .machine
            .base_commit
            .as_deref()
            .map(|s| &s[..s.len().min(12)])
            .unwrap_or("-");
        println!(
            "{:<24} {:<8} {:<14} {}",
            row.machine.handle,
            state,
            base,
            row.snapshots.len()
        );
        for s in &row.snapshots {
            let sha = s.head_sha.as_deref().unwrap_or("(no commit)");
            println!("  {} @ {}", s.snapshot_id, &sha[..sha.len().min(12)]);
        }
    }
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
        got_vmm::firmware_installed(),
        "machine firmware is missing — see the install section of the README",
    );

    let store_on_apfs = {
        // clonefile requires APFS; probe by cloning a scratch file.
        let dir = got_store::got_home();
        std::fs::create_dir_all(&dir).ok();
        let probe = dir.join(".doctor-probe");
        let clone = dir.join(".doctor-probe-clone");
        let _ = std::fs::remove_file(&probe);
        let _ = std::fs::remove_file(&clone);
        std::fs::write(&probe, b"probe").is_ok()
            && got_store::cow_clone(&probe, &clone).is_ok()
            && {
                let _ = std::fs::remove_file(&probe);
                let _ = std::fs::remove_file(&clone);
                true
            }
    };
    check(
        "storage supports copy-on-write clones",
        store_on_apfs,
        "~/.got must live on an APFS volume",
    );

    check(
        "base image present",
        got_core::default_base_image().exists(),
        "build one with: scripts/build-base-image.sh (see README)",
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
        "re-install got, or run: codesign --force --sign - --entitlements entitlements.plist $(which got)",
    );

    Ok(if ok { 0 } else { 1 })
}

fn cmd_shim(args: &[String]) -> Result<i32> {
    let [handle, overlay, cpus, ram] = args else {
        bail!("internal: bad supervisor invocation");
    };
    got_core::shim::run(handle, overlay, cpus.parse()?, ram.parse()?)?;
    Ok(0)
}
