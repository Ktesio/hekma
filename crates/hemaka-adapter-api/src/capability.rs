//! Capability Declaration types (spine AD-4): capability × OS → support level.
//!
//! A [`CapabilityDeclaration`] records, for each declared [`Capability`], the
//! [`SupportLevel`] on each [`OsId`]. The engine surfaces the **effective**
//! declaration — the projection onto the running OS via
//! [`CapabilityDeclaration::effective`] — everywhere capabilities are shown.
//!
//! This is data, not conditional compilation: the per-OS matrix is stored and
//! projected at runtime, so it is unit-testable for every OS on any host.
//!
//! ## Scope this story
//!
//! This story SEEDS the capability set (`pause`, `interaction`) and the support
//! classification. The full set freezes with the Adapter Contract (epic 6). Keep
//! the type additively extensible; do not build negotiation or a frozen set.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::os::OsId;

/// How well a capability is supported on a given OS (spine AD-4 classification).
///
/// The serde wire form is kebab-case (`guaranteed`, `best-effort`,
/// `unsupported`) so an `adapter.toml` can spell the levels naturally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportLevel {
    /// The capability works reliably on this OS.
    Guaranteed,
    /// The capability is attempted but may be approximate or unreliable.
    BestEffort,
    /// The capability is not available on this OS.
    Unsupported,
}

impl SupportLevel {
    /// The kebab-case wire name, matching the serde form.
    pub fn as_str(&self) -> &'static str {
        match self {
            SupportLevel::Guaranteed => "guaranteed",
            SupportLevel::BestEffort => "best-effort",
            SupportLevel::Unsupported => "unsupported",
        }
    }
}

impl std::fmt::Display for SupportLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declarable capability key (spine AD-4 exemplars; seed set this story).
///
/// `[ASSUMPTION]` — the exact key set is a seed and freezes with the Adapter
/// Contract (epic 6). Two keys are modeled now: [`Capability::Pause`] (the AD-4
/// per-OS exemplar: guaranteed on Unix via SIGSTOP, best-effort on Windows) and
/// [`Capability::Interaction`] (stdin/stdout wiring). The serde wire form is
/// lowercase so `[capabilities.pause]` / `[capabilities.interaction]` tables key
/// naturally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Suspend/resume the agent's execution (AD-4 per-OS exemplar).
    Pause,
    /// Send input to / read output from the agent (interaction wiring).
    Interaction,
}

impl Capability {
    /// Every capability key in the seed set (used to enumerate declarations).
    pub const ALL: [Capability; 2] = [Capability::Pause, Capability::Interaction];

    /// The lowercase wire name, matching the serde form and manifest keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::Pause => "pause",
            Capability::Interaction => "interaction",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Capability Declaration: per-capability, per-OS support levels (AD-4).
///
/// Stored as an ordered map from [`Capability`] to a per-[`OsId`] map of
/// [`SupportLevel`], keeping serialization deterministic (ordered keys). A
/// capability with no entry for an OS projects to [`SupportLevel::Unsupported`]
/// — absence means "not supported there", never an error.
///
/// The manifest shape this mirrors:
///
/// ```toml
/// [capabilities.pause]
/// linux = "guaranteed"
/// macos = "guaranteed"
/// windows = "best-effort"
///
/// [capabilities.interaction]
/// linux = "guaranteed"
/// macos = "guaranteed"
/// windows = "guaranteed"
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityDeclaration {
    /// capability → (os → support level). Ordered for deterministic output.
    by_capability: BTreeMap<Capability, BTreeMap<OsId, SupportLevel>>,
}

impl CapabilityDeclaration {
    /// An empty declaration (no capabilities). Rejected at registration — an
    /// adapter must declare at least one capability (AC2).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `level` for `capability` on `os`, returning `self` for chaining.
    ///
    /// Used by native adapters (e.g. the conformance mock) to build a
    /// declaration in code; the manifest path deserializes straight into the
    /// same shape.
    pub fn with(mut self, capability: Capability, os: OsId, level: SupportLevel) -> Self {
        self.by_capability
            .entry(capability)
            .or_default()
            .insert(os, level);
        self
    }

    /// `true` when no capability is declared (a necessary but insufficient
    /// rejection predicate — see [`Self::has_any_support`] for the full bar).
    pub fn is_empty(&self) -> bool {
        self.by_capability.is_empty()
    }

    /// `true` when at least one capability declares at least one OS at a level
    /// other than [`SupportLevel::Unsupported`] (the AC2 registration bar).
    ///
    /// [`Self::is_empty`] only catches a declaration with zero capability keys;
    /// it passes a declaration whose keys all have an empty per-OS map, or whose
    /// every (OS → level) entry is [`SupportLevel::Unsupported`] — neither of
    /// which actually promises support anywhere. This predicate is stricter: an
    /// adapter that supports *nothing on any OS* is not a viable adapter and is
    /// rejected at registration, uniformly for manifest and native adapters.
    pub fn has_any_support(&self) -> bool {
        self.by_capability.values().any(|per_os| {
            per_os
                .values()
                .any(|&level| level != SupportLevel::Unsupported)
        })
    }

    /// The number of declared capabilities.
    pub fn len(&self) -> usize {
        self.by_capability.len()
    }

    /// The declared capabilities, in deterministic order.
    pub fn capabilities(&self) -> impl Iterator<Item = Capability> + '_ {
        self.by_capability.keys().copied()
    }

    /// The support level for `capability` on `os`.
    ///
    /// Absence (capability not declared, or declared but with no entry for this
    /// OS) is [`SupportLevel::Unsupported`] — the honest default.
    pub fn support(&self, capability: Capability, os: OsId) -> SupportLevel {
        self.by_capability
            .get(&capability)
            .and_then(|per_os| per_os.get(&os).copied())
            .unwrap_or(SupportLevel::Unsupported)
    }

    /// Project the whole declaration onto a single OS (spine AD-4 "effective").
    ///
    /// Returns the current-OS view the engine persists and `kt` renders: every
    /// declared capability paired with its support level on `os`.
    pub fn effective(&self, os: OsId) -> EffectiveCapabilities {
        let entries = self
            .by_capability
            .keys()
            .map(|&capability| (capability, self.support(capability, os)))
            .collect();
        EffectiveCapabilities { os, entries }
    }
}

/// The Capability Declaration projected onto one OS (AD-4 "effective").
///
/// This is what the engine persists with an Agent Instance and what `kt`
/// renders. It is a flat list of (capability, support-level) for a single
/// [`OsId`], deterministically ordered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveCapabilities {
    /// The OS this projection was taken for.
    pub os: OsId,
    /// Declared capabilities and their support level on [`EffectiveCapabilities::os`].
    pub entries: Vec<(Capability, SupportLevel)>,
}

impl EffectiveCapabilities {
    /// `true` when no capability is present in the projection.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CapabilityDeclaration {
        CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Macos, SupportLevel::Guaranteed)
            .with(Capability::Pause, OsId::Windows, SupportLevel::BestEffort)
            .with(
                Capability::Interaction,
                OsId::Linux,
                SupportLevel::Guaranteed,
            )
    }

    #[test]
    fn empty_declaration_is_empty() {
        let decl = CapabilityDeclaration::new();
        assert!(decl.is_empty());
        assert_eq!(decl.len(), 0);
        assert!(decl.effective(OsId::Linux).is_empty());
    }

    #[test]
    fn has_any_support_true_for_a_normal_declaration() {
        // A normal declaration with at least one non-Unsupported level passes.
        assert!(sample().has_any_support());
    }

    #[test]
    fn has_any_support_false_for_empty_declaration() {
        // Zero capability keys → no support anywhere.
        assert!(!CapabilityDeclaration::new().has_any_support());
    }

    #[test]
    fn has_any_support_false_when_all_entries_unsupported() {
        // A capability key exists but every (OS → level) entry is Unsupported:
        // is_empty() is false, yet the adapter promises support nowhere, so the
        // stricter bar rejects it.
        let decl = CapabilityDeclaration::new()
            .with(Capability::Pause, OsId::Linux, SupportLevel::Unsupported)
            .with(Capability::Pause, OsId::Macos, SupportLevel::Unsupported)
            .with(
                Capability::Interaction,
                OsId::Windows,
                SupportLevel::Unsupported,
            );
        assert!(!decl.is_empty(), "keys are present");
        assert!(
            !decl.has_any_support(),
            "all-Unsupported must fail the support bar"
        );
    }

    #[test]
    fn has_any_support_true_when_one_os_is_best_effort() {
        // Even a single best-effort entry counts as viable support.
        let decl = CapabilityDeclaration::new().with(
            Capability::Pause,
            OsId::Windows,
            SupportLevel::BestEffort,
        );
        assert!(decl.has_any_support());
    }

    #[test]
    fn support_returns_declared_level_per_os() {
        let decl = sample();
        assert_eq!(
            decl.support(Capability::Pause, OsId::Linux),
            SupportLevel::Guaranteed
        );
        assert_eq!(
            decl.support(Capability::Pause, OsId::Windows),
            SupportLevel::BestEffort
        );
    }

    #[test]
    fn missing_os_or_capability_projects_to_unsupported() {
        let decl = sample();
        // Interaction has no Windows entry.
        assert_eq!(
            decl.support(Capability::Interaction, OsId::Windows),
            SupportLevel::Unsupported
        );
        // Nothing declared for Other.
        assert_eq!(
            decl.support(Capability::Pause, OsId::Other),
            SupportLevel::Unsupported
        );
    }

    #[test]
    fn effective_projects_every_declared_capability_for_each_os() {
        let decl = sample();
        // Drive ALL modeled OSes as data — no host gating.
        for os in OsId::MODELED {
            let eff = decl.effective(os);
            assert_eq!(eff.os, os);
            // Both declared capabilities appear, ordered (Pause < Interaction by
            // enum discriminant order in the BTreeMap).
            let caps: Vec<Capability> = eff.entries.iter().map(|(c, _)| *c).collect();
            assert_eq!(caps, vec![Capability::Pause, Capability::Interaction]);
        }

        // Concrete per-OS levels.
        let linux = effective_map(&decl, OsId::Linux);
        assert_eq!(linux[&Capability::Pause], SupportLevel::Guaranteed);
        assert_eq!(linux[&Capability::Interaction], SupportLevel::Guaranteed);

        let windows = effective_map(&decl, OsId::Windows);
        assert_eq!(windows[&Capability::Pause], SupportLevel::BestEffort);
        // Interaction unlisted on Windows → Unsupported.
        assert_eq!(windows[&Capability::Interaction], SupportLevel::Unsupported);
    }

    fn effective_map(
        decl: &CapabilityDeclaration,
        os: OsId,
    ) -> std::collections::BTreeMap<Capability, SupportLevel> {
        decl.effective(os).entries.into_iter().collect()
    }

    #[test]
    fn support_level_serde_round_trips() {
        for level in [
            SupportLevel::Guaranteed,
            SupportLevel::BestEffort,
            SupportLevel::Unsupported,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{}\"", level.as_str()));
            let back: SupportLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
        // best-effort specifically uses the kebab form.
        assert_eq!(
            serde_json::to_string(&SupportLevel::BestEffort).unwrap(),
            "\"best-effort\""
        );
    }

    #[test]
    fn capability_serde_and_all_are_consistent() {
        assert_eq!(Capability::ALL.len(), 2);
        for cap in Capability::ALL {
            let json = serde_json::to_string(&cap).unwrap();
            assert_eq!(json, format!("\"{}\"", cap.as_str()));
            let back: Capability = serde_json::from_str(&json).unwrap();
            assert_eq!(back, cap);
            assert_eq!(cap.to_string(), cap.as_str());
        }
    }

    #[test]
    fn declaration_serde_round_trips_through_json() {
        let decl = sample();
        let json = serde_json::to_string(&decl).unwrap();
        let back: CapabilityDeclaration = serde_json::from_str(&json).unwrap();
        assert_eq!(back, decl);
    }

    #[test]
    fn capabilities_iterates_declared_keys_in_order() {
        let decl = sample();
        let caps: Vec<Capability> = decl.capabilities().collect();
        assert_eq!(caps, vec![Capability::Pause, Capability::Interaction]);
    }
}
