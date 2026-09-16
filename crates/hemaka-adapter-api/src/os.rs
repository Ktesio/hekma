//! [`OsId`] — the operating-system identifier, resolved as **data** (spine AD-4).
//!
//! ## Per-OS-as-data rule (CRITICAL — do NOT reach for conditional compilation)
//!
//! Capability Declarations are keyed by operating system (capability × OS →
//! support level, AD-4). The engine surfaces the *effective* declaration for the
//! running OS. The mechanism is a **runtime** identifier — [`OsId::current`]
//! reads [`std::env::consts::OS`] (a plain `&str` known at runtime) and maps it
//! to a variant. It never uses a compile-time platform attribute.
//!
//! This is a deliberate architectural choice with two payoffs:
//!
//! 1. The OS-conditional-compilation CI gate (which forbids platform attributes
//!    outside the engine's `backends/` home) stays green — there is nothing for
//!    it to flag here.
//! 2. Per-OS behavior becomes **unit-testable on every CI runner**: a test can
//!    drive [`OsId::Linux`], [`OsId::Macos`], and [`OsId::Windows`] as data
//!    regardless of the host it runs on, instead of only exercising the matching
//!    platform's branch.

use serde::{Deserialize, Serialize};

/// An operating-system identifier used to key per-OS Capability Declarations.
///
/// Resolved from [`std::env::consts::OS`] at runtime (see [`OsId::current`]), so
/// no compile-time platform selection is involved. The [`OsId::Other`] catch-all
/// keeps the type total: an OS this build was not written for still resolves to a
/// value (declarations simply have no entry for it, projecting to
/// [`SupportLevel::Unsupported`](crate::SupportLevel)).
///
/// The serde wire form is lowercase (`linux`, `macos`, `windows`, `other`) so an
/// `adapter.toml` `[capabilities]` table can key support levels by OS name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsId {
    /// Linux (`std::env::consts::OS == "linux"`).
    Linux,
    /// macOS (`std::env::consts::OS == "macos"`).
    Macos,
    /// Windows (`std::env::consts::OS == "windows"`).
    Windows,
    /// Any other operating system this build does not model explicitly.
    Other,
}

impl OsId {
    /// The three operating systems Ktesio models per-OS support for.
    ///
    /// [`OsId::Other`] is intentionally excluded: it is the catch-all for
    /// unmodeled systems, not a target a declaration keys support against.
    /// Tests iterate this to drive per-OS projection as data on any host.
    pub const MODELED: [OsId; 3] = [OsId::Linux, OsId::Macos, OsId::Windows];

    /// Resolve the running operating system from [`std::env::consts::OS`].
    ///
    /// `std::env::consts::OS` is a runtime string constant describing the target
    /// the binary was built for; matching on it keeps this resolution free of
    /// any compile-time platform selection (the per-OS-as-data rule).
    pub fn current() -> Self {
        Self::from_os_str(std::env::consts::OS)
    }

    /// Map a `std::env::consts::OS`-style string to an [`OsId`].
    ///
    /// Separated from [`OsId::current`] so it can be unit-tested with every
    /// possible input on any host (the whole point of modeling the OS as data).
    pub fn from_os_str(os: &str) -> Self {
        match os {
            "linux" => OsId::Linux,
            "macos" => OsId::Macos,
            "windows" => OsId::Windows,
            _ => OsId::Other,
        }
    }

    /// The lowercase wire name, matching the serde form and the manifest keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            OsId::Linux => "linux",
            OsId::Macos => "macos",
            OsId::Windows => "windows",
            OsId::Other => "other",
        }
    }
}

impl std::fmt::Display for OsId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_os_str_maps_known_targets() {
        assert_eq!(OsId::from_os_str("linux"), OsId::Linux);
        assert_eq!(OsId::from_os_str("macos"), OsId::Macos);
        assert_eq!(OsId::from_os_str("windows"), OsId::Windows);
    }

    #[test]
    fn from_os_str_maps_unknown_to_other() {
        assert_eq!(OsId::from_os_str("freebsd"), OsId::Other);
        assert_eq!(OsId::from_os_str(""), OsId::Other);
        assert_eq!(OsId::from_os_str("Linux"), OsId::Other); // case-sensitive
    }

    #[test]
    fn current_returns_a_modeled_os_on_supported_hosts() {
        // On every host CI runs (Linux, macOS, Windows) current() must be one of
        // the modeled variants — proving the runtime resolution works without
        // any compile-time platform selection.
        let current = OsId::current();
        assert!(
            OsId::MODELED.contains(&current),
            "unexpected host OS id: {current}"
        );
    }

    #[test]
    fn as_str_and_display_agree_and_round_trip_serde() {
        for os in [OsId::Linux, OsId::Macos, OsId::Windows, OsId::Other] {
            assert_eq!(os.to_string(), os.as_str());
            let json = serde_json::to_string(&os).expect("serialize");
            assert_eq!(json, format!("\"{}\"", os.as_str()));
            let back: OsId = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, os);
        }
    }

    #[test]
    fn modeled_excludes_other() {
        assert!(!OsId::MODELED.contains(&OsId::Other));
        assert_eq!(OsId::MODELED.len(), 3);
    }
}
