//! # hemaka-adapter-api
//!
//! Home of the **Adapter Contract** (architecture spine AD-1, AD-2, AD-3): the
//! types and traits every agent adapter implements, plus the `adapter.toml`
//! manifest schema — defined here and **only** here, versioned independently
//! from the engine under the contract semver.
//!
//! This crate depends on **nothing internal** (spine AD-1): it is pure contract
//! types + the [`AgentAdapter`] trait, with only serde, `toml` (the manifest
//! parser it owns), `semver`, and `thiserror`. The engine depends on this crate
//! and consumes its parsed form; `kt` depends on the engine's public API plus
//! these types.
//!
//! ## What this crate exposes
//!
//! - [`AgentAdapter`] — the Adapter Contract trait (lifecycle op signatures +
//!   declaration accessors).
//! - [`OsId`] — the operating-system identifier resolved as **data** at runtime
//!   ([`OsId::current`]), never via compile-time platform selection (AD-4).
//! - [`Capability`], [`SupportLevel`], [`CapabilityDeclaration`],
//!   [`EffectiveCapabilities`] — the per-OS Capability Declaration and its
//!   projection onto the running OS (AD-4).
//! - [`MeteringSource`] — the declared Metering Source (AD-7). "No viable
//!   source" is modeled as a validation error, not a variant (FR-19 hard line).
//! - [`Manifest`] / [`ManifestError`] — the `adapter.toml` serde schema, with
//!   [`Manifest::from_toml_str`] and [`Manifest::validate`] (mandatory-section +
//!   viable-metering checks, each naming the failing section — AC2/AC4).
//! - [`CONTRACT_VERSION`] — the **frozen Adapter Contract v1** semver
//!   (FR-27/FR-30).
//! - [`negotiate_contract_version`] + [`ContractVersionError`] +
//!   [`COMPATIBILITY_RULE`] — the FR-30 version negotiation the engine enforces
//!   at manifest registration (story 6-6): a manifest loads only when its
//!   declared contract major matches this build's contract major.
//!
//! ## Frozen at v1 (story 6-6)
//!
//! The contract was seeded at 0.1.0 and additively bumped to 0.4.0 while it was
//! still unpublished. Story 6-6 **freezes it as 1.0.0**: from here on the
//! surface is governed by the published versioning + deprecation policy
//! (`docs/adapter-contract.md`) — breaking changes require a major bump and a
//! migration note, deprecations are announced at least one minor ahead, and the
//! engine refuses a manifest whose contract major differs from this build's
//! ([`negotiate_contract_version`]). The `cargo-semver-checks` CI job guards
//! the crate's Rust API on two baselines: an **armed in-repo baseline** that
//! diffs this surface against the contract-v1 freeze commit (`4119db3`) on
//! every run (retro #160 — any breaking public-surface change vs the freeze
//! baseline fails CI, announced or not; announcement enforcement is review
//! discipline in the pre-publish window), and the crates.io-published
//! baseline, which stays dormant (notice-only) until the crates publish at
//! story 7-4. The docs say so plainly.

mod adapter;
mod capability;
mod config;
mod manifest;
mod metering;
mod os;

use thiserror::Error;

/// The Adapter Contract version this build implements (spine FR-27/FR-30 seed).
///
/// A semver string. This SEEDS the versioned contract; it is **not** frozen
/// here (epic 6.6 freezes v1 and adds negotiation). Manifests declare the
/// version they target via `contract_version`; this story stores it, and does
/// not yet negotiate or enforce compatibility beyond presence.
///
/// Bumped `0.1.0 → 0.2.0` in story 2-2 (FR-12): the manifest schema gained an
/// OPTIONAL `[config]` mapping section + the trait's `config_mapping()` accessor —
/// an additive minor bump.
///
/// Bumped `0.2.0 → 0.3.0` in story 3-1 (FR-19 self-reported metering): the
/// Adapter Contract's METERING surface gains a documented usage-reporting CHANNEL —
/// a `self-reported` adapter conveys the agent's own usage accounting to the engine
/// by emitting `KTESIO_USAGE {json}` sentinel lines on the agent's stdout (parsed
/// out of the AD-12 capture; see the [`metering`] module + `docs/manifest.md`). This
/// is a DOCUMENTARY, back-compat addition (no new trait method — the channel is a
/// stdout convention, and the `[metering]` section is unchanged), so it is an
/// additive MINOR bump under 0.x semver: nothing was removed or changed
/// incompatibly. Unlike story 2-4's engine-INTERNAL `SecretResolver` (no bump), this
/// touches the adapter-FACING contract, so the bump is real — the semver-check CI
/// job guards it.
///
/// Bumped `0.3.0 → 0.4.0` in story 4-1 (FR-24 send input): the manifest
/// `[interaction]` block's `channel` field firms up from a free-form `String`
/// to the closed [`manifest::InteractionChannelKind`] enum (single variant
/// `Stdio` this story) — an unrecognized value now fails to PARSE instead of
/// being silently accepted-and-ignored. This is an adapter-FACING, additive/
/// clarifying change (mirrors 3-1's own reasoning): no existing shipped
/// manifest sets a non-`"stdio"` channel today, so nothing breaks; the
/// semver-check CI job guards it.
///
/// **Frozen `0.4.0 → 1.0.0` in story 6-6 (FR-30, the v1 freeze):** the contract
/// is tagged v1 after Stories 6.4/6.5 validated it against a second agent
/// (opencode) and the 6-5 checkpoint ratifications landed. v1 ships the
/// negotiated rule ([`negotiate_contract_version`]: compatible iff the manifest
/// major equals this build's contract major) and the versioning + deprecation
/// policy published at `docs/adapter-contract.md`. The previously-unpublished
/// 0.x seeds are NOT grandfathered: the contract was never released, so no
/// back-compat obligation exists — a manifest still declaring a 0.x version
/// names both versions and fails to load. The `--json` memory wire surface
/// (story 5-2's deferral) landed IN this freeze, so the frozen v1 surface is
/// complete: adding it after the freeze would have been a breaking wire change.
pub const CONTRACT_VERSION: &str = "1.0.0";

/// The Adapter Contract compatibility rule the engine enforces at registration
/// (FR-30; ratified at the 6-6 checkpoint). Quoted VERBATIM by the negotiation
/// error so every failing load states the rule identically — keep this string,
/// the error text, and `docs/adapter-contract.md#versioning` in lockstep.
pub const COMPATIBILITY_RULE: &str = "compatible iff the major versions match";

/// The published policy page the negotiation error points at (the rule's
/// normative home, NFR-7).
pub const COMPATIBILITY_POLICY_DOC: &str = "docs/adapter-contract.md#versioning";

/// The SINGLE SOURCE of the AI-6 strict-parse requirement text. Quoted by
/// BOTH rejection sites — [`ContractVersionError::Unparseable`] and
/// `Manifest::validate`'s `contract_version` detail (which interpolates this
/// const, so the two messages cannot drift apart) — and stated on
/// `docs/adapter-contract.md#versioning`. Keep all three in lockstep by
/// editing only this string.
pub const STRICT_SEMVER_REQUIREMENT: &str = "strict `X.Y.Z` required — no `v` prefix, no \
     partial versions like `1` or `1.0`; prerelease/build suffixes such as `1.0.0-rc.1` parse \
     and negotiate by major";

/// Why a manifest cannot load under this build's Adapter Contract (story 6-6,
/// FR-30). `thiserror`, never `miette` (conventions). Each message states the
/// FACTS: both versions (manifest's and engine's) and the compatibility rule.
#[derive(Debug, Error)]
pub enum ContractVersionError {
    /// The manifest's `contract_version` is not a strict `X.Y.Z` semver
    /// version, so there is nothing to negotiate against. The strictness is
    /// deliberate (AI-6, resolved at the 6-6 freeze): `1` and `1.0` are NOT
    /// versions, `v1.0.0` is not the field's grammar, and accepting them would
    /// make "which contract do you target?" ambiguous. Prerelease and
    /// build-metadata suffixes (`1.0.0-rc.1+build.5`) DO parse as semver —
    /// negotiation compares majors only, so a same-major prerelease is
    /// compatible (the stance is documented at
    /// `docs/adapter-contract.md#versioning`).
    ///
    /// The requirement clause interpolates [`STRICT_SEMVER_REQUIREMENT`] — the
    /// single source also quoted by `Manifest::validate`'s identical rejection
    /// — so the two diagnostics cannot drift.
    #[error("'{manifest_version}' is not a valid semver version ({STRICT_SEMVER_REQUIREMENT})")]
    Unparseable {
        /// The rejected value, verbatim.
        manifest_version: String,
    },

    /// The manifest targets a different contract MAJOR than this build speaks.
    /// Names BOTH versions and quotes the rule — the FR-30 informative
    /// rejection. A pre-v1 `0.x` manifest lands here: the contract was never
    /// published under 0.x, so those seeds carry no back-compat obligation.
    #[error(
        "incompatible adapter contract: manifest declares {manifest_version}, engine speaks \
         {engine_version} — {rule} (contract v1 policy, {doc})"
    )]
    Incompatible {
        /// The manifest's declared `contract_version`.
        manifest_version: String,
        /// This build's [`CONTRACT_VERSION`].
        engine_version: String,
        /// The quoted [`COMPATIBILITY_RULE`].
        rule: &'static str,
        /// The published policy page ([`COMPATIBILITY_POLICY_DOC`]).
        doc: &'static str,
    },
}

/// Negotiate a manifest's declared `contract_version` against this build's
/// [`CONTRACT_VERSION`] (FR-30, story 6-6).
///
/// The ENGINE calls this at manifest registration (the single load gate): a
/// manifest loads only when its contract major equals this build's contract
/// major. Prerelease/build-metadata suffixes of the same major are compatible
/// (majors compare equal); any `0.x` manifest is incompatible — the pre-v1
/// seeds were never published, so there is nothing to stay compatible WITH.
///
/// A strict `X.Y.Z` semver parse is required first (AI-6: no `v` prefix, no
/// partials — see [`ContractVersionError::Unparseable`]). `Manifest::validate`
/// already rejects non-semver values, so the engine's call ordering makes
/// [`ContractVersionError::Unparseable`] defensive-depth rather than a live
/// path.
pub fn negotiate_contract_version(manifest_version: &str) -> Result<(), ContractVersionError> {
    let trimmed = manifest_version.trim();
    let parsed =
        semver::Version::parse(trimmed).map_err(|_| ContractVersionError::Unparseable {
            manifest_version: manifest_version.to_string(),
        })?;
    // CONTRACT_VERSION is a build-time constant pinned to parse by a unit test,
    // so the expect is unreachable by construction.
    let engine =
        semver::Version::parse(CONTRACT_VERSION).expect("CONTRACT_VERSION is valid semver");
    if parsed.major == engine.major {
        return Ok(());
    }
    Err(ContractVersionError::Incompatible {
        manifest_version: manifest_version.to_string(),
        engine_version: CONTRACT_VERSION.to_string(),
        rule: COMPATIBILITY_RULE,
        doc: COMPATIBILITY_POLICY_DOC,
    })
}

pub use adapter::{AdapterError, AgentAdapter};
pub use capability::{Capability, CapabilityDeclaration, EffectiveCapabilities, SupportLevel};
pub use config::{ConfigMapping, ConfigTarget, FilePlacement, FileTarget};
pub use manifest::{
    AdapterIdentity, Interaction, InteractionChannelKind, Lifecycle, Manifest, ManifestError,
    Metering, OpTemplate,
};
pub use metering::MeteringSource;
pub use os::OsId;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_version_is_frozen_v1() {
        // The v1 FREEZE (story 6-6): the contract tags 1.0.0 and the engine
        // negotiates on its major. Pinning the exact triple makes an
        // unannounced bump fail here first — the "announce" gate.
        let parsed = semver::Version::parse(CONTRACT_VERSION).expect("CONTRACT_VERSION is semver");
        assert_eq!(parsed.major, 1);
        assert_eq!(parsed.minor, 0);
        assert_eq!(parsed.patch, 0);
    }

    #[test]
    fn negotiation_accepts_the_engine_version_and_any_same_major() {
        // The exact engine version loads, as does any same-major spelling —
        // including prerelease/build-metadata forms (AI-6's documented stance:
        // negotiation compares majors only).
        for version in [
            CONTRACT_VERSION,
            "1.9.9",
            "1.0.0-rc.1",
            "1.2.3+build.7",
            " 1.0.0 ", // surrounding whitespace is trimmed
        ] {
            assert!(
                negotiate_contract_version(version).is_ok(),
                "{version} must negotiate as compatible"
            );
        }
    }

    #[test]
    fn negotiation_rejects_a_different_major_naming_both_versions_and_the_rule() {
        // FR-30's informative rejection: BOTH versions named + the rule quoted.
        let err = negotiate_contract_version("2.1.0").unwrap_err();
        assert!(
            matches!(err, ContractVersionError::Incompatible { .. }),
            "{err}"
        );
        let text = err.to_string();
        assert!(text.contains("2.1.0"), "{text}");
        assert!(text.contains(CONTRACT_VERSION), "{text}");
        assert!(text.contains(COMPATIBILITY_RULE), "{text}");
        assert!(text.contains(COMPATIBILITY_POLICY_DOC), "{text}");
    }

    #[test]
    fn negotiation_rejects_a_different_major_even_with_prerelease_or_build_metadata() {
        // AI-64 independent pass (M3, 2026-09-04): every fixture negotiated a
        // PLAIN different major, so flipping `major ==` to `major == … ||
        // !pre.is_empty()` survived the whole suite. These fixtures pin that a
        // different major is incompatible EVEN WHEN it carries a prerelease or
        // build-metadata suffix — suffixes are never a compatibility token.
        for version in [
            "2.0.0-rc.1",
            "2.0.0+build.9",
            "2.0.0-rc.1+build.9",
            "0.9.0-beta",
        ] {
            let err = negotiate_contract_version(version).unwrap_err();
            assert!(
                matches!(err, ContractVersionError::Incompatible { .. }),
                "{version} must be INCOMPATIBLE (major differs): {err}"
            );
        }
    }

    #[test]
    fn unparseable_message_quotes_the_shared_strict_parse_requirement() {
        // Drift check for STRICT_SEMVER_REQUIREMENT (finding 14): the
        // Unparseable diagnostic must quote the SHARED const, not a private
        // literal — Manifest::validate quotes the same one for the identical
        // rejection.
        let err = negotiate_contract_version("v1.0.0").unwrap_err();
        assert!(
            matches!(err, ContractVersionError::Unparseable { .. }),
            "{err}"
        );
        assert!(
            err.to_string().contains(STRICT_SEMVER_REQUIREMENT),
            "the Unparseable message must quote the shared requirement verbatim: {err}"
        );
    }

    #[test]
    fn negotiation_does_not_grandfather_pre_v1_manifests() {
        // The 0.x seeds were never published: a manifest still targeting them
        // is INCOMPATIBLE with contract v1 (ratified at the 6-6 checkpoint).
        for version in ["0.1.0", "0.3.0", "0.4.0"] {
            let err = negotiate_contract_version(version).unwrap_err();
            assert!(
                matches!(err, ContractVersionError::Incompatible { .. }),
                "{version} must be incompatible: {err}"
            );
        }
    }

    #[test]
    fn negotiation_rejects_the_strict_parse_edges() {
        // AI-6 verdict: strict `X.Y.Z` only — no partials, no `v` prefix.
        for version in ["1", "1.0", "v1.0.0", "banana", ""] {
            let err = negotiate_contract_version(version).unwrap_err();
            assert!(
                matches!(err, ContractVersionError::Unparseable { .. }),
                "{version:?} must fail the strict parse: {err}"
            );
        }
    }

    #[test]
    fn public_surface_is_reachable() {
        // A smoke test that the re-exported contract surface composes: build a
        // declaration, project it, and confirm the metering type is usable.
        let decl = CapabilityDeclaration::new().with(
            Capability::Pause,
            OsId::current(),
            SupportLevel::Guaranteed,
        );
        let eff: EffectiveCapabilities = decl.effective(OsId::current());
        assert_eq!(eff.entries.len(), 1);
        assert_eq!(MeteringSource::SelfReported.as_str(), "self-reported");
    }
}
