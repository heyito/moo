//! The machine supervisor. `got` has no daemon (plan.md §4.1): each running
//! machine is owned by one detached shim process that boots the guest as a
//! child and proxies the exec protocol between a per-machine UNIX socket and
//! the guest's serial channel.
//!
//! Lifetime: the shim exits when the guest powers off (agent __poweroff__ or
//! guest-initiated), removing its socket and pid file and marking the
//! machine `sealed` in the registry.

use anyhow::{Context, Result};
use got_store::{runtime_dir, Registry};
use got_vmm::proto;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

pub fn socket_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.sock", sanitize(handle)))
}

pub fn pid_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.pid", sanitize(handle)))
}

pub fn console_log_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.console.log", sanitize(handle)))
}

pub fn sanitize(handle: &str) -> String {
    handle.replace('/', "%2F")
}

/// Entry point for the hidden `__shim` subcommand. Blocks until the guest
/// powers off. The caller (got new) has already set the loader path so the
/// forked guest can find its firmware.
pub fn run(handle: &str, overlay: &str, cpus: u8, ram_mib: u32) -> Result<()> {
    std::fs::create_dir_all(runtime_dir())?;
    let sock = socket_path(handle);
    let _ = std::fs::remove_file(&sock);

    // Detach from the caller's session so the machine outlives the CLI call.
    unsafe { libc::setsid() };

    // Serial channel: host-to-guest and guest-to-host pipes.
    let mut h2g = [0i32; 2];
    let mut g2h = [0i32; 2];
    unsafe {
        anyhow::ensure!(libc::pipe(h2g.as_mut_ptr()) == 0, "pipe failed");
        anyhow::ensure!(libc::pipe(g2h.as_mut_ptr()) == 0, "pipe failed");
    }

    let console_log = console_log_path(handle);
    let vmm_pid = unsafe { libc::fork() };
    anyhow::ensure!(vmm_pid >= 0, "fork failed");
    if vmm_pid == 0 {
        // Child becomes the machine.
        unsafe {
            libc::close(h2g[1]);
            libc::close(g2h[0]);
        }
        let cfg = got_vmm::MachineConfig {
            overlay,
            cpus,
            ram_mib,
            console_log: console_log.to_str().unwrap(),
            serial_input_fd: h2g[0],
            serial_output_fd: g2h[1],
        };
        // Only returns on failure.
        let err = got_vmm::enter(&cfg).unwrap_err();
        eprintln!("{:#}", err);
        std::process::exit(1);
    }

    unsafe {
        libc::close(h2g[0]);
        libc::close(g2h[1]);
    }
    let mut guest_in = unsafe { std::fs::File::from_raw_fd(h2g[1]) };
    let mut guest_out = unsafe { std::fs::File::from_raw_fd(g2h[0]) };

    std::fs::write(pid_path(handle), format!("{}\n", std::process::id()))?;
    let listener = UnixListener::bind(&sock).context("bind machine socket")?;

    // Reap the guest and exit the shim when it powers off.
    let handle_owned = handle.to_string();
    std::thread::spawn(move || {
        let mut status = 0i32;
        unsafe { libc::waitpid(vmm_pid, &mut status, 0) };
        let _ = std::fs::remove_file(socket_path(&handle_owned));
        let _ = std::fs::remove_file(pid_path(&handle_owned));
        if let Ok(reg) = Registry::open() {
            let _ = reg.set_lifecycle(&handle_owned, "sealed");
        }
        std::process::exit(0);
    });

    if let Ok(reg) = Registry::open() {
        let _ = reg.set_lifecycle(handle, "live");
    }

    // One request/response per connection, strictly serialized — the serial
    // channel has no multiplexing.
    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let Ok(req) = proto::read_request(&mut conn) else { continue };
        if proto::send_request(&mut guest_in, &req).is_err() {
            let _ = proto::write_response(&mut conn, 255, b"machine is shutting down");
            continue;
        }
        match proto::read_response(&mut guest_out) {
            Ok((code, out)) => {
                let _ = proto::write_response(&mut conn, code, &out);
            }
            Err(_) => {
                let _ = proto::write_response(&mut conn, 255, b"machine is shutting down");
            }
        }
    }
    Ok(())
}

/// Client side: send one command to a running machine's shim.
pub fn request(handle: &str, cmd: &[u8]) -> Result<(u8, Vec<u8>)> {
    let sock = socket_path(handle);
    let mut conn = std::os::unix::net::UnixStream::connect(&sock)
        .with_context(|| format!("machine '{}' is not running", handle))?;
    proto::send_request(&mut conn, cmd)?;
    Ok(proto::read_response(&mut conn)?)
}

/// True if the machine's supervisor socket accepts connections.
pub fn is_running(handle: &str) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path(handle)).is_ok()
}
