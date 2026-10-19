//! The host's shutdown-signal story (story 17-2, AD-20: the HOST owns process
//! signals — the engine still may not), reworked by the review findings:
//!
//! * **Armed BEFORE the engine open** (review finding 1): registration is a
//!   separate, awaited step ([`arm`]) — a SIGTERM arriving while the engine
//!   opens (adoption can take real time) is CAPTURED by the already-live
//!   streams and held on a watch, never delivered to the default
//!   disposition, and the drain still runs once the open settles.
//! * **Registration failure is a typed error** (review finding 2): [`arm`]
//!   returns `Err` with the surfaced reason — mapped to
//!   [`ServeError::Startup`](crate::ServeError) by the caller — never a
//!   `.expect` panic inside the operator's long-running process.
//! * **Repeats during the drain are swallowed and logged** (review finding
//!   3): once the first signal resolves the wait, the streams STAY alive in
//!   a detached holder task for the rest of the process; every further
//!   signal is noted through the sink (one honest line) instead of killing
//!   the host mid-ladder — a drain can legitimately run for minutes (N
//!   agents × escalation windows), and a service manager's second TERM must
//!   not turn a graceful drain into an abrupt death.
//!
//! * Unix: SIGTERM (the service-manager contract) and SIGINT (the foreground
//!   Ctrl+C equivalent), first-come — a `select!` over two tokio signal
//!   streams.
//! * Windows: the console Ctrl+C handler stream (there is no SIGTERM — the
//!   documented OS asymmetry, matching the engine's own signal story).
//!
//! This file is the crate's ONLY OS-conditional code (the OS-cfg gate's
//! allowlist names it); every other module is cfg-free.

use hekma_engine::DiagnosticSink;

/// The armed shutdown-signal source: the streams are registered, the first
/// signal's name resolves through [`ArmedSignals::received`], and further
/// signals are swallowed (and noted) by the detached holder task for the
/// rest of the process. Dropping the struct does NOT stop the holder (tokio
/// tasks are detached when their handle drops) — the streams stay armed
/// until process exit, which is the point: no window where a signal reverts
/// to the default disposition.
pub(crate) struct ArmedSignals {
    /// Resolves to the first signal's NAME (`Some`) exactly once; `None`
    /// only before the first signal (the channel's initial value).
    pub(crate) received: tokio::sync::watch::Receiver<Option<&'static str>>,
}

/// Register the host's shutdown-signal streams. MUST be called INSIDE the
/// host's runtime (the streams bind to its reactor) and BEFORE the engine
/// opens (review finding 1). Registration failure is the surfaced `Err`
/// (review finding 2) — the caller maps it to a typed
/// [`ServeError::Startup`](crate::ServeError), it is never a panic.
///
/// The returned watch resolves when the first signal arrives; repeats are
/// swallowed and noted through `sink` by the detached holder task (review
/// finding 3).
pub(crate) async fn arm(sink: DiagnosticSink) -> Result<ArmedSignals, String> {
    let (tx, rx) = tokio::sync::watch::channel(None);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())
            .map_err(|err| format!("could not register the SIGTERM stream: {err}"))?;
        let mut interrupt = signal(SignalKind::interrupt())
            .map_err(|err| format!("could not register the SIGINT stream: {err}"))?;
        tokio::spawn(async move {
            let name = tokio::select! {
                _ = terminate.recv() => "SIGTERM",
                _ = interrupt.recv() => "SIGINT",
            };
            let _ = tx.send(Some(name));
            // Review finding 3: the streams live on for the WHOLE drain (and
            // the process). Every further signal is swallowed here — noted,
            // never lethal — because a second TERM from a service manager or
            // an impatient operator must not kill the host mid-ladder.
            loop {
                let repeat = tokio::select! {
                    _ = terminate.recv() => "SIGTERM",
                    _ = interrupt.recv() => "SIGINT",
                };
                note_repeat(&sink, repeat);
            }
        });
        Ok(ArmedSignals { received: rx })
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::ctrl_c;
        // Eager registration (the stream constructor, not the event future):
        // a failure surfaces HERE, typed — never a panic.
        let mut stream =
            ctrl_c().map_err(|err| format!("could not register the Ctrl+C handler: {err}"))?;
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                let _ = tx.send(Some("Ctrl+C"));
            }
            // Review finding 3, Windows half: hold the handler for the whole
            // drain; every further Ctrl+C is swallowed (noted, never lethal).
            loop {
                if stream.recv().await.is_some() {
                    note_repeat(&sink, "Ctrl+C");
                } else {
                    // The stream ended (never observed in practice); back off
                    // and re-register — dying here would be the exact outcome
                    // the swallow exists to prevent.
                    match ctrl_c() {
                        Ok(mut renewed) => {
                            if renewed.recv().await.is_some() {
                                note_repeat(&sink, "Ctrl+C");
                            }
                        }
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        }
                    }
                }
            }
        });
        Ok(ArmedSignals { received: rx })
    }
}

fn note_repeat(sink: &DiagnosticSink, name: &'static str) {
    // Via the Notifier so the line is byte-identical with every other host
    // notice (`[hekma] `-prefixed, one line, best-effort).
    super::Notifier::over(sink.clone()).note(&format!(
        "a further {name} arrived during the drain; swallowed — one drain is \
         already running and will complete"
    ));
}

/// The REAL OS-signal source, in the factory shape [`crate::serve`] hands to
/// [`crate::serve_with_signal`]: arm the streams (registration errors are
/// the surfaced `Err`), then resolve to the first signal's name. The holder
/// task inside [`arm`] keeps the streams armed through the drain regardless
/// of this future's resolution.
pub(crate) async fn wait_for_signal(sink: DiagnosticSink) -> Result<&'static str, String> {
    let armed = arm(sink).await?;
    let mut received = armed.received;
    received
        .changed()
        .await
        .map_err(|err| format!("the shutdown-signal wait ended unexpectedly: {err}"))?;
    let current = *received.borrow_and_update();
    match current {
        Some(name) => Ok(name),
        // The holder sent the initial `None` — only possible if the holder
        // task died before any signal; surfaced, never guessed.
        None => Err("the shutdown-signal wait produced no signal".to_string()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    /// A `Write` adapter into a shared buffer (the SharedSink shape) so the
    /// swallow-loop's repeat notes are observable.
    #[derive(Clone)]
    struct SharedCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The wait's registration half is exercised on the real OS by the serve
    /// CLI end-to-end test (a delivered SIGTERM resolves the wait);
    /// in-process, deliver a real SIGTERM to THIS test process and prove
    /// (a) the wait resolves to its name — no mock, the actual kernel path;
    /// (b) a SECOND SIGTERM during the "drain" is swallowed with a sink
    /// note, not a default-disposition death (review finding 3).
    #[tokio::test]
    async fn a_delivered_sigterm_resolves_and_repeats_are_swallowed_with_a_note() {
        let shared = Arc::new(Mutex::new(Vec::new()));
        let sink: DiagnosticSink =
            Arc::new(Mutex::new(Box::new(SharedCapture(Arc::clone(&shared)))));
        // Arm FIRST (the streams install their handlers), then deliver.
        let armed = super::arm(sink).await.expect("arm");
        let mut received = armed.received.clone();
        // Give the registration a beat to land before the signal.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let deliver = |flag: &str| {
            Command::new("kill")
                .args([flag, &std::process::id().to_string()])
                .status()
                .expect("kill shell-out")
                .success()
        };
        assert!(deliver("-TERM"), "the first kill shell-out must succeed");
        tokio::time::timeout(std::time::Duration::from_secs(10), received.changed())
            .await
            .expect("the wait must resolve after the signal")
            .expect("the watch must not fail");
        assert_eq!(*received.borrow_and_update(), Some("SIGTERM"));
        // The "drain" window: a second TERM must be swallowed (the holder
        // loop notes it; the process obviously survives — this assert runs).
        assert!(deliver("-TERM"), "the second kill shell-out must succeed");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let text = String::from_utf8(shared.lock().unwrap().clone()).unwrap();
            if text.contains("a further SIGTERM arrived during the drain") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the repeat was never noted; sink so far:\n{text}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// `wait_for_signal` (the real factory) resolves to the delivered
    /// signal's name through the same kernel path — the seam `serve` hands
    /// to `serve_with_signal` end-to-end, minus the engine.
    #[tokio::test]
    async fn the_real_factory_resolves_to_the_delivered_signal_name() {
        let sink: DiagnosticSink = Arc::new(Mutex::new(Box::new(SharedCapture(Arc::new(
            Mutex::new(Vec::new()),
        )))));
        let wait = tokio::spawn(super::wait_for_signal(sink));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("kill shell-out")
            .success());
        let name = tokio::time::timeout(std::time::Duration::from_secs(10), wait)
            .await
            .expect("the factory must resolve after the signal")
            .expect("the factory task must not panic")
            .expect("registration succeeded, so the output is Ok");
        assert_eq!(name, "SIGTERM");
    }
}
