//! WP0 spike guest agent: exec + quiesce channel for the transport bake-off.
//!
//! Runs as the machine's init workload and serves a framed protocol:
//!   request:  u32 BE length + command bytes (run via /bin/sh -c)
//!   response: 1 byte exit code + u32 BE length + combined stdout/stderr
//!
//! Reserved commands: "__quiesce__" (sync), "__poweroff__" (sync + power
//! off), and "__synctree__\0<target>\0<gzipped tar>" (replace a directory
//! tree — the host-side working-tree sync).
//!
//! Transports under test:
//!   --vsock   listen on vsock port 1024, one connection per exec
//!   --serial  serve framed requests sequentially on the "moo-exec" serial port

#[cfg(target_os = "linux")]
mod agent {
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::process::Command;

    const VSOCK_PORT: u32 = 1024;
    const SERIAL_PORT_NAME: &str = "moo-exec";
    const SYNCTREE_PREFIX: &[u8] = b"__synctree__\0";

    // The machine's private network plan. Fixed for every machine: each one
    // sits behind its own host-side proxy, so addresses never collide.
    const GUEST_IP: [u8; 4] = [192, 168, 127, 2];
    const GATEWAY_IP: [u8; 4] = [192, 168, 127, 1];
    const NETMASK: [u8; 4] = [255, 255, 255, 0];

    /// Name of the bookkeeping file a synced tree carries so the next sync
    /// can delete files that disappeared on the host without touching
    /// guest-only artifacts (node_modules, build output, .env, ...).
    const MANIFEST: &str = ".moo-sync-manifest";

    // ---- network bring-up ------------------------------------------------
    //
    // The agent is the machine's init: nothing else configures interfaces.
    // Bring up loopback and the virtio NIC with the fixed private plan,
    // using the legacy ioctl interface so no userspace tools are required.

    const SIOCGIFFLAGS: u64 = 0x8913;
    const SIOCSIFFLAGS: u64 = 0x8914;
    const SIOCSIFADDR: u64 = 0x8916;
    const SIOCSIFNETMASK: u64 = 0x891c;
    const SIOCADDRT: u64 = 0x890B;
    const IFF_UP: i16 = 0x1;
    const IFF_RUNNING: i16 = 0x40;
    const RTF_UP: u16 = 0x0001;
    const RTF_GATEWAY: u16 = 0x0002;

    /// Generic 16-byte sockaddr as the kernel's ioctl ABI expects.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct SockaddrGen {
        sa_family: u16,
        sa_data: [u8; 14],
    }

    impl SockaddrGen {
        fn inet(addr: [u8; 4]) -> Self {
            let mut sa_data = [0u8; 14];
            // sockaddr_in: 2 bytes port (zero), then the IPv4 address.
            sa_data[2..6].copy_from_slice(&addr);
            SockaddrGen {
                sa_family: libc::AF_INET as u16,
                sa_data,
            }
        }
        fn zero() -> Self {
            SockaddrGen {
                sa_family: libc::AF_INET as u16,
                sa_data: [0u8; 14],
            }
        }
    }

    /// struct ifreq (uapi/linux/if.h): 16-byte name + 24-byte union.
    #[repr(C)]
    struct IfReq {
        name: [u8; 16],
        data: [u8; 24],
    }

    impl IfReq {
        fn new(ifname: &str) -> Self {
            let mut name = [0u8; 16];
            name[..ifname.len()].copy_from_slice(ifname.as_bytes());
            IfReq {
                name,
                data: [0u8; 24],
            }
        }
        fn with_sockaddr(ifname: &str, sa: SockaddrGen) -> Self {
            let mut req = Self::new(ifname);
            let bytes: [u8; 16] = unsafe { std::mem::transmute(sa) };
            req.data[..16].copy_from_slice(&bytes);
            req
        }
    }

    /// struct rtentry (uapi/linux/route.h), 64-bit layout.
    #[repr(C)]
    struct RtEntry {
        rt_pad1: u64,
        rt_dst: SockaddrGen,
        rt_gateway: SockaddrGen,
        rt_genmask: SockaddrGen,
        rt_flags: u16,
        rt_pad2: i16,
        rt_pad3: u64,
        rt_pad4: *mut libc::c_void,
        rt_metric: i16,
        rt_dev: *mut libc::c_char,
        rt_mtu: u64,
        rt_window: u64,
        rt_irtt: u16,
    }

    unsafe fn ioctl(fd: i32, req: u64, arg: *mut libc::c_void) -> i32 {
        libc::ioctl(fd, req as _, arg)
    }

    fn if_up(fd: i32, ifname: &str) -> Result<(), String> {
        let mut req = IfReq::new(ifname);
        unsafe {
            if ioctl(fd, SIOCGIFFLAGS, &mut req as *mut _ as *mut _) != 0 {
                return Err(format!(
                    "{}: get flags: {}",
                    ifname,
                    std::io::Error::last_os_error()
                ));
            }
            let mut flags = i16::from_ne_bytes([req.data[0], req.data[1]]);
            flags |= IFF_UP | IFF_RUNNING;
            req.data[..2].copy_from_slice(&flags.to_ne_bytes());
            if ioctl(fd, SIOCSIFFLAGS, &mut req as *mut _ as *mut _) != 0 {
                return Err(format!(
                    "{}: set flags: {}",
                    ifname,
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }

    /// Configure lo and eth0; best-effort, errors go to the console log.
    fn net_setup() {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 {
            eprintln!(
                "moo-agent: net: no AF_INET sockets: {}",
                std::io::Error::last_os_error()
            );
            return;
        }

        if let Err(e) = if_up(fd, "lo") {
            eprintln!("moo-agent: net: {e}");
        }

        // The NIC can probe slightly after the agent starts; wait for it.
        let mut addr_set = false;
        for _ in 0..300 {
            let mut req = IfReq::with_sockaddr("eth0", SockaddrGen::inet(GUEST_IP));
            if unsafe { ioctl(fd, SIOCSIFADDR, &mut req as *mut _ as *mut _) } == 0 {
                addr_set = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !addr_set {
            eprintln!("moo-agent: net: eth0 did not appear; machine has no network");
            unsafe { libc::close(fd) };
            return;
        }

        let mut req = IfReq::with_sockaddr("eth0", SockaddrGen::inet(NETMASK));
        if unsafe { ioctl(fd, SIOCSIFNETMASK, &mut req as *mut _ as *mut _) } != 0 {
            eprintln!(
                "moo-agent: net: eth0 netmask: {}",
                std::io::Error::last_os_error()
            );
        }
        if let Err(e) = if_up(fd, "eth0") {
            eprintln!("moo-agent: net: {e}");
        }

        let mut route = RtEntry {
            rt_pad1: 0,
            rt_dst: SockaddrGen::zero(),
            rt_gateway: SockaddrGen::inet(GATEWAY_IP),
            rt_genmask: SockaddrGen::zero(),
            rt_flags: RTF_UP | RTF_GATEWAY,
            rt_pad2: 0,
            rt_pad3: 0,
            rt_pad4: std::ptr::null_mut(),
            rt_metric: 0,
            rt_dev: std::ptr::null_mut(),
            rt_mtu: 0,
            rt_window: 0,
            rt_irtt: 0,
        };
        if unsafe { ioctl(fd, SIOCADDRT, &mut route as *mut _ as *mut _) } != 0 {
            eprintln!(
                "moo-agent: net: default route: {}",
                std::io::Error::last_os_error()
            );
        }
        unsafe { libc::close(fd) };

        let dns = format!(
            "nameserver {}.{}.{}.{}\n",
            GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3]
        );
        if let Err(e) = fs::write("/etc/resolv.conf", dns) {
            eprintln!("moo-agent: net: resolv.conf: {e}");
        }
    }

    /// Install a synced tree: extract the gzipped tar over `target`, then
    /// delete files the previous sync installed that are absent from this
    /// one. Files the guest created on its own are never deleted.
    fn sync_tree(target: &str, gz_tar: &[u8]) -> Result<String, String> {
        let target = Path::new(target);
        if !target.is_absolute() {
            return Err(format!(
                "sync target must be absolute: {}",
                target.display()
            ));
        }
        fs::create_dir_all(target).map_err(|e| format!("create {}: {}", target.display(), e))?;

        let previous: BTreeSet<String> = fs::read_to_string(target.join(MANIFEST))
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default();

        let gz = flate2::read::GzDecoder::new(gz_tar);
        let mut archive = tar::Archive::new(gz);
        archive.set_preserve_permissions(true);
        archive.set_overwrite(true);

        let mut installed: BTreeSet<String> = BTreeSet::new();
        let mut bytes: u64 = 0;
        for entry in archive
            .entries()
            .map_err(|e| format!("read archive: {e}"))?
        {
            let mut entry = entry.map_err(|e| format!("read entry: {e}"))?;
            let rel = entry
                .path()
                .map_err(|e| format!("entry path: {e}"))?
                .to_string_lossy()
                .into_owned();
            bytes += entry.size();
            if !matches!(entry.header().entry_type(), tar::EntryType::Directory) {
                installed.insert(rel.trim_end_matches('/').to_string());
            }
            entry
                .unpack_in(target)
                .map_err(|e| format!("unpack {rel}: {e}"))?;
        }

        // Remove what the last sync installed but this one didn't.
        let mut dirs_to_prune: BTreeSet<std::path::PathBuf> = BTreeSet::new();
        for stale in previous.difference(&installed) {
            let path = target.join(stale);
            match fs::symlink_metadata(&path) {
                Ok(meta) if !meta.is_dir() => {
                    let _ = fs::remove_file(&path);
                    if let Some(parent) = path.parent() {
                        dirs_to_prune.insert(parent.to_path_buf());
                    }
                }
                _ => {}
            }
        }
        // Best-effort empty-directory cleanup, deepest paths first.
        for dir in dirs_to_prune.iter().rev() {
            let mut cur = dir.clone();
            while cur.starts_with(target) && cur != *target {
                if fs::remove_dir(&cur).is_err() {
                    break; // not empty (or gone) — stop climbing
                }
                match cur.parent() {
                    Some(p) => cur = p.to_path_buf(),
                    None => break,
                }
            }
        }

        let manifest_body = installed.iter().cloned().collect::<Vec<_>>().join("\n");
        fs::write(target.join(MANIFEST), manifest_body)
            .map_err(|e| format!("write manifest: {e}"))?;

        Ok(format!(
            "synced {} files ({:.1} MB) to {}",
            installed.len(),
            bytes as f64 / 1e6,
            target.display()
        ))
    }

    /// Parse and run a `__synctree__` frame: `<prefix><target>\0<gz tar>`.
    fn handle_synctree(frame: &[u8], stream: &mut impl Write) {
        let body = &frame[SYNCTREE_PREFIX.len()..];
        let Some(nul) = body.iter().position(|&b| b == 0) else {
            let _ = write_response(stream, 1, b"malformed sync frame");
            return;
        };
        let Ok(target) = std::str::from_utf8(&body[..nul]) else {
            let _ = write_response(stream, 1, b"malformed sync target");
            return;
        };
        match sync_tree(target, &body[nul + 1..]) {
            Ok(msg) => {
                let _ = write_response(stream, 0, msg.as_bytes());
            }
            Err(msg) => {
                let _ = write_response(stream, 1, msg.as_bytes());
            }
        }
    }

    fn read_frame(stream: &mut impl Read) -> std::io::Result<Vec<u8>> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn write_response(stream: &mut impl Write, code: u8, out: &[u8]) -> std::io::Result<()> {
        stream.write_all(&[code])?;
        stream.write_all(&(out.len() as u32).to_be_bytes())?;
        stream.write_all(out)?;
        stream.flush()
    }

    /// Returns false when the machine should power off.
    fn handle(cmd: &[u8], stream: &mut (impl Read + Write)) -> bool {
        if cmd.starts_with(SYNCTREE_PREFIX) {
            handle_synctree(cmd, stream);
            return true;
        }
        match cmd {
            b"__quiesce__" => {
                unsafe { libc::sync() };
                let _ = write_response(stream, 0, b"");
                true
            }
            b"__poweroff__" => {
                unsafe { libc::sync() };
                let _ = write_response(stream, 0, b"");
                unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF) };
                false
            }
            _ => {
                let cmd_str = String::from_utf8_lossy(cmd).into_owned();
                match Command::new("/bin/sh").arg("-c").arg(&cmd_str).output() {
                    Ok(out) => {
                        let mut combined = out.stdout;
                        combined.extend_from_slice(&out.stderr);
                        let code = out.status.code().unwrap_or(-1).clamp(0, 255) as u8;
                        let _ = write_response(stream, code, &combined);
                    }
                    Err(e) => {
                        let _ = write_response(stream, 127, e.to_string().as_bytes());
                    }
                }
                true
            }
        }
    }

    fn vsock_loop() -> ! {
        use std::os::unix::io::FromRawFd;
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "vsock socket");
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as _;
            addr.svm_port = VSOCK_PORT;
            addr.svm_cid = libc::VMADDR_CID_ANY;
            let rc = libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as u32,
            );
            assert!(rc == 0, "vsock bind");
            assert!(libc::listen(fd, 8) == 0, "vsock listen");
            loop {
                let conn = libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut());
                if conn < 0 {
                    continue;
                }
                let mut stream = std::fs::File::from_raw_fd(conn);
                if let Ok(cmd) = read_frame(&mut stream) {
                    if !handle(&cmd, &mut stream) {
                        std::process::exit(0);
                    }
                }
            }
        }
    }

    fn find_serial_port() -> Option<String> {
        for entry in fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
            let name_path = entry.path().join("name");
            if let Ok(name) = fs::read_to_string(&name_path) {
                if name.trim() == SERIAL_PORT_NAME {
                    return Some(format!("/dev/{}", entry.file_name().to_string_lossy()));
                }
            }
        }
        None
    }

    fn serial_loop() -> ! {
        // The port node can appear slightly after boot; poll briefly.
        let dev = (0..500)
            .find_map(|_| {
                find_serial_port().or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                })
            })
            .expect("serial port moo-exec not found");
        let mut stream = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev)
            .expect("open serial port");
        loop {
            match read_frame(&mut stream) {
                Ok(cmd) => {
                    if !handle(&cmd, &mut stream) {
                        std::process::exit(0);
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }

    /// The machine's boot hook: /etc/rc.local, the traditional convention.
    /// Provisioning writes service starts (database, cache, ...) here so
    /// they come back after reboots and snapshot restores.
    fn run_rc_local() {
        use std::os::unix::fs::PermissionsExt;
        let rc = Path::new("/etc/rc.local");
        let executable = fs::metadata(rc)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !executable {
            return;
        }
        match Command::new("/bin/sh").arg("/etc/rc.local").output() {
            Ok(out) if !out.status.success() => {
                eprintln!(
                    "moo-agent: rc.local exited {}: {}",
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => eprintln!("moo-agent: rc.local: {e}"),
            _ => {}
        }
    }

    pub fn run() -> ! {
        net_setup();
        run_rc_local();
        let mode = std::env::args().nth(1).unwrap_or_else(|| "--vsock".into());
        match mode.as_str() {
            "--vsock" => vsock_loop(),
            "--serial" => serial_loop(),
            _ => {
                eprintln!("usage: moo-agent --vsock|--serial");
                std::process::exit(2);
            }
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    agent::run();
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("moo-agent only runs inside a Linux guest");
        std::process::exit(1);
    }
}
