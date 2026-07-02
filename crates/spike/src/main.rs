//! WP0 spike: prove the kill gate + transport bake-off from mvp-plan.md §4.
//!
//! Commands:
//!   spike boot <disk.img> -- <cmd> [args...]   boot a machine from an ext4 disk, run cmd
//!   spike killgate <base.img>                  full gate: clone -> boot -> save -> restore -> verify
//!   spike vsock-bench <base.img> <N>           exec round-trip latency over vsock
//!   spike serial-bench <base.img> <N>          exec round-trip latency over virtio-serial
//!
//! This is throwaway code by design (plan.md M0). No backend names in output.

use std::env;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::raw::c_char;
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::exit;
use std::time::{Duration, Instant};

const DISK_FORMAT_RAW: u32 = 0;
const SYNC_MODE_RELAXED: u32 = 1;
const AGENT_VSOCK_PORT: u32 = 1024;
const SERIAL_PORT_NAME: &str = "got-exec";

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
    fn krun_add_vsock_port2(
        ctx_id: u32,
        port: u32,
        c_filepath: *const c_char,
        listen: bool,
    ) -> i32;
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

extern "C" {
    // APFS copy-on-write clone. Declared here because libc crate doesn't expose it.
    fn clonefile(src: *const c_char, dst: *const c_char, flags: u32) -> i32;
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("string contains NUL")
}

const FIRMWARE_DIR: &str = "/opt/homebrew/opt/libkrunfw/lib";

/// The engine dlopens its firmware bundle by leaf name, so the directory must
/// be on the loader's fallback path — which dyld reads only at process start.
/// Re-exec ourselves once with it set. WP0 finding: the real CLI should ship
/// the firmware next to its own binary (or link it explicitly) instead.
fn ensure_firmware_loadable() {
    if !Path::new(FIRMWARE_DIR).join("libkrunfw.5.dylib").exists() {
        eprintln!("error: machine firmware not installed");
        exit(1);
    }
    let key = "DYLD_FALLBACK_LIBRARY_PATH";
    let current = env::var(key).unwrap_or_default();
    if current.split(':').any(|p| p == FIRMWARE_DIR) {
        return;
    }
    let value = if current.is_empty() {
        FIRMWARE_DIR.to_string()
    } else {
        format!("{}:{}", current, FIRMWARE_DIR)
    };
    let exe = env::current_exe().expect("current exe");
    let err = std::process::Command::new(exe)
        .args(env::args().skip(1))
        .env(key, value)
        .exec();
    // exec only returns on failure
    eprintln!("error: relaunch failed: {}", err);
    exit(1);
}

fn check(rc: i32, what: &str) {
    if rc < 0 {
        eprintln!("error: {} failed ({})", what, rc);
        exit(1);
    }
}

/// CoW-clone src to dst, returning elapsed milliseconds.
fn cow_clone(src: &str, dst: &str) -> f64 {
    if Path::new(dst).exists() {
        std::fs::remove_file(dst).expect("remove existing clone target");
    }
    let csrc = cstr(src);
    let cdst = cstr(dst);
    let t = Instant::now();
    let rc = unsafe { clonefile(csrc.as_ptr(), cdst.as_ptr(), 0) };
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if rc != 0 {
        eprintln!(
            "error: copy-on-write clone {} -> {} failed: {}",
            src,
            dst,
            std::io::Error::last_os_error()
        );
        exit(1);
    }
    ms
}

/// Flush a file's data all the way to the physical drive (F_FULLFSYNC).
fn full_fsync(path: &str) {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open overlay for flush");
    let rc = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_FULLFSYNC) };
    if rc != 0 {
        eprintln!(
            "error: full flush of {} failed: {}",
            path,
            std::io::Error::last_os_error()
        );
        exit(1);
    }
}

/// Transport wiring for a machine that runs the exec agent.
enum AgentTransport {
    /// Host connects to a UNIX socket; forwarded to the guest agent's vsock listener.
    Vsock { host_sock: String },
    /// Host reads/writes pipe fds wired to a named virtio-serial port.
    Serial { input_fd: RawFd, output_fd: RawFd },
}

/// Configure and enter the machine. Never returns (the process becomes the VMM).
fn enter_machine(
    disk: &str,
    argv: &[String],
    transport: Option<&AgentTransport>,
    console_log: Option<&str>,
) -> ! {
    unsafe {
        krun_set_log_level(0);
        let ctx = krun_create_ctx();
        check(ctx, "create context");
        let ctx = ctx as u32;

        check(krun_set_vm_config(ctx, 1, 512), "vm config");

        let block_id = cstr("root");
        let disk_path = cstr(disk);
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
            "attach disk",
        );

        let device = cstr("/dev/vda");
        let fstype = cstr("ext4");
        check(
            krun_set_root_disk_remount(ctx, device.as_ptr(), fstype.as_ptr(), std::ptr::null()),
            "root remount",
        );

        match transport {
            Some(AgentTransport::Vsock { host_sock }) => {
                let path = cstr(host_sock);
                check(
                    krun_add_vsock_port2(ctx, AGENT_VSOCK_PORT, path.as_ptr(), true),
                    "exec channel",
                );
            }
            Some(AgentTransport::Serial { input_fd, output_fd }) => {
                let console_id = krun_add_virtio_console_multiport(ctx);
                check(console_id, "exec console");
                let name = cstr(SERIAL_PORT_NAME);
                check(
                    krun_add_console_port_inout(
                        ctx,
                        console_id as u32,
                        name.as_ptr(),
                        *input_fd,
                        *output_fd,
                    ),
                    "exec port",
                );
            }
            None => {}
        }

        if let Some(log) = console_log {
            let log_path = cstr(log);
            check(krun_set_console_output(ctx, log_path.as_ptr()), "console log");
        }

        let workdir = cstr("/");
        check(krun_set_workdir(ctx, workdir.as_ptr()), "workdir");

        // argv[0] is the exec path; the guest init sets the program name
        // itself, so the arg array holds only argv[1..].
        let exec_path = cstr(&argv[0]);
        let c_args: Vec<CString> = argv[1..].iter().map(|a| cstr(a)).collect();
        let mut arg_ptrs: Vec<*const c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
        arg_ptrs.push(std::ptr::null());

        let envs = [
            cstr("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
            cstr("HOME=/root"),
            cstr("TERM=xterm"),
        ];
        let mut env_ptrs: Vec<*const c_char> = envs.iter().map(|e| e.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());

        check(
            krun_set_exec(ctx, exec_path.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr()),
            "set exec",
        );

        // Takes over the process; exits with the workload's exit code.
        let rc = krun_start_enter(ctx);
        eprintln!("error: machine failed to start ({})", rc);
        exit(1);
    }
}

/// Fork a child that boots the machine. Returns the child pid.
fn spawn_machine(
    disk: &str,
    argv: &[String],
    transport: Option<AgentTransport>,
    console_log: Option<&str>,
) -> i32 {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        eprintln!("error: fork failed");
        exit(1);
    }
    if pid == 0 {
        enter_machine(disk, argv, transport.as_ref(), console_log);
    }
    pid
}

fn wait_machine(pid: i32) -> i32 {
    let mut status: i32 = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    if rc < 0 {
        eprintln!("error: wait failed");
        exit(1);
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    }
}

/// Fork a child that boots the machine and runs argv inside it.
/// Returns (guest exit code, elapsed ms for the machine's full lifecycle).
///
/// NOTE (WP0 finding): the guest init does NOT propagate the workload's exit
/// code to the host — this exit code is only meaningful for VMM-level failure.
/// Real exec semantics must go through the agent transport (see *-bench).
fn boot_and_run(disk: &str, argv: &[String]) -> (i32, f64) {
    let t = Instant::now();
    let pid = spawn_machine(disk, argv, None, None);
    let code = wait_machine(pid);
    (code, t.elapsed().as_secs_f64() * 1000.0)
}

/// Boot the machine, run argv, capture console output to a file and return it.
/// Used to verify guest-side state honestly (exit codes don't propagate).
fn boot_and_capture(disk: &str, argv: &[String]) -> (String, f64) {
    let log = format!("/tmp/got-spike-capture-{}.log", std::process::id());
    let _ = std::fs::remove_file(&log);
    let t = Instant::now();
    let pid = spawn_machine(disk, argv, None, Some(&log));
    wait_machine(pid);
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let out = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    (out, ms)
}

fn sh(cmd: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), cmd.into()]
}

fn agent_argv(mode: &str) -> Vec<String> {
    vec!["/usr/local/bin/got-agent".into(), mode.into()]
}

// ---- framed exec protocol (host side) ----

fn send_request(w: &mut impl Write, cmd: &[u8]) -> std::io::Result<()> {
    w.write_all(&(cmd.len() as u32).to_be_bytes())?;
    w.write_all(cmd)?;
    w.flush()
}

fn read_response(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut code = [0u8; 1];
    r.read_exact(&mut code)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut out = vec![0u8; len];
    r.read_exact(&mut out)?;
    Ok((code[0], out))
}

fn print_stats(label: &str, mut samples: Vec<f64>) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let avg: f64 = samples.iter().sum::<f64>() / n as f64;
    println!(
        "{}: n={}  min {:.2} ms  p50 {:.2} ms  avg {:.2} ms  max {:.2} ms",
        label,
        n,
        samples[0],
        samples[n / 2],
        avg,
        samples[n - 1]
    );
}

// ---- vsock bench ----

fn vsock_exec(sock: &str, cmd: &[u8]) -> std::io::Result<(u8, Vec<u8>)> {
    let mut stream = UnixStream::connect(sock)?;
    send_request(&mut stream, cmd)?;
    read_response(&mut stream)
}

fn cmd_vsock_bench(base: &str, n: usize) {
    let sock = "/tmp/got-spike-exec.sock";
    let _ = std::fs::remove_file(sock);
    let dir = Path::new(base).parent().unwrap().to_str().unwrap().to_string();
    let disk = format!("{}/bench-vsock.img", dir);
    cow_clone(base, &disk);

    let t_boot = Instant::now();
    let pid = spawn_machine(
        &disk,
        &agent_argv("--vsock"),
        Some(AgentTransport::Vsock { host_sock: sock.into() }),
        Some("/tmp/got-spike-console.log"),
    );

    // Wait until the agent answers its first exec (includes boot time).
    // Optional initial delay (GOT_SPIKE_DELAY_MS) to rule out poll artifacts.
    if let Ok(delay) = env::var("GOT_SPIKE_DELAY_MS") {
        std::thread::sleep(Duration::from_millis(delay.parse().unwrap()));
    }
    let mut ready_ms = None;
    for _ in 0..2000 {
        if let Ok((0, _)) = vsock_exec(sock, b"true") {
            ready_ms = Some(t_boot.elapsed().as_secs_f64() * 1000.0);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let Some(ready_ms) = ready_ms else {
        eprintln!("error: exec channel never became ready");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        exit(1);
    };
    println!("machine ready (boot to first exec): {:.1} ms", ready_ms);

    // Real exec semantics: exit codes and output must round-trip.
    let (code, _) = vsock_exec(sock, b"exit 7").expect("exec failed");
    assert_eq!(code, 7, "exit code propagation");
    let (code, out) = vsock_exec(sock, b"echo out; echo err >&2").expect("exec failed");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8_lossy(&out), "out\nerr\n", "output capture");
    println!("exit codes and stdout/stderr round-trip correctly");

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        let (code, _) = vsock_exec(sock, b"true").expect("exec failed");
        assert_eq!(code, 0);
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    print_stats("vsock exec round-trip", samples);

    let (code, _) = vsock_exec(sock, b"__quiesce__").expect("quiesce failed");
    assert_eq!(code, 0, "quiesce");
    let _ = vsock_exec(sock, b"__poweroff__");
    wait_machine(pid);
    let _ = std::fs::remove_file(&disk);
}

// ---- serial bench ----

fn cmd_serial_bench(base: &str, n: usize) {
    let dir = Path::new(base).parent().unwrap().to_str().unwrap().to_string();
    let disk = format!("{}/bench-serial.img", dir);
    cow_clone(base, &disk);

    // host-to-guest and guest-to-host pipes
    let mut h2g = [0i32; 2];
    let mut g2h = [0i32; 2];
    unsafe {
        assert_eq!(libc::pipe(h2g.as_mut_ptr()), 0);
        assert_eq!(libc::pipe(g2h.as_mut_ptr()), 0);
    }

    let t_boot = Instant::now();
    let pid = spawn_machine(
        &disk,
        &agent_argv("--serial"),
        Some(AgentTransport::Serial { input_fd: h2g[0], output_fd: g2h[1] }),
        Some("/tmp/got-spike-console.log"),
    );

    // Parent keeps the far ends.
    unsafe {
        libc::close(h2g[0]);
        libc::close(g2h[1]);
    }
    use std::os::unix::io::FromRawFd;
    let mut wr = unsafe { std::fs::File::from_raw_fd(h2g[1]) };
    let mut rd = unsafe { std::fs::File::from_raw_fd(g2h[0]) };

    // First exec: the pipe buffers the request until the agent opens the port.
    send_request(&mut wr, b"true").expect("send");
    let (code, _) = read_response(&mut rd).expect("first exec");
    assert_eq!(code, 0);
    println!(
        "machine ready (boot to first exec): {:.1} ms",
        t_boot.elapsed().as_secs_f64() * 1000.0
    );

    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let t = Instant::now();
        send_request(&mut wr, b"true").expect("send");
        let (code, _) = read_response(&mut rd).expect("exec");
        assert_eq!(code, 0);
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    print_stats("serial exec round-trip", samples);

    send_request(&mut wr, b"__quiesce__").expect("quiesce send");
    let (code, _) = read_response(&mut rd).expect("quiesce");
    assert_eq!(code, 0, "quiesce");
    send_request(&mut wr, b"__poweroff__").expect("poweroff send");
    let _ = read_response(&mut rd);
    wait_machine(pid);
    let _ = std::fs::remove_file(&disk);
}

// ---- parallel machines smoke test ----

fn cmd_parallel(base: &str, n: usize) {
    let dir = Path::new(base).parent().unwrap().to_str().unwrap().to_string();
    println!("== parallel: {} machines, boot + exec + verify + shutdown ==", n);
    let t_all = Instant::now();

    struct Member {
        pid: i32,
        wr: std::fs::File,
        rd: std::fs::File,
        disk: String,
    }
    let mut members = Vec::with_capacity(n);

    for i in 0..n {
        let disk = format!("{}/par-{}.img", dir, i);
        cow_clone(base, &disk);
        let mut h2g = [0i32; 2];
        let mut g2h = [0i32; 2];
        unsafe {
            assert_eq!(libc::pipe(h2g.as_mut_ptr()), 0);
            assert_eq!(libc::pipe(g2h.as_mut_ptr()), 0);
        }
        let pid = spawn_machine(
            &disk,
            &agent_argv("--serial"),
            Some(AgentTransport::Serial { input_fd: h2g[0], output_fd: g2h[1] }),
            Some(&format!("/tmp/got-spike-par-{}.log", i)),
        );
        unsafe {
            libc::close(h2g[0]);
            libc::close(g2h[1]);
        }
        use std::os::unix::io::FromRawFd;
        members.push(Member {
            pid,
            wr: unsafe { std::fs::File::from_raw_fd(h2g[1]) },
            rd: unsafe { std::fs::File::from_raw_fd(g2h[0]) },
            disk,
        });
    }
    println!("[spawn] {} machines forked in {:.1} ms", n, t_all.elapsed().as_secs_f64() * 1000.0);

    // Each machine: write a unique marker, read it back. Requests are sent to
    // all machines before any response is awaited, so the guests run
    // concurrently.
    for (i, m) in members.iter_mut().enumerate() {
        let cmd = format!("echo machine-{} > /whoami && cat /whoami", i);
        send_request(&mut m.wr, cmd.as_bytes()).expect("send");
    }
    for (i, m) in members.iter_mut().enumerate() {
        let (code, out) = read_response(&mut m.rd).expect("exec");
        assert_eq!(code, 0, "machine {} exec failed", i);
        let expected = format!("machine-{}\n", i);
        assert_eq!(
            String::from_utf8_lossy(&out),
            expected,
            "machine {} returned wrong identity",
            i
        );
    }
    let ready_ms = t_all.elapsed().as_secs_f64() * 1000.0;
    println!("[exec] all {} machines answered with distinct state ({:.1} ms total)", n, ready_ms);

    for m in members.iter_mut() {
        send_request(&mut m.wr, b"__poweroff__").expect("poweroff send");
        let _ = read_response(&mut m.rd);
    }
    for m in &members {
        wait_machine(m.pid);
        let _ = std::fs::remove_file(&m.disk);
    }
    println!("[drop] all machines shut down cleanly");
    println!("---");
    println!(
        "PARALLEL: PASS ({} machines, {:.1} ms spawn-to-verified)",
        n, ready_ms
    );
}

// ---- end-to-end (M0 exit gate) ----

/// Registry: one line per snapshot, "handle head_sha content_hash".
fn registry_path(dir: &str) -> String {
    format!("{}/registry.txt", dir)
}

fn registry_lookup(dir: &str, handle: &str, head_sha: &str) -> Option<String> {
    let content = std::fs::read_to_string(registry_path(dir)).ok()?;
    content
        .lines()
        .rev()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let (h, sha, hash) = (parts.next()?, parts.next()?, parts.next()?);
            (h == handle && sha == head_sha).then(|| hash.to_string())
        })
}

fn registry_record(dir: &str, handle: &str, head_sha: &str, content_hash: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(registry_path(dir))
        .expect("open registry");
    writeln!(f, "{} {} {}", handle, head_sha, content_hash).expect("write registry");
}

fn git_head_sha() -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("run git");
    if !out.status.success() {
        eprintln!("error: not in a git repository (or no commits yet)");
        exit(1);
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn content_hash(path: &str) -> String {
    // Spike-only: shells out to b3sum. M1 uses the blake3 crate in-process.
    let out = std::process::Command::new("b3sum")
        .args(["--no-names", path])
        .output()
        .expect("run b3sum");
    if !out.status.success() {
        eprintln!("error: content hash failed");
        exit(1);
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn cmd_e2e(base: &str, handle: &str) {
    let dir = Path::new(base).parent().unwrap().to_str().unwrap().to_string();
    let snapdir = format!("{}/snapshots", dir);
    std::fs::create_dir_all(&snapdir).expect("create snapshot dir");
    let live = format!("{}/{}-live.img", dir, handle.replace('/', "-"));

    println!("== end-to-end: new -> run -> save(SHA) -> drop -> restore-by-SHA ==");
    let head_sha = git_head_sha();
    println!("[git] HEAD is {}", &head_sha[..12]);

    // new: fork a live overlay from the base image.
    cow_clone(base, &live);
    println!("[new] machine '{}' created", handle);

    // run: mutate state with something tied to this commit.
    let mutation = format!("echo {} > /state-of-commit && sync", head_sha);
    let (_, ms) = boot_and_run(&live, &sh(&mutation));
    println!("[run] state mutated inside machine ({:.1} ms)", ms);

    // save: quiesce (machine already exited), flush, hash, CoW-clone, record.
    let t = Instant::now();
    full_fsync(&live);
    let hash = content_hash(&live);
    let snap_path = format!("{}/{}", snapdir, hash);
    if !Path::new(&snap_path).exists() {
        cow_clone(&live, &snap_path);
    }
    registry_record(&dir, handle, &head_sha, &hash);
    println!(
        "[save] snapshot {} for commit {} ({:.1} ms)",
        &hash[..12],
        &head_sha[..12],
        t.elapsed().as_secs_f64() * 1000.0
    );

    // drop: destroy the live overlay. The snapshot survives.
    std::fs::remove_file(&live).expect("drop live overlay");
    println!("[drop] machine '{}' destroyed, snapshot kept", handle);

    // new again: restore the snapshot recorded for (handle, HEAD).
    let t = Instant::now();
    let found = registry_lookup(&dir, handle, &head_sha).unwrap_or_else(|| {
        eprintln!("FAIL: no snapshot recorded for (handle, HEAD)");
        exit(1);
    });
    let restored = format!("{}/{}-restored.img", dir, handle.replace('/', "-"));
    cow_clone(&format!("{}/{}", snapdir, found), &restored);
    let (out, _) = boot_and_capture(&restored, &sh("cat /state-of-commit"));
    let restore_ms = t.elapsed().as_secs_f64() * 1000.0;
    if !out.contains(&head_sha) {
        eprintln!("FAIL: restored state does not match the saved commit");
        exit(1);
    }
    println!(
        "[new] machine '{}' restored from snapshot for {} and verified ({:.1} ms)",
        handle,
        &head_sha[..12],
        restore_ms
    );

    std::fs::remove_file(&restored).expect("drop restored overlay");
    println!("[drop] restored machine destroyed");
    println!("---");
    println!("END-TO-END: PASS");
}

// ---- kill gate ----

fn cmd_killgate(base: &str) {
    let dir = Path::new(base).parent().unwrap().to_str().unwrap().to_string();
    let work = format!("{}/killgate-live.img", dir);
    let snap = format!("{}/killgate-snapshot.img", dir);
    let restored = format!("{}/killgate-restored.img", dir);

    println!("== kill gate: machine boot + CoW snapshot restore, target < 2000 ms ==");

    // 1. Fork a live overlay from the base image.
    let clone_ms = cow_clone(base, &work);
    println!("[1] fork live overlay from base       {:>8.1} ms", clone_ms);

    // 2. Boot it and mutate state (write a marker file).
    let (code, boot1_ms) = boot_and_run(&work, &sh("echo got-was-here > /marker && sync"));
    if code != 0 {
        eprintln!("error: state mutation run exited {}", code);
        exit(1);
    }
    println!("[2] boot + mutate state + shutdown    {:>8.1} ms", boot1_ms);

    // 3. Save: flush to physical disk, then CoW-clone the overlay (quiesced:
    //    the machine has exited, so this is the sealed-clone path).
    let t = Instant::now();
    full_fsync(&work);
    let fsync_ms = t.elapsed().as_secs_f64() * 1000.0;
    let save_ms = cow_clone(&work, &snap);
    println!(
        "[3] save snapshot (flush + clone)     {:>8.1} ms  (flush {:.1} ms, clone {:.1} ms)",
        fsync_ms + save_ms,
        fsync_ms,
        save_ms
    );

    // 4. Restore: clone the snapshot into a fresh overlay and boot it.
    //    Verify via console output — init exit codes do not propagate.
    let restore_ms = cow_clone(&snap, &restored);
    let (out, boot2_ms) = boot_and_capture(&restored, &sh("cat /marker"));
    println!("[4] restore clone                     {:>8.1} ms", restore_ms);
    println!("[5] boot restored + verify state      {:>8.1} ms", boot2_ms);
    if !out.contains("got-was-here") {
        eprintln!("FAIL: restored machine is missing the saved state");
        exit(1);
    }

    let gate_ms = restore_ms + boot2_ms;
    println!("---");
    println!("state verified: marker written before save is present after restore");
    println!("gate total (restore + boot):          {:>8.1} ms", gate_ms);
    if gate_ms < 2000.0 {
        println!("KILL GATE: PASS ({:.1} ms < 2000 ms)", gate_ms);
    } else {
        println!("KILL GATE: FAIL ({:.1} ms >= 2000 ms)", gate_ms);
        exit(1);
    }
}

fn main() {
    ensure_firmware_loadable();
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("boot") => {
            // spike boot <disk.img> -- <cmd> [args...]
            let disk = args.get(2).unwrap_or_else(|| usage());
            let sep = args.iter().position(|a| a == "--").unwrap_or_else(|| {
                usage();
            });
            let cmd: Vec<String> = args[sep + 1..].to_vec();
            if cmd.is_empty() {
                usage();
            }
            let (code, ms) = boot_and_run(disk, &cmd);
            eprintln!("(machine lifecycle: {:.1} ms, guest exit {})", ms, code);
            exit(code);
        }
        Some("killgate") => {
            let base = args.get(2).unwrap_or_else(|| usage());
            cmd_killgate(base);
        }
        Some("vsock-bench") => {
            let base = args.get(2).unwrap_or_else(|| usage());
            let n: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(100);
            cmd_vsock_bench(base, n);
        }
        Some("serial-bench") => {
            let base = args.get(2).unwrap_or_else(|| usage());
            let n: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(100);
            cmd_serial_bench(base, n);
        }
        Some("e2e") => {
            let base = args.get(2).unwrap_or_else(|| usage());
            let handle = args.get(3).map(String::as_str).unwrap_or("spike-machine");
            cmd_e2e(base, handle);
        }
        Some("parallel") => {
            let base = args.get(2).unwrap_or_else(|| usage());
            let n: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(6);
            cmd_parallel(base, n);
        }
        _ => {
            usage();
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: spike boot <disk.img> -- <cmd> [args...]");
    eprintln!("       spike killgate <base.img>");
    eprintln!("       spike vsock-bench <base.img> [n]");
    eprintln!("       spike serial-bench <base.img> [n]");
    eprintln!("       spike e2e <base.img> [handle]");
    eprintln!("       spike parallel <base.img> [n]");
    exit(2);
}
