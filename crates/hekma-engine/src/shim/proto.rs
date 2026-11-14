//! `shim-rpc v1` (story 24-1): one JSON object per line over the Shim's
//! stream. The engine sends [`Request`]s, each with an `id`; the Shim sends
//! [`ShimFrame`]s — a reply carrying the request's `id`, or a push (`exit`,
//! `gone`) with none. The engine's `hello` is always first and carries the
//! protocol version and the connection secret.
//!
//! No frame's `Debug` prints a secret, an environment value or stdin bytes:
//! the spawn request carries the agent's environment in the clear (the Run
//! Key crosses this boundary by design, AD-23) and is the one place it
//! does. Error replies carry a fixed code and a static message that echoes
//! nothing the peer sent.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ports::{LogCaptureMode, ProcessFingerprint, SpawnSpec};

/// A `shim-rpc` version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// This build's version.
    pub const CURRENT: Version = Version {
        major: super::SHIM_PROTOCOL_MAJOR,
        minor: super::SHIM_PROTOCOL_MINOR,
    };

    /// Whether an engine at [`Version::CURRENT`] speaks `other`: the same
    /// major, and its own minor or the previous one.
    pub fn engine_accepts(other: Version) -> bool {
        other.major == Self::CURRENT.major
            && (other.minor == Self::CURRENT.minor || other.minor + 1 == Self::CURRENT.minor)
    }
}

/// What to launch: [`SpawnSpec`]'s fields as plain data.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimSpawn {
    pub exec: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_dir: PathBuf,
    pub log_file: Option<PathBuf>,
    pub attributed_log_path: Option<PathBuf>,
    pub stderr_log_file: Option<PathBuf>,
    pub instance_name: String,
    pub pipe_stdin: bool,
    pub detach: bool,
    pub pipe_stdout: bool,
    pub inherit_env: bool,
    pub capture_mode: String,
}

impl ShimSpawn {
    /// The DTO of `spec`.
    pub fn of(spec: &SpawnSpec) -> Self {
        ShimSpawn {
            exec: spec.exec.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            working_dir: spec.working_dir.clone(),
            log_file: spec.log_file.clone(),
            attributed_log_path: spec.attributed_log_path.clone(),
            stderr_log_file: spec.stderr_log_file.clone(),
            instance_name: spec.instance_name.clone(),
            pipe_stdin: spec.pipe_stdin,
            detach: spec.detach,
            pipe_stdout: spec.pipe_stdout,
            inherit_env: spec.inherit_env,
            capture_mode: spec.capture_mode.as_str().to_string(),
        }
    }

    /// The [`SpawnSpec`] to launch; `None` for a spec the backends refuse
    /// to take (an unknown capture mode, or capture paths that are neither
    /// all set nor all unset) — refused as a bad frame, never a panic
    /// inside the Shim.
    pub fn spec(&self) -> Option<SpawnSpec> {
        let paths = [
            self.log_file.is_some(),
            self.attributed_log_path.is_some(),
            self.stderr_log_file.is_some(),
        ];
        if paths.iter().any(|set| *set) && !paths.iter().all(|set| *set) {
            return None;
        }
        Some(SpawnSpec {
            exec: self.exec.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            working_dir: self.working_dir.clone(),
            log_file: self.log_file.clone(),
            attributed_log_path: self.attributed_log_path.clone(),
            stderr_log_file: self.stderr_log_file.clone(),
            instance_name: self.instance_name.clone(),
            pipe_stdin: self.pipe_stdin,
            detach: self.detach,
            pipe_stdout: self.pipe_stdout,
            inherit_env: self.inherit_env,
            capture_mode: self.capture_mode.parse::<LogCaptureMode>().ok()?,
        })
    }
}

impl std::fmt::Debug for ShimSpawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The environment's NAMES only — its values may hold the Run Key.
        f.debug_struct("ShimSpawn")
            .field("exec", &self.exec)
            .field("instance_name", &self.instance_name)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// One engine → Shim request.
#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

/// What the engine asks.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Always first: the engine's protocol version, the connection secret
    /// (hex) and the engine's pid.
    Hello {
        v: Version,
        secret: String,
        engine_pid: u32,
    },
    /// Launch the child (once per Shim).
    Spawn {
        spec: ShimSpawn,
    },
    /// Stop the child: the graceful window, then the forced kill.
    Stop {
        graceful_ms: u64,
    },
    Pause,
    Resume,
    /// Is the child alive? (reaps it when it exited)
    Poll,
    /// Write bytes (hex) to the child's stdin, bounded by the backend.
    WriteStdin {
        data: String,
    },
    /// The Shim's state.
    Status,
    /// The engine has the buffered exit; the Shim may leave.
    AckExit,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let op = match &self.op {
            Op::Hello { v, .. } => format!("Hello {{ v: {v:?}, secret: <redacted> }}"),
            Op::Spawn { spec } => format!("Spawn {{ {spec:?} }}"),
            Op::Stop { graceful_ms } => format!("Stop {{ graceful_ms: {graceful_ms} }}"),
            Op::WriteStdin { data } => format!("WriteStdin {{ {} hex chars }}", data.len()),
            Op::Pause => "Pause".into(),
            Op::Resume => "Resume".into(),
            Op::Poll => "Poll".into(),
            Op::Status => "Status".into(),
            Op::AckExit => "AckExit".into(),
        };
        write!(f, "Request {{ id: {}, {op} }}", self.id)
    }
}

/// A child exit the Shim saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub code: Option<i32>,
    /// RFC 3339.
    pub at: String,
}

/// The Shim's state, as `hello_ack` and `status` report it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimStatus {
    /// The child has been launched.
    pub launched: bool,
    /// The child's fingerprint, once launched.
    pub child: Option<(u32, u64)>,
    /// The child's exit, once it exited (kept until acknowledged).
    pub exit: Option<ExitRecord>,
    /// The spawn request's environment has been dropped from memory.
    pub env_dropped: bool,
    /// The child has a stdin pipe.
    pub has_stdin: bool,
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// The hello's secret does not match.
    SecretMismatch,
    /// The connecting process is not the Shim's owner.
    PeerRefused,
    /// Another engine holds the Shim's one control connection.
    Busy,
    /// The engine's protocol version is not one this Shim speaks.
    Version,
    /// A request this Shim does not know.
    OpUnknown,
    /// The child is gone.
    ChildGone,
    /// A frame that is not JSON, or not a request.
    BadFrame,
    /// `spawn` after a launch, or an operation before one.
    WrongState,
    /// The OS backend refused the operation.
    Backend,
}

impl ErrorCode {
    /// The static message (never an echo of the peer's input).
    pub fn message(self) -> &'static str {
        match self {
            ErrorCode::SecretMismatch => "the connection secret does not match this Shim",
            ErrorCode::PeerRefused => "the connecting process is not this Shim's owner",
            ErrorCode::Busy => "another engine holds this Shim's control connection",
            ErrorCode::Version => "this Shim does not speak the engine's protocol version",
            ErrorCode::OpUnknown => "the request is not one this Shim knows",
            ErrorCode::ChildGone => "the child process is gone",
            ErrorCode::BadFrame => "the frame is not a valid request",
            ErrorCode::WrongState => "the request does not fit the Shim's state",
            ErrorCode::Backend => "the operating system refused the operation",
        }
    }
}

/// A reply's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    HelloAck { v: Version, status: ShimStatus },
    Spawned { pid: u32, start_time: u64 },
    Stopped { forced: bool },
    Polled { exit: Option<ExitRecord> },
    Status { status: ShimStatus },
    Done,
    Error { code: ErrorCode, message: String },
}

impl Outcome {
    /// The error reply for `code`.
    pub fn error(code: ErrorCode) -> Self {
        Outcome::Error {
            code,
            message: code.message().to_string(),
        }
    }
}

/// One Shim → engine frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ShimFrame {
    /// The reply to request `id`.
    Reply { id: u64, outcome: Outcome },
    /// The child exited (pushed to the connected engine).
    Exit { exit: ExitRecord },
}

impl ShimFrame {
    /// The frame as one line (newline-terminated).
    pub fn line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_default();
        line.push('\n');
        line
    }
}

/// The fingerprint of a `spawned` reply.
pub fn fingerprint(pid: u32, start_time: u64) -> ProcessFingerprint {
    ProcessFingerprint::new(pid, start_time)
}

/// Hex of stdin bytes (the frame's encoding).
pub fn encode_bytes(bytes: &[u8]) -> String {
    super::secret::to_hex(bytes)
}

/// Bytes of a hex string; `None` when malformed.
pub fn decode_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_debug_never_prints_a_secret_or_an_env_value() {
        let hello = Request {
            id: 1,
            op: Op::Hello {
                v: Version::CURRENT,
                secret: "SENTINEL-SECRET".into(),
                engine_pid: 7,
            },
        };
        let line = serde_json::to_string(&hello).unwrap();
        assert!(line.contains("\"op\":\"hello\""), "{line}");
        let back: Request = serde_json::from_str(&line).unwrap();
        assert!(matches!(back.op, Op::Hello { engine_pid: 7, .. }));
        assert!(!format!("{hello:?}").contains("SENTINEL"));
        let mut env = BTreeMap::new();
        env.insert("HEKMA_RUN_KEY".to_string(), "SENTINEL-KEY".to_string());
        let spec = ShimSpawn {
            exec: "agent".into(),
            args: Vec::new(),
            env,
            working_dir: "/tmp".into(),
            log_file: None,
            attributed_log_path: None,
            stderr_log_file: None,
            instance_name: "bot".into(),
            pipe_stdin: true,
            detach: false,
            pipe_stdout: false,
            inherit_env: false,
            capture_mode: "full".into(),
        };
        let spawn = Request {
            id: 2,
            op: Op::Spawn { spec: spec.clone() },
        };
        let debug = format!("{spawn:?}");
        assert!(
            debug.contains("HEKMA_RUN_KEY") && !debug.contains("SENTINEL"),
            "{debug}"
        );
        assert_eq!(ShimSpawn::of(&spec.spec().unwrap()), spec);
        let stdin = Request {
            id: 3,
            op: Op::WriteStdin {
                data: encode_bytes(b"SENTINEL"),
            },
        };
        assert!(!format!("{stdin:?}").contains(&encode_bytes(b"SENTINEL")));
        assert_eq!(decode_bytes(&encode_bytes(b"hi\n")).unwrap(), b"hi\n");
        assert!(decode_bytes("zz").is_none() && decode_bytes("abc").is_none());
        let reply = ShimFrame::Reply {
            id: 2,
            outcome: Outcome::error(ErrorCode::SecretMismatch),
        };
        let line = reply.line();
        assert!(
            line.ends_with('\n') && line.contains("secret-mismatch"),
            "{line}"
        );
        assert_eq!(
            serde_json::from_str::<ShimFrame>(line.trim()).unwrap(),
            reply
        );
    }

    #[test]
    fn the_engine_speaks_its_own_minor_and_the_previous_one() {
        let v = Version::CURRENT;
        assert!(Version::engine_accepts(v));
        assert!(!Version::engine_accepts(Version {
            major: v.major + 1,
            minor: 0
        }));
        assert!(!Version::engine_accepts(Version {
            major: v.major,
            minor: v.minor + 1
        }));
    }
}
