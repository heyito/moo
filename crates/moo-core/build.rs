//! Cross-compiles the guest agent (static Linux binary) and embeds it in
//! this crate, so image builds can inject it without any external artifact.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let agent_dir = manifest_dir.parent().unwrap().join("guest-agent");
    println!("cargo:rerun-if-changed={}", agent_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", agent_dir.join("Cargo.toml").display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    // Separate target dir: avoids deadlocking on the parent build's lock.
    let target_dir = out_dir.join("agent-target");

    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--release",
            "--target",
            "aarch64-unknown-linux-musl",
            "--target-dir",
            target_dir.to_str().unwrap(),
        ])
        .env("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld")
        .current_dir(&agent_dir)
        .status()
        .expect("run guest agent build");
    assert!(status.success(), "guest agent build failed");

    let built = target_dir
        .join("aarch64-unknown-linux-musl")
        .join("release")
        .join("moo-agent");
    std::fs::copy(&built, out_dir.join("moo-agent.bin")).expect("copy agent binary");
}
