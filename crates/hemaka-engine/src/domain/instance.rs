//! [`AgentInstance`] — the registry entity (spine Glossary term, verbatim).

use serde::{Deserialize, Serialize};

use super::lifecycle::LifecycleState;
use super::name::InstanceName;

/// A registered Agent Instance: one managed agent in the Fleet.
///
/// This is the domain view of a row in the `agent_instances` table. The
/// engine is the sole path authority, so `agent_home` is always a path the
/// engine computed (never one a caller supplied). Timestamps are RFC 3339 UTC
/// strings (spine convention); this story does not parse them back, so they
/// are kept as strings rather than a parsed time type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInstance {
    /// Fleet-unique validated name.
    pub name: InstanceName,
    /// Agent kind (adapter identity arrives in story 1.3; a free string today).
    pub kind: String,
    /// Current Lifecycle State (`Registered` for freshly created instances).
    pub state: LifecycleState,
    /// Absolute Agent Home path, engine-computed (path authority).
    pub agent_home: String,
    /// RFC 3339 UTC creation timestamp.
    pub created_at: String,
    /// RFC 3339 UTC last-update timestamp (equals `created_at` at creation).
    pub updated_at: String,
}
