//! Bypass detection's per-OS enumerator (story 22-4, AD-23): the remote
//! addresses of every ESTABLISHED TCP connection held by a process tree.
//!
//! * **macOS:** `proc_listallpids` + `proc_pidinfo(PROC_PIDTBSDINFO)` for
//!   the parent map, `proc_pidinfo(PROC_PIDLISTFDS)` for each tree process's
//!   descriptors, and `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` for each socket
//!   — libproc through `nix::libc`, the source the backend already uses for
//!   start-times. `libc` binds the calls but not `struct socket_fdinfo`, so
//!   the few fields read here are read at their offsets in xnu's
//!   `<sys/proc_info.h>` layout (stable since 10.5; pinned by a test against
//!   a real connection).
//! * **Linux:** one `/proc` scan for the parent map, each tree process's
//!   `/proc/<pid>/fd` `socket:[inode]` links, and the `/proc/net/tcp{,6}`
//!   rows in state `01` (ESTABLISHED) whose inode is one of them.
//! * **Other Unix:** unavailable.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

/// Why the tree could not be enumerated (reported as `unavailable`, never
/// as clean).
pub type EnumerateError = String;

/// The tree rooted at `root`: `root` and every transitive child.
fn tree(root: u32, parents: &HashMap<u32, u32>) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in parents {
        children.entry(ppid).or_default().push(pid);
    }
    let mut seen = HashSet::from([root]);
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &child in children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
            if seen.insert(child) {
                stack.push(child);
            }
        }
    }
    seen
}

/// The remote addresses of the established TCP connections held by `root`
/// and its descendants (deduplicated).
pub fn tree_remote_addrs(root: u32) -> Result<Vec<SocketAddr>, EnumerateError> {
    let mut addrs = enumerate(root)?;
    addrs.sort();
    addrs.dedup();
    Ok(addrs)
}

#[cfg(target_os = "macos")]
fn enumerate(root: u32) -> Result<Vec<SocketAddr>, EnumerateError> {
    macos::enumerate(root)
}

#[cfg(target_os = "linux")]
fn enumerate(root: u32) -> Result<Vec<SocketAddr>, EnumerateError> {
    linux::enumerate(root)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn enumerate(_root: u32) -> Result<Vec<SocketAddr>, EnumerateError> {
    Err("bypass detection is not implemented on this operating system".to_string())
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use nix::libc;

    use super::EnumerateError;

    /// `PROC_PIDFDSOCKETINFO` (`<sys/proc_info.h>`).
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    /// `PROC_PIDLISTFDS`.
    const PROC_PIDLISTFDS: libc::c_int = 1;
    /// `PROX_FDTYPE_SOCKET`.
    const PROX_FDTYPE_SOCKET: u32 = 2;
    /// `SOCKINFO_TCP`.
    const SOCKINFO_TCP: i32 = 2;
    /// `TSI_S_ESTABLISHED`.
    const TSI_S_ESTABLISHED: i32 = 4;
    /// `INI_IPV4` / `INI_IPV6` (`insi_vflag`).
    const INI_IPV4: u8 = 0x1;
    const INI_IPV6: u8 = 0x2;

    /// `sizeof(struct socket_fdinfo)` on every supported macOS.
    const SOCKET_FDINFO_SIZE: usize = 792;
    // Offsets into `struct socket_fdinfo` (`proc_fileinfo` is 24 bytes;
    // `socket_info` follows at 24): `soi_kind` at 24 + 232, `soi_proto` at
    // 24 + 240; inside it `struct tcp_sockinfo` starts with `in_sockinfo`,
    // whose `insi_vflag` is at +24 and `insi_faddr` (16 bytes; an IPv4
    // address sits in its last 4) at +32; `tcpsi_state` follows the 80-byte
    // `in_sockinfo`.
    const SOI_KIND: usize = 256;
    const PROTO: usize = 264;
    /// `insi_fport`: the foreign port, network byte order in an `int`.
    const FPORT: usize = PROTO;
    const VFLAG: usize = PROTO + 24;
    const FADDR: usize = PROTO + 32;
    const TCP_STATE: usize = PROTO + 80;

    fn read_i32(buf: &[u8], at: usize) -> i32 {
        i32::from_ne_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
    }

    fn all_pids() -> Result<Vec<u32>, EnumerateError> {
        // SAFETY: a null buffer asks for the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Err("the process list could not be read".to_string());
        }
        let mut pids = vec![0 as libc::c_int; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::c_int>()) as libc::c_int;
        // SAFETY: `pids` holds `bytes` bytes; the call writes at most that.
        let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, bytes) };
        if got <= 0 {
            return Err("the process list could not be read".to_string());
        }
        pids.truncate(got as usize);
        Ok(pids
            .into_iter()
            .filter(|p| *p > 0)
            .map(|p| p as u32)
            .collect())
    }

    fn parent(pid: u32) -> Option<u32> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a zeroed proc_bsdinfo of exactly `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (written == size).then_some(info.pbi_ppid)
    }

    fn socket_fds(pid: u32) -> Vec<i32> {
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0
            };
            1024
        ];
        let bytes = (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as libc::c_int;
        // SAFETY: `fds` holds `bytes` bytes; the call writes at most that.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr() as *mut libc::c_void,
                bytes,
            )
        };
        if written <= 0 {
            return Vec::new();
        }
        fds.truncate(written as usize / std::mem::size_of::<libc::proc_fdinfo>());
        fds.into_iter()
            .filter(|fd| fd.proc_fdtype == PROX_FDTYPE_SOCKET)
            .map(|fd| fd.proc_fd)
            .collect()
    }

    fn remote(pid: u32, fd: i32) -> Option<SocketAddr> {
        let mut buf = [0u8; 1024];
        // SAFETY: `buf` holds 1024 bytes, more than `struct socket_fdinfo`;
        // the call writes at most the size it is given.
        let written = unsafe {
            libc::proc_pidfdinfo(
                pid as libc::c_int,
                fd,
                PROC_PIDFDSOCKETINFO,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as libc::c_int,
            )
        };
        if written as usize != SOCKET_FDINFO_SIZE {
            return None;
        }
        if read_i32(&buf, SOI_KIND) != SOCKINFO_TCP
            || read_i32(&buf, TCP_STATE) != TSI_S_ESTABLISHED
        {
            return None;
        }
        let addr = &buf[FADDR..FADDR + 16];
        let ip = match buf[VFLAG] {
            v if v & INI_IPV4 != 0 => {
                IpAddr::V4(Ipv4Addr::new(addr[12], addr[13], addr[14], addr[15]))
            }
            v if v & INI_IPV6 != 0 => {
                let octets: [u8; 16] = addr.try_into().ok()?;
                IpAddr::V6(Ipv6Addr::from(octets))
            }
            _ => return None,
        };
        let port = u16::from_be(read_i32(&buf, FPORT) as u16);
        Some(SocketAddr::new(ip, port))
    }

    pub(super) fn enumerate(root: u32) -> Result<Vec<SocketAddr>, EnumerateError> {
        let parents: HashMap<u32, u32> = all_pids()?
            .into_iter()
            .filter_map(|pid| parent(pid).map(|ppid| (pid, ppid)))
            .collect();
        let mut addrs = Vec::new();
        for pid in super::tree(root, &parents) {
            for fd in socket_fds(pid) {
                addrs.extend(remote(pid, fd));
            }
        }
        Ok(addrs)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{HashMap, HashSet};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use super::EnumerateError;

    fn parent(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after = stat.rsplit_once(')')?.1;
        after.split_whitespace().nth(1)?.parse().ok()
    }

    fn socket_inodes(pid: u32, into: &mut HashSet<u64>) {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                let target = target.to_string_lossy();
                if let Some(inode) = target
                    .strip_prefix("socket:[")
                    .and_then(|rest| rest.strip_suffix(']'))
                    .and_then(|n| n.parse().ok())
                {
                    into.insert(inode);
                }
            }
        }
    }

    /// One `/proc/net/tcp{,6}` remote address (`ADDR:PORT`, hex, the kernel's
    /// little-endian words).
    pub(super) fn parse_remote(raw: &str) -> Option<SocketAddr> {
        let (addr, port) = raw.split_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let ip = match addr.len() {
            8 => {
                let word = u32::from_str_radix(addr, 16).ok()?;
                IpAddr::V4(Ipv4Addr::from(word.to_le_bytes()))
            }
            32 => {
                let mut octets = [0u8; 16];
                for (i, chunk) in octets.chunks_mut(4).enumerate() {
                    let word = u32::from_str_radix(&addr[i * 8..i * 8 + 8], 16).ok()?;
                    chunk.copy_from_slice(&word.to_le_bytes());
                }
                IpAddr::V6(Ipv6Addr::from(octets))
            }
            _ => return None,
        };
        Some(SocketAddr::new(ip, port))
    }

    pub(super) fn enumerate(root: u32) -> Result<Vec<SocketAddr>, EnumerateError> {
        let entries = std::fs::read_dir("/proc").map_err(|err| format!("/proc: {err}"))?;
        let parents: HashMap<u32, u32> = entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| parent(pid).map(|ppid| (pid, ppid)))
            .collect();
        let mut inodes = HashSet::new();
        for pid in super::tree(root, &parents) {
            socket_inodes(pid, &mut inodes);
        }
        let mut addrs = Vec::new();
        let mut read_any = false;
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            // A kernel without IPv6 has no tcp6 table; one table read is a
            // check, none is not.
            let Ok(text) = std::fs::read_to_string(table) else {
                continue;
            };
            read_any = true;
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                // sl local rem st tx:rx tr:when retrnsmt uid timeout inode
                if fields.len() < 10 || fields[3] != "01" {
                    continue;
                }
                let inode: u64 = fields[9].parse().unwrap_or(0);
                if inodes.contains(&inode) {
                    addrs.extend(parse_remote(fields[2]));
                }
            }
        }
        if !read_any {
            return Err("the kernel's TCP tables (/proc/net/tcp) could not be read".to_string());
        }
        Ok(addrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_is_the_root_and_every_transitive_child() {
        let parents = HashMap::from([(2, 1), (3, 2), (4, 1), (5, 99), (6, 3)]);
        let found = tree(2, &parents);
        assert_eq!(found, HashSet::from([2, 3, 6]));
    }

    #[test]
    fn this_processes_own_established_connection_is_found_and_a_listener_is_not() {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(tree_remote_addrs(std::process::id()).is_err());
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let before = tree_remote_addrs(std::process::id()).unwrap();
        assert!(!before.contains(&addr), "a listener is not a connection");
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (_server, _) = listener.accept().unwrap();
        let after = tree_remote_addrs(std::process::id()).unwrap();
        // The client's remote is the listener; the accepted socket's remote
        // is the client's own port.
        assert!(after.contains(&addr), "{after:?}");
        assert!(after.contains(&client.local_addr().unwrap()), "{after:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_net_addresses_parse_in_the_kernels_word_order() {
        assert_eq!(
            linux::parse_remote("0100007F:1F90"),
            Some("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(
            linux::parse_remote("00000000000000000000000001000000:0050"),
            Some("[::1]:80".parse().unwrap())
        );
        assert_eq!(linux::parse_remote("zz:1"), None);
    }
}
