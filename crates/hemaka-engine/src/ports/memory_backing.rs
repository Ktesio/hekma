//! The Memory Backing port surface (spine AD-11) — story 5-1.
//!
//! AD-11: *"`filesystem` (engine-managed directory inside the Agent Home;
//! survives restarts byte-identically) and `native` (delegation marker; engine
//! guarantees only Agent Home persistence). Attach/detach permitted only while
//! the Agent Instance is not `running`. The backing descriptor is handed to the
//! adapter at start; richer backings are Deferred behind this port."*
//!
//! ## This module IS the seam — deliberately NOT a trait (A-8)
//!
//! The `filesystem` implementation is pure path authority inside the engine (one
//! idempotent directory creation at attach + one defensive self-heal at start);
//! there is no behavior to dispatch. A trait with one real impl and one marker
//! would be exactly the "speculative port tree" the ports module warns against,
//! so the port is this MODULE: its types are the extension point a richer
//! backing (vector store, tiered, …) lands behind in a later epic.
//!
//! ## Descriptor delivery (AD-11 Delivery clause / Q-1 ruling)
//!
//! The backing "descriptor" rides the EXISTING layered-config seam: at start the
//! engine injects the managed directory path at the reserved unified-config key
//! ([`crate::domain::MEMORY_DIR_KEY`]) as an invocation override, and the
//! adapter's already-declared `[config]` mapping routes it into the agent's
//! native mechanism. Delivery is OFFERED, not imposed — whether the agent
//! receives it is the adapter's declared choice — which is why
//! [`MemoryBackingStatus::declared`] exists: the public read reports whether
//! the resolved mapping actually targets the key.

use std::path::PathBuf;

use crate::domain::InstanceName;

/// Which Memory Backing is attached to an Agent Instance (FR-16's closed v1
/// vocabulary).
///
/// The wire form is snake_case (`filesystem`, `native`), matching the `kind`
/// column stored by the SQLite [`StateStore`](super::StateStore) — the same
/// wire-string discipline as
/// [`LifecycleState`](crate::domain::LifecycleState). The variant set ships the
/// FULL vocabulary now so story 5-2 adds `native`'s BEHAVIOR without a schema or
/// enum-shape break.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryBackingKind {
    /// An engine-managed directory inside the Agent Home whose contents survive
    /// stop/start cycles and engine restarts byte-identically (story 5-1). The
    /// engine creates it and never touches its contents (operator data, DC-7).
    Filesystem,
    /// An explicit delegation marker: Ktesio guarantees only Agent Home
    /// persistence. RESERVED for story 5-2 — the vocabulary ships now so 5-2's
    /// behavior lands without a breaking edit; nothing in 5-1 implements it.
    Native,
}

impl MemoryBackingKind {
    /// Snake_case wire form used in the DB `kind` column and diagnostics.
    ///
    /// Kept in lockstep with the vocabulary so the store can persist a plain
    /// string without pulling in serde.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryBackingKind::Filesystem => "filesystem",
            MemoryBackingKind::Native => "native",
        }
    }

    /// Parse the wire form back into a [`MemoryBackingKind`].
    ///
    /// Returns `None` for an unrecognized string (e.g. a value written by a
    /// future schema version); callers decide how to treat that.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "filesystem" => Some(MemoryBackingKind::Filesystem),
            "native" => Some(MemoryBackingKind::Native),
            _ => None,
        }
    }
}

/// The persisted Memory Backing attachment for ONE Agent Instance (spine AD-6:
/// attachment metadata lives as typed columns in the one SQLite state store;
/// never a JSON blob). Exactly ONE backing per instance — the row is UNIQUE on
/// the instance FK and cascade-deletes with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryBacking {
    /// The Agent Instance this backing is attached to.
    pub name: InstanceName,
    /// Which backing kind is attached.
    pub kind: MemoryBackingKind,
    /// RFC-3339 UTC timestamp of the attachment (conventions row).
    pub attached_at: String,
}

/// The guarantee level an attached Memory Backing provides (story 5-2, Q-2
/// ratified: a TYPED field, not a rendered string — it stays an engine-API type
/// only until Epic 6 freezes its wire form alongside the deferred `--json`
/// memory surface).
///
/// The vocabulary names what Ktesio actually stands behind for each kind
/// (NFR-7's guarantees-vs-delegation boundary), so an operator read can state
/// the boundary without parsing prose:
///
/// * [`GuaranteeLevel::ManagedDirByteDurable`] — the engine-managed directory
///   exists (created at attach, self-healed at start) and its contents survive
///   stop/start cycles and engine restarts byte-identically; it travels with
///   the Agent Home.
/// * [`GuaranteeLevel::HomePersistenceOnly`] — pure delegation: memory
///   semantics belong to the agent's native mechanism; Ktesio guarantees only
///   that the Agent Home itself persists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuaranteeLevel {
    /// `filesystem`: engine-managed directory, byte-durable across restarts,
    /// travels with the Agent Home (story 5-1's shipped behavior).
    ManagedDirByteDurable,
    /// `native`: delegation marker — only Agent Home persistence is
    /// guaranteed; memory semantics belong to the agent (story 5-2).
    HomePersistenceOnly,
}

impl GuaranteeLevel {
    /// Snake_case wire form used in diagnostics and human output.
    ///
    /// Kept in lockstep with the variant set (same string discipline as
    /// [`MemoryBackingKind::as_str`]) so a future Epic-6 wire freeze reuses it
    /// verbatim.
    pub fn as_str(&self) -> &'static str {
        match self {
            GuaranteeLevel::ManagedDirByteDurable => "managed_dir_byte_durable",
            GuaranteeLevel::HomePersistenceOnly => "home_persistence_only",
        }
    }

    /// The one-sentence NFR-7 boundary statement for this level — THE sentence
    /// both attach confirmations and docs render (DC-6), worded once here so
    /// every surface says exactly the same thing.
    pub fn boundary_statement(&self) -> &'static str {
        match self {
            GuaranteeLevel::ManagedDirByteDurable => {
                "Ktesio guarantees the managed directory: it exists, survives restarts \
                 byte-identically, and travels with the Agent Home."
            }
            GuaranteeLevel::HomePersistenceOnly => {
                "Ktesio guarantees only Agent Home persistence; memory semantics belong to \
                 the agent."
            }
        }
    }

    /// The level implied by a backing kind — the single mapping point between
    /// the two vocabularies (total over the closed FR-16 set).
    pub fn for_kind(kind: MemoryBackingKind) -> Self {
        match kind {
            MemoryBackingKind::Filesystem => GuaranteeLevel::ManagedDirByteDurable,
            MemoryBackingKind::Native => GuaranteeLevel::HomePersistenceOnly,
        }
    }
}

/// The public read of an instance's Memory Backing (story 5-1, Task 4.5): what
/// is attached, where the engine-managed directory lives (path authority),
/// the DC-10 delivery fact — whether the adapter's declared `[config]` mapping
/// targets the reserved key, i.e. whether the injected path will actually reach
/// the agent (a filesystem-only question; a non-filesystem backing delivers
/// nothing, so `declared` is always `false` there) — and story 5-2's typed
/// guarantee level. Shaped for reuse by future status surfaces,
/// not for one call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryBackingStatus {
    /// The attached backing kind.
    pub kind: MemoryBackingKind,
    /// The engine-computed managed directory (path authority). For
    /// [`MemoryBackingKind::Filesystem`] it exists (created at attach, self-healed
    /// at every start); for other kinds it is the computed location only.
    pub dir: PathBuf,
    /// Whether the instance's adapter DECLARES a config mapping for the reserved
    /// key ([`crate::domain::MEMORY_DIR_KEY`]) — the Q-1 honesty rule: delivery
    /// is offered, not imposed, and an operator must be able to learn which it
    /// is. For a `filesystem` backing, `false` means the start still succeeds
    /// but the agent will NOT receive the path (a stderr notice says so at
    /// start). For a NON-filesystem backing it is always `false`: nothing is
    /// delivered at all, so there is no delivery to declare (never read it as a
    /// promise or a decline — see the struct docs). Named for what it reports —
    /// a declared TARGET, not proof of runtime receipt (the agent may ignore
    /// the delivered value; only a richer status surface could observe that).
    pub declared: bool,
    /// The guarantee level this backing provides (story 5-2, Q-2 ratified typed
    /// field) — the machine-readable form of the NFR-7 boundary. Derived from
    /// the kind via [`GuaranteeLevel::for_kind`]; kept as its own field so
    /// Epic 6's wire freeze has a stable key and richer future kinds can map
    /// differently.
    pub guarantee: GuaranteeLevel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_form_round_trips_for_every_variant() {
        let all = [MemoryBackingKind::Filesystem, MemoryBackingKind::Native];
        for kind in all {
            assert_eq!(MemoryBackingKind::from_wire(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn wire_strings_are_pinned_verbatim() {
        // The 6-6 freeze adopted these snake_case strings VERBATIM as the
        // `--json` wire contract (docs/adapter-contract.md); the round-trip
        // test above is swap-invariant (it cannot tell `filesystem` from
        // `native` so long as they differ), so the exact literals are pinned
        // here by VALUE. Changing one is a breaking v1 wire change: announce
        // it, then edit this assertion deliberately.
        assert_eq!(MemoryBackingKind::Filesystem.as_str(), "filesystem");
        assert_eq!(MemoryBackingKind::Native.as_str(), "native");
        assert_eq!(
            GuaranteeLevel::ManagedDirByteDurable.as_str(),
            "managed_dir_byte_durable"
        );
        assert_eq!(
            GuaranteeLevel::HomePersistenceOnly.as_str(),
            "home_persistence_only"
        );
    }

    #[test]
    fn from_wire_rejects_unknown() {
        // The rejection branch is part of the contract (a future/typo'd token
        // must not decode into a real kind).
        assert_eq!(MemoryBackingKind::from_wire("bogus"), None);
        assert_eq!(MemoryBackingKind::from_wire(""), None);
        assert_eq!(MemoryBackingKind::from_wire("Filesystem"), None);
    }

    #[test]
    fn guarantee_level_maps_every_kind_and_round_trips_its_str() {
        // The kind→level mapping is total over the closed FR-16 vocabulary and
        // each level's wire form round-trips (same discipline as the kind).
        let all = [MemoryBackingKind::Filesystem, MemoryBackingKind::Native];
        for kind in all {
            let level = GuaranteeLevel::for_kind(kind);
            assert_eq!(
                GuaranteeLevel::for_kind(kind),
                level,
                "the mapping must be pure"
            );
            assert!(!level.as_str().is_empty());
            assert!(!level.boundary_statement().is_empty());
            assert!(level.boundary_statement().ends_with('.'));
        }
        assert_eq!(
            GuaranteeLevel::for_kind(MemoryBackingKind::Filesystem),
            GuaranteeLevel::ManagedDirByteDurable
        );
        assert_eq!(
            GuaranteeLevel::for_kind(MemoryBackingKind::Native),
            GuaranteeLevel::HomePersistenceOnly
        );
        assert_ne!(
            GuaranteeLevel::ManagedDirByteDurable,
            GuaranteeLevel::HomePersistenceOnly
        );
    }

    #[test]
    fn boundary_statements_name_the_boundary_distinctly() {
        // NFR-7 wording pins: the filesystem sentence promises the managed
        // directory; the native sentence delegates and names Agent Home
        // persistence as ALL Ktesio guarantees.
        let fs = GuaranteeLevel::ManagedDirByteDurable.boundary_statement();
        assert!(fs.contains("managed directory"));
        assert!(fs.contains("byte-identically"));
        let native = GuaranteeLevel::HomePersistenceOnly.boundary_statement();
        assert!(native.contains("only Agent Home persistence"));
        assert!(native.contains("belong to the agent"));
    }
}
