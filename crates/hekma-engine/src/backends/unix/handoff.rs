//! Story 24-2: the handoff channel's Unix transport — a Unix domain socket
//! in the state directory's owner-only `shims/` directory, the peer's uid
//! checked against our own before any byte, newline-delimited frames, and
//! the two descriptors an in-place upgrade hands over (the metering proxy's
//! listening socket and the locked `host.lock`) passed with `SCM_RIGHTS`.

use std::fs::File;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};

/// The listening end (the predecessor).
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    owner: u32,
}

/// One connected end.
#[derive(Debug)]
pub struct Conn {
    stream: UnixStream,
    pending: Vec<u8>,
}

/// Bind the channel at `path` (its directory made owner-only; a stale
/// socket replaced).
pub fn listen(path: &Path) -> io::Result<Listener> {
    if let Some(dir) = path.parent() {
        super::shim::prepare_dir(dir)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(Listener {
        listener,
        path: path.to_path_buf(),
        owner: nix::unistd::getuid().as_raw(),
    })
}

impl Listener {
    /// The first connection from a process of our own user within `bound`
    /// (a connection from another user is dropped unread).
    pub fn accept(&self, bound: Duration) -> io::Result<Conn> {
        let deadline = Instant::now() + bound;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    if super::shim::peer_uid(&stream).is_ok_and(|uid| uid == self.owner) {
                        return Ok(Conn {
                            stream,
                            pending: Vec::new(),
                        });
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no successor connected to the handoff channel in time",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Connect to the channel at `path`.
pub fn connect(path: &Path) -> io::Result<Conn> {
    Ok(Conn {
        stream: UnixStream::connect(path)?,
        pending: Vec::new(),
    })
}

impl Conn {
    /// Send one frame line.
    pub fn send(&mut self, line: &str) -> io::Result<()> {
        self.stream.write_all(line.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()
    }

    /// Send one frame line with `listener` (when there is one) and `lock`
    /// attached.
    pub fn send_with(
        &mut self,
        line: &str,
        listener: Option<&TcpListener>,
        lock: &File,
    ) -> io::Result<()> {
        let mut fds: Vec<RawFd> = vec![lock.as_raw_fd()];
        fds.extend(listener.map(AsRawFd::as_raw_fd));
        let bytes = format!("{line}\n");
        let iov = [IoSlice::new(bytes.as_bytes())];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        let sent = sendmsg::<()>(
            self.stream.as_raw_fd(),
            &iov,
            &cmsg,
            MsgFlags::empty(),
            None,
        )
        .map_err(io::Error::from)?;
        if sent < bytes.len() {
            self.stream.write_all(&bytes.as_bytes()[sent..])?;
        }
        Ok(())
    }

    /// Bound the next reads. macOS refuses the option (`EINVAL`) on a
    /// socket whose peer already closed; a read there cannot block (it
    /// returns what is buffered, then the end of the stream), so that one
    /// refusal is ignored.
    fn bound_reads(&self, bound: Duration) -> io::Result<()> {
        match self.stream.set_read_timeout(Some(bound)) {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
            other => other,
        }
    }

    /// The next frame line within `bound`.
    pub fn recv(&mut self, bound: Duration) -> io::Result<String> {
        self.bound_reads(bound)?;
        loop {
            if let Some(line) = self.take_line() {
                return Ok(line);
            }
            let mut chunk = [0u8; 4096];
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                return Err(closed());
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }

    /// The next frame line and the descriptors sent with it: the lock file
    /// and the listening socket, each when sent (a frame sent without any,
    /// such as an abort, carries neither).
    pub fn recv_with(
        &mut self,
        bound: Duration,
    ) -> io::Result<(String, Option<File>, Option<TcpListener>)> {
        self.bound_reads(bound)?;
        let mut buf = [0u8; 4096];
        let mut space = nix::cmsg_space!([RawFd; 2]);
        let (read, fds) = {
            let mut iov = [IoSliceMut::new(&mut buf)];
            let msg = recvmsg::<()>(
                self.stream.as_raw_fd(),
                &mut iov,
                Some(&mut space),
                MsgFlags::empty(),
            )
            .map_err(io::Error::from)?;
            let mut fds: Vec<RawFd> = Vec::new();
            for cmsg in msg.cmsgs().map_err(io::Error::from)? {
                if let ControlMessageOwned::ScmRights(received) = cmsg {
                    fds.extend(received);
                }
            }
            (msg.bytes, fds)
        };
        // Own every received descriptor at once (closed on any error path),
        // close-on-exec like every descriptor Rust opens.
        let mut owned: Vec<File> = fds
            .into_iter()
            .map(|fd| {
                // SAFETY: the kernel just installed `fd` in this process for
                // us; nothing else owns it.
                unsafe {
                    nix::libc::fcntl(fd, nix::libc::F_SETFD, nix::libc::FD_CLOEXEC);
                    File::from_raw_fd(fd)
                }
            })
            .collect();
        if read == 0 {
            return Err(closed());
        }
        self.pending.extend_from_slice(&buf[..read]);
        let line = match self.take_line() {
            Some(line) => line,
            None => self.recv(bound)?,
        };
        if owned.is_empty() {
            return Ok((line, None, None));
        }
        let lock = Some(owned.remove(0));
        let listener = owned.pop().map(|file| {
            // SAFETY: the descriptor is a listening TCP socket (the
            // predecessor sent its metering proxy's listener); ownership
            // moves from `file` to the listener.
            unsafe { TcpListener::from_raw_fd(std::os::fd::IntoRawFd::into_raw_fd(file)) }
        });
        if let Some(listener) = &listener {
            listener.local_addr()?;
        }
        Ok((line, lock, listener))
    }

    fn take_line(&mut self) -> Option<String> {
        let end = self.pending.iter().position(|b| *b == b'\n')?;
        let line: Vec<u8> = self.pending.drain(..=end).collect();
        Some(String::from_utf8_lossy(&line[..end]).into_owned())
    }
}

/// The other side is gone.
fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the other side closed the handoff channel",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_both_descriptors_cross_and_the_lock_is_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shims").join("h-test.sock");
        let listener = listen(&path).unwrap();
        let lock_path = dir.path().join("host.lock");
        let lock = File::create(&lock_path).unwrap();
        lock.try_lock().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = proxy.local_addr().unwrap().port();
        let successor = std::thread::spawn(move || {
            let mut conn = connect(&path).unwrap();
            conn.send("hello").unwrap();
            let (line, lock, listener) = conn.recv_with(Duration::from_secs(5)).unwrap();
            conn.send("ready").unwrap();
            (line, lock, listener.unwrap().local_addr().unwrap().port())
        });
        let mut conn = listener.accept(Duration::from_secs(5)).unwrap();
        assert_eq!(conn.recv(Duration::from_secs(5)).unwrap(), "hello");
        conn.send_with("offer", Some(&proxy), &lock).unwrap();
        assert_eq!(conn.recv(Duration::from_secs(5)).unwrap(), "ready");
        let (line, received_lock, received_port) = successor.join().unwrap();
        let received_lock = received_lock.expect("the lock was sent");
        assert_eq!(line, "offer");
        assert_eq!(received_port, port, "the same listening socket");
        // The lock lives on the open file: closing ours leaves it held by
        // the received copy, and a fresh open still cannot take it.
        drop(lock);
        let other = File::open(&lock_path).unwrap();
        assert!(other.try_lock().is_err(), "the successor holds the lock");
        drop(received_lock);
        assert!(other.try_lock().is_ok(), "released with the last copy");
    }

    #[test]
    fn no_successor_times_out_and_a_closed_peer_is_an_end_of_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shims").join("h-idle.sock");
        let listener = listen(&path).unwrap();
        let err = listener.accept(Duration::from_millis(60)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        let client = std::thread::spawn({
            let path = path.clone();
            move || drop(connect(&path).unwrap())
        });
        let mut conn = listener.accept(Duration::from_secs(5)).unwrap();
        client.join().unwrap();
        let err = conn.recv(Duration::from_secs(5)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        drop(listener);
        assert!(!path.exists(), "the channel's socket is removed with it");
    }

    #[test]
    fn a_stale_channel_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shims").join("h-stale.sock");
        let first = listen(&path).unwrap();
        std::mem::forget(first); // left behind, as by a crashed host
        assert!(path.exists());
        let second = listen(&path).unwrap();
        let client = std::thread::spawn({
            let path = path.clone();
            move || connect(&path).unwrap().send("hi").unwrap()
        });
        let mut conn = second.accept(Duration::from_secs(5)).unwrap();
        client.join().unwrap();
        assert_eq!(conn.recv(Duration::from_secs(5)).unwrap(), "hi");
    }
}
