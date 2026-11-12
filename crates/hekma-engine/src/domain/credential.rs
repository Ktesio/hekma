//! Run Keys and their sibling credentials (story 22-1, spine AD-22).
//!
//! A Run Key (`hk_run_…`) is a Run's own credential: the holder may act on
//! that Run's subtree as its Caller. A refresh credential (`hk_ref_…`) trades
//! itself, once, for a new Run Key and refresh pair; an announce credential
//! (`hk_ann_…`) has its format, class and matrix row here and is issued by a
//! later story.
//!
//! * **Generation.** 32 bytes from the OS CSPRNG (`getrandom`), with NO
//!   fallback: an RNG failure is [`CredentialError::RandomSourceFailed`] and
//!   the start, open or refresh asking for the token is refused before
//!   anything is written. (A Run id falls back to the clock because an id is
//!   not a secret; a token never does.)
//! * **Format.** The class prefix plus the 32 bytes in Crockford base32 (the
//!   Run id alphabet), 52 characters, no padding: 59 bytes in all.
//! * **At rest.** Only the SHA-256 of the whole token string is stored
//!   ([`TokenHash`]); the [`TokenHash::token_id`] (its first 12 hex) is a
//!   display label, never a lookup, join or uniqueness key.
//! * **In memory.** The cleartext lives in a [`SecretString`] from mint to
//!   delivery; every type here that holds one has a redacting `Debug` and no
//!   serde.
//! * **Comparison.** A presented token is looked up by its hash; the matched
//!   entry's 32 bytes are then compared in constant time ([`TokenHash::ct_eq`]).
//!   No code compares token text with `==`.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::secret::SecretString;
use super::usage::{RunId, CROCKFORD};

/// The Run Key prefix.
pub const RUN_KEY_PREFIX: &str = "hk_run_";
/// The refresh credential prefix.
pub const REFRESH_PREFIX: &str = "hk_ref_";
/// The announce credential prefix.
pub const ANNOUNCE_PREFIX: &str = "hk_ann_";
/// The proxy-access credential prefix (a per-binding credential for the
/// Metering Proxy beyond loopback; issued, accepted by nothing yet).
pub const PROXY_ACCESS_PREFIX: &str = "hk_pxy_";
/// The number of Crockford base32 characters after the prefix.
pub const TOKEN_BODY_LEN: usize = 52;
/// The number of hex characters in a token id.
pub const TOKEN_ID_LEN: usize = 12;
/// The schema version stamped on every [`CredentialEvent`] (AD-14).
pub const CREDENTIAL_EVENT_SCHEMA_VERSION: u32 = 1;

/// The random bytes in a token.
const TOKEN_BYTES: usize = 32;
/// Every prefix is the same length.
const PREFIX_LEN: usize = 7;

/// The class of a credential (AD-20's credential classes this story adds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenClass {
    /// A Run's own credential (`hk_run_`).
    RunKey,
    /// A single-use refresh credential (`hk_ref_`).
    Refresh,
    /// An announce credential (`hk_ann_`): format and matrix row only.
    Announce,
    /// A per-binding proxy-access credential (`hk_pxy_`, story 23-1,
    /// the security review's P6-A): issued and stored, accepted by
    /// nothing in this release.
    ProxyAccess,
}

impl TokenClass {
    /// Every class.
    pub const ALL: [TokenClass; 4] = [
        TokenClass::RunKey,
        TokenClass::Refresh,
        TokenClass::Announce,
        TokenClass::ProxyAccess,
    ];

    /// The token prefix.
    pub fn prefix(self) -> &'static str {
        match self {
            TokenClass::RunKey => RUN_KEY_PREFIX,
            TokenClass::Refresh => REFRESH_PREFIX,
            TokenClass::Announce => ANNOUNCE_PREFIX,
            TokenClass::ProxyAccess => PROXY_ACCESS_PREFIX,
        }
    }

    /// The stable wire form (events, documents).
    pub fn as_str(self) -> &'static str {
        match self {
            TokenClass::RunKey => "run-key",
            TokenClass::Refresh => "refresh",
            TokenClass::Announce => "announce",
            TokenClass::ProxyAccess => "proxy-access",
        }
    }

    /// The stored form (`tokens.class`).
    pub(crate) fn store_str(self) -> &'static str {
        match self {
            TokenClass::RunKey => "run",
            TokenClass::Refresh => "refresh",
            TokenClass::Announce => "announce",
            TokenClass::ProxyAccess => "proxy-access",
        }
    }

    /// Parse the stored form.
    pub(crate) fn from_store(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.store_str() == raw)
    }

    /// The class whose prefix `raw` starts with (exact case — the public
    /// structure that routes a presentation before it is hashed).
    pub fn of_prefix(raw: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|class| raw.starts_with(class.prefix()))
    }
}

/// A stored token's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenState {
    /// Honoured.
    Active,
    /// A used refresh credential, kept so a second presentation is detected.
    Retired,
    /// Revoked: never honoured again.
    Revoked,
}

impl TokenState {
    /// The stable wire and stored form.
    pub fn as_str(self) -> &'static str {
        match self {
            TokenState::Active => "active",
            TokenState::Retired => "retired",
            TokenState::Revoked => "revoked",
        }
    }

    /// Parse the stored form.
    pub(crate) fn from_store(raw: &str) -> Option<Self> {
        [TokenState::Active, TokenState::Retired, TokenState::Revoked]
            .into_iter()
            .find(|state| state.as_str() == raw)
    }
}

/// The SHA-256 of a whole token string: the only thing the store keeps, and
/// the snapshot's lookup key. Its `Debug` shows the token id only.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash([u8; 32]);

impl TokenHash {
    /// Hash a token string (the engine's std-only SHA-256 — the one
    /// implementation on the token path).
    pub(crate) fn of(token: &str) -> Self {
        TokenHash(crate::sha256::digest(token.as_bytes()))
    }

    /// The hash of a token string, for an embedder that must name one (a
    /// Host's tests building a [`CredentialMatch`]). Hashing reveals
    /// nothing; the engine looks up only by hash.
    pub fn of_token(token: &str) -> Self {
        Self::of(token)
    }

    /// Rebuild from stored bytes.
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        TokenHash(bytes)
    }

    /// The raw bytes (the store's `BLOB`).
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The token id: the first 12 lowercase hex of the hash. A display label
    /// for records, events and reports — never a lookup, join or uniqueness
    /// key (48 bits collide at Run scale).
    pub fn token_id(&self) -> String {
        #[cfg(test)]
        if let Some(forced) = tests::forced_token_id() {
            return forced;
        }
        self.0[..TOKEN_ID_LEN / 2]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Compare in time independent of where the first difference is.
    pub(crate) fn ct_eq(&self, other: &TokenHash) -> bool {
        let mut diff = 0u8;
        for (left, right) in self.0.iter().zip(other.0.iter()) {
            diff |= left ^ right;
        }
        diff == 0
    }
}

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenHash({})", self.token_id())
    }
}

/// Why a credential could not be minted.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The operating system's random source failed; there is no fallback.
    #[error("the operating system's random source failed ({detail}), so no credential was issued")]
    RandomSourceFailed {
        /// The source's own error text (never a value).
        detail: String,
    },
}

/// A freshly minted token: its class, its cleartext (a [`SecretString`]) and
/// its hash. `Debug` shows the class and token id only; there is no serde.
pub struct MintedToken {
    class: TokenClass,
    secret: SecretString,
    hash: TokenHash,
}

impl MintedToken {
    /// The class.
    pub fn class(&self) -> TokenClass {
        self.class
    }

    /// The cleartext, for its one delivery.
    pub fn secret(&self) -> &SecretString {
        &self.secret
    }

    /// The token id.
    pub fn token_id(&self) -> String {
        self.hash.token_id()
    }

    /// The hash the store keeps.
    #[cfg(test)]
    pub(crate) fn hash(&self) -> TokenHash {
        self.hash
    }
    /// The value-free part a writer job needs (class + hash): the cleartext
    /// never enters a job.
    pub(crate) fn stub(&self) -> TokenStub {
        TokenStub {
            class: self.class,
            hash: self.hash,
        }
    }
}

/// A minted token without its value: what a writer job records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TokenStub {
    pub class: TokenClass,
    pub hash: TokenHash,
}

impl fmt::Debug for MintedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MintedToken")
            .field("class", &self.class)
            .field("token_id", &self.token_id())
            .finish()
    }
}

/// Mint one token of `class`: 32 bytes from the OS CSPRNG, no fallback.
pub(crate) fn mint(class: TokenClass) -> Result<MintedToken, CredentialError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    fill_random(&mut bytes)?;
    let token = format!("{}{}", class.prefix(), encode_body(&bytes));
    let hash = TokenHash::of(&token);
    Ok(MintedToken {
        class,
        secret: SecretString::new(token),
        hash,
    })
}

/// The OS random source, with the test seam that makes it fail.
fn fill_random(bytes: &mut [u8]) -> Result<(), CredentialError> {
    #[cfg(test)]
    if tests::rng_fails() {
        return Err(CredentialError::RandomSourceFailed {
            detail: "test seam: the random source is failing".to_string(),
        });
    }
    getrandom::getrandom(bytes).map_err(|err| CredentialError::RandomSourceFailed {
        detail: err.to_string(),
    })
}

/// Crockford base32 over `bytes` as one big-endian bit string, zero-padded
/// at the end to a whole character (32 bytes → 52 characters).
fn encode_body(bytes: &[u8; TOKEN_BYTES]) -> String {
    let mut out = String::with_capacity(TOKEN_BODY_LEN);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((buffer >> bits) & 0x1F) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(CROCKFORD[((buffer << (5 - bits)) & 0x1F) as usize] as char);
    }
    out
}

/// Whether `byte` is a Crockford base32 character (exact case).
fn is_body_char(byte: u8) -> bool {
    CROCKFORD.contains(&byte)
}

/// Parse a presented bearer as a token: the class prefix, exactly
/// [`TOKEN_BODY_LEN`] Crockford characters, nothing else. Public structure
/// only — the value is wrapped at once and never compared as text.
pub fn parse_presented(raw: &str) -> Option<(TokenClass, SecretString)> {
    let class = TokenClass::of_prefix(raw)?;
    let body = &raw.as_bytes()[PREFIX_LEN..];
    if body.len() != TOKEN_BODY_LEN || !body.iter().all(|byte| is_body_char(*byte)) {
        return None;
    }
    Some((class, SecretString::new(raw)))
}

/// The hash of a parsed presentation (the facade's lookup key).
pub(crate) fn hash_presented(presented: &SecretString) -> TokenHash {
    TokenHash::of(presented.expose_secret())
}

/// How many body characters after a prefix make a URL fragment count as a
/// token: enough to refuse a pasted or truncated token, few enough that no
/// ordinary path segment is caught by accident.
const URL_TOKEN_MIN_BODY: usize = 8;

/// What a URL scan found: whether any token-shaped text is present, and the
/// complete tokens among it (normalised to their canonical case, so an
/// exposed token can be revoked even when a client lowercased it).
#[derive(Default)]
pub struct UrlScan {
    /// A prefix (any case) followed by at least eight token characters.
    pub found: bool,
    /// Every complete token in the text, as a [`SecretString`].
    pub tokens: Vec<SecretString>,
}

impl fmt::Debug for UrlScan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UrlScan")
            .field("found", &self.found)
            .field("tokens", &self.tokens.len())
            .finish()
    }
}

/// Scan URL text (a path or query, raw or percent-decoded) for credentials:
/// a class prefix in any case followed by token characters in any case.
pub fn scan_url_text(text: &str) -> UrlScan {
    let mut scan = UrlScan::default();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + PREFIX_LEN <= bytes.len() {
        let window = &bytes[index..index + PREFIX_LEN];
        let class = TokenClass::ALL
            .into_iter()
            .find(|class| window.eq_ignore_ascii_case(class.prefix().as_bytes()));
        let Some(class) = class else {
            index += 1;
            continue;
        };
        let body_start = index + PREFIX_LEN;
        let body_len = bytes[body_start..]
            .iter()
            .take_while(|byte| is_body_char(byte.to_ascii_uppercase()))
            .count();
        if body_len >= URL_TOKEN_MIN_BODY {
            scan.found = true;
            if body_len >= TOKEN_BODY_LEN {
                let body: String =
                    text[body_start..body_start + TOKEN_BODY_LEN].to_ascii_uppercase();
                scan.tokens
                    .push(SecretString::new(format!("{}{body}", class.prefix())));
            }
        }
        // Advance ONE byte, never past the body: a prefix can begin inside
        // what was just read (`hk_run_hk_run_…`, or a body ending in `HK`
        // that starts `hk_run_…`), and skipping it would let a real token
        // through behind a decoy.
        index += 1;
    }
    scan
}

/// Percent-decode `text` (`%XX` → byte; `+` kept as is), lossily as UTF-8.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Scan a URL's path and query, raw and percent-decoded (decoded twice, so
/// a double-encoded token is caught too).
pub fn scan_url(path_and_query: &str) -> UrlScan {
    let once = percent_decode(path_and_query);
    let twice = percent_decode(&once);
    let mut scan = UrlScan::default();
    for text in [path_and_query, once.as_str(), twice.as_str()] {
        let part = scan_url_text(text);
        scan.found |= part.found;
        for token in part.tokens {
            if !scan.tokens.iter().any(|seen| {
                TokenHash::of(seen.expose_secret()).ct_eq(&TokenHash::of(token.expose_secret()))
            }) {
                scan.tokens.push(token);
            }
        }
    }
    scan
}

/// A Run Key and its paired refresh credential, issued once (a Control API
/// child open, a refresh). `Debug` shows the token ids only; there is no
/// serde — the one-time response serializes through one greppable site.
pub struct IssuedCredentials {
    run_id: RunId,
    run_key: MintedToken,
    refresh: MintedToken,
}

impl IssuedCredentials {
    pub(crate) fn new(run_id: RunId, run_key: MintedToken, refresh: MintedToken) -> Self {
        IssuedCredentials {
            run_id,
            run_key,
            refresh,
        }
    }

    /// The Run the credentials belong to.
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// The Run Key's cleartext.
    pub fn run_key(&self) -> &SecretString {
        self.run_key.secret()
    }

    /// The refresh credential's cleartext.
    pub fn refresh_credential(&self) -> &SecretString {
        self.refresh.secret()
    }

    /// The Run Key's token id.
    pub fn token_id(&self) -> String {
        self.run_key.token_id()
    }

    /// The refresh credential's token id.
    pub fn refresh_token_id(&self) -> String {
        self.refresh.token_id()
    }
}

impl fmt::Debug for IssuedCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCredentials")
            .field("run_id", &self.run_id)
            .field("token_id", &self.token_id())
            .field("refresh_token_id", &self.refresh_token_id())
            .finish()
    }
}

/// A presented credential the published snapshot recognised
/// ([`crate::Engine::authenticate`]). Carries no token; its hash rides as an
/// opaque [`TokenHash`] so a Caller-authorized job can re-validate the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialMatch {
    /// The class.
    pub class: TokenClass,
    /// The Run it belongs to.
    pub run_id: RunId,
    /// Its token id (a label).
    pub token_id: String,
    /// Whether it was issued to a supervised launch (such a key has no
    /// network use and is refused beyond loopback).
    pub supervised: bool,
    /// The hash, for the in-job re-validation.
    pub hash: TokenHash,
}

impl CredentialMatch {
    /// The actor this credential acts as: its Run's Caller.
    pub fn actor(&self) -> crate::domain::runs::RunActor {
        crate::domain::runs::RunActor::Caller {
            run_id: self.run_id.clone(),
            token_hash: self.hash,
        }
    }
}

/// What a presentation the snapshot did not recognise is, read from the
/// store ([`crate::Engine::credential_state`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresentedState {
    /// Nothing was ever issued with this value.
    Unknown,
    /// It was issued and is no longer honoured (revoked).
    Revoked,
    /// A refresh credential that was already traded once (`retired`):
    /// presenting it again is reuse, which the refresh job acts on. Every
    /// other route treats it as revoked.
    Used(CredentialMatch),
    /// An active credential the snapshot does not index (a refresh
    /// credential, or a key whose Run is held elsewhere).
    Active(CredentialMatch),
}

/// One credential of a Run, as `hekma run keys` and `GET /v0/runs/{id}/keys`
/// show it: metadata only, never a token or a hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSummary {
    /// Its token id.
    pub token_id: String,
    /// Its class.
    pub class: TokenClass,
    /// Its state.
    pub state: TokenState,
    /// The launch generation it was issued to (supervised launches only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_generation: Option<u64>,
    /// The token id of the credential it was issued under, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
    /// RFC 3339 issue time.
    pub issued_at: String,
    /// RFC 3339 revocation (or retirement) time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    /// Why it was revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoke_cause: Option<String>,
}

/// One credential a revocation revoked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokedCredential {
    /// The Run it belonged to.
    pub run_id: RunId,
    /// Its class.
    pub class: TokenClass,
    /// Its token id.
    pub token_id: String,
    /// Whether it was revoked by the cascade (it belonged to a credential
    /// issued under one of the Run's own).
    pub cascaded: bool,
}

/// What `revoke-keys` did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeOutcome {
    /// The Run asked for.
    pub run_id: RunId,
    /// Whether the cascade ran.
    pub cascade: bool,
    /// Every credential revoked, the Run's own first.
    pub revoked: Vec<RevokedCredential>,
    /// Whether the `credential-reuse` Anomaly was cleared.
    pub reuse_cleared: bool,
}

/// What happened to a credential (the `change` of a [`CredentialEvent`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialChange {
    /// It was issued.
    Issued,
    /// It was replaced by a refresh (a used refresh credential is retired,
    /// its paired Run Key revoked).
    Rotated,
    /// It was revoked.
    Revoked,
    /// A used refresh credential was presented again.
    ReuseDetected,
}

/// One credential event (AD-14): on the bus, in the outbox and on SSE.
/// Never a token, never a hash — the token id is the label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialEvent {
    /// [`CREDENTIAL_EVENT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// What happened.
    pub change: CredentialChange,
    /// The credential's class.
    pub class: TokenClass,
    /// Its token id.
    pub token_id: String,
    /// Its Run.
    pub run_id: RunId,
    /// The launch generation, for a supervised launch's key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_generation: Option<u64>,
    /// Who issued it: a token id, or `operator`, `host` or `engine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
    /// The token id it replaces, on a refresh's `issued`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_token_id: Option<String>,
    /// Why (`process-exited`, `run-ended`, `operator`, `refreshed`,
    /// `refresh-reuse`, `exposed-in-url`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// RFC 3339 time of the change.
    pub at: String,
}

/// Test-only seams (an RNG failure, a forced token id) and the unit tests.
#[cfg(test)]
pub(crate) mod tests;
