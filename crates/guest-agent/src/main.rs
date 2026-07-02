//! WP0 spike guest agent: exec + quiesce channel for the transport bake-off.
//!
//! Runs as the machine's init workload and serves a framed protocol:
//!   request:  u32 BE length + command bytes (run via /bin/sh -c)
//!   response: 1 byte exit code + u32 BE length + combined stdout/stderr
//!
//! Reserved commands: "__quiesce__" (sync) and "__poweroff__" (sync + power off).
//!
//! Transports under test:
//!   --vsock   listen on vsock port 1024, one connection per exec
//!   --serial  serve framed requests sequentially on the "got-exec" serial port

#[cfg(target_os = "linux")]
mod agent {
    use std::fs;
    use std::io::{Read, Write};
    use std::process::Command;

    const VSOCK_PORT: u32 = 1024;
    const SERIAL_PORT_NAME: &str = "got-exec";

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
            .expect("serial port got-exec not found");
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

    pub fn run() -> ! {
        let mode = std::env::args().nth(1).unwrap_or_else(|| "--vsock".into());
        match mode.as_str() {
            "--vsock" => vsock_loop(),
            "--serial" => serial_loop(),
            _ => {
                eprintln!("usage: got-agent --vsock|--serial");
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
        eprintln!("got-agent only runs inside a Linux guest");
        std::process::exit(1);
    }
}
