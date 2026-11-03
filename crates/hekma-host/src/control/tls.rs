//! TLS for the operator's `--listen` listener (story 21-4, AD-20: a
//! non-loopback Control API bind requires transport encryption).
//!
//! [`load_server_config`] reads the PEM certificate chain and private key and
//! builds the `rustls` server config BEFORE the host takes its lock (an
//! unreadable file or a bad key is a typed usage refusal naming the path).
//! [`TlsListener`] is an axum [`Listener`](axum::serve::Listener): an accept
//! task hands every TCP connection to its own handshake task (a slow or
//! hostile client never blocks other accepts), and only completed handshakes
//! reach the router. A plain-HTTP request on this listener fails the
//! handshake and is closed — it is never served.
//!
//! The stack is the vendored rustls + ring pair already in the tree through
//! `hyper-rustls` (`tokio-rustls`, `rustls-pki-types` for PEM): no new
//! package, no system TLS (the one-rustls gate stays green).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// How long one client may take to complete its TLS handshake.
const HANDSHAKE_BOUND: Duration = Duration::from_secs(10);

/// How many completed handshakes may wait for the router's accept.
const ACCEPT_QUEUE: usize = 64;

/// Build the TLS server config from a PEM certificate chain and a PEM
/// private key. `Err` names the file at fault (the caller maps it to the
/// usage-class `InvalidOptions`).
pub(crate) fn load_server_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|err| {
            format!(
                "could not read the TLS certificate {}: {err}",
                cert_path.display()
            )
        })?
        .collect::<Result<_, _>>()
        .map_err(|err| {
            format!(
                "the TLS certificate {} is not valid PEM: {err}",
                cert_path.display()
            )
        })?;
    if certs.is_empty() {
        return Err(format!(
            "the TLS certificate {} holds no PEM certificate",
            cert_path.display()
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|err| {
        format!(
            "could not read a PEM private key from {}: {err}",
            key_path.display()
        )
    })?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| format!("could not configure TLS: {err}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| {
            format!(
                "the TLS key {} does not fit the certificate {}: {err}",
                key_path.display(),
                cert_path.display()
            )
        })?;
    Ok(Arc::new(config))
}

/// A TLS listener for axum: completed handshakes only.
pub(crate) struct TlsListener {
    local_addr: SocketAddr,
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    /// Wrap a bound TCP listener. The returned handle is the accept task —
    /// the caller aborts it at shutdown (it holds the socket).
    pub(crate) fn new(
        listener: TcpListener,
        config: Arc<rustls::ServerConfig>,
    ) -> std::io::Result<(Self, JoinHandle<()>)> {
        let local_addr = listener.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, ready) = mpsc::channel(ACCEPT_QUEUE);
        let task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => {
                        // A transient accept error (fd pressure): back off
                        // briefly, the axum `TcpListener` discipline.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    // A failed or overrunning handshake (a plain-HTTP client,
                    // a wrong certificate, a stalled peer) closes that one
                    // connection; nothing reaches the router.
                    if let Ok(Ok(tls)) =
                        tokio::time::timeout(HANDSHAKE_BOUND, acceptor.accept(stream)).await
                    {
                        let _ = tx.send((tls, peer)).await;
                    }
                });
            }
        });
        Ok((TlsListener { local_addr, ready }, task))
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(pair) => pair,
            // The accept task is gone (aborted at shutdown): never resolve —
            // the graceful shutdown ends the serve future instead.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_malformed_files_are_named_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.pem");
        let err = load_server_config(&missing, &missing).unwrap_err();
        assert!(err.contains("nope.pem"), "{err}");

        let garbage = dir.path().join("garbage.pem");
        std::fs::write(&garbage, "not pem at all").unwrap();
        let err = load_server_config(&garbage, &garbage).unwrap_err();
        assert!(err.contains("garbage.pem"), "{err}");
    }

    #[test]
    fn a_matching_pair_builds_and_a_mismatched_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let one = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let two = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        let other_key = dir.path().join("other.pem");
        std::fs::write(&cert, one.cert.pem()).unwrap();
        std::fs::write(&key, one.signing_key.serialize_pem()).unwrap();
        std::fs::write(&other_key, two.signing_key.serialize_pem()).unwrap();
        assert!(load_server_config(&cert, &key).is_ok());
        let err = load_server_config(&cert, &other_key).unwrap_err();
        // The message is not echoed: it names the key file (a test fixture,
        // but the assertion has no need to print it).
        assert!(err.contains("does not fit"), "a mismatched key is refused");
        // A key file in the cert slot holds no certificate.
        let err = load_server_config(&key, &key).unwrap_err();
        assert!(err.contains("no PEM certificate"), "{err}");
    }
}
