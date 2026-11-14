//! The engine's side of a Shim connection (story 24-1): `hello` first,
//! then one request at a time; an `exit` pushed between replies is kept
//! for [`ShimClient::take_pushed_exit`].

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use super::proto::{
    encode_bytes, ErrorCode, ExitRecord, Op, Outcome, Request, ShimFrame, ShimSpawn, ShimStatus,
    Version,
};
use super::server::ShimStream;

/// Why a client call failed.
#[derive(Debug, thiserror::Error)]
pub enum ShimClientError {
    /// The connection failed or closed.
    #[error("the Shim connection failed: {0}")]
    Io(#[from] std::io::Error),
    /// The Shim refused the request.
    #[error("the Shim refused the request: {}", .0.message())]
    Refused(ErrorCode),
    /// The Shim speaks a protocol version this engine does not.
    #[error("the Shim speaks shim-rpc {}.{}, which this engine does not", .0.major, .0.minor)]
    Version(Version),
    /// The Shim answered something unexpected.
    #[error("the Shim answered an unexpected frame")]
    Unexpected,
}

/// One connected, admitted engine.
pub struct ShimClient {
    writer: Box<dyn ShimStream>,
    reader: BufReader<Box<dyn ShimStream>>,
    next_id: u64,
    pushed: Option<ExitRecord>,
}

impl std::fmt::Debug for ShimClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShimClient")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl ShimClient {
    /// Say hello over `stream` with `secret`: the client and the Shim's
    /// state, or the refusal (`busy`, `secret-mismatch`, `peer-refused`) or
    /// a version this engine does not speak.
    pub fn hello(
        stream: Box<dyn ShimStream>,
        secret: &[u8; 32],
        bound: Duration,
    ) -> Result<(Self, ShimStatus), ShimClientError> {
        stream.set_read_bound(Some(bound))?;
        let reader = BufReader::new(stream.try_clone_stream()?);
        let mut client = ShimClient {
            writer: stream,
            reader,
            next_id: 1,
            pushed: None,
        };
        let outcome = client.call(Op::Hello {
            v: Version::CURRENT,
            secret: super::secret::to_hex(secret),
            engine_pid: std::process::id(),
        })?;
        match outcome {
            Outcome::HelloAck { v, status } if Version::engine_accepts(v) => Ok((client, status)),
            Outcome::HelloAck { v, .. } => Err(ShimClientError::Version(v)),
            _ => Err(ShimClientError::Unexpected),
        }
    }

    /// Send `op` and wait for its reply (keeping any pushed exit). A
    /// refusal is [`ShimClientError::Refused`].
    fn call(&mut self, op: Op) -> Result<Outcome, ShimClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&Request { id, op }).map_err(|_| {
            ShimClientError::Io(std::io::Error::other("the request could not be encoded"))
        })?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        loop {
            let mut text = String::new();
            if self.reader.read_line(&mut text)? == 0 {
                return Err(ShimClientError::Io(
                    std::io::ErrorKind::UnexpectedEof.into(),
                ));
            }
            match serde_json::from_str::<ShimFrame>(text.trim()) {
                Ok(ShimFrame::Exit { exit }) => self.pushed = Some(exit),
                Ok(ShimFrame::Reply {
                    outcome: Outcome::Error { code, .. },
                    ..
                }) => return Err(ShimClientError::Refused(code)),
                Ok(ShimFrame::Reply { id: got, outcome }) if got == id => return Ok(outcome),
                Ok(ShimFrame::Reply { .. }) => {}
                Err(_) => return Err(ShimClientError::Unexpected),
            }
        }
    }

    /// Launch the child: its pid and start-time token.
    pub fn spawn(&mut self, spec: ShimSpawn) -> Result<(u32, u64), ShimClientError> {
        match self.call(Op::Spawn { spec })? {
            Outcome::Spawned { pid, start_time } => Ok((pid, start_time)),
            _ => Err(ShimClientError::Unexpected),
        }
    }

    /// Stop the child within `graceful` (then forced): whether it was forced.
    pub fn stop(&mut self, graceful: Duration) -> Result<bool, ShimClientError> {
        let graceful_ms = u64::try_from(graceful.as_millis()).unwrap_or(u64::MAX);
        match self.call(Op::Stop { graceful_ms })? {
            Outcome::Stopped { forced } => Ok(forced),
            _ => Err(ShimClientError::Unexpected),
        }
    }

    pub fn pause(&mut self) -> Result<(), ShimClientError> {
        self.done(Op::Pause)
    }

    pub fn resume(&mut self) -> Result<(), ShimClientError> {
        self.done(Op::Resume)
    }

    /// The child's exit, when it exited.
    pub fn poll(&mut self) -> Result<Option<ExitRecord>, ShimClientError> {
        match self.call(Op::Poll)? {
            Outcome::Polled { exit } => Ok(exit),
            _ => Err(ShimClientError::Unexpected),
        }
    }

    pub fn write_stdin(&mut self, bytes: &[u8]) -> Result<(), ShimClientError> {
        self.done(Op::WriteStdin {
            data: encode_bytes(bytes),
        })
    }

    pub fn status(&mut self) -> Result<ShimStatus, ShimClientError> {
        match self.call(Op::Status)? {
            Outcome::Status { status } => Ok(status),
            _ => Err(ShimClientError::Unexpected),
        }
    }

    /// Tell the Shim its buffered exit has been taken: it may leave.
    pub fn ack_exit(&mut self) -> Result<(), ShimClientError> {
        self.done(Op::AckExit)
    }

    /// An exit the Shim pushed, once.
    pub fn take_pushed_exit(&mut self) -> Option<ExitRecord> {
        self.pushed.take()
    }

    fn done(&mut self, op: Op) -> Result<(), ShimClientError> {
        match self.call(op)? {
            Outcome::Done => Ok(()),
            _ => Err(ShimClientError::Unexpected),
        }
    }
}
