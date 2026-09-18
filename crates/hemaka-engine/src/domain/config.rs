//! The unified layered-config MODEL (spine AD-9, FR-11) — story 2-1.
//!
//! This is the FOUNDATION of Epic 2. It owns exactly two things, both of which
//! are pure and I/O-free (mirroring [`super::transition`] / [`super::restart`]):
//!
//! * The **deterministic precedence resolver** ([`resolve`]): a total, pure fold
//!   of the four ordered [`ConfigLayer`]s — engine defaults < agent-kind defaults
//!   < Agent Home instance config < invocation overrides — into a single
//!   [`EffectiveConfig`]. A later layer wins for a given key, EVERY time, on
//!   EVERY machine (no clock, no env, no OS). The merge is DEEP / per-leaf: an
//!   instance `a.b` overrides only `a.b`, leaving a kind-level `a.c` intact.
//! * **Write-time validation** ([`validate_write`]): accept a known unified key
//!   OR an `agent.*` pass-through key; reject any other key with the nearest
//!   valid key suggested (a tiny hand-rolled Levenshtein over the known-key set).
//!   Runs BEFORE any persistence, so a rejected `set` touches nothing.
//!
//! ## The ONE forward-design seam (for story 2-3, FR-13)
//!
//! [`resolve`] records, per resolved leaf key, the WINNING [`SourceLayer`] (a tag
//! carried alongside each value in [`EffectiveConfig`]). Story 2-3 renders "each
//! value's source layer" and persists the `EffectiveConfig` snapshot at start;
//! by keeping the tag here, that story is purely ADDITIVE. 2-1 does NOT render or
//! persist provenance — it just does not discard it. This is the only capability
//! 2-1 builds ahead of its own AC, and it is cheap (a tag threaded through the
//! fold).
//!
//! ## Explicitly OUT of scope (later Epic-2 stories)
//!
//! * The per-adapter NATIVE-mechanism mapping (files/env/flags) — **2-2 (FR-12)**.
//!   2-1 only reserves the `agent.*` pass-through namespace + honors the bypass.
//! * Provenance RENDERING + the persisted `EffectiveConfig` snapshot — **2-3
//!   (FR-13)**. 2-1 only leaves the [`SourceLayer`] seam.
//!
//! ## SECRETS masking lives here now (story 2-4, FR-14, AD-10)
//!
//! A `secret:NAME`-prefixed leaf VALUE is classified by [`is_secret_ref`] /
//! [`secret_name`] (beside [`is_pass_through`] / [`pass_through_tail`], same
//! non-empty-child discipline) and MASKED by [`ResolvedValue::display`] — the ONE
//! render path the human table, `config get --json`, AND the persisted
//! `effective-config.json` snapshot all share (2-3 AC8), so this single choke
//! point masks all three at once. Resolution (env + the 0600 secrets file) lives
//! in [`super::super::ports`] / the supervisor, not here: this module stays I/O-free
//! and only CLASSIFIES + MASKS. The resolved cleartext reaches the adapter through
//! the supervisor's start seam ([`super::supervisor`]) via
//! [`super::secret::SecretString::expose_secret`] — display and delivery diverge.
//!
//! The on-disk layer plumbing (loading the instance `config.toml`, the embedded
//! engine defaults, writing a key) lives alongside the registry under path
//! authority — see [`super::registry`]. This module is deliberately I/O-free so
//! the resolver + validation stay exhaustively unit-testable in-process.

use std::collections::BTreeMap;

use thiserror::Error;
use toml::Value;

use super::budget::{BreachAction, TokenBudget};
use super::cost::{CostCap, Micros, Rate, MICROS_PER_DOLLAR};

/// The engine-namespace config key for the PER-RUN token ceiling (story 3-2,
/// AD-9). A validated key (NOT `agent.*` pass-through): its value parses as a
/// `u64` at write time. Absent → the per-run scope is unset (never breaches).
pub const BUDGET_TOKENS_PER_RUN_KEY: &str = "budget.tokens.per_run";

/// The engine-namespace config key for the CUMULATIVE token ceiling (story 3-2).
/// A validated key; its value parses as a `u64` at write time. Absent → the
/// cumulative scope is unset.
pub const BUDGET_TOKENS_CUMULATIVE_KEY: &str = "budget.tokens.cumulative";

/// The engine-namespace config key for the Breach Action (story 3-2, AC-C). A
/// validated key; its value parses as a [`BreachAction`] (`pause`/`stop`/`warn`)
/// at write time. Absent → [`BreachAction::default`] (`pause`, the ratified
/// default). Story 3-3: the SAME key governs a DOLLAR breach too (one action for
/// both token and dollar breaches — see [`resolve_cost`]).
pub const BUDGET_BREACH_ACTION_KEY: &str = "budget.breach_action";

/// The engine-namespace config key for the per-direction INPUT Rate (story 3-3,
/// FR-20). A validated key; its value is a DOLLAR STRING (e.g. `"3.00"`) parsed to
/// micro-dollars-per-1M-tokens at WRITE time (rejecting a non-numeric or sub-micro
/// value — see [`validate_cost_value`]). Absent → no input Rate; because BOTH
/// directions are required for a Rate to be "supplied", an absent input direction
/// makes the whole Rate inert (AC-B).
pub const COST_RATE_INPUT_KEY: &str = "cost.rate.input";

/// The engine-namespace config key for the per-direction OUTPUT Rate (story 3-3,
/// FR-20). A dollar-string leaf parsed to micros at write time, exactly like
/// [`COST_RATE_INPUT_KEY`]. Absent → the Rate is inert (both directions required).
pub const COST_RATE_OUTPUT_KEY: &str = "cost.rate.output";

/// The engine-namespace config key for the PER-RUN dollar Cost Cap (story 3-3,
/// FR-21). A dollar-string leaf parsed to micros at write time. Absent → the
/// per-run dollar scope is unset (never breaches). A cap set with no Rate is inert
/// (AC-B).
pub const BUDGET_DOLLARS_PER_RUN_KEY: &str = "budget.dollars.per_run";

/// The engine-namespace config key for the CUMULATIVE dollar Cost Cap (story 3-3,
/// FR-21). A dollar-string leaf parsed to micros at write time. Absent → the
/// cumulative dollar scope is unset.
pub const BUDGET_DOLLARS_CUMULATIVE_KEY: &str = "budget.dollars.cumulative";

/// The engine-namespace config key for the ENGINE-OBSERVED upstream provider base
/// URL (story 3-4, FR-19/AD-7). The OPERATOR sets it to the agent's REAL
/// OpenAI-compatible provider endpoint (e.g. `"http://127.0.0.1:1234"` for a local
/// gateway, or an `https://` hosted provider since story 12-3's vendored
/// rustls+ring); the engine's loopback forward listener FORWARDS observed traffic
/// there. Absent → an `engine-observed` instance cannot start its listener (a
/// typed start error naming this key), since there is nowhere to forward.
/// ENGINE-INTERNAL
/// config — it does NOT touch the Adapter Contract surface (no `CONTRACT_VERSION`
/// bump), exactly like the budget/cost keys.
pub const METERING_UPSTREAM_BASE_URL_KEY: &str = "metering.upstream_base_url";

/// The RESERVED engine-namespace config key the engine INJECTS the loopback
/// listener address into at start (story 3-4, AC6). The engine writes
/// `http://127.0.0.1:<port>` here as an INVOCATION-OVERRIDE leaf at `starting` for
/// an `engine-observed` instance, and the adapter's EXISTING `[config]` mapping
/// (story 2-2) delivers it into the agent's native mechanism — the adapter
/// declares e.g. `[config."metering.base_url"] env = "OPENAI_BASE_URL"`, pointing
/// the agent's OpenAI-compatible `base_url` at the listener (FR-19 "the Adapter
/// routes the Agent's model traffic through an Engine-provided interception
/// point"). A KNOWN key so a mapping can target it, but the OPERATOR does NOT set
/// it — the address is ephemeral (known only at spawn) and ENGINE-computed (the
/// engine is the sole authority on the listen address, AC-B). Reusing the existing
/// config-mapping means NO new contract surface (no `CONTRACT_VERSION` bump). A
/// hand-set value in a lower layer is harmless — the engine's start-time override
/// always wins (the invocation layer is strongest).
pub const METERING_BASE_URL_KEY: &str = "metering.base_url";

/// The RESERVED engine-namespace unified-config key the engine INJECTS the
/// managed Memory Backing directory path into at start (story 5-1, FR-15 /
/// spine AD-11 Delivery clause). The engine writes the absolute managed-directory
/// path here as an INVOCATION-OVERRIDE leaf at start for an instance with a
/// `filesystem` Memory Backing attached, and the adapter's EXISTING `[config]`
/// mapping (story 2-2) delivers it into the agent's native mechanism — the
/// adapter declares e.g. `[config."memory.dir"] env = "MEMORY_DIR"`, pointing its
/// agent at the managed memory location. A KNOWN key so a mapping can target it,
/// but the OPERATOR does NOT set it — the path is ENGINE-computed (the sole path
/// authority) and known only once the Agent Home exists. Delivery is OFFERED, not
/// imposed: whether the agent receives the value is the adapter's declared
/// choice, and an unmapped key stays a silent no-op (story 2-2 Decision 6) —
/// which is why the engine says so aloud at start when nothing targets this key
/// (DC-10), and why the public backing read reports it too. Reusing the existing
/// config-mapping means NO new Adapter Contract surface (no `CONTRACT_VERSION`
/// bump), exactly like `metering.base_url`. A hand-set value in a lower layer is
/// harmless — the engine's start-time override always wins (the invocation layer
/// is strongest). The injected value is a DELIVERY MECHANISM, not operator
/// configuration: it is deliberately never written to the effective-config
/// snapshot (3-4's honest-provenance split, extended by 5-1's CORRECTION).
pub const MEMORY_DIR_KEY: &str = "memory.dir";

/// The reserved pass-through namespace prefix (spine AD-9's `agent.*`), story
/// 2-1 (AC7). A key under this prefix BYPASSES unknown-key validation and is
/// delivered verbatim (the mapping into an agent's native mechanism is 2-2,
/// FR-12). Recorded decision: the exact prefix is `agent.` (a dotted segment, so
/// a key named exactly `agent` — with no child — is NOT pass-through).
pub const PASS_THROUGH_PREFIX: &str = "agent.";

/// The reserved SECRET-reference prefix (spine AD-10's `secret:NAME`), story 2-4
/// (FR-14, AC4). A resolved leaf VALUE whose string form begins with this prefix
/// followed by a NON-empty `NAME` is secret-classified: it is RESOLVED (env → the
/// 0600 secrets file) to a [`super::secret::SecretString`] at start and MASKED
/// everywhere it is displayed/logged/serialized. Recorded decision (Assumption 1):
/// the prefix is `secret:` and a bare `secret:` with NO name is NOT a secret
/// reference — it is ordinary opaque text (mirroring how a bare `agent` is not
/// pass-through). `NAME` is the lookup key handed to the resolver, not the secret,
/// so it MAY remain visible; the VALUE never is.
pub const SECRET_PREFIX: &str = "secret:";

/// The fixed masked rendering a secret-classified leaf shows in every display
/// surface (the human table, `config get --json`, and the persisted
/// `effective-config.json` snapshot), story 2-4 (AC8). Recorded decision
/// (Assumption 8): a fixed token that HIDES the value while still signaling "a
/// secret is here". It deliberately does NOT echo the resolved cleartext; the
/// `NAME` is not shown either (the mask is value-independent), so two different
/// secret leaves render identically. The SOLE un-mask is the `--reveal` read flag
/// (AC-C), which never routes through this method. Kept distinct from
/// [`super::secret::REDACTED`] (the `SecretString` `Display`/`Debug` token) only in
/// spelling — both communicate "redacted".
pub const SECRET_MASK: &str = "secret:****";

/// The Levenshtein distance threshold for the nearest-valid-key suggestion
/// (AC-B / AC6). A candidate is only suggested when its edit distance to the
/// offending key is `<=` this bound; beyond it the error honestly says "no close
/// match" rather than suggesting nonsense. `[ASSUMPTION]` recorded: `3` catches
/// the common near-misses (`modle`→`model`, a transposition + a typo) without
/// matching unrelated keys.
const SUGGESTION_MAX_DISTANCE: usize = 3;

/// The layer that supplied a resolved value (spine AD-9's precedence order).
///
/// The variant ORDER encodes precedence directly: `EngineDefault` is the weakest
/// and `InvocationOverride` the strongest, so the derived [`Ord`] makes "a later
/// layer wins" a simple `>=` comparison. [`resolve`] folds the four layers
/// weakest-first and records the winning layer per leaf key. This tag is the
/// seam story 2-3 (FR-13) renders + persists — 2-1 only records it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum SourceLayer {
    /// Engine-owned defaults, the same for every instance (weakest). Story 2-1:
    /// an embedded `const` TOML (see [`super::registry`]).
    EngineDefault,
    /// Agent-kind defaults supplied by the resolved adapter. Empty for `mock`.
    KindDefault,
    /// The per-Agent-Home instance `config.toml` (`kt agent config set` writes
    /// here).
    Instance,
    /// Ephemeral invocation-time overrides (strongest). Not persisted in 2-1.
    InvocationOverride,
}

impl SourceLayer {
    /// The stable wire/label form (`"engine-default"`, …). Matches the serde
    /// kebab-case rename so a DB/JSON string and this label never diverge (the
    /// [`super::restart::RestartPolicy`] convention).
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceLayer::EngineDefault => "engine-default",
            SourceLayer::KindDefault => "kind-default",
            SourceLayer::Instance => "instance",
            SourceLayer::InvocationOverride => "invocation-override",
        }
    }

    /// The four layers in precedence order (weakest → strongest). This is the
    /// order [`resolve`] folds them, and the index each occupies in the
    /// `[ConfigLayer; 4]` input.
    pub const ORDER: [SourceLayer; 4] = [
        SourceLayer::EngineDefault,
        SourceLayer::KindDefault,
        SourceLayer::Instance,
        SourceLayer::InvocationOverride,
    ];
}

impl std::fmt::Display for SourceLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One config layer: a parsed TOML table (the deserialized form of that layer's
/// TOML text). Constructed by the engine's path-authority loaders (from the
/// embedded engine-defaults string, an adapter's kind-defaults, the instance
/// `config.toml`, or an invocation-override map). An EMPTY table is a valid layer
/// (e.g. a kind with no defaults resolves to an empty layer, NOT an error —
/// AC8).
///
/// A newtype over [`toml::value::Table`] so the resolver's signature is explicit
/// and the "empty layer" concept has a name; it derefs to the table for reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigLayer {
    table: toml::value::Table,
}

impl ConfigLayer {
    /// An empty layer (the "no defaults" / absent case — AC8).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Wrap an already-parsed TOML table as a layer.
    pub fn from_table(table: toml::value::Table) -> Self {
        Self { table }
    }

    /// Parse a layer from TOML text, naming the layer + source path on failure so
    /// a malformed layer surfaces a typed [`ConfigError::MalformedLayer`] (never a
    /// panic — AC8). `path` is a human label for the source (a file path, or e.g.
    /// `<engine-defaults>` for the embedded constant).
    pub fn parse(layer: SourceLayer, path: &str, text: &str) -> Result<Self, ConfigError> {
        let table = text
            .parse::<toml::Table>()
            .map_err(|e| ConfigError::MalformedLayer {
                layer,
                path: path.to_string(),
                detail: e.to_string(),
            })?;
        Ok(Self { table })
    }

    /// The underlying parsed table (read access).
    pub fn as_table(&self) -> &toml::value::Table {
        &self.table
    }

    /// Whether this layer contributes nothing (the absent/empty case).
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }
}

/// A single resolved leaf: the winning value + the layer that supplied it.
///
/// The [`SourceLayer`] is the story-2-3 provenance seam (FR-13). 2-1 populates it
/// but renders/persists nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedValue {
    /// The winning value (a scalar or array `toml::Value`; nested tables are
    /// walked, so a `ResolvedValue` is always a LEAF, never a table).
    pub value: Value,
    /// The layer that supplied this value (AD-9 provenance seam for 2-3).
    pub source: SourceLayer,
}

impl ResolvedValue {
    /// Render the winning value for human/plain output WITHOUT exposing the
    /// underlying `toml::Value` type to callers (so `kt` needs no `toml`
    /// dependency — AD-2 keeps the engine the sole owner of the config TOML
    /// crate). A string renders as its bare contents (`gpt-4`, no quotes); every
    /// other scalar/array uses TOML's own inline form (`42`, `true`,
    /// `["a", "b"]`).
    ///
    /// SECRET MASKING (story 2-4, AC8 — the single choke point): a
    /// secret-classified string value ([`is_secret_ref`], i.e. `secret:NAME`)
    /// renders as the fixed [`SECRET_MASK`], NEVER as-is and never as the resolved
    /// cleartext. Because 2-3 routed the human table, `config get --json`, AND the
    /// persisted `effective-config.json` snapshot through THIS one method, masking
    /// here masks all three at once (no per-surface branch). The `--reveal` read
    /// (AC-C) is the SOLE un-mask and does NOT go through `display()`; the adapter
    /// delivery path (AC9) reaches the real cleartext via
    /// [`super::secret::SecretString::expose_secret`], not here — display and
    /// delivery deliberately diverge.
    pub fn display(&self) -> String {
        display_value(&self.value)
    }
}

/// Render a resolved [`toml::Value`] leaf for human/plain output (see
/// [`ResolvedValue::display`]). A secret-classified string ([`is_secret_ref`])
/// masks to [`SECRET_MASK`] (story 2-4, AC8) — the ONE place the mask is applied.
fn display_value(value: &Value) -> String {
    match value {
        Value::String(s) if is_secret_ref(s) => SECRET_MASK.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The resolved effective config (spine AD-9): every LEAF key (dotted path)
/// mapped to its winning value + [`SourceLayer`] provenance.
///
/// Keys are dotted paths into the merged TOML tree (`a.b.c`), so a per-leaf merge
/// is directly observable: setting `a.b` at a higher layer changes only the
/// `a.b` entry, leaving `a.c` (from a lower layer) untouched. A [`BTreeMap`]
/// keeps iteration deterministic (sorted), reinforcing "same inputs → same
/// output". The provenance tag on each [`ResolvedValue`] is the 2-3 seam; 2-1's
/// CLI renders VALUES only (see [`EffectiveConfig::values`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectiveConfig {
    leaves: BTreeMap<String, ResolvedValue>,
}

impl EffectiveConfig {
    /// Look up a single resolved leaf by its dotted key (value + provenance).
    pub fn get(&self, key: &str) -> Option<&ResolvedValue> {
        self.leaves.get(key)
    }

    /// Look up just the winning VALUE for a dotted key (provenance dropped) — the
    /// `kt agent config get <key>` read (source layers are 2-3, AC5/AC11).
    pub fn value(&self, key: &str) -> Option<&Value> {
        self.leaves.get(key).map(|r| &r.value)
    }

    /// The rendered winning value for a dotted key, if present — the display form
    /// `kt agent config get <key>` prints, WITHOUT exposing `toml::Value` to
    /// callers (AD-2). See [`ResolvedValue::display`].
    pub fn value_display(&self, key: &str) -> Option<String> {
        self.leaves.get(key).map(|r| r.display())
    }

    /// The stable SOURCE-LAYER label for a dotted key, if present — the winning
    /// layer that supplied the value (`engine-default` / `kind-default` /
    /// `instance` / `invocation-override`), read from the [`SourceLayer`] tag 2-1
    /// records per leaf. This is the story-2-3 provenance accessor (FR-13): it
    /// lets `kt` render "each value's source layer" WITHOUT owning `toml`/config
    /// internals (AD-2), symmetric with [`value_display`](Self::value_display) and
    /// [`is_unvalidated`](Self::is_unvalidated). `kt` never re-derives the layer.
    pub fn source_label(&self, key: &str) -> Option<&'static str> {
        self.leaves.get(key).map(|r| r.source.as_str())
    }

    /// Iterate all resolved leaves (dotted key → value + provenance), sorted by
    /// key. The full-map `kt agent config get <name>` read.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ResolvedValue)> {
        self.leaves.iter()
    }

    /// Remove a leaf by its dotted key, returning it if present. Used by the
    /// engine to drop a reserved IDENTITY key (`name`) that the registration step
    /// seeds into the instance `config.toml` — it is instance identity, not user
    /// config, so it must not be presented as a settable key (review patch #4).
    pub fn remove(&mut self, key: &str) -> Option<ResolvedValue> {
        self.leaves.remove(key)
    }

    /// A values-only view: dotted key → winning value, sorted by key (provenance
    /// dropped). This is what 2-1's CLI renders — source layers are 2-3 (AC5).
    pub fn values(&self) -> BTreeMap<String, Value> {
        self.leaves
            .iter()
            .map(|(k, r)| (k.clone(), r.value.clone()))
            .collect()
    }

    /// Whether the effective config is empty (the all-empty-layers case).
    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Whether the leaf at `key` is UNVALIDATED — i.e. it lives under the
    /// `agent.*` pass-through namespace and so bypassed known-key validation
    /// (story 2-2, AC-B/AC7). The MINIMAL 2-3 seam: `kt agent config get` marks
    /// each such leaf "unvalidated" in its output, derived PURELY from the
    /// pass-through prefix (reusing [`is_pass_through`]) — NOT from a new persisted
    /// field. A known key (e.g. `model`) is validated, so this is `false`. Lets
    /// `kt` render the marker without owning the `agent.*` boundary or any config
    /// internals (AD-2); 2-3's richer per-value provenance rendering is additive.
    pub fn is_unvalidated(&self, key: &str) -> bool {
        is_pass_through(key)
    }

    /// Number of resolved leaf keys.
    pub fn len(&self) -> usize {
        self.leaves.len()
    }
}

/// Resolve the four ordered layers into an [`EffectiveConfig`] (spine AD-9, the
/// AC-A heart). PURE: no I/O, no clock, no env — the SAME inputs always yield the
/// SAME output on every machine/OS.
///
/// `layers` is indexed by precedence, weakest first, matching
/// [`SourceLayer::ORDER`]: `[EngineDefault, KindDefault, Instance,
/// InvocationOverride]`. The merge is a STRUCTURAL tree-merge, weakest-first: at
/// each key a stronger layer's contribution overrides the weaker one, and — the
/// subtle correctness point — the stronger layer's SHAPE wins and PRUNES the
/// weaker layer's now-orphaned leaves:
///
/// * A strong SCALAR at `a.b` MASKS a weak `[a.b] c = 1` subtree — the resolved
///   tree carries `a.b = "scalar"` and NO orphaned `a.b.c` (a shape-agnostic
///   flatten would keep both, yielding a self-contradictory `a.b="scalar"` +
///   `a.b.c=1` with stale provenance).
/// * A strong SUBTREE at `a.b` REPLACES a weak scalar at `a.b`, then merges
///   per-leaf into a weak subtree (so a weak sibling `a.b.d` survives while a weak
///   `a.b.c` overridden by the strong layer is replaced).
///
/// The result is that the merge is DEEP / per-leaf where shapes AGREE (an
/// instance `a.b` overrides only `a.b`, a kind-level sibling `a.c` survives — the
/// AC4 guard), the tree is never self-contradictory where shapes DISAGREE, and
/// every surviving leaf's recorded [`SourceLayer`] reflects the layer that
/// actually DEFINES it (no stale/orphan provenance — the 2-3 seam stays honest).
pub fn resolve(layers: [ConfigLayer; 4]) -> EffectiveConfig {
    // Build one merged tree, folding weakest → strongest so a stronger layer's
    // shape wins and prunes at each node.
    let mut root: BTreeMap<String, MergedNode> = BTreeMap::new();
    for (layer, source) in layers.iter().zip(SourceLayer::ORDER) {
        merge_table_into(&mut root, layer.as_table(), source);
    }
    // Flatten the merged tree once to dotted leaves (each already tagged with the
    // layer that defines it).
    let mut leaves: BTreeMap<String, ResolvedValue> = BTreeMap::new();
    for (key, node) in &root {
        node.flatten_into(key.clone(), &mut leaves);
    }
    EffectiveConfig { leaves }
}

/// One node of the merged config tree during [`resolve`]: either a resolved LEAF
/// (a scalar/array value + the layer that defines it) or a SUBTREE (nested
/// children). Modeling the merge as a tree — rather than flattening each layer
/// independently — is what lets a stronger layer's SHAPE prune a weaker layer's
/// orphaned leaves on a scalar↔subtree collision.
enum MergedNode {
    /// A leaf value defined by `source`.
    Leaf { value: Value, source: SourceLayer },
    /// A subtree of child nodes, keyed by segment (sorted for determinism).
    Subtree(BTreeMap<String, MergedNode>),
}

impl MergedNode {
    /// Flatten this merged node into dotted-key leaves under `prefix`. An empty
    /// subtree contributes no leaves (config values live at leaves; 2-1 never
    /// represents an explicitly empty table as a value).
    fn flatten_into(&self, prefix: String, out: &mut BTreeMap<String, ResolvedValue>) {
        match self {
            MergedNode::Leaf { value, source } => {
                out.insert(
                    prefix,
                    ResolvedValue {
                        value: value.clone(),
                        source: *source,
                    },
                );
            }
            MergedNode::Subtree(children) => {
                for (key, child) in children {
                    let dotted = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    child.flatten_into(dotted, out);
                }
            }
        }
    }
}

/// Merge a TOML `table` from `source` INTO the subtree map `children`, weakest
/// layers merged first. A stronger layer's contribution wins per key, its SHAPE
/// pruning the weaker layer's orphans:
/// * a strong SUBTREE ensures the slot is a subtree (masking any weaker leaf),
///   then recurses so weak siblings survive while overlapping leaves are
///   overridden;
/// * a strong LEAF REPLACES whatever was in the slot — a weaker leaf OR a weaker
///   subtree — pruning that subtree's now-orphaned leaves so the resolved tree is
///   never self-contradictory and provenance never goes stale.
fn merge_table_into(
    children: &mut BTreeMap<String, MergedNode>,
    table: &toml::value::Table,
    source: SourceLayer,
) {
    for (key, value) in table {
        match value {
            Value::Table(child_table) => {
                let slot = children
                    .entry(key.clone())
                    .or_insert_with(|| MergedNode::Subtree(BTreeMap::new()));
                // If the weaker slot was a leaf, mask it with a fresh subtree.
                if !matches!(slot, MergedNode::Subtree(_)) {
                    *slot = MergedNode::Subtree(BTreeMap::new());
                }
                let MergedNode::Subtree(slot_children) = slot else {
                    unreachable!("slot was just ensured to be a subtree");
                };
                merge_table_into(slot_children, child_table, source);
            }
            leaf => {
                children.insert(
                    key.clone(),
                    MergedNode::Leaf {
                        value: leaf.clone(),
                        source,
                    },
                );
            }
        }
    }
}

/// The 2-1 KNOWN-KEY set: the unified config keys this story recognizes
/// (AC6/AC7). Deliberately SMALL and honest — the unified schema GROWS additively
/// in later stories; 2-1 needs only enough to make write-time validation + the
/// nearest-key suggestion testable.
///
/// `[ASSUMPTION]` recorded (the exact set + rationale):
/// * `model` — the SOLE known writable key in 2-1: a representative agent-native
///   config key (the one story 2-2 will map into an agent's native mechanism) and
///   the driver of the `modle`→`model` suggestion test.
///
/// HONESTY RULE (review decision #1, Islam): 2-1 ships ONLY keys it can honestly
/// honor. `restart.policy` was DROPPED from this set — the reaper reads the
/// Restart Policy from the SQLite spawn record, not config, so a
/// `config set restart.policy` would have been a misleading no-op. Wiring
/// config → engine-runtime (a config-sourced Restart Policy) is deferred to a
/// later story; until then config does not advertise a key it does not control.
///
/// Adding unified keys later is purely additive (a new entry here); 2-1 does NOT
/// freeze the schema. Keys are compared as full DOTTED paths. (The resolver
/// itself is key-agnostic — it merges whatever TOML the layers hold; this set
/// governs only WRITE validation.)
///
/// Story 3-2 (AD-7/AD-9) ADDS the three engine-namespace Token-Budget keys
/// ([`BUDGET_TOKENS_PER_RUN_KEY`], [`BUDGET_TOKENS_CUMULATIVE_KEY`],
/// [`BUDGET_BREACH_ACTION_KEY`]) — validated keys whose VALUES are additionally
/// type-checked by [`validate_write`] (a budget number must parse as `u64`, the
/// breach action as [`BreachAction`]); they are NOT `agent.*` pass-through.
///
/// Story 3-3 (AD-8/AD-9) ADDS the four engine-namespace dollar keys
/// ([`COST_RATE_INPUT_KEY`], [`COST_RATE_OUTPUT_KEY`], [`BUDGET_DOLLARS_PER_RUN_KEY`],
/// [`BUDGET_DOLLARS_CUMULATIVE_KEY`]) — validated keys whose VALUES are dollar
/// strings parsed to micros + type-checked by [`validate_write`] (a malformed
/// dollar value or one with sub-micro precision is rejected at write time, never
/// silently defaulted); they are NOT `agent.*` pass-through.
const KNOWN_KEYS: &[&str] = &[
    "model",
    BUDGET_TOKENS_PER_RUN_KEY,
    BUDGET_TOKENS_CUMULATIVE_KEY,
    BUDGET_BREACH_ACTION_KEY,
    COST_RATE_INPUT_KEY,
    COST_RATE_OUTPUT_KEY,
    BUDGET_DOLLARS_PER_RUN_KEY,
    BUDGET_DOLLARS_CUMULATIVE_KEY,
    // Story 3-4 (engine-observed metering): the operator-set real-upstream URL, and
    // the engine-injected loopback base_url the adapter's mapping delivers. Both are
    // engine-namespace keys (NOT `agent.*` pass-through); neither touches the Adapter
    // Contract surface (no CONTRACT_VERSION bump).
    METERING_UPSTREAM_BASE_URL_KEY,
    METERING_BASE_URL_KEY,
    // Story 5-1 (Memory Backing, AD-11 Delivery clause): the engine-injected managed
    // memory directory path the adapter's mapping delivers. Engine-namespace (NOT
    // `agent.*` pass-through); does NOT touch the Adapter Contract surface (no
    // CONTRACT_VERSION bump). The operator never sets it — it is a delivery
    // mechanism, not operator configuration, and is never persisted into the snapshot.
    MEMORY_DIR_KEY,
];

/// Whether `key` is a recognized unified config key (an exact dotted-path match
/// against [`KNOWN_KEYS`]).
fn is_known_key(key: &str) -> bool {
    KNOWN_KEYS.contains(&key)
}

/// Whether `key` lives under the reserved `agent.*` pass-through namespace (AC7).
/// Requires a non-empty child after the prefix (so a bare `agent` is NOT
/// pass-through — it would be an ordinary unknown key).
///
/// PUBLIC (story 2-2): both the start-seam mapping application (which delivers a
/// pass-through leaf VERBATIM) and `kt`'s `config get` (which marks a pass-through
/// leaf as "unvalidated") ask this ONE boundary rather than re-implementing the
/// prefix check — so the `agent.*` namespace has a single source of truth. `kt`
/// reaches it through the [`EffectiveConfig::is_unvalidated`] accessor (it never
/// re-parses config — AD-2); the engine's mapping application calls it directly.
pub fn is_pass_through(key: &str) -> bool {
    key.strip_prefix(PASS_THROUGH_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// The pass-through KEY-TAIL: the part of a `agent.*` key AFTER the `agent.`
/// prefix (story 2-2). Returns `Some("foo.bar")` for `agent.foo.bar`, `None` for a
/// non-pass-through key. This is the VERBATIM name delivered into the native
/// mechanism (AC6: no rewriting of the tail). Reuses [`is_pass_through`]'s
/// non-empty-child rule (a bare `agent` yields `None`).
pub fn pass_through_tail(key: &str) -> Option<&str> {
    key.strip_prefix(PASS_THROUGH_PREFIX)
        .filter(|rest| !rest.is_empty())
}

/// Whether a resolved leaf VALUE is a `secret:NAME` reference (story 2-4, AC4).
/// A value is secret-classified iff its string form begins with [`SECRET_PREFIX`]
/// followed by a NON-empty `NAME` — mirroring [`is_pass_through`]'s non-empty-child
/// rule (a bare `secret:` with no name is NOT a secret reference; recorded
/// Assumption 1). This ONE predicate governs BOTH resolution (at start, in the
/// supervisor) and masking (at display, in [`ResolvedValue::display`]), so the two
/// can never disagree — a value is secret for masking exactly when it is secret for
/// resolution. Operates on the VALUE string (not a key); the classified value's
/// key is irrelevant (a `secret:` under `model` classifies exactly like one under
/// `agent.foo`).
pub fn is_secret_ref(value: &str) -> bool {
    value
        .strip_prefix(SECRET_PREFIX)
        .is_some_and(|name| !name.is_empty())
}

/// The secret NAME: the lookup key AFTER the `secret:` prefix (story 2-4, AC4/AC5).
/// Returns `Some("OPENAI_KEY")` for `secret:OPENAI_KEY`, `None` for a non-secret or
/// bare-`secret:` value. This is the key the [`super::super::ports::SecretResolver`]
/// looks up (env var name / secrets-file key). Reuses [`is_secret_ref`]'s
/// non-empty-child rule, so it is the exact mirror of [`pass_through_tail`].
pub fn secret_name(value: &str) -> Option<&str> {
    value
        .strip_prefix(SECRET_PREFIX)
        .filter(|name| !name.is_empty())
}

/// Validate a config WRITE at write time (spine AD-9, AC-B / AC6 / AC7), BEFORE
/// anything is persisted — a rejected write must leave the instance config
/// byte-unchanged (the caller enforces the "validate then persist" ordering; this
/// function is the pure gate).
///
/// A key is VALID when it is a known unified key ([`KNOWN_KEYS`]) OR it lives
/// under the `agent.*` pass-through namespace (AC7 — pass-through keys skip the
/// known-key check and round-trip verbatim; the native mapping is 2-2). Any other
/// key is REJECTED with [`ConfigError::UnknownKey`] naming the offending key and
/// carrying the nearest known key (or `None` when nothing is within
/// [`SUGGESTION_MAX_DISTANCE`], so the diagnostic says "no close match" rather
/// than suggesting nonsense).
///
/// `value` is inspected ONLY for the keys that carry a typed contract — the
/// story-3-2 Token-Budget keys (AC-C: an unknown Breach-Action string or a
/// malformed budget number is rejected at WRITE time with a clear diagnostic,
/// never silently defaulted). For every OTHER known key (e.g. `model`) and for
/// `agent.*` pass-through keys the value is NOT inspected: validation is the KEY
/// namespace only. In particular a `secret:NAME` VALUE is stored verbatim as an
/// ordinary TOML string — write-time validation neither resolves nor rejects it
/// (story 2-4 resolves + masks a `secret:` value at START and DISPLAY, not at
/// write; the reference is what is persisted, AD-10). So a
/// `set model secret:OPENAI_KEY` is accepted here exactly like any other
/// known-key write.
///
/// A key with an EMPTY dotted segment (`agent..b`, `agent.foo.`, `.x`, a bare
/// `.`) is rejected up front (review patch #5): an empty segment would otherwise
/// persist a `""` key in the TOML tree — a malformed, un-addressable key. It is
/// reported as an [`ConfigError::UnknownKey`] with no suggestion (the shape is
/// wrong, not a near-miss of a known key).
pub fn validate_write(key: &str, value: &str) -> Result<(), ConfigError> {
    // Reject empty dotted segments first (a malformed key shape). An empty `key`
    // has one empty segment and is caught here too.
    if has_empty_segment(key) {
        return Err(ConfigError::UnknownKey {
            key: key.to_string(),
            suggestion: None,
        });
    }
    if is_known_key(key) {
        // The story-3-2 budget keys + the story-3-3 dollar keys additionally
        // TYPE-CHECK their value at write time (never silently defaulted). `model`
        // (and any future value-free known key) skips this.
        validate_budget_value(key, value)?;
        validate_cost_value(key, value)?;
        return Ok(());
    }
    if is_pass_through(key) {
        return Ok(());
    }
    Err(ConfigError::UnknownKey {
        key: key.to_string(),
        suggestion: nearest_known_key(key),
    })
}

/// Type-check the VALUE of a story-3-2 Token-Budget key at write time (AC-C).
/// A budget number (`budget.tokens.*`) must parse as a `u64`; the breach action
/// (`budget.breach_action`) must parse as a [`BreachAction`]. A non-budget key is
/// a no-op (its value is not inspected). Rejects with [`ConfigError::InvalidValue`]
/// naming the key + reason so `kt` renders a remediation (the write is rejected
/// BEFORE any persistence — the "validate then persist" atomicity).
fn validate_budget_value(key: &str, value: &str) -> Result<(), ConfigError> {
    match key {
        BUDGET_TOKENS_PER_RUN_KEY | BUDGET_TOKENS_CUMULATIVE_KEY => value
            .trim()
            .parse::<u64>()
            .map(|_| ())
            .map_err(|_| ConfigError::InvalidValue {
                key: key.to_string(),
                value: value.to_string(),
                reason: "expected a non-negative whole number of tokens (u64)".to_string(),
            }),
        BUDGET_BREACH_ACTION_KEY => value
            .trim()
            .parse::<BreachAction>()
            .map(|_| ())
            .map_err(|e| ConfigError::InvalidValue {
                key: key.to_string(),
                value: value.to_string(),
                reason: e.to_string(),
            }),
        _ => Ok(()),
    }
}

/// Type-check the VALUE of a story-3-3 dollar key at write time (AC-A/AC-B/AC11).
/// The Rate directions (`cost.rate.*`) and the dollar cap scopes
/// (`budget.dollars.*`) are all DOLLAR STRINGS parsed to micro-dollars: a malformed
/// value (non-numeric, negative, or sub-micro precision) is rejected BEFORE any
/// persistence with a clear diagnostic (never silently defaulted — the honesty
/// rule; a bad value must not crash ingestion, so we gate it at write). A non-cost
/// key is a no-op.
fn validate_cost_value(key: &str, value: &str) -> Result<(), ConfigError> {
    match key {
        COST_RATE_INPUT_KEY
        | COST_RATE_OUTPUT_KEY
        | BUDGET_DOLLARS_PER_RUN_KEY
        | BUDGET_DOLLARS_CUMULATIVE_KEY => {
            parse_dollars_to_micros(value)
                .map(|_| ())
                .map_err(|reason| ConfigError::InvalidValue {
                    key: key.to_string(),
                    value: value.to_string(),
                    reason,
                })
        }
        _ => Ok(()),
    }
}

/// Parse a DOLLAR STRING (e.g. `"3.00"`, `"0"`, `"15"`, `"0.000003"`) into
/// integer micro-dollars (story 3-3 — the money-parse crux, AC11). PURE + total.
///
/// Recorded decision (the config leaf is a decimal-dollar STRING parsed to micros,
/// NOT a raw micros integer — the operator writes `"3.00"`, not `3000000`): a
/// human-facing Rate is a dollar figure. Rules (rejected → `Err(reason)`, never a
/// silent default):
///
/// * At most ONE `.`; a leading/trailing sign is NOT accepted (a Rate/cap is
///   non-negative — a `-` is rejected).
/// * The fractional part is at most 6 digits (the micro-dollar quantum). MORE than
///   6 fractional digits is SUB-MICRO precision and is REJECTED (a clear
///   diagnostic) rather than silently truncated — the story's "reject a value with
///   sub-micro precision".
/// * The integer and fractional parts must be ASCII digits (an empty integer part,
///   as in `".5"`, is treated as `0`; an empty fractional part, as in `"3."`, is
///   treated as `0`). A non-digit (`"abc"`, `"3.0x"`) is rejected.
/// * The whole-dollar magnitude must fit in the micro-dollar `i64` domain (a
///   value above ~9.2e12 dollars overflows and is rejected — implausible for a
///   real Rate/cap, so the ceiling loses nothing real).
pub fn parse_dollars_to_micros(value: &str) -> Result<Micros, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("expected a dollar amount like \"3.00\", got an empty value".to_string());
    }
    // No sign: a Rate/cap is non-negative.
    if trimmed.starts_with('-') || trimmed.starts_with('+') {
        return Err(format!(
            "'{trimmed}' is not a valid dollar amount (a Rate/cap must be non-negative, no sign)"
        ));
    }
    let mut parts = trimmed.splitn(2, '.');
    let int_part = parts.next().unwrap_or("");
    let frac_part = parts.next();
    // A second '.' would remain in frac_part; reject a multi-dot value.
    if let Some(frac) = frac_part {
        if frac.contains('.') {
            return Err(format!(
                "'{trimmed}' is not a valid dollar amount (more than one decimal point)"
            ));
        }
    }
    // Integer dollars: an empty int part (".5") is 0.
    let dollars: u64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse::<u64>().map_err(|_| {
            format!(
                "'{trimmed}' is not a valid dollar amount (the whole-dollar part is not a number)"
            )
        })?
    };
    // Fractional micros: pad/reject to exactly 6 digits (the micro quantum).
    let frac_micros: u64 = match frac_part {
        None | Some("") => 0,
        Some(frac) => {
            if !frac.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!(
                    "'{trimmed}' is not a valid dollar amount (the fractional part is not a number)"
                ));
            }
            if frac.len() > 6 {
                return Err(format!(
                    "'{trimmed}' has sub-micro-dollar precision (more than 6 decimal places); \
                     the finest supported unit is one micro-dollar ($0.000001)"
                ));
            }
            // Right-pad to 6 digits so "3" fractional means 300_000 micros, "000003"
            // means 3 micros.
            let padded = format!("{frac:0<6}");
            padded.parse::<u64>().map_err(|_| {
                format!("'{trimmed}' is not a valid dollar amount (the fractional part is not a number)")
            })?
        }
    };
    // dollars × 1e6 + frac_micros, in i64, rejecting an overflow (implausible Rate).
    let micros = (dollars as i128)
        .checked_mul(MICROS_PER_DOLLAR as i128)
        .and_then(|d| d.checked_add(frac_micros as i128))
        .filter(|m| *m <= i64::MAX as i128)
        .ok_or_else(|| {
            format!("'{trimmed}' is too large to represent as micro-dollars (max ~$9.2 trillion)")
        })?;
    Ok(Micros(micros as i64))
}

/// Resolve the current [`TokenBudget`] + [`BreachAction`] from an already-resolved
/// [`EffectiveConfig`] (story 3-2, AC-B/AC-C — the LIVE read the evaluator calls on
/// EACH ingestion).
///
/// A read of the CURRENT resolved config (NOT a start-time snapshot): a budget
/// changed while `running` is reflected on the very next `UsageEvent` (AC-B
/// "changes apply immediately") because the supervisor re-resolves + re-reads here
/// each time. Absent scopes → `None` (never breach); an absent action →
/// [`BreachAction::default`] (`pause`).
///
/// ROBUSTNESS (AD-12 — enforcement must never crash ingestion): a value that is
/// somehow present but MALFORMED (e.g. a negative or non-integer that slipped past
/// write-validation via a hand-edited `config.toml`, or an unknown action string)
/// is treated as ABSENT for that scope/action rather than erroring — the honest
/// degrade is "no ceiling / default action", never a panic mid-ingestion. Write
/// validation ([`validate_write`]) is the primary gate; this read is defensive.
pub fn resolve_token_budget(effective: &EffectiveConfig) -> (TokenBudget, BreachAction) {
    let per_run = budget_u64(effective, BUDGET_TOKENS_PER_RUN_KEY);
    let cumulative = budget_u64(effective, BUDGET_TOKENS_CUMULATIVE_KEY);
    let action = effective
        .value(BUDGET_BREACH_ACTION_KEY)
        .and_then(|v| v.as_str())
        .and_then(|s| s.trim().parse::<BreachAction>().ok())
        .unwrap_or_default();
    (
        TokenBudget {
            per_run,
            cumulative,
        },
        action,
    )
}

/// Read a `u64` budget ceiling from a resolved leaf, coercing the TOML value
/// (story 3-2). Accepts a TOML integer (the natural `set` form) or a numeric
/// STRING (a `secret:`-free string leaf), rejecting a negative / non-integer as
/// ABSENT (the defensive degrade — see [`resolve_token_budget`]).
fn budget_u64(effective: &EffectiveConfig, key: &str) -> Option<u64> {
    let value = effective.value(key)?;
    match value {
        // A TOML integer: accept only a non-negative value (a negative ceiling is
        // meaningless — treat as absent).
        Value::Integer(i) => u64::try_from(*i).ok(),
        // A string leaf that parses as u64 (defensive; the `set` path stores a
        // number, but a hand-edited quoted value still resolves honestly).
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// Resolve the current [`Rate`] (per-direction, if BOTH set) + [`CostCap`] +
/// [`BreachAction`] from an already-resolved [`EffectiveConfig`] (story 3-3,
/// AC-A/AC-B/AC-C/AC11 — the LIVE read the dollar enforcement calls on EACH
/// ingestion, mirroring [`resolve_token_budget`]).
///
/// A read of the CURRENT resolved config (NOT a start-time snapshot): a Rate/cap
/// changed while `running` is reflected on the very next `UsageEvent` (AC-A "a Rate
/// change re-prices FUTURE consumption only"; AC-B "changeable while running")
/// because the supervisor re-resolves + re-reads here each time. The Rate REQUIRES
/// BOTH directions to be "supplied" (Key design decision 2): a half-configured Rate
/// (only input, or only output) resolves to `None` — inert, treated as no-Rate
/// (AC-B), avoiding a silently-half-priced ledger. Absent cap scopes → `None`
/// (never breach); the SAME `budget.breach_action` key governs a dollar breach
/// (one action for both dimensions).
///
/// ROBUSTNESS (AD-12 — enforcement must never crash ingestion): a value that is
/// somehow present but MALFORMED (slipped past write-validation via a hand-edited
/// `config.toml`) is treated as ABSENT for that direction/scope rather than
/// erroring — the honest degrade is "no Rate / no ceiling", never a panic
/// mid-ingestion. [`validate_write`] is the primary gate; this read is defensive.
pub fn resolve_cost(effective: &EffectiveConfig) -> (Option<Rate>, CostCap, BreachAction) {
    // Both directions required for a "supplied" Rate (AC6/AC-B). A missing or
    // malformed direction → no Rate (inert).
    let input = cost_micros_leaf(effective, COST_RATE_INPUT_KEY);
    let output = cost_micros_leaf(effective, COST_RATE_OUTPUT_KEY);
    let rate = match (input, output) {
        (Some(i), Some(o)) => Some(Rate::new(micros_to_u64(i), micros_to_u64(o))),
        _ => None,
    };
    let cap = CostCap {
        per_run: cost_micros_leaf(effective, BUDGET_DOLLARS_PER_RUN_KEY),
        cumulative: cost_micros_leaf(effective, BUDGET_DOLLARS_CUMULATIVE_KEY),
    };
    // The SAME breach-action key governs dollar breaches too (story 3-3): resolve it
    // exactly as `resolve_token_budget` does (absent → the ratified `pause` default).
    let action = effective
        .value(BUDGET_BREACH_ACTION_KEY)
        .and_then(|v| v.as_str())
        .and_then(|s| s.trim().parse::<BreachAction>().ok())
        .unwrap_or_default();
    (rate, cap, action)
}

/// Resolve the ENGINE-OBSERVED upstream provider base URL from an already-resolved
/// [`EffectiveConfig`] (story 3-4, AC6) — the operator's real OpenAI-compatible
/// endpoint the loopback listener forwards to. Returns the trimmed string value of
/// [`METERING_UPSTREAM_BASE_URL_KEY`], or `None` when unset / not a string /
/// empty. A secret-classified value is NOT a URL → `None` (defensive; the URL is
/// not a secret). The listener validates it is a usable `http://` or `https://`
/// URL at start (12-3: https dials vendored rustls+ring); this only reads the leaf.
pub fn resolve_upstream_base_url(effective: &EffectiveConfig) -> Option<String> {
    let value = effective.value(METERING_UPSTREAM_BASE_URL_KEY)?;
    match value {
        Value::String(s) if !is_secret_ref(s) && !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Read a dollar-string leaf into [`Micros`], coercing the TOML value (story 3-3,
/// the defensive resolve). Accepts a TOML string (the `set` path stores the dollar
/// string verbatim) parsed via [`parse_dollars_to_micros`], or a bare TOML integer
/// interpreted as WHOLE DOLLARS (a hand-edited `budget.dollars.cumulative = 5`
/// resolves to $5). A malformed / negative value → `None` (absent — the defensive
/// degrade). A `secret:`-classified string is not a dollar value → `None`.
fn cost_micros_leaf(effective: &EffectiveConfig, key: &str) -> Option<Micros> {
    let value = effective.value(key)?;
    match value {
        // The `set` path stores the dollar string; parse it to micros.
        Value::String(s) if !is_secret_ref(s) => parse_dollars_to_micros(s).ok(),
        // A hand-edited bare integer means whole dollars (defensive convenience).
        Value::Integer(i) => u64::try_from(*i)
            .ok()
            .and_then(|d| (d as i128).checked_mul(MICROS_PER_DOLLAR as i128))
            .filter(|m| *m <= i64::MAX as i128)
            .map(|m| Micros(m as i64)),
        _ => None,
    }
}

/// The non-negative micro count of a [`Micros`] as `u64` for the [`Rate`] fields
/// (which are `u64` micros-per-1M). A cost value is non-negative; a defensive
/// negative clamps to 0.
fn micros_to_u64(m: Micros) -> u64 {
    u64::try_from(m.get()).unwrap_or(0)
}

/// Whether a dotted key has any EMPTY segment (a leading/trailing/doubled dot, or
/// an empty key). Splitting `"a..b"` yields `["a", "", "b"]`; splitting `""`
/// yields `[""]` — both have an empty segment. Used to fail malformed keys before
/// they reach the store (review patch #5).
fn has_empty_segment(key: &str) -> bool {
    key.split('.').any(str::is_empty)
}

/// The nearest known key to `key` within [`SUGGESTION_MAX_DISTANCE`] edit
/// distance, or `None` when the closest known key is farther than the threshold
/// (AC6: "no close match" rather than suggesting nonsense).
///
/// Ties (two known keys equidistant from `key`) break on the CANDIDATE STRING
/// (lexicographically smallest wins) — NOT on [`KNOWN_KEYS`] array order — so the
/// suggestion is deterministic and stable regardless of how the array is ordered
/// or later reordered (review patch #6). Keying on `(distance, candidate)` makes
/// `min_by_key` total and order-independent.
fn nearest_known_key(key: &str) -> Option<String> {
    KNOWN_KEYS
        .iter()
        .map(|candidate| (levenshtein(key, candidate), *candidate))
        .filter(|(distance, _)| *distance <= SUGGESTION_MAX_DISTANCE)
        .min_by(|(da, ca), (db, cb)| da.cmp(db).then_with(|| ca.cmp(cb)))
        .map(|(_, candidate)| candidate.to_string())
}

/// A tiny hand-rolled Levenshtein edit distance (AC6 — RECOMMENDED "no new heavy
/// dependency"). Classic two-row dynamic-programming table over Unicode scalar
/// values; adequate for the short ASCII config keys 2-1 compares. PURE.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // `prev[j]` = distance between a[..i] and b[..j]; rolled two-row table.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr: Vec<usize> = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let substitution_cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1) // deletion
                .min(curr[j] + 1) // insertion
                .min(prev[j] + substitution_cost); // substitution
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Errors from the unified-config surface (spine AD-9, story 2-1). `thiserror`,
/// never `miette` — `kt` wraps these into diagnostics (conventions). Each variant
/// names the offending key/layer/path so `kt` can render a remediation (NFR-1),
/// mirroring [`super::error::RegistryError`].
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A config write targeted an unknown key OUTSIDE the `agent.*` pass-through
    /// namespace (AC-B). NAMES the offending key and carries the nearest valid
    /// known key (`None` when nothing is within [`SUGGESTION_MAX_DISTANCE`]). The
    /// write is rejected BEFORE any persistence — nothing is written.
    #[error("unknown config key '{key}'{}", suggestion_hint(.suggestion))]
    UnknownKey {
        /// The rejected key.
        key: String,
        /// The nearest known key, if one is within the suggestion threshold.
        suggestion: Option<String>,
    },

    /// A config write would place a child UNDER an existing scalar value — e.g.
    /// setting `agent.a.b` when `agent.a` is already a scalar (review patch #3).
    /// Accepting it would silently DESTROY the existing scalar; instead the write
    /// FAILS CLOSED here, BEFORE any persistence, leaving the instance config
    /// byte-unchanged (AC-B atomicity). NAMES the requested key + the conflicting
    /// ancestor so the operator can unset/rename it first.
    #[error(
        "cannot set config key '{key}': '{conflicting_ancestor}' is already a value, not a table; \
         unset or rename it first"
    )]
    WriteShapeConflict {
        /// The key the write requested.
        key: String,
        /// The ancestor segment that is a scalar (so a child cannot be nested).
        conflicting_ancestor: String,
    },

    /// A config write supplied a VALUE that failed the key's typed contract
    /// (story 3-2, AC-C) — a `budget.tokens.*` value that is not a `u64`, or a
    /// `budget.breach_action` that is not `pause`/`stop`/`warn`. Rejected BEFORE
    /// any persistence (the instance `config.toml` is left byte-unchanged), NAMES
    /// the key + the offending value + the reason so `kt` renders a remediation.
    /// (Distinct from [`ConfigError::UnknownKey`]: the KEY is known/valid — its
    /// VALUE is wrong.)
    #[error("invalid value '{value}' for config key '{key}': {reason}")]
    InvalidValue {
        /// The (known) key whose value was rejected.
        key: String,
        /// The offending value.
        value: String,
        /// Why it was rejected (the expected type / accepted set).
        reason: String,
    },

    /// A config layer's TOML failed to parse OR the instance layer could not be
    /// read/written/serialized (AC8). NAMES the layer + source path so the
    /// diagnostic can point at it; never a panic on malformed input or I/O
    /// failure.
    #[error("the {layer} config layer at {path} is not valid TOML: {detail}")]
    MalformedLayer {
        /// Which of the four layers failed.
        layer: SourceLayer,
        /// The source path (a file path, or `<engine-defaults>` for the embedded
        /// constant).
        path: String,
        /// The underlying parse / I/O detail.
        detail: String,
    },

    /// The supplied instance name failed the naming rule at the facade (before
    /// any layer is touched). NAMES the candidate + the reason (as a string, so
    /// this domain type does not couple to `NameError` — it mirrors the other
    /// facades' shape while keeping the config surface self-contained).
    #[error("invalid Agent Instance name '{name}': {reason}")]
    InvalidName {
        /// The rejected candidate string.
        name: String,
        /// The specific rule that failed (rendered).
        reason: String,
    },

    /// A config operation targeted an instance that is not registered. NAMES it
    /// (mirrors [`super::error::RegistryError::NotFound`]) so the config surface
    /// never leaks a registry error type across the AD-1 boundary.
    #[error("no Agent Instance named '{name}' is registered")]
    NotFound {
        /// The missing instance name.
        name: String,
    },

    /// The state store could not be consulted while resolving/setting config
    /// (e.g. the instance-existence check failed). NAMES the instance + detail.
    #[error("state store error for Agent Instance '{name}': {detail}")]
    Store {
        /// The instance the operation was for.
        name: String,
        /// The underlying store error detail.
        detail: String,
    },

    /// A `secret:NAME` reference could not be REVEALED for a `config get --reveal`
    /// read (story 2-4, AC-C/AC11). The `--reveal` flag re-resolves secrets LIVE
    /// through the resolver; a resolution failure here is a DIAGNOSTIC on the read
    /// surface (stderr, non-zero exit), NEVER a crash and NEVER a leak — `detail`
    /// carries the [`crate::ports::SecretError`] message, which names the `NAME` +
    /// resolvers tried but no value. Kept on the config-surface error so `kt`'s
    /// `config get` maps it consistently with the other config diagnostics.
    #[error("could not reveal a secret for the config read: {detail}")]
    SecretReveal {
        /// The underlying secret-resolution detail (names the NAME + resolvers,
        /// never a value).
        detail: String,
    },
}

/// Render the trailing "; did you mean 'X'?" / "; no close match" hint for a
/// [`ConfigError::UnknownKey`] message from the optional suggestion.
fn suggestion_hint(suggestion: &Option<String>) -> String {
    match suggestion {
        Some(nearest) => format!("; did you mean '{nearest}'?"),
        None => "; no close match among the known keys".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a single-layer input with the other three layers empty, at a chosen
    /// precedence slot, from TOML text.
    fn one_layer(source: SourceLayer, text: &str) -> [ConfigLayer; 4] {
        let mut layers = [
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
        ];
        let idx = SourceLayer::ORDER
            .iter()
            .position(|s| *s == source)
            .unwrap();
        layers[idx] = ConfigLayer::parse(source, "<test>", text).unwrap();
        layers
    }

    // ---- SourceLayer precedence ordering (the encoded AD-9 order) ----

    #[test]
    fn source_layer_order_encodes_precedence_weakest_to_strongest() {
        // AD-9: engine < kind < instance < invocation. The derived Ord must agree.
        assert!(SourceLayer::EngineDefault < SourceLayer::KindDefault);
        assert!(SourceLayer::KindDefault < SourceLayer::Instance);
        assert!(SourceLayer::Instance < SourceLayer::InvocationOverride);
        // ORDER lists them weakest-first and is strictly increasing.
        let ordered = SourceLayer::ORDER;
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "{:?} !< {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn source_layer_wire_form_round_trips() {
        for layer in SourceLayer::ORDER {
            let json = serde_json::to_string(&layer).unwrap();
            // as_str() matches the kebab-case serde form (no divergence).
            assert_eq!(json, format!("\"{}\"", layer.as_str()));
            let back: SourceLayer = serde_json::from_str(&json).unwrap();
            assert_eq!(back, layer);
            assert_eq!(layer.to_string(), layer.as_str());
        }
    }

    // ---- resolve(): key present in exactly one layer (×4) ----

    #[test]
    fn key_in_only_engine_default_resolves_to_it() {
        let eff = resolve(one_layer(SourceLayer::EngineDefault, "model = \"a\""));
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("a".into()));
        assert_eq!(r.source, SourceLayer::EngineDefault);
    }

    #[test]
    fn key_in_only_kind_default_resolves_to_it() {
        let eff = resolve(one_layer(SourceLayer::KindDefault, "model = \"b\""));
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("b".into()));
        assert_eq!(r.source, SourceLayer::KindDefault);
    }

    #[test]
    fn key_in_only_instance_resolves_to_it() {
        let eff = resolve(one_layer(SourceLayer::Instance, "model = \"c\""));
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("c".into()));
        assert_eq!(r.source, SourceLayer::Instance);
    }

    #[test]
    fn key_in_only_invocation_override_resolves_to_it() {
        let eff = resolve(one_layer(SourceLayer::InvocationOverride, "model = \"d\""));
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("d".into()));
        assert_eq!(r.source, SourceLayer::InvocationOverride);
    }

    // ---- resolve(): each adjacent precedence pair (stronger wins) ----

    /// Build a two-layer input (both slots set) from TOML text.
    fn two_layers(
        weak: SourceLayer,
        weak_text: &str,
        strong: SourceLayer,
        strong_text: &str,
    ) -> [ConfigLayer; 4] {
        let mut layers = [
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
        ];
        let wi = SourceLayer::ORDER.iter().position(|s| *s == weak).unwrap();
        let si = SourceLayer::ORDER
            .iter()
            .position(|s| *s == strong)
            .unwrap();
        layers[wi] = ConfigLayer::parse(weak, "<weak>", weak_text).unwrap();
        layers[si] = ConfigLayer::parse(strong, "<strong>", strong_text).unwrap();
        layers
    }

    #[test]
    fn kind_beats_engine_for_the_same_key() {
        let eff = two_layers(
            SourceLayer::EngineDefault,
            "model = \"engine\"",
            SourceLayer::KindDefault,
            "model = \"kind\"",
        );
        let eff = resolve(eff);
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("kind".into()));
        assert_eq!(r.source, SourceLayer::KindDefault);
    }

    #[test]
    fn instance_beats_kind_for_the_same_key_every_time() {
        // AC-A heart: the SAME key set at kind + instance resolves to instance.
        let eff = resolve(two_layers(
            SourceLayer::KindDefault,
            "model = \"kind\"",
            SourceLayer::Instance,
            "model = \"instance\"",
        ));
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("instance".into()));
        assert_eq!(r.source, SourceLayer::Instance);
    }

    #[test]
    fn invocation_override_beats_instance_for_the_same_key() {
        let eff = resolve(two_layers(
            SourceLayer::Instance,
            "model = \"instance\"",
            SourceLayer::InvocationOverride,
            "model = \"override\"",
        ));
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("override".into()));
        assert_eq!(r.source, SourceLayer::InvocationOverride);
    }

    // ---- resolve(): key in ALL FOUR layers (top wins) ----

    #[test]
    fn key_in_all_four_layers_resolves_to_the_strongest() {
        let layers = [
            ConfigLayer::parse(SourceLayer::EngineDefault, "<e>", "model = \"engine\"").unwrap(),
            ConfigLayer::parse(SourceLayer::KindDefault, "<k>", "model = \"kind\"").unwrap(),
            ConfigLayer::parse(SourceLayer::Instance, "<i>", "model = \"instance\"").unwrap(),
            ConfigLayer::parse(
                SourceLayer::InvocationOverride,
                "<o>",
                "model = \"override\"",
            )
            .unwrap(),
        ];
        let eff = resolve(layers);
        let r = eff.get("model").unwrap();
        assert_eq!(r.value, Value::String("override".into()));
        assert_eq!(r.source, SourceLayer::InvocationOverride);
        // Exactly one leaf (all four set the SAME key).
        assert_eq!(eff.len(), 1);
    }

    // ---- resolve(): DEEP / per-leaf merge (sibling survives) ----

    #[test]
    fn nested_table_merge_is_per_leaf_sibling_survives() {
        // AC4 subtle correctness: instance sets a.b; kind's a.c must SURVIVE (a
        // shallow whole-table replace would drop it — a data-loss bug).
        let layers = two_layers(
            SourceLayer::KindDefault,
            "[a]\nb = \"kind-b\"\nc = \"kind-c\"\n",
            SourceLayer::Instance,
            "[a]\nb = \"instance-b\"\n",
        );
        let eff = resolve(layers);
        // a.b overridden by instance...
        let ab = eff.get("a.b").unwrap();
        assert_eq!(ab.value, Value::String("instance-b".into()));
        assert_eq!(ab.source, SourceLayer::Instance);
        // ...a.c survives from kind (the per-leaf merge, NOT a table replace).
        let ac = eff.get("a.c").unwrap();
        assert_eq!(ac.value, Value::String("kind-c".into()));
        assert_eq!(ac.source, SourceLayer::KindDefault);
        assert_eq!(eff.len(), 2);
    }

    #[test]
    fn deeply_nested_dotted_keys_flatten_and_merge_per_leaf() {
        // Deeper nesting still merges per-leaf: engine deep.x.y and instance
        // deep.x.z coexist; instance deep.x.y wins.
        let layers = two_layers(
            SourceLayer::EngineDefault,
            "[deep.x]\ny = 1\nz = 2\n",
            SourceLayer::Instance,
            "[deep.x]\ny = 99\n",
        );
        let eff = resolve(layers);
        assert_eq!(eff.value("deep.x.y"), Some(&Value::Integer(99)));
        assert_eq!(eff.get("deep.x.y").unwrap().source, SourceLayer::Instance);
        assert_eq!(eff.value("deep.x.z"), Some(&Value::Integer(2)));
        assert_eq!(
            eff.get("deep.x.z").unwrap().source,
            SourceLayer::EngineDefault
        );
    }

    // ---- resolve(): empty-layer and all-empty cases ----

    #[test]
    fn a_single_empty_layer_contributes_nothing() {
        // A kind with no defaults (empty layer) does not error and adds no leaves;
        // the other layer resolves normally (AC8).
        let eff = resolve(two_layers(
            SourceLayer::KindDefault, // empty via parse of ""
            "",
            SourceLayer::Instance,
            "model = \"only-instance\"",
        ));
        assert_eq!(eff.len(), 1);
        assert_eq!(
            eff.value("model"),
            Some(&Value::String("only-instance".into()))
        );
    }

    #[test]
    fn all_empty_layers_resolve_to_an_empty_effective_map() {
        let eff = resolve([
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
            ConfigLayer::empty(),
        ]);
        assert!(eff.is_empty());
        assert_eq!(eff.len(), 0);
        assert_eq!(eff.value("anything"), None);
        assert!(eff.values().is_empty());
    }

    #[test]
    fn resolve_is_deterministic_for_the_same_inputs() {
        // The pure-function contract: same inputs → identical output (independent
        // of iteration nondeterminism — BTreeMap sorts, so this is stable).
        let build = || {
            two_layers(
                SourceLayer::EngineDefault,
                "b = 2\na = 1\n",
                SourceLayer::Instance,
                "c = 3\n",
            )
        };
        assert_eq!(resolve(build()), resolve(build()));
        // Values-only view is sorted + stable too.
        let vals = resolve(build()).values();
        let keys: Vec<&String> = vals.keys().collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_nested_table_contributes_no_leaves() {
        // A structural empty table `[a]` with nothing under it adds no leaf.
        let eff = resolve(one_layer(SourceLayer::Instance, "[a]\n"));
        assert!(eff.is_empty());
    }

    // ---- resolve(): SHAPE COLLISIONS — stronger shape wins + prunes (patch #2) ----

    #[test]
    fn strong_scalar_masks_weak_subtree_no_orphan_leaves() {
        // Review patch #2: a weak layer defines the SUBTREE [a] b=1, c=2; a strong
        // layer defines the SCALAR a="scalar". The strong scalar must WIN and PRUNE
        // the weak subtree — the resolved tree has a="scalar" (Instance) and NO
        // orphaned a.b / a.c. Never a self-contradictory a="scalar" + a.b=1.
        let layers = two_layers(
            SourceLayer::KindDefault,
            "[a]\nb = 1\nc = 2\n",
            SourceLayer::Instance,
            "a = \"scalar\"\n",
        );
        let eff = resolve(layers);
        // a is the strong scalar, tagged Instance.
        let a = eff.get("a").unwrap();
        assert_eq!(a.value, Value::String("scalar".into()));
        assert_eq!(a.source, SourceLayer::Instance);
        // The weak subtree leaves are PRUNED — no orphans, no stale provenance.
        assert_eq!(eff.get("a.b"), None);
        assert_eq!(eff.get("a.c"), None);
        assert_eq!(eff.len(), 1);
    }

    #[test]
    fn strong_subtree_replaces_weak_scalar_and_prunes_it() {
        // Symmetric: a weak SCALAR a="scalar"; a strong SUBTREE [a] b=1. The strong
        // subtree must REPLACE the weak scalar — the resolved tree has a.b=1
        // (Instance) and NO orphaned scalar `a`.
        let layers = two_layers(
            SourceLayer::KindDefault,
            "a = \"scalar\"\n",
            SourceLayer::Instance,
            "[a]\nb = 1\n",
        );
        let eff = resolve(layers);
        let ab = eff.get("a.b").unwrap();
        assert_eq!(ab.value, Value::Integer(1));
        assert_eq!(ab.source, SourceLayer::Instance);
        // The weak scalar `a` is gone (masked by the subtree).
        assert_eq!(eff.get("a"), None);
        assert_eq!(eff.len(), 1);
    }

    #[test]
    fn subtree_over_scalar_preserves_strong_siblings_and_prunes_weak_scalar() {
        // A strong subtree with MULTIPLE leaves over a weak scalar: all strong
        // leaves survive with correct provenance; the weak scalar is pruned.
        let layers = two_layers(
            SourceLayer::EngineDefault,
            "a = \"weak-scalar\"\n",
            SourceLayer::Instance,
            "[a]\nb = 1\nc = 2\n",
        );
        let eff = resolve(layers);
        assert_eq!(eff.value("a.b"), Some(&Value::Integer(1)));
        assert_eq!(eff.value("a.c"), Some(&Value::Integer(2)));
        assert_eq!(eff.get("a.b").unwrap().source, SourceLayer::Instance);
        assert_eq!(eff.get("a.c").unwrap().source, SourceLayer::Instance);
        assert_eq!(eff.get("a"), None);
        assert_eq!(eff.len(), 2);
    }

    #[test]
    fn scalar_over_subtree_across_three_layers_tracks_the_defining_layer() {
        // Provenance honesty under a collision spanning >2 layers: engine subtree
        // a.b, kind subtree a.c, instance SCALAR a. The instance scalar prunes BOTH
        // weaker subtrees; the surviving leaf is tagged Instance (the layer that
        // actually defines the winning shape) — no stale engine/kind provenance.
        let layers = [
            ConfigLayer::parse(SourceLayer::EngineDefault, "<e>", "[a]\nb = 1\n").unwrap(),
            ConfigLayer::parse(SourceLayer::KindDefault, "<k>", "[a]\nc = 2\n").unwrap(),
            ConfigLayer::parse(SourceLayer::Instance, "<i>", "a = \"wins\"\n").unwrap(),
            ConfigLayer::empty(),
        ];
        let eff = resolve(layers);
        assert_eq!(eff.value("a"), Some(&Value::String("wins".into())));
        assert_eq!(eff.get("a").unwrap().source, SourceLayer::Instance);
        assert_eq!(eff.get("a.b"), None);
        assert_eq!(eff.get("a.c"), None);
        assert_eq!(eff.len(), 1);
    }

    // ---- values-only view keeps provenance out (the 2-1 CLI surface) ----

    #[test]
    fn resolved_value_display_renders_strings_bare_and_others_inline() {
        // String → bare (no quotes); non-string scalars/arrays → TOML inline form.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "s = \"gpt-4\"\nn = 42\nb = true\narr = [1, 2]\n",
        ));
        assert_eq!(eff.value_display("s").as_deref(), Some("gpt-4"));
        assert_eq!(eff.value_display("n").as_deref(), Some("42"));
        assert_eq!(eff.value_display("b").as_deref(), Some("true"));
        assert_eq!(eff.value_display("arr").as_deref(), Some("[1, 2]"));
        // The ResolvedValue::display() method agrees with value_display().
        assert_eq!(eff.get("n").unwrap().display(), "42");
        // A missing key has no display.
        assert_eq!(eff.value_display("nope"), None);
    }

    // ---- Story 2-4: the secret:NAME classifier + extractor (AC4) ----

    #[test]
    fn is_secret_ref_requires_the_prefix_and_a_non_empty_name() {
        // A `secret:NAME` with a non-empty NAME classifies; a bare `secret:` does
        // NOT (Assumption 1 — ordinary opaque text, mirroring a bare `agent`);
        // a value without the prefix never classifies.
        assert!(is_secret_ref("secret:OPENAI_KEY"));
        assert!(is_secret_ref("secret:A")); // a single-char name is enough
        assert!(!is_secret_ref("secret:")); // bare prefix, no name → not a secret
        assert!(!is_secret_ref("not-a-secret"));
        assert!(!is_secret_ref("secretsomething")); // no colon → not the namespace
        assert!(!is_secret_ref("")); // empty value
                                     // The prefix must be at the START (a secret: mid-string is not a ref).
        assert!(!is_secret_ref("model=secret:X"));
    }

    #[test]
    fn secret_name_extracts_the_lookup_key_after_the_prefix() {
        // secret_name is the mirror of pass_through_tail: Some(NAME) for a real
        // reference, None for a bare prefix or a non-secret value.
        assert_eq!(secret_name("secret:OPENAI_KEY"), Some("OPENAI_KEY"));
        // A dotted / structured NAME is returned verbatim (the resolver's key).
        assert_eq!(secret_name("secret:MY.KEY"), Some("MY.KEY"));
        assert_eq!(secret_name("secret:"), None);
        assert_eq!(secret_name("not-a-secret"), None);
        assert_eq!(secret_name(""), None);
    }

    #[test]
    fn is_secret_ref_classifies_by_value_regardless_of_key() {
        // The classification is on the VALUE, not the key: a `secret:` value under
        // the known key `model` classifies exactly like one under `agent.foo`, so
        // resolution + masking agree wherever the reference sits.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "model = \"secret:MODEL_KEY\"\n[agent]\ntoken = \"secret:TOK\"\n",
        ));
        assert!(is_secret_ref(eff.value("model").unwrap().as_str().unwrap()));
        assert!(is_secret_ref(
            eff.value("agent.token").unwrap().as_str().unwrap()
        ));
    }

    // ---- Story 2-4: display() masks a secret-classified value (AC8) ----

    #[test]
    fn display_masks_a_secret_classified_value_at_the_single_choke_point() {
        // AC8: a `secret:NAME` leaf renders as the fixed SECRET_MASK through the ONE
        // display path (value_display + ResolvedValue::display), NOT as the
        // reference and NEVER as cleartext. A non-secret leaf is unchanged.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "key = \"secret:OPENAI_KEY\"\nmodel = \"gpt-4\"\nn = 42\n",
        ));
        // The secret leaf masks (via value_display AND the ResolvedValue method).
        assert_eq!(eff.value_display("key").as_deref(), Some(SECRET_MASK));
        assert_eq!(eff.get("key").unwrap().display(), SECRET_MASK);
        // The mask never contains the NAME's value nor the word cleartext.
        assert!(!eff
            .value_display("key")
            .unwrap()
            .contains("OPENAI_KEY_VALUE"));
        // Non-secret leaves are UNCHANGED (regression guard for the 2-1 renderer).
        assert_eq!(eff.value_display("model").as_deref(), Some("gpt-4"));
        assert_eq!(eff.value_display("n").as_deref(), Some("42"));
    }

    #[test]
    fn display_does_not_mask_a_bare_secret_prefix_value() {
        // A bare `secret:` (no NAME) is NOT a secret reference (Assumption 1), so it
        // renders as-is opaque text, NOT the mask.
        let eff = resolve(one_layer(SourceLayer::Instance, "key = \"secret:\"\n"));
        assert_eq!(eff.value_display("key").as_deref(), Some("secret:"));
    }

    #[test]
    fn source_label_reports_the_winning_layer_for_each_leaf() {
        // Story 2-3 (FR-13): the provenance accessor `kt` renders. A leaf resolved
        // from each of the four layers reports the corresponding stable label; the
        // label matches SourceLayer::as_str() (so it never diverges from the tag);
        // a missing key has no label.
        for source in SourceLayer::ORDER {
            let eff = resolve(one_layer(source, "model = \"x\""));
            assert_eq!(eff.source_label("model"), Some(source.as_str()));
        }
        // Precedence still governs the label: instance beats kind for the same key.
        let eff = resolve(two_layers(
            SourceLayer::KindDefault,
            "model = \"kind\"",
            SourceLayer::Instance,
            "model = \"instance\"",
        ));
        assert_eq!(eff.source_label("model"), Some("instance"));
        // A missing key has no source label (symmetric with value_display).
        assert_eq!(eff.source_label("nope"), None);
    }

    #[test]
    fn values_view_drops_provenance_but_keeps_values() {
        let eff = resolve(two_layers(
            SourceLayer::KindDefault,
            "model = \"kind\"",
            SourceLayer::Instance,
            "temperature = 1\n",
        ));
        let vals = eff.values();
        assert_eq!(vals.get("model"), Some(&Value::String("kind".into())));
        assert_eq!(vals.get("temperature"), Some(&Value::Integer(1)));
        // get() still carries the tag (2-3 seam) even though values() drops it.
        assert_eq!(eff.get("model").unwrap().source, SourceLayer::KindDefault);
    }

    // ---- validate_write(): known key accepted ----

    #[test]
    fn validate_write_accepts_a_known_scalar_key() {
        assert!(validate_write("model", "gpt-4").is_ok());
    }

    #[test]
    fn validate_write_rejects_restart_policy_now_dropped() {
        // Review decision #1: `restart.policy` was DROPPED from the known-key set
        // (the reaper reads the policy from SQLite, not config — a config write
        // would be a misleading no-op). It is now an unknown key. `model` is the
        // sole known key.
        assert!(!is_known_key("restart.policy"));
        assert!(validate_write("restart.policy", "never").is_err());
        // `model` + the three story-3-2 Token-Budget keys + the four story-3-3 dollar
        // keys (Rate ×2 + Cost Cap ×2) + the two story-3-4 engine-observed metering
        // keys (the operator-set upstream URL + the engine-injected loopback base_url)
        // + the story 5-1 engine-injected managed-memory key are the known set.
        assert_eq!(
            KNOWN_KEYS,
            &[
                "model",
                "budget.tokens.per_run",
                "budget.tokens.cumulative",
                "budget.breach_action",
                "cost.rate.input",
                "cost.rate.output",
                "budget.dollars.per_run",
                "budget.dollars.cumulative",
                "metering.upstream_base_url",
                "metering.base_url",
                "memory.dir",
            ]
        );
        // Story 3-4: both metering keys are known (a mapping can target them) and
        // accepted as ordinary string writes (no special value type-check).
        assert!(is_known_key("metering.upstream_base_url"));
        assert!(is_known_key("metering.base_url"));
        assert!(validate_write("metering.upstream_base_url", "http://127.0.0.1:1234").is_ok());
    }

    #[test]
    fn memory_dir_is_known_but_is_a_delivery_mechanism_not_operator_config() {
        // Story 5-1 (AD-11 Delivery clause): the reserved key is KNOWN so an
        // adapter's mapping can target it — but the operator never sets it (the
        // engine injects it at start), and it deliberately does NOT touch the
        // Adapter Contract surface (no CONTRACT_VERSION bump, DC-5).
        assert!(is_known_key("memory.dir"));
        assert!(!is_pass_through("memory.dir"));
    }

    #[test]
    fn validate_write_accepts_a_dotted_agent_pass_through_key() {
        // The dotted-WRITE path is exercised via the agent.* namespace (there is
        // no known dotted unified key in 2-1 after the restart.policy drop).
        assert!(validate_write("agent.tools.web_search", "on").is_ok());
    }

    // ---- validate_write(): agent.* pass-through accepted, sibling rejected ----

    #[test]
    fn validate_write_accepts_any_agent_pass_through_key() {
        // AC7: an agent.* key bypasses the known-key check (delivered verbatim).
        assert!(validate_write("agent.anything", "x").is_ok());
        assert!(validate_write("agent.deep.nested.key", "x").is_ok());
    }

    #[test]
    fn validate_write_rejects_an_equally_unknown_non_agent_key() {
        // AC7 companion: an equally-unknown NON-agent.* key is rejected.
        let err = validate_write("notagent.foo", "x").unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKey { key, .. } if key == "notagent.foo"));
    }

    #[test]
    fn bare_agent_key_is_not_pass_through() {
        // `agent` with no child is NOT pass-through (the prefix needs a child).
        let err = validate_write("agent", "x").unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKey { .. }));
        assert!(!is_pass_through("agent"));
        assert!(!is_pass_through("agentfoo")); // no dot → not the namespace
        assert!(is_pass_through("agent.x"));
    }

    // ---- validate_write(): empty dotted segments rejected (patch #5) ----

    #[test]
    fn validate_write_rejects_empty_dotted_segments() {
        // Patch #5: a key with an empty segment would persist a "" key — reject it
        // up front (no suggestion; the shape is wrong, not a near-miss). Covers a
        // doubled dot, a trailing dot, a leading dot, a bare dot, and the empty key
        // — including under the agent.* namespace (which otherwise bypasses the
        // known-key check).
        for bad in ["agent..b", "agent.foo.", ".x", ".", "", "a..b", "model."] {
            let err = validate_write(bad, "v").unwrap_err();
            match err {
                ConfigError::UnknownKey { key, suggestion } => {
                    assert_eq!(key, bad);
                    assert_eq!(suggestion, None, "empty-segment keys get no suggestion");
                }
                other => panic!("expected UnknownKey for {bad:?}, got {other:?}"),
            }
        }
        assert!(has_empty_segment("agent..b"));
        assert!(has_empty_segment(""));
        assert!(!has_empty_segment("agent.a.b"));
        assert!(!has_empty_segment("model"));
    }

    // ---- validate_write(): unknown key rejected WITH nearest suggestion ----

    #[test]
    fn validate_write_rejects_unknown_key_and_suggests_the_nearest() {
        // AC6 fixed near-miss: `modle` → suggest `model`.
        let err = validate_write("modle", "x").unwrap_err();
        match err {
            ConfigError::UnknownKey { key, suggestion } => {
                assert_eq!(key, "modle");
                assert_eq!(suggestion.as_deref(), Some("model"));
            }
            other => panic!("expected UnknownKey, got {other:?}"),
        }
    }

    #[test]
    fn unknown_key_error_message_names_key_and_suggestion() {
        let err = validate_write("modle", "x").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("modle"), "{msg}");
        assert!(msg.contains("did you mean 'model'"), "{msg}");
    }

    // ---- validate_write(): far-miss → NO suggestion (honest "no close match") ----

    #[test]
    fn validate_write_far_miss_yields_no_suggestion() {
        // AC6: a far-off key gets None (not nonsense). "zzzzzzzzzz" is far from
        // every known key (distance > threshold).
        let err = validate_write("zzzzzzzzzz", "x").unwrap_err();
        match err {
            ConfigError::UnknownKey { suggestion, .. } => assert_eq!(suggestion, None),
            other => panic!("expected UnknownKey, got {other:?}"),
        }
        // And the message says so honestly.
        let msg = validate_write("zzzzzzzzzz", "x").unwrap_err().to_string();
        assert!(msg.contains("no close match"), "{msg}");
    }

    #[test]
    fn validate_write_does_not_inspect_the_value_secret_is_opaque() {
        // validate_write checks the KEY only; a `secret:` VALUE is stored verbatim
        // (story 2-4 resolves + masks it at START/DISPLAY, not at write time). A
        // known key with a secret: value is accepted.
        assert!(validate_write("model", "secret:OPENAI_KEY").is_ok());
    }

    // ---- Levenshtein unit coverage (the suggestion engine) ----

    #[test]
    fn levenshtein_basic_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("model", "model"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("modle", "model"), 2); // transposition = 2 edits
        assert_eq!(levenshtein("kitten", "sitting"), 3); // classic example
    }

    #[test]
    fn nearest_known_key_picks_within_threshold_else_none() {
        assert_eq!(nearest_known_key("model").as_deref(), Some("model"));
        assert_eq!(nearest_known_key("modle").as_deref(), Some("model"));
        assert_eq!(nearest_known_key("modl").as_deref(), Some("model")); // 1 deletion
                                                                         // Far off → None (beyond the threshold).
        assert_eq!(nearest_known_key("zzzzzzzzzz"), None);
    }

    #[test]
    fn nearest_known_key_tie_break_is_deterministic_on_candidate_string() {
        // Review patch #6: with a synthetic equidistant set the tie must break on
        // the CANDIDATE STRING (lexicographically smallest), independent of array
        // order. Assert the tie-break policy directly on the comparator so it holds
        // regardless of how KNOWN_KEYS is (re)ordered: given two equidistant
        // candidates "alpha" and "beta", "alpha" wins.
        let cands = [(1usize, "beta"), (1usize, "alpha")];
        let picked = cands
            .iter()
            .copied()
            .min_by(|(da, ca), (db, cb)| da.cmp(db).then_with(|| ca.cmp(cb)))
            .map(|(_, c)| c);
        assert_eq!(
            picked,
            Some("alpha"),
            "tie must break on the smaller string"
        );
    }

    // ---- ConfigLayer / MalformedLayer ----

    #[test]
    fn config_layer_parse_rejects_malformed_toml_naming_the_layer_and_path() {
        let err = ConfigLayer::parse(SourceLayer::Instance, "/x/config.toml", "not = = valid")
            .unwrap_err();
        match err {
            ConfigError::MalformedLayer { layer, path, .. } => {
                assert_eq!(layer, SourceLayer::Instance);
                assert_eq!(path, "/x/config.toml");
            }
            other => panic!("expected MalformedLayer, got {other:?}"),
        }
        // The message names the layer + path (never a panic).
        let msg = ConfigLayer::parse(SourceLayer::Instance, "/x/config.toml", "bad = = ")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("instance"), "{msg}");
        assert!(msg.contains("/x/config.toml"), "{msg}");
    }

    #[test]
    fn config_layer_empty_helpers() {
        let empty = ConfigLayer::empty();
        assert!(empty.is_empty());
        assert!(empty.as_table().is_empty());
        let full = ConfigLayer::parse(SourceLayer::Instance, "<t>", "model = \"x\"").unwrap();
        assert!(!full.is_empty());
        assert_eq!(
            ConfigLayer::from_table(full.as_table().clone()).as_table(),
            full.as_table()
        );
    }

    // ---- Story 3-2: Token-Budget config keys (AC-B/AC-C, AD-9) ----

    #[test]
    fn validate_write_accepts_valid_budget_keys() {
        // The three budget keys are known validated keys; a well-typed value passes.
        assert!(validate_write("budget.tokens.per_run", "1000").is_ok());
        assert!(validate_write("budget.tokens.cumulative", "50000").is_ok());
        assert!(validate_write("budget.breach_action", "pause").is_ok());
        assert!(validate_write("budget.breach_action", "stop").is_ok());
        assert!(validate_write("budget.breach_action", "warn").is_ok());
        // Zero is a valid (if degenerate) ceiling.
        assert!(validate_write("budget.tokens.per_run", "0").is_ok());
        // Surrounding whitespace is tolerated (trimmed).
        assert!(validate_write("budget.tokens.cumulative", "  42  ").is_ok());
    }

    #[test]
    fn validate_write_rejects_a_malformed_budget_number() {
        // AC-C: a non-numeric / negative budget value is rejected at write time
        // (InvalidValue), naming the key + value — never silently defaulted.
        for bad in ["lots", "-5", "1.5", "12x", ""] {
            let err = validate_write("budget.tokens.per_run", bad).unwrap_err();
            match err {
                ConfigError::InvalidValue { key, value, .. } => {
                    assert_eq!(key, "budget.tokens.per_run");
                    assert_eq!(value, bad);
                }
                other => panic!("expected InvalidValue for {bad:?}, got {other:?}"),
            }
        }
        // The message is a helpful remediation.
        let msg = validate_write("budget.tokens.cumulative", "nope")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("budget.tokens.cumulative"), "{msg}");
        assert!(msg.contains("nope"), "{msg}");
    }

    #[test]
    fn validate_write_rejects_an_unknown_breach_action() {
        // AC-C: an unknown Breach-Action string is rejected at write time, naming
        // it + the accepted set.
        let err = validate_write("budget.breach_action", "throttle").unwrap_err();
        match err {
            ConfigError::InvalidValue { key, value, reason } => {
                assert_eq!(key, "budget.breach_action");
                assert_eq!(value, "throttle");
                assert!(reason.contains("pause"), "{reason}");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn resolve_token_budget_reads_the_current_resolved_values() {
        // AC-B: the evaluator's live read. A resolved config with both scopes + an
        // action yields the right TokenBudget + BreachAction.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[budget]\nbreach_action = \"stop\"\n[budget.tokens]\nper_run = 100\ncumulative = 5000\n",
        ));
        let (budget, action) = resolve_token_budget(&eff);
        assert_eq!(budget.per_run, Some(100));
        assert_eq!(budget.cumulative, Some(5000));
        assert_eq!(action, BreachAction::Stop);
    }

    #[test]
    fn resolve_token_budget_absent_keys_yield_none_and_pause_default() {
        // Absent scopes → None (never breach); absent action → Pause (the ratified
        // default).
        let eff = resolve(one_layer(SourceLayer::Instance, "model = \"x\"\n"));
        let (budget, action) = resolve_token_budget(&eff);
        assert_eq!(budget.per_run, None);
        assert_eq!(budget.cumulative, None);
        assert!(!budget.is_set());
        assert_eq!(action, BreachAction::Pause);
    }

    #[test]
    fn resolve_token_budget_reflects_a_changed_value_no_caching() {
        // AC-B "changes apply immediately": resolving a config with a LOWERED
        // ceiling yields the new value — there is no caching in the resolve path.
        let high = resolve(one_layer(
            SourceLayer::Instance,
            "[budget.tokens]\ncumulative = 10000\n",
        ));
        assert_eq!(resolve_token_budget(&high).0.cumulative, Some(10000));
        let low = resolve(one_layer(
            SourceLayer::Instance,
            "[budget.tokens]\ncumulative = 10\n",
        ));
        assert_eq!(resolve_token_budget(&low).0.cumulative, Some(10));
    }

    #[test]
    fn resolve_token_budget_degrades_a_malformed_present_value_to_absent() {
        // Defensive read (AD-12): a value that slipped past write-validation (a
        // hand-edited negative / non-integer, or an unknown action string) is
        // treated as ABSENT rather than crashing ingestion.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[budget]\nbreach_action = \"bogus\"\n[budget.tokens]\nper_run = -3\ncumulative = \"lots\"\n",
        ));
        let (budget, action) = resolve_token_budget(&eff);
        assert_eq!(
            budget.per_run, None,
            "a negative ceiling degrades to absent"
        );
        assert_eq!(
            budget.cumulative, None,
            "a non-numeric string ceiling degrades to absent"
        );
        assert_eq!(
            action,
            BreachAction::Pause,
            "an unknown action degrades to the default"
        );
    }

    #[test]
    fn resolve_budget_and_cap_ignore_a_leaf_of_the_wrong_toml_type() {
        // The sibling of the malformed-VALUE degrade above: a leaf of the wrong
        // TOML TYPE entirely (a bool, a float, an array — shapes the `set` path can
        // never produce, but a hand-edited config can) must also read as ABSENT.
        // This is the safe direction only because absent means "no ceiling to
        // enforce"; the dangerous failure would be coercing `true` into a ceiling
        // of 1 (or a float `0.5` into 0), which would throttle or instantly breach
        // an agent the operator never budgeted. Both the token and the dollar
        // resolvers are pinned, since they coerce independently.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[budget.tokens]\nper_run = true\ncumulative = 1.5\n\
             [budget.dollars]\ncumulative = false\n\
             [cost.rate]\ninput = [1, 2]\n",
        ));

        let (budget, _) = resolve_token_budget(&eff);
        assert_eq!(budget.per_run, None, "a boolean is not a token ceiling");
        assert_eq!(budget.cumulative, None, "a float is not a token ceiling");

        let (rate, cap, _) = resolve_cost(&eff);
        assert_eq!(cap.cumulative, None, "a boolean is not a dollar cap");
        // A Rate needs BOTH directions; an array input leaf reads as absent, so the
        // Rate stays inert rather than half-configured.
        assert!(
            rate.is_none(),
            "a non-scalar rate leaf must not configure a Rate"
        );
    }

    #[test]
    fn resolve_token_budget_accepts_a_numeric_string_ceiling() {
        // A quoted numeric value (a hand-edited string leaf) still resolves to the
        // ceiling (defensive coercion).
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[budget.tokens]\nper_run = \"250\"\n",
        ));
        assert_eq!(resolve_token_budget(&eff).0.per_run, Some(250));
    }

    // ---- Story 3-3: dollar-string → micros parse (AC11) ----

    #[test]
    fn parse_dollars_accepts_common_forms() {
        assert_eq!(parse_dollars_to_micros("3.00").unwrap(), Micros(3_000_000));
        assert_eq!(parse_dollars_to_micros("3").unwrap(), Micros(3_000_000));
        assert_eq!(parse_dollars_to_micros("15").unwrap(), Micros(15_000_000));
        assert_eq!(parse_dollars_to_micros("0").unwrap(), Micros(0));
        assert_eq!(parse_dollars_to_micros("0.00").unwrap(), Micros(0));
        // Sub-cent but at/above the micro quantum: $0.000003 = 3 micros ($3/1M).
        assert_eq!(parse_dollars_to_micros("0.000003").unwrap(), Micros(3));
        // Partial fractional digits right-pad: "0.3" = $0.30 = 300_000 micros.
        assert_eq!(parse_dollars_to_micros("0.3").unwrap(), Micros(300_000));
        // Leading-dot integer defaults to 0: ".5" = $0.50.
        assert_eq!(parse_dollars_to_micros(".5").unwrap(), Micros(500_000));
        // Trailing-dot fractional defaults to 0: "3." = $3.00.
        assert_eq!(parse_dollars_to_micros("3.").unwrap(), Micros(3_000_000));
        // Surrounding whitespace is trimmed.
        assert_eq!(
            parse_dollars_to_micros("  3.00  ").unwrap(),
            Micros(3_000_000)
        );
    }

    #[test]
    fn parse_dollars_rejects_sub_micro_precision() {
        // MORE than 6 fractional digits is sub-micro precision — REJECTED (a clear
        // diagnostic), never silently truncated.
        let err = parse_dollars_to_micros("3.0000001").unwrap_err();
        assert!(err.contains("sub-micro"), "{err}");
        assert!(err.contains("0.000001"), "{err}");
        // Exactly 6 digits is fine (the quantum).
        assert!(parse_dollars_to_micros("3.000001").is_ok());
    }

    #[test]
    fn parse_dollars_rejects_non_numeric_and_signed_and_multi_dot() {
        assert!(parse_dollars_to_micros("abc").is_err());
        assert!(parse_dollars_to_micros("3.0x").is_err());
        assert!(parse_dollars_to_micros("").is_err());
        // A Rate/cap is non-negative — a sign is rejected.
        assert!(parse_dollars_to_micros("-3.00").is_err());
        assert!(parse_dollars_to_micros("+3.00").is_err());
        // More than one decimal point.
        assert!(parse_dollars_to_micros("3.0.0").is_err());
        // An implausibly huge value overflows the micro domain and is rejected.
        assert!(parse_dollars_to_micros("100000000000000").is_err());
    }

    // ---- Story 3-3: validate_write on the dollar keys (AC-A/AC11) ----

    #[test]
    fn validate_write_accepts_valid_dollar_keys() {
        assert!(validate_write(COST_RATE_INPUT_KEY, "3.00").is_ok());
        assert!(validate_write(COST_RATE_OUTPUT_KEY, "15.00").is_ok());
        assert!(validate_write(BUDGET_DOLLARS_PER_RUN_KEY, "5").is_ok());
        assert!(validate_write(BUDGET_DOLLARS_CUMULATIVE_KEY, "50.00").is_ok());
    }

    #[test]
    fn validate_write_rejects_a_malformed_dollar_value_with_a_diagnostic() {
        // AC11: a malformed Rate/cap dollar value is rejected at WRITE time (never
        // silently defaulted; a bad value must not crash ingestion). It is an
        // InvalidValue (the KEY is known — its VALUE is wrong), naming key + value.
        let err = validate_write(COST_RATE_INPUT_KEY, "three dollars").unwrap_err();
        match err {
            ConfigError::InvalidValue { key, value, reason } => {
                assert_eq!(key, COST_RATE_INPUT_KEY);
                assert_eq!(value, "three dollars");
                assert!(!reason.is_empty(), "a remediation reason is present");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
        // Sub-micro precision on a cap is likewise rejected.
        assert!(matches!(
            validate_write(BUDGET_DOLLARS_CUMULATIVE_KEY, "5.0000001"),
            Err(ConfigError::InvalidValue { .. })
        ));
    }

    // ---- Story 3-3: resolve_cost (AC-A/AC-B/AC-C/AC11) ----

    #[test]
    fn resolve_cost_requires_both_rate_directions_else_inert() {
        // Both directions present → a real Rate.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\ninput = \"3.00\"\noutput = \"15.00\"\n",
        ));
        let (rate, _cap, _action) = resolve_cost(&eff);
        assert_eq!(
            rate,
            Some(Rate::new(3_000_000, 15_000_000)),
            "both directions set → the Rate is supplied"
        );

        // Only input set → inert (no Rate), avoiding a silently-half-priced ledger.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\ninput = \"3.00\"\n",
        ));
        assert_eq!(resolve_cost(&eff).0, None, "a half-Rate is inert (AC-B)");

        // Only output set → inert.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\noutput = \"15.00\"\n",
        ));
        assert_eq!(resolve_cost(&eff).0, None);
    }

    #[test]
    fn resolve_cost_reads_the_dollar_cap_scopes_and_action() {
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\ninput = \"1.00\"\noutput = \"1.00\"\n\
             [budget.dollars]\nper_run = \"5.00\"\ncumulative = \"50.00\"\n\
             [budget]\nbreach_action = \"stop\"\n",
        ));
        let (rate, cap, action) = resolve_cost(&eff);
        assert_eq!(rate, Some(Rate::new(1_000_000, 1_000_000)));
        assert_eq!(cap.per_run, Some(Micros(5_000_000)));
        assert_eq!(cap.cumulative, Some(Micros(50_000_000)));
        // The SAME breach-action key governs the dollar breach.
        assert_eq!(action, BreachAction::Stop);
    }

    #[test]
    fn resolve_upstream_base_url_reads_the_operator_key_or_none() {
        // Story 3-4 (AC6): the operator's real provider endpoint is read from
        // `metering.upstream_base_url` (trimmed); absent / empty / secret → None.
        let set = resolve(one_layer(
            SourceLayer::Instance,
            "[metering]\nupstream_base_url = \"  http://127.0.0.1:1234  \"\n",
        ));
        assert_eq!(
            resolve_upstream_base_url(&set).as_deref(),
            Some("http://127.0.0.1:1234"),
            "trimmed operator upstream"
        );
        // Absent → None (an engine-observed start then fails fast with a clear error).
        let absent = resolve(one_layer(SourceLayer::Instance, "model = \"x\"\n"));
        assert_eq!(resolve_upstream_base_url(&absent), None);
        // A secret-classified value is NOT a URL → None (defensive).
        let secret = resolve(one_layer(
            SourceLayer::Instance,
            "[metering]\nupstream_base_url = \"secret:UPSTREAM\"\n",
        ));
        assert_eq!(resolve_upstream_base_url(&secret), None);
        // An empty string → None.
        let empty = resolve(one_layer(
            SourceLayer::Instance,
            "[metering]\nupstream_base_url = \"   \"\n",
        ));
        assert_eq!(resolve_upstream_base_url(&empty), None);
    }

    #[test]
    fn resolve_cost_absent_yields_no_rate_no_cap_default_action() {
        // AC-B: nothing configured → no Rate, no cap, the ratified `pause` default.
        let eff = resolve(one_layer(SourceLayer::Instance, "model = \"x\"\n"));
        let (rate, cap, action) = resolve_cost(&eff);
        assert_eq!(rate, None);
        assert!(!cap.is_set());
        assert_eq!(action, BreachAction::Pause);
    }

    #[test]
    fn resolve_cost_degrades_a_malformed_value_to_absent_never_panics() {
        // AD-12 defensive read: a hand-edited malformed value resolves to absent for
        // that direction/scope rather than erroring. Malformed input → no Rate.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\ninput = \"garbage\"\noutput = \"15.00\"\n",
        ));
        assert_eq!(resolve_cost(&eff).0, None, "a malformed direction → inert");
    }

    #[test]
    fn resolve_cost_accepts_a_bare_integer_leaf_as_whole_dollars() {
        // A hand-edited bare integer means whole dollars (defensive convenience):
        // `cumulative = 5` → $5.00 = 5_000_000 micros.
        let eff = resolve(one_layer(
            SourceLayer::Instance,
            "[budget.dollars]\ncumulative = 5\n",
        ));
        assert_eq!(resolve_cost(&eff).1.cumulative, Some(Micros(5_000_000)));
    }

    #[test]
    fn resolve_cost_reflects_a_live_rate_change_no_caching() {
        // AC-A/AC-B: a Rate changed in config is reflected on the next resolve (a
        // live read, no caching) — the mechanism behind "re-prices future only".
        let eff1 = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\ninput = \"3.00\"\noutput = \"15.00\"\n",
        ));
        assert_eq!(
            resolve_cost(&eff1).0,
            Some(Rate::new(3_000_000, 15_000_000))
        );
        // A fresh resolve of a CHANGED config sees the new Rate immediately.
        let eff2 = resolve(one_layer(
            SourceLayer::Instance,
            "[cost.rate]\ninput = \"6.00\"\noutput = \"30.00\"\n",
        ));
        assert_eq!(
            resolve_cost(&eff2).0,
            Some(Rate::new(6_000_000, 30_000_000))
        );
    }
}
