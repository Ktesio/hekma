//! The in-place upgrade's handoff channel (story 24-2, spine AD-20/AD-23):
//! how a Service Host hands its state directory to the successor it
//! launched, with no refused connection on the metering proxy.
//!
//! The predecessor opens a channel — a socket in the state directory's
//! owner-only `shims/` directory, a peer of another user refused — and
//! launches its successor with a one-time token in the environment
//! ([`HANDOFF_TOKEN_ENV`], never in argv). The successor connects and says
//! `hello` (protocol version, token, pid); the predecessor checks both,
//! stops its Control API, releases its Shims (no stop), closes its store
//! writer, and sends `offer` with two descriptors: the metering proxy's
//! listening socket (still open, so the kernel's backlog keeps accepting)
//! and the locked `host.lock` (an advisory lock lives on the open file, so
//! the successor holds the same lock with no unowned moment). The successor
//! opens its engine on the inherited socket, reconnects to every Shim,
//! serves, and answers `ready`; only then does the predecessor exit.
//!
//! No frame ever carries a bearer, a Run Key or a Shim secret; the token is
//! compared by hash and never written to disk.

use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backends::{HandoffConn, HandoffListener};

/// `handoff-rpc` major version: a different major is never spoken.
pub const HANDOFF_PROTOCOL_MAJOR: u32 = 1;
/// `handoff-rpc` minor version: this build's.
pub const HANDOFF_PROTOCOL_MINOR: u32 = 0;

/// The environment variable that carries the one-time token to the
/// successor.
pub const HANDOFF_TOKEN_ENV: &str = "HEKMA_HANDOFF_TOKEN";

/// How long the predecessor waits for its successor to connect.
pub const SUCCESSOR_CONNECT_BOUND: Duration = Duration::from_secs(30);

/// How long the predecessor waits for its successor's `ready`.
pub const SUCCESSOR_READY_BOUND: Duration = Duration::from_secs(120);

/// One frame (a JSON line).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// Successor → predecessor, first.
    Hello {
        major: u32,
        minor: u32,
        token: String,
        pid: u32,
    },
    /// Predecessor → successor, with the descriptors attached.
    Offer {
        major: u32,
        minor: u32,
        /// Whether the metering proxy's socket is attached.
        proxy: bool,
    },
    /// Successor → predecessor: serving.
    Ready { pid: u32, reattached: usize },
    /// Either side: the hand-over stops here.
    Abort { reason: String },
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Frame::Hello {
                major, minor, pid, ..
            } => write!(
                f,
                "Hello {{ v: {major}.{minor}, pid: {pid}, token: <redacted> }}"
            ),
            Frame::Offer {
                major,
                minor,
                proxy,
            } => {
                write!(f, "Offer {{ v: {major}.{minor}, proxy: {proxy} }}")
            }
            Frame::Ready { pid, reattached } => {
                write!(f, "Ready {{ pid: {pid}, reattached: {reattached} }}")
            }
            Frame::Abort { reason } => write!(f, "Abort {{ reason: {reason:?} }}"),
        }
    }
}

/// Why a hand-over did not happen.
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    /// The channel failed (bind, connect, a read or write, a timeout).
    #[error("the handoff channel failed: {0}")]
    Io(#[from] std::io::Error),
    /// The other side sent something this side does not speak.
    #[error("the other side sent an unexpected frame: {0}")]
    Unexpected(String),
    /// The successor presented the wrong token.
    #[error("the successor presented the wrong handoff token")]
    WrongToken,
    /// The other side speaks another protocol major.
    #[error("the other side speaks handoff-rpc {major}.{minor}, which this build does not")]
    Version { major: u32, minor: u32 },
    /// The other side gave up.
    #[error("the other side aborted the hand-over: {0}")]
    Aborted(String),
}

/// Whether this OS can hand a Service Host over in place (Unix today).
pub fn supported() -> bool {
    crate::backends::shim_supported()
}

/// A fresh one-time token: 32 random bytes, hex.
pub fn new_token() -> Result<String, HandoffError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| HandoffError::Io(std::io::Error::other(err.to_string())))?;
    Ok(crate::shim::secret::to_hex(&bytes))
}

/// Where a hand-over's channel lives: a short random name in the state
/// directory's owner-only `shims/` directory (socket paths are short).
pub fn channel_path(state_base: &Path) -> Result<PathBuf, HandoffError> {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| HandoffError::Io(std::io::Error::other(err.to_string())))?;
    Ok(state_base
        .join("shims")
        .join(format!("h-{}.sock", crate::shim::secret::to_hex(&bytes))))
}

fn encode(frame: &Frame) -> String {
    serde_json::to_string(frame).expect("frames serialize")
}

fn decode(line: &str) -> Result<Frame, HandoffError> {
    serde_json::from_str(line.trim()).map_err(|_| HandoffError::Unexpected(redact(line)))
}

/// A line for an error message, never echoing a token-sized secret.
fn redact(line: &str) -> String {
    if line.contains("token") {
        "<a frame carrying a token>".to_string()
    } else {
        line.chars().take(120).collect()
    }
}

fn same_token(presented: &str, expected: &str) -> bool {
    crate::sha256::digest(presented.as_bytes()) == crate::sha256::digest(expected.as_bytes())
}

/// The predecessor's side: the channel, waiting for its successor.
pub struct Predecessor {
    listener: HandoffListener,
    path: PathBuf,
    token: String,
}

impl Predecessor {
    /// Open a channel in `state_base` with a fresh token.
    pub fn open(state_base: &Path) -> Result<Self, HandoffError> {
        let path = channel_path(state_base)?;
        let listener = crate::backends::handoff_listen(&path)?;
        Ok(Predecessor {
            listener,
            path,
            token: new_token()?,
        })
    }

    /// The channel's path (the successor's `--handoff-from`).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The token to hand the successor in [`HANDOFF_TOKEN_ENV`].
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The successor's `hello` within `bound`, checked: the protocol major
    /// and the token. A wrong token is answered `abort` and refused.
    pub fn await_successor(&self, bound: Duration) -> Result<Link, HandoffError> {
        let mut conn = self.listener.accept(bound)?;
        match decode(&conn.recv(bound)?)? {
            Frame::Hello {
                major,
                minor,
                token,
                pid,
            } => {
                if major != HANDOFF_PROTOCOL_MAJOR {
                    let _ = conn.send(&encode(&Frame::Abort {
                        reason: "another handoff-rpc major".to_string(),
                    }));
                    return Err(HandoffError::Version { major, minor });
                }
                if !same_token(&token, &self.token) {
                    let _ = conn.send(&encode(&Frame::Abort {
                        reason: "wrong token".to_string(),
                    }));
                    return Err(HandoffError::WrongToken);
                }
                Ok(Link {
                    conn,
                    successor_pid: pid,
                })
            }
            other => Err(HandoffError::Unexpected(format!("{other:?}"))),
        }
    }
}

/// The predecessor's connection to a checked successor.
#[derive(Debug)]
pub struct Link {
    conn: HandoffConn,
    successor_pid: u32,
}

impl Link {
    /// The successor's pid (from its `hello`).
    pub fn successor_pid(&self) -> u32 {
        self.successor_pid
    }

    /// Tell the successor the hand-over will not happen.
    pub fn abort(mut self, reason: &str) {
        let _ = self.conn.send(&encode(&Frame::Abort {
            reason: reason.to_string(),
        }));
    }

    /// Hand over the metering proxy's socket (when there is one) and the
    /// locked `host.lock`.
    pub fn offer(
        &mut self,
        listener: Option<&TcpListener>,
        lock: &File,
    ) -> Result<(), HandoffError> {
        let frame = Frame::Offer {
            major: HANDOFF_PROTOCOL_MAJOR,
            minor: HANDOFF_PROTOCOL_MINOR,
            proxy: listener.is_some(),
        };
        self.conn.send_with(&encode(&frame), listener, lock)?;
        Ok(())
    }

    /// The successor's `ready` (its pid and how many agents it reconnected
    /// to) within `bound`.
    pub fn await_ready(&mut self, bound: Duration) -> Result<(u32, usize), HandoffError> {
        match decode(&self.conn.recv(bound)?)? {
            Frame::Ready { pid, reattached } => Ok((pid, reattached)),
            Frame::Abort { reason } => Err(HandoffError::Aborted(reason)),
            other => Err(HandoffError::Unexpected(format!("{other:?}"))),
        }
    }
}

/// What the successor received.
#[derive(Debug)]
pub struct Joined {
    conn: HandoffConn,
    lock: Option<File>,
    listener: Option<TcpListener>,
}

impl Joined {
    /// The locked `host.lock` (the same lock the predecessor held), once.
    pub fn take_lock(&mut self) -> Option<File> {
        self.lock.take()
    }

    /// The metering proxy's listening socket, when the predecessor ran one,
    /// once.
    pub fn take_listener(&mut self) -> Option<TcpListener> {
        self.listener.take()
    }

    /// Tell the predecessor this host serves now.
    pub fn ready(mut self, reattached: usize) -> Result<(), HandoffError> {
        self.conn.send(&encode(&Frame::Ready {
            pid: std::process::id(),
            reattached,
        }))?;
        Ok(())
    }

    /// Tell the predecessor this host could not take over.
    pub fn abort(mut self, reason: &str) {
        let _ = self.conn.send(&encode(&Frame::Abort {
            reason: reason.to_string(),
        }));
    }
}

/// The successor's side: connect to `path`, present `token`, and receive
/// the hand-over within `bound`.
pub fn join(path: &Path, token: &str, bound: Duration) -> Result<Joined, HandoffError> {
    let mut conn = crate::backends::handoff_connect(path)?;
    conn.send(&encode(&Frame::Hello {
        major: HANDOFF_PROTOCOL_MAJOR,
        minor: HANDOFF_PROTOCOL_MINOR,
        token: token.to_string(),
        pid: std::process::id(),
    }))?;
    let (line, lock, listener) = conn.recv_with(bound)?;
    match decode(&line)? {
        Frame::Offer { major, minor, .. } if major != HANDOFF_PROTOCOL_MAJOR => {
            Err(HandoffError::Version { major, minor })
        }
        Frame::Offer { .. } => match lock {
            Some(lock) => Ok(Joined {
                conn,
                lock: Some(lock),
                listener,
            }),
            None => Err(HandoffError::Unexpected(
                "an offer without the host lock".to_string(),
            )),
        },
        Frame::Abort { reason } => Err(HandoffError::Aborted(reason)),
        other => Err(HandoffError::Unexpected(format!("{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix() -> bool {
        std::env::consts::FAMILY == "unix"
    }

    #[test]
    fn a_successor_with_the_token_receives_the_socket_and_the_lock() {
        if !unix() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let predecessor = Predecessor::open(state.path()).unwrap();
        assert!(predecessor.path().starts_with(state.path().join("shims")));
        assert_eq!(predecessor.token().len(), 64);
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = proxy.local_addr().unwrap().port();
        let lock = File::create(state.path().join("host.lock")).unwrap();
        let (path, token) = (
            predecessor.path().to_path_buf(),
            predecessor.token().to_string(),
        );
        let successor = std::thread::spawn(move || {
            let mut joined = join(&path, &token, Duration::from_secs(5)).unwrap();
            assert!(joined.take_lock().is_some());
            let port = joined.take_listener().unwrap().local_addr().unwrap().port();
            assert!(joined.take_listener().is_none(), "taken once");
            joined.ready(2).unwrap();
            port
        });
        let mut link = predecessor.await_successor(Duration::from_secs(5)).unwrap();
        assert_eq!(link.successor_pid(), std::process::id());
        link.offer(Some(&proxy), &lock).unwrap();
        assert_eq!(link.await_ready(Duration::from_secs(5)).unwrap().1, 2);
        assert_eq!(successor.join().unwrap(), port);
    }

    #[test]
    fn a_wrong_token_is_refused_and_an_abort_reaches_the_other_side() {
        if !unix() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let predecessor = Predecessor::open(state.path()).unwrap();
        let path = predecessor.path().to_path_buf();
        let impostor = std::thread::spawn({
            let path = path.clone();
            move || join(&path, "not-the-token", Duration::from_secs(5)).unwrap_err()
        });
        assert!(matches!(
            predecessor.await_successor(Duration::from_secs(5)),
            Err(HandoffError::WrongToken)
        ));
        let err = impostor.join().unwrap();
        assert!(
            matches!(&err, HandoffError::Aborted(reason) if reason == "wrong token"),
            "{err}"
        );
        // The real successor, refused by the predecessor before the offer.
        let token = predecessor.token().to_string();
        let successor = std::thread::spawn(move || join(&path, &token, Duration::from_secs(5)));
        let link = predecessor.await_successor(Duration::from_secs(5)).unwrap();
        link.abort("the predecessor could not stop");
        let err = successor.join().unwrap().unwrap_err();
        assert!(
            matches!(&err, HandoffError::Aborted(reason) if reason.contains("could not stop")),
            "{err}"
        );
        // A successor that gives up instead.
        let token = predecessor.token().to_string();
        let path = predecessor.path().to_path_buf();
        let quitter = std::thread::spawn(move || {
            let joined = join(&path, &token, Duration::from_secs(5)).unwrap();
            joined.abort("migration failed");
        });
        let mut link = predecessor.await_successor(Duration::from_secs(5)).unwrap();
        let lock = File::create(state.path().join("host.lock")).unwrap();
        link.offer(None, &lock).unwrap();
        quitter.join().unwrap();
        let ended = link.await_ready(Duration::from_secs(5));
        assert!(
            matches!(&ended, Err(HandoffError::Aborted(reason)) if reason == "migration failed"),
            "{ended:?}"
        );
        assert!(!format!(
            "{:?}",
            Frame::Hello {
                major: 1,
                minor: 0,
                token: "SECRET".into(),
                pid: 1
            }
        )
        .contains("SECRET"));
    }

    /// What a confused peer's case expects of the error.
    type Expect = fn(&HandoffError) -> bool;

    /// A raw peer: `lines` sent as they are (a confused or foreign side).
    fn raw_successor(path: PathBuf, lines: Vec<String>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut conn = crate::backends::handoff_connect(&path).unwrap();
            for line in lines {
                conn.send(&line).unwrap();
            }
            let _ = conn.recv(Duration::from_secs(2));
        })
    }

    #[test]
    fn a_confused_successor_is_refused_by_what_it_sent() {
        if !unix() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let predecessor = Predecessor::open(state.path()).unwrap();
        let path = predecessor.path().to_path_buf();
        let foreign = encode(&Frame::Hello {
            major: HANDOFF_PROTOCOL_MAJOR + 1,
            minor: 0,
            token: predecessor.token().to_string(),
            pid: 1,
        });
        let cases: Vec<(String, Expect)> = vec![
            (foreign, |err| {
                matches!(err, HandoffError::Version { major: 2, .. })
            }),
            (
                "not json".to_string(),
                |err| matches!(err, HandoffError::Unexpected(line) if line == "not json"),
            ),
            (
                encode(&Frame::Ready {
                    pid: 1,
                    reattached: 0,
                }),
                |err| matches!(err, HandoffError::Unexpected(text) if text.contains("Ready")),
            ),
            (
                r#"{"frame":"hello","token":"x"}"#.to_string(),
                |err| matches!(err, HandoffError::Unexpected(text) if text.contains("carrying a token")),
            ),
        ];
        for (line, expected) in cases {
            let peer = raw_successor(path.clone(), vec![line]);
            let err = predecessor
                .await_successor(Duration::from_secs(5))
                .unwrap_err();
            assert!(expected(&err), "{err}");
            peer.join().unwrap();
        }
    }

    #[test]
    fn a_confused_predecessor_is_refused_by_what_it_sent() {
        if !unix() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let lock_path = state.path().join("host.lock");
        let answers: Vec<(Option<Frame>, &str, Expect)> = vec![
            (
                Some(Frame::Offer {
                    major: 9,
                    minor: 0,
                    proxy: false,
                }),
                "with-lock",
                |err| matches!(err, HandoffError::Version { major: 9, .. }),
            ),
            (
                Some(Frame::Offer {
                    major: HANDOFF_PROTOCOL_MAJOR,
                    minor: 0,
                    proxy: false,
                }),
                "no-lock",
                |err| matches!(err, HandoffError::Unexpected(text) if text.contains("host lock")),
            ),
            (
                Some(Frame::Ready {
                    pid: 1,
                    reattached: 0,
                }),
                "no-lock",
                |err| matches!(err, HandoffError::Unexpected(text) if text.contains("Ready")),
            ),
            (None, "no-lock", |err| {
                matches!(err, HandoffError::Unexpected(_))
            }),
        ];
        for (frame, how, expected) in answers {
            let path = channel_path(state.path()).unwrap();
            let listener = crate::backends::handoff_listen(&path).unwrap();
            let lock = File::create(&lock_path).unwrap();
            let predecessor = std::thread::spawn(move || {
                let mut conn = listener.accept(Duration::from_secs(5)).unwrap();
                let _hello = conn.recv(Duration::from_secs(5)).unwrap();
                let line = frame.as_ref().map_or("garbage".to_string(), encode);
                if how == "with-lock" {
                    conn.send_with(&line, None, &lock).unwrap();
                } else {
                    conn.send(&line).unwrap();
                }
                let _ = conn.recv(Duration::from_secs(2));
            });
            let err = join(&path, "token", Duration::from_secs(5)).unwrap_err();
            assert!(expected(&err), "{err}");
            predecessor.join().unwrap();
        }
    }

    #[test]
    fn frames_debug_without_their_token_and_the_os_answer_is_stated() {
        let frames = [
            Frame::Hello {
                major: 1,
                minor: 0,
                token: "SECRET".into(),
                pid: 7,
            },
            Frame::Offer {
                major: 1,
                minor: 0,
                proxy: true,
            },
            Frame::Ready {
                pid: 7,
                reattached: 3,
            },
            Frame::Abort {
                reason: "why".into(),
            },
        ];
        let text: Vec<String> = frames.iter().map(|frame| format!("{frame:?}")).collect();
        assert!(text.iter().all(|line| !line.contains("SECRET")));
        assert_eq!(text[2], "Ready { pid: 7, reattached: 3 }");
        assert_eq!(supported(), unix());
        assert_eq!(redact(&"x".repeat(500)).len(), 120);
    }
}
