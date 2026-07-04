//! The machine supervisor. `got` has no daemon (plan.md §4.1): each running
//! machine is owned by one detached shim process that boots the guest as a
//! child and proxies the exec protocol between a per-machine UNIX socket and
//! the guest's serial channel.
//!
//! Lifetime: the shim exits when the guest powers off (agent __poweroff__ or
//! guest-initiated), removing its socket and pid file and marking the
//! machine `sealed` in the registry.

use anyhow::{bail, Context, Result};
use got_store::{runtime_dir, Registry};
use got_vmm::proto;
use std::io::{Read, Write};
use std::os::unix::io::FromRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn socket_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.sock", sanitize(handle)))
}

pub fn pid_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.pid", sanitize(handle)))
}

pub fn console_log_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.console.log", sanitize(handle)))
}

// The network proxy parses its socket arguments as URLs and decodes
// percent-escapes, so these paths must not contain '%' (a "%2F" would be
// decoded back to '/' and bind in a nonexistent directory).
pub(crate) fn net_socket_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.net.sock", sanitize(handle).replace('%', "_")))
}

pub(crate) fn net_api_path(handle: &str) -> PathBuf {
    runtime_dir().join(format!("{}.net.api.sock", sanitize(handle).replace('%', "_")))
}

pub fn sanitize(handle: &str) -> String {
    handle.replace('/', "%2F")
}

/// Start this machine's private network proxy and wait for its sockets.
/// One instance per machine: each guest gets its own network, so identical
/// guest addressing never collides across machines.
fn start_net_proxy(handle: &str) -> Result<std::process::Child> {
    let bin = got_vmm::net_proxy_path()
        .context("machine network runtime is missing — re-run scripts/install.sh")?;
    let net_sock = net_socket_path(handle);
    let api_sock = net_api_path(handle);
    let _ = std::fs::remove_file(&net_sock);
    let _ = std::fs::remove_file(&api_sock);

    let child = std::process::Command::new(bin)
        .arg("-listen-vfkit")
        .arg(format!("unixgram://{}", net_sock.display()))
        .arg("-listen")
        .arg(format!("unix://{}", api_sock.display()))
        .arg("-ssh-port")
        .arg("-1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("start machine network")?;

    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(5) {
        if net_sock.exists() && api_sock.exists() {
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("machine network did not come up");
}

/// One minimal HTTP/1.1 POST over the proxy's control socket.
fn net_api_post(api_sock: &Path, path: &str, body: &str) -> Result<()> {
    let mut conn = UnixStream::connect(api_sock).context("machine network control socket")?;
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: net\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        body.len(),
        body
    );
    conn.write_all(req.as_bytes())?;
    let mut resp = String::new();
    let _ = conn.read_to_string(&mut resp);
    let status_ok = resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200");
    anyhow::ensure!(status_ok, "network control request failed: {}", resp.lines().next().unwrap_or(""));
    Ok(())
}

/// Publish each "host:guest" pair: host 127.0.0.1:<host> forwards to the
/// guest's address inside its private network.
fn expose_ports(handle: &str, port_map: &[String]) -> Result<()> {
    let api = net_api_path(handle);
    for pair in port_map {
        let Some((host, guest)) = pair.split_once(':') else { continue };
        let body = format!(
            r#"{{"local":"127.0.0.1:{}","remote":"{}:{}"}}"#,
            host,
            got_vmm::GUEST_IP,
            guest
        );
        net_api_post(&api, "/services/forwarder/expose", &body)
            .with_context(|| format!("expose port {} -> {}", host, guest))?;
    }
    Ok(())
}

/// Entry point for the hidden `__shim` subcommand. Blocks until the guest
/// powers off. The caller (got new) has already set the loader path so the
/// forked guest can find its firmware. Everything else about the machine
/// (overlay, resources, ports) comes from its registry row.
pub fn run(handle: &str) -> Result<()> {
    let machine = Registry::open()?
        .get_machine(handle)?
        .with_context(|| format!("no machine '{}'", handle))?;
    let overlay = machine.overlay_path.clone();
    let port_map: Vec<String> = machine
        .port_map
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    std::fs::create_dir_all(runtime_dir())?;
    let sock = socket_path(handle);
    let _ = std::fs::remove_file(&sock);

    // Detach from the caller's session so the machine outlives the CLI call.
    // The network proxy is spawned after this, so it shares the machine's
    // process group and dies with it on `drop --force`.
    unsafe { libc::setsid() };

    // The machine's private network: proxy first, then port forwards.
    let net_proxy = start_net_proxy(handle)?;
    let net_proxy_pid = net_proxy.id() as i32;
    expose_ports(handle, &port_map)?;
    let net_socket = net_socket_path(handle);

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
            overlay: &overlay,
            cpus: machine.cpus,
            ram_mib: machine.ram_mib,
            console_log: console_log.to_str().unwrap(),
            net_socket: net_socket.to_str().unwrap(),
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

    // Reap the guest and exit the shim when it powers off. The network
    // proxy is part of the machine: it goes down with the guest.
    let handle_owned = handle.to_string();
    std::thread::spawn(move || {
        let mut status = 0i32;
        unsafe { libc::waitpid(vmm_pid, &mut status, 0) };
        unsafe { libc::kill(net_proxy_pid, libc::SIGTERM) };
        let _ = std::fs::remove_file(socket_path(&handle_owned));
        let _ = std::fs::remove_file(pid_path(&handle_owned));
        let _ = std::fs::remove_file(net_socket_path(&handle_owned));
        let _ = std::fs::remove_file(net_api_path(&handle_owned));
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
