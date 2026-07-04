//! Host side of the guest agent's framed exec protocol.
//!
//! request:  u32 BE length + command bytes (run via /bin/sh -c)
//! response: 1 byte exit code + u32 BE length + combined stdout/stderr
//!
//! Reserved commands understood by the agent:
//!   __quiesce__   flush guest filesystems (used by save)
//!   __poweroff__  flush and power the machine off (used by drop)
//!   __synctree__  replace a directory tree with a gzipped tar payload
//!                 (used by the automatic working-tree sync)

use std::io::{Read, Write};

pub const QUIESCE: &[u8] = b"__quiesce__";
pub const POWEROFF: &[u8] = b"__poweroff__";

/// Prefix of a sync-tree frame: `__synctree__\0<target>\0<gzipped tar>`.
pub const SYNCTREE_PREFIX: &[u8] = b"__synctree__\0";

/// Build a sync-tree frame for `target` (absolute guest path) carrying a
/// gzipped tar of the tree to install there.
pub fn synctree_frame(target: &str, gz_tar: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(SYNCTREE_PREFIX.len() + target.len() + 1 + gz_tar.len());
    frame.extend_from_slice(SYNCTREE_PREFIX);
    frame.extend_from_slice(target.as_bytes());
    frame.push(0);
    frame.extend_from_slice(gz_tar);
    frame
}

pub fn send_request(w: &mut impl Write, cmd: &[u8]) -> std::io::Result<()> {
    w.write_all(&(cmd.len() as u32).to_be_bytes())?;
    w.write_all(cmd)?;
    w.flush()
}

pub fn read_request(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn write_response(w: &mut impl Write, code: u8, out: &[u8]) -> std::io::Result<()> {
    w.write_all(&[code])?;
    w.write_all(&(out.len() as u32).to_be_bytes())?;
    w.write_all(out)?;
    w.flush()
}

pub fn read_response(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut code = [0u8; 1];
    r.read_exact(&mut code)?;
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut out = vec![0u8; len];
    r.read_exact(&mut out)?;
    Ok((code[0], out))
}
