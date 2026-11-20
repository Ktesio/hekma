//! Pending Run sets and their set tokens (story 21-6, spine AD-20's "act on
//! a set token", PRD FR-50).
//!
//! `POST /v0/runs/select` resolves a Selector ONCE and mints a token for the
//! exact Run ids it matched; a set-form verb route (`POST /v0/runs/stop`,
//! `PATCH /v0/runs/labels`, `POST /v0/runs/limits`, …) takes the token in
//! its BODY and acts on exactly those ids. The token is a correlation handle
//! for the credential that minted it — the bearer still authenticates every
//! call — and it follows AD-22's hygiene anyway:
//!
//! * 256 bits from the OS CSPRNG, rendered `hkset_` + Crockford base32;
//! * stored only as its SHA-256 (never the value), beside the SHA-256 of
//!   the creating credential — a token presented by another credential is
//!   `set-token-foreign` (403), so a token leaked into a log is useless to
//!   another client;
//! * never in a URL (it rides the request body; URLs land in logs);
//! * single-use: one act consumes it (`set-token-used`, 409, after) — a
//!   retry after a partial failure re-selects, because the set may have
//!   changed;
//! * expiring after [`SET_TOKEN_TTL`] (`set-token-expired`, 409), lazily —
//!   checked when presented, never by a timer;
//! * at most [`MAX_PENDING_SETS`] remembered, the oldest evicted.
//!
//! Pending sets live in host MEMORY only: a set is a conversation, not
//! state. A host restart forgets them (`set-token-not-found`, 404 — the
//! client re-selects), which is also why nothing reaches `state.db`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring::rand::SecureRandom;

/// How long a pending set may be acted on.
pub(crate) const SET_TOKEN_TTL: Duration = Duration::from_secs(10 * 60);

/// How many sets the host remembers at once (pending and consumed); the
/// oldest is evicted past this.
pub(crate) const MAX_PENDING_SETS: usize = 64;

/// The token's prefix.
pub(crate) const SET_TOKEN_PREFIX: &str = "hkset_";

/// Crockford's base32 alphabet (no I, L, O, U).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Why a presented token cannot be acted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetTokenError {
    /// No pending set has this token (never minted, evicted, or minted by a
    /// host that has since restarted).
    NotFound,
    /// The set's lifetime ran out.
    Expired,
    /// The set was already acted on.
    Used,
    /// The token was minted for another credential.
    Foreign,
}

impl SetTokenError {
    /// The stable `error.code`.
    pub(crate) fn code(self) -> &'static str {
        match self {
            SetTokenError::NotFound => "set-token-not-found",
            SetTokenError::Expired => "set-token-expired",
            SetTokenError::Used => "set-token-used",
            SetTokenError::Foreign => "set-token-foreign",
        }
    }

    /// The human message (never echoes the token).
    pub(crate) fn message(self) -> &'static str {
        match self {
            SetTokenError::NotFound => {
                "no pending Run set has this set token — it was never issued, was evicted, or \
                 was issued before the Service Host restarted; select the Runs again with \
                 POST /v0/runs/select"
            }
            SetTokenError::Expired => {
                "this set token has expired (a selected set may be acted on for 10 minutes); \
                 select the Runs again with POST /v0/runs/select"
            }
            SetTokenError::Used => {
                "this set token was already used — one act consumes it; select the Runs again \
                 with POST /v0/runs/select (the set may have changed)"
            }
            SetTokenError::Foreign => {
                "this set token was issued to another credential; select the Runs with your own"
            }
        }
    }
}

/// One remembered set.
struct Pending {
    token_hash: [u8; 32],
    creator_hash: [u8; 32],
    run_ids: Vec<String>,
    expires: Instant,
    used: bool,
}

/// A freshly minted set token.
pub(crate) struct MintedSet {
    /// The token value (returned to the client once, never stored).
    pub token: String,
    /// When it expires, RFC 3339 UTC.
    pub expires_at: String,
}

/// The clock the store reads (injected by tests).
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The host's pending-set store (shared by every listener).
#[derive(Clone)]
pub(crate) struct SetStore {
    sets: Arc<Mutex<Vec<Pending>>>,
    ttl: Duration,
    cap: usize,
    clock: Clock,
}

impl std::fmt::Debug for SetStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never a hash, never a token — the count is the only fact.
        let count = self.sets.lock().map(|sets| sets.len()).unwrap_or(0);
        write!(f, "SetStore({count} sets)")
    }
}

impl SetStore {
    /// The production store: [`SET_TOKEN_TTL`], [`MAX_PENDING_SETS`], the
    /// monotonic clock. `ttl` overrides the lifetime (the serve options'
    /// test seam).
    pub(crate) fn new(ttl: Option<Duration>) -> Self {
        Self::with_clock(
            ttl.unwrap_or(SET_TOKEN_TTL),
            MAX_PENDING_SETS,
            Arc::new(Instant::now),
        )
    }

    /// A store with an injected lifetime, capacity and clock (tests).
    pub(crate) fn with_clock(ttl: Duration, cap: usize, clock: Clock) -> Self {
        Self {
            sets: Arc::new(Mutex::new(Vec::new())),
            ttl,
            cap: cap.max(1),
            clock,
        }
    }

    /// Mint a token for `run_ids`, bound to `creator` (the creating
    /// credential's SHA-256). Evicts the oldest set past the capacity.
    pub(crate) fn mint(
        &self,
        creator: [u8; 32],
        run_ids: Vec<String>,
    ) -> Result<MintedSet, String> {
        let mut bytes = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "the operating system's random source failed".to_string())?;
        let token = format!("{SET_TOKEN_PREFIX}{}", crockford(&bytes));
        let now = (self.clock)();
        let expires = now + self.ttl;
        let expires_at = rfc3339_after(self.ttl);
        let mut sets = self
            .sets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while sets.len() >= self.cap {
            sets.remove(0);
        }
        sets.push(Pending {
            token_hash: sha256(token.as_bytes()),
            creator_hash: creator,
            run_ids,
            expires,
            used: false,
        });
        Ok(MintedSet { token, expires_at })
    }

    /// Consume the set `token` names for `presenter` (the presenting
    /// credential's SHA-256): its Run ids, or why it cannot be acted on. A
    /// foreign credential learns nothing about the set's state.
    pub(crate) fn consume(
        &self,
        token: &str,
        presenter: [u8; 32],
    ) -> Result<Vec<String>, SetTokenError> {
        let hash = sha256(token.as_bytes());
        let now = (self.clock)();
        let mut sets = self
            .sets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(set) = sets
            .iter_mut()
            .find(|set| super::auth::constant_time_eq(&set.token_hash, &hash))
        else {
            return Err(SetTokenError::NotFound);
        };
        if !super::auth::constant_time_eq(&set.creator_hash, &presenter) {
            return Err(SetTokenError::Foreign);
        }
        if set.used {
            return Err(SetTokenError::Used);
        }
        if now >= set.expires {
            return Err(SetTokenError::Expired);
        }
        set.used = true;
        Ok(std::mem::take(&mut set.run_ids))
    }
}

/// Crockford base32 of `bytes` (no padding).
fn crockford(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(CROCKFORD[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// The SHA-256 of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Now + `ttl` as RFC 3339 UTC (seconds).
fn rfc3339_after(ttl: Duration) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let at = now.as_secs().saturating_add(ttl.as_secs());
    let days = at / 86_400;
    let secs = at % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's days-to-civil (proleptic Gregorian, UTC).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clocked(ttl: Duration, cap: usize) -> (SetStore, Arc<AtomicU64>) {
        let origin = Instant::now();
        let offset = Arc::new(AtomicU64::new(0));
        let reader = Arc::clone(&offset);
        let clock: Clock =
            Arc::new(move || origin + Duration::from_millis(reader.load(Ordering::SeqCst)));
        (SetStore::with_clock(ttl, cap, clock), offset)
    }

    const ME: [u8; 32] = [1; 32];
    const OTHER: [u8; 32] = [2; 32];

    #[test]
    fn a_token_is_prefixed_crockford_256_bits_and_never_kept() {
        let (store, _) = clocked(SET_TOKEN_TTL, MAX_PENDING_SETS);
        let minted = store.mint(ME, vec!["run_A".into()]).unwrap();
        assert!(minted.token.starts_with(SET_TOKEN_PREFIX));
        let body = &minted.token[SET_TOKEN_PREFIX.len()..];
        assert_eq!(body.len(), 52, "256 bits in base32 is 52 characters");
        assert!(body.bytes().all(|b| CROCKFORD.contains(&b)), "{body}");
        let other = store.mint(ME, vec![]).unwrap();
        assert_ne!(minted.token, other.token);
        assert!(!format!("{store:?}").contains(&minted.token));
        assert!(minted.expires_at.ends_with('Z') && minted.expires_at.len() == 20);
    }

    #[test]
    fn a_set_is_single_use_bound_to_its_creator_and_expires_lazily() {
        let (store, clock) = clocked(Duration::from_secs(600), MAX_PENDING_SETS);
        let minted = store
            .mint(ME, vec!["run_A".into(), "run_B".into()])
            .unwrap();
        // A foreign credential learns nothing (and does not consume it).
        assert_eq!(
            store.consume(&minted.token, OTHER),
            Err(SetTokenError::Foreign)
        );
        assert_eq!(
            store.consume(&minted.token, ME).unwrap(),
            vec!["run_A".to_string(), "run_B".to_string()]
        );
        assert_eq!(store.consume(&minted.token, ME), Err(SetTokenError::Used));
        assert_eq!(
            store.consume("hkset_nope", ME),
            Err(SetTokenError::NotFound)
        );
        // Expiry: no timer, checked when presented.
        let late = store.mint(ME, vec!["run_C".into()]).unwrap();
        clock.store(600_000, Ordering::SeqCst);
        assert_eq!(store.consume(&late.token, ME), Err(SetTokenError::Expired));
        for error in [
            SetTokenError::NotFound,
            SetTokenError::Expired,
            SetTokenError::Used,
            SetTokenError::Foreign,
        ] {
            assert!(error.code().starts_with("set-token-"));
            assert!(!error.message().is_empty());
        }
    }

    #[test]
    fn the_oldest_set_is_evicted_past_the_cap() {
        let (store, _) = clocked(SET_TOKEN_TTL, 2);
        let first = store.mint(ME, vec!["a".into()]).unwrap();
        let second = store.mint(ME, vec!["b".into()]).unwrap();
        let third = store.mint(ME, vec!["c".into()]).unwrap();
        assert_eq!(
            store.consume(&first.token, ME),
            Err(SetTokenError::NotFound)
        );
        assert_eq!(
            store.consume(&second.token, ME).unwrap(),
            vec!["b".to_string()]
        );
        assert_eq!(
            store.consume(&third.token, ME).unwrap(),
            vec!["c".to_string()]
        );
    }

    #[test]
    fn the_production_store_and_the_calendar_render() {
        let store = SetStore::new(None);
        assert_eq!(store.ttl, SET_TOKEN_TTL);
        assert_eq!(store.cap, MAX_PENDING_SETS);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_406), (2025, 11, 14));
        assert_eq!(crockford(&[0xff]), "ZW");
    }
}
