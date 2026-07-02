//! Machine backend: boots a hardware-isolated Linux runtime from a disk
//! overlay and wires up the exec/quiesce channel.
//!
//! This crate is the *only* place the hypervisor is named. Nothing in here
//! may leak backend identifiers into user-facing strings — errors returned
//! from this crate speak in terms of "machine", "firmware", and "engine".

pub mod proto;

use anyhow::{bail, Result};
use std::ffi::CString;
use std::os::raw::c_char;
use std::os::unix::io::RawFd;
use std::path::Path;

/// Directory where the boot firmware is installed (Homebrew keg).
pub const FIRMWARE_DIR: &str = "/opt/homebrew/opt/libkrunfw/lib";
const FIRMWARE_LEAF: &str = "libkrunfw.5.dylib";

/// Environment variable the loader reads at process start. Any process that
/// will boot a machine must be launched with the firmware dir on this path.
pub const LOADER_PATH_VAR: &str = "DYLD_FALLBACK_LIBRARY_PATH";

/// Name of the serial port the guest agent serves the exec protocol on.
pub const EXEC_PORT_NAME: &str = "got-exec";

const DISK_FORMAT_RAW: u32 = 0;
const SYNC_MODE_RELAXED: u32 = 1;

#[link(name = "krun")]
extern "C" {
    fn krun_set_log_level(level: u32) -> i32;
    fn krun_create_ctx() -> i32;
    fn krun_set_vm_config(ctx_id: u32, num_vcpus: u8, ram_mib: u32) -> i32;
    fn krun_add_disk3(
        ctx_id: u32,
        block_id: *const c_char,
        disk_path: *const c_char,
        disk_format: u32,
        read_only: bool,
        direct_io: bool,
        sync_mode: u32,
    ) -> i32;
    fn krun_set_root_disk_remount(
        ctx_id: u32,
        device: *const c_char,
        fstype: *const c_char,
        options: *const c_char,
    ) -> i32;
    fn krun_set_workdir(ctx_id: u32, workdir_path: *const c_char) -> i32;
    fn krun_set_exec(
        ctx_id: u32,
        exec_path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> i32;
    fn krun_set_console_output(ctx_id: u32, c_filepath: *const c_char) -> i32;
    fn krun_add_virtio_console_multiport(ctx_id: u32) -> i32;
    fn krun_add_console_port_inout(
        ctx_id: u32,
        console_id: u32,
        name: *const c_char,
        input_fd: i32,
        output_fd: i32,
    ) -> i32;
    fn krun_start_enter(ctx_id: u32) -> i32;
}

/// True if the boot firmware is installed where we expect it.
pub fn firmware_installed() -> bool {
    Path::new(FIRMWARE_DIR).join(FIRMWARE_LEAF).exists()
}

/// Value to set `LOADER_PATH_VAR` to when spawning a machine-booting process.
pub fn loader_path_value() -> String {
    match std::env::var(LOADER_PATH_VAR) {
        Ok(cur) if !cur.is_empty() => {
            if cur.split(':').any(|p| p == FIRMWARE_DIR) {
                cur
            } else {
                format!("{}:{}", cur, FIRMWARE_DIR)
            }
        }
        _ => FIRMWARE_DIR.to_string(),
    }
}

/// Everything needed to boot one machine running the guest exec agent.
pub struct MachineConfig<'a> {
    pub overlay: &'a str,
    pub cpus: u8,
    pub ram_mib: u32,
    pub console_log: &'a str,
    /// Read end of the host-to-guest pipe (guest agent input).
    pub serial_input_fd: RawFd,
    /// Write end of the guest-to-host pipe (guest agent output).
    pub serial_output_fd: RawFd,
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("string contains NUL")
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc < 0 {
        bail!("machine engine rejected {} ({})", what, rc);
    }
    Ok(())
}

/// Configure and enter the machine. On success this never returns: the
/// calling process becomes the machine and exits when the guest powers off.
/// Returns only on configuration failure.
pub fn enter(cfg: &MachineConfig) -> Result<()> {
    unsafe {
        check(krun_set_log_level(0), "log level")?;
        let ctx = krun_create_ctx();
        check(ctx, "context")?;
        let ctx = ctx as u32;

        check(krun_set_vm_config(ctx, cfg.cpus, cfg.ram_mib), "resources")?;

        let block_id = cstr("root");
        let disk_path = cstr(cfg.overlay);
        check(
            krun_add_disk3(
                ctx,
                block_id.as_ptr(),
                disk_path.as_ptr(),
                DISK_FORMAT_RAW,
                false,
                false,
                SYNC_MODE_RELAXED,
            ),
            "disk",
        )?;

        let device = cstr("/dev/vda");
        let fstype = cstr("ext4");
        check(
            krun_set_root_disk_remount(ctx, device.as_ptr(), fstype.as_ptr(), std::ptr::null()),
            "root filesystem",
        )?;

        let console_id = krun_add_virtio_console_multiport(ctx);
        check(console_id, "exec channel")?;
        let port_name = cstr(EXEC_PORT_NAME);
        check(
            krun_add_console_port_inout(
                ctx,
                console_id as u32,
                port_name.as_ptr(),
                cfg.serial_input_fd,
                cfg.serial_output_fd,
            ),
            "exec port",
        )?;

        let log_path = cstr(cfg.console_log);
        check(krun_set_console_output(ctx, log_path.as_ptr()), "console log")?;

        let workdir = cstr("/");
        check(krun_set_workdir(ctx, workdir.as_ptr()), "workdir")?;

        let exec_path = cstr("/usr/local/bin/got-agent");
        let args = [cstr("--serial")];
        let arg_ptrs: [*const c_char; 2] = [args[0].as_ptr(), std::ptr::null()];
        let envs = [
            cstr("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
            cstr("HOME=/root"),
            cstr("TERM=xterm"),
        ];
        let env_ptrs: [*const c_char; 4] = [
            envs[0].as_ptr(),
            envs[1].as_ptr(),
            envs[2].as_ptr(),
            std::ptr::null(),
        ];
        check(
            krun_set_exec(ctx, exec_path.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr()),
            "agent",
        )?;

        let rc = krun_start_enter(ctx);
        bail!("machine failed to start ({})", rc);
    }
}
