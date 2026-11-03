//! Engine notes on the wire (story 21-4): the engine's own diagnostics that
//! fire WHILE a write request runs (a start's "memory.dir is not mapped"
//! note, an adoption or resume note, an enforcement breadcrumb) used to land
//! on the stderr of the CLI command whose in-process engine raised them. Now
//! the engine lives in the host, so the host taps its diagnostic sink: every
//! non-GET request opens a capture, and the lines the engine wrote during it
//! ride back in the `x-hekma-engine-notes` response header (percent-encoded
//! bytes, the engine's own `[hekma] …` lines verbatim). The CLI writes them to
//! its stderr unchanged — the operator sees what they saw before.
//!
//! The capture is by time window, not by causation: a note another request
//! (or the reaper) raised while this one ran rides along too — the same
//! bytes the old in-process engine would have printed during the command.
//! The engine lines also still reach the host's own stream (stderr or
//! `serve.log`), so nothing is lost when no client reads them.

use std::io::Write;
use std::sync::{Arc, Mutex, Weak};

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use hekma_engine::DiagnosticSink;

use super::ApiState;

/// The response header carrying the captured engine notes.
pub const ENGINE_NOTES_HEADER: &str = "x-hekma-engine-notes";

/// The most raw note bytes one capture keeps (the rest stay in the host's
/// log).
const MAX_NOTE_BYTES: usize = 16 * 1024;

/// The most ENCODED header bytes one response carries — well inside every
/// client's response-header limit (`ureq`'s default is 64 KiB) even after
/// percent-encoding expands non-ASCII text.
const MAX_HEADER_BYTES: usize = 16 * 1024;

type Buffer = Arc<Mutex<Vec<u8>>>;

/// The open captures (weak: a finished request's buffer is pruned).
type OpenCaptures = Arc<Mutex<Vec<Weak<Mutex<Vec<u8>>>>>>;

/// The tap over the engine's diagnostic sink.
#[derive(Clone, Default)]
pub(crate) struct NoteTap {
    open: OpenCaptures,
    /// The notes written before the first write request (the engine's open:
    /// adoption, resume and re-bind notes) — kept, in AUTO mode only, for
    /// the first write request, so the command that auto-started the host
    /// sees what its own engine open would have printed. `None` once taken
    /// (or when disabled).
    startup: Arc<Mutex<Option<Vec<u8>>>>,
}

impl NoteTap {
    /// A tap that keeps the startup notes for the first write request (the
    /// auto-started host's: its stderr is `serve.log`, which no operator
    /// watches).
    pub(crate) fn keeping_startup_notes() -> Self {
        NoteTap {
            startup: Arc::new(Mutex::new(Some(Vec::new()))),
            ..NoteTap::default()
        }
    }

    /// Take the kept startup notes (once).
    fn take_startup(&self) -> Vec<u8> {
        self.startup
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_default()
    }

    /// A diagnostic sink that writes through to `base` AND into every open
    /// capture.
    pub(crate) fn sink_over(&self, base: DiagnosticSink) -> DiagnosticSink {
        Arc::new(Mutex::new(Box::new(TeeWriter {
            base,
            tap: self.clone(),
        })))
    }

    /// Open a capture; the bytes written while it is held are its notes.
    fn capture(&self) -> Buffer {
        let buffer: Buffer = Arc::new(Mutex::new(Vec::new()));
        let mut open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        open.retain(|weak| weak.strong_count() > 0);
        open.push(Arc::downgrade(&buffer));
        buffer
    }

    fn record(&self, bytes: &[u8]) {
        if let Some(startup) = self
            .startup
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            if startup.len() < MAX_NOTE_BYTES {
                startup.extend_from_slice(bytes);
            }
        }
        let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
        for weak in open.iter() {
            if let Some(buffer) = weak.upgrade() {
                let mut buffer = buffer.lock().unwrap_or_else(|p| p.into_inner());
                if buffer.len() < MAX_NOTE_BYTES {
                    buffer.extend_from_slice(bytes);
                }
            }
        }
    }
}

struct TeeWriter {
    base: DiagnosticSink,
    tap: NoteTap,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tap.record(buf);
        let mut base = self.base.lock().unwrap_or_else(|p| p.into_inner());
        base.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut base = self.base.lock().unwrap_or_else(|p| p.into_inner());
        base.flush()
    }
}

/// Encode `bytes` for the header, whole lines only, within
/// [`MAX_HEADER_BYTES`]; what does not fit is named in a last line (never a
/// silent cut — the full text is in the host's log).
fn bounded_header(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut taken = 0;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let encoded = encode(line);
        if out.len() + encoded.len() > MAX_HEADER_BYTES {
            break;
        }
        out.push_str(&encoded);
        taken += line.len();
    }
    if taken < bytes.len() {
        out.push_str(&encode(
            format!(
                "[hekma] {} more bytes of engine notes are in the Service Host's log\n",
                bytes.len() - taken
            )
            .as_bytes(),
        ));
    }
    out
}

/// Percent-encode bytes into a header-safe value (unreserved ASCII stays).
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b' ') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Decode [`encode`]'s output (a malformed escape is kept literally).
pub fn decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let pair = &bytes[index + 1..index + 3];
            let hex = std::str::from_utf8(pair).unwrap_or("");
            if let (true, Ok(byte)) = (
                pair.iter().all(u8::is_ascii_hexdigit),
                u8::from_str_radix(hex, 16),
            ) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// The capture layer: every non-GET request's engine notes ride back in the
/// [`ENGINE_NOTES_HEADER`] (success and error responses alike).
pub(crate) async fn capture_engine_notes(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() == Method::GET {
        return next.run(request).await;
    }
    let mut bytes = state.notes.take_startup();
    let buffer = state.notes.capture();
    let mut response = next.run(request).await;
    bytes.extend(std::mem::take(
        &mut *buffer.lock().unwrap_or_else(|p| p.into_inner()),
    ));
    if !bytes.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&bounded_header(&bytes)) {
            response.headers_mut().insert(ENGINE_NOTES_HEADER, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_tap_writes_through_and_captures_only_while_open() {
        let base = Shared::default();
        let tap = NoteTap::default();
        let sink = tap.sink_over(Arc::new(Mutex::new(Box::new(base.clone()))));
        sink.lock().unwrap().write_all(b"[hekma] before\n").unwrap();
        let capture = tap.capture();
        sink.lock().unwrap().write_all(b"[hekma] during\n").unwrap();
        sink.lock().unwrap().flush().unwrap();
        assert_eq!(capture.lock().unwrap().as_slice(), b"[hekma] during\n");
        drop(capture);
        sink.lock().unwrap().write_all(b"[hekma] after\n").unwrap();
        assert_eq!(
            base.0.lock().unwrap().as_slice(),
            b"[hekma] before\n[hekma] during\n[hekma] after\n",
            "every line still reaches the host's own stream"
        );
        // A dropped capture is pruned at the next open.
        let again = tap.capture();
        assert_eq!(tap.open.lock().unwrap().len(), 1);
        drop(again);
    }

    #[test]
    fn startup_notes_are_kept_once_for_the_first_write() {
        let tap = NoteTap::keeping_startup_notes();
        tap.record(b"[hekma] adopted\n");
        let capture = tap.capture();
        tap.record(b"[hekma] during\n");
        assert_eq!(tap.take_startup(), b"[hekma] adopted\n[hekma] during\n");
        assert!(tap.take_startup().is_empty(), "taken once");
        tap.record(b"[hekma] later\n");
        assert!(tap.take_startup().is_empty(), "no longer collecting");
        assert_eq!(
            capture.lock().unwrap().as_slice(),
            b"[hekma] during\n[hekma] later\n"
        );
        // The default tap keeps nothing.
        let plain = NoteTap::default();
        plain.record(b"x");
        assert!(plain.take_startup().is_empty());
    }

    #[test]
    fn the_capture_is_bounded() {
        let tap = NoteTap::default();
        let capture = tap.capture();
        let line = vec![b'x'; 1024];
        for _ in 0..64 {
            tap.record(&line);
        }
        let len = capture.lock().unwrap().len();
        assert!(
            (MAX_NOTE_BYTES..MAX_NOTE_BYTES + 1024).contains(&len),
            "{len}"
        );
    }

    #[test]
    fn the_header_is_bounded_at_a_line_and_names_the_rest() {
        let line = format!("[hekma] {}\n", "—".repeat(100));
        let many = line.repeat(200);
        let header = bounded_header(many.as_bytes());
        assert!(header.len() <= MAX_HEADER_BYTES + 200, "{}", header.len());
        let text = String::from_utf8(decode(&header)).unwrap();
        assert!(text.ends_with("of engine notes are in the Service Host's log\n"));
        assert!(text.starts_with(&line), "whole lines only");
        // A small note passes whole.
        assert_eq!(decode(&bounded_header(b"[hekma] x\n")), b"[hekma] x\n");
    }

    #[test]
    fn the_header_encoding_round_trips_any_bytes() {
        let text = "[hekma] memory.dir — is not mapped; 100% sure\n\ttab \u{1F600}\n";
        let encoded = encode(text.as_bytes());
        assert!(HeaderValue::from_str(&encoded).is_ok(), "{encoded}");
        assert_eq!(decode(&encoded), text.as_bytes());
        // A malformed escape is kept literally, never a panic.
        assert_eq!(decode("50%"), b"50%");
        assert_eq!(decode("%zz"), b"%zz");
        assert_eq!(decode("%4"), b"%4");
        assert_eq!(decode("%+1"), b"%+1");
    }
}
