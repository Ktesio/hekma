//! The `otlp-metrics v2` attribute projection (story 24-5, FR-59): pure
//! and unit-pinned.
//!
//! * The Control Depth vocabulary — `observed`, `wire`, `announced`,
//!   `supervised` — read from the event, never guessed.
//! * The Operator's label allow-list: `hekma.label.<key>` for allow-listed
//!   keys only (empty by default), bound and Caller values alike, each key
//!   under a distinct-value cap. Values past the cap fold into one
//!   [`OVERFLOW_VALUE`] series; the first values seen keep theirs. The set
//!   of values seen resets at restart (cumulative sums do too).
//! * The instance window: the [`INSTANCE_WINDOW`] most-recently-observed
//!   Agent Instances keep their series; older ones leave the payload. A
//!   window, never a drop.

use std::collections::{BTreeMap, BTreeSet};

use hekma_engine::ControlDepth;

/// The value an allow-listed key's values fold into past the cap.
pub const OVERFLOW_VALUE: &str = "__hekma_overflow__";

/// How many Agent Instances keep their series.
pub const INSTANCE_WINDOW: usize = 256;

/// The `hekma.control_depth` value.
pub fn depth_str(depth: ControlDepth) -> &'static str {
    depth.as_str()
}

/// The label allow-list and its per-key distinct-value registry.
#[derive(Clone, Debug, Default)]
pub struct LabelProjection {
    keys: Vec<String>,
    cap: usize,
    seen: BTreeMap<String, BTreeSet<String>>,
    /// Observations folded since start (`hekma.otlp.folded`).
    folded: u64,
    /// Keys that folded since the last drain (the once-per-flush note).
    folded_keys: BTreeSet<String>,
}

impl LabelProjection {
    /// An allow-list of `keys` under a per-key cap of `cap` distinct values.
    pub fn new(keys: Vec<String>, cap: u32) -> Self {
        LabelProjection {
            keys,
            cap: cap.max(1) as usize,
            ..LabelProjection::default()
        }
    }

    /// The `hekma.label.<key>` attributes for a Run's `labels`: one per
    /// allow-listed key the Run carries, in allow-list order. A value past
    /// the key's cap folds into [`OVERFLOW_VALUE`] and is counted.
    pub fn project(&mut self, labels: &[(String, String)]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for key in &self.keys {
            let Some((_, value)) = labels.iter().find(|(k, _)| k == key) else {
                continue;
            };
            let seen = self.seen.entry(key.clone()).or_default();
            let kept = if seen.contains(value) {
                value.clone()
            } else if seen.len() < self.cap {
                seen.insert(value.clone());
                value.clone()
            } else {
                self.folded = self.folded.saturating_add(1);
                self.folded_keys.insert(key.clone());
                OVERFLOW_VALUE.to_string()
            };
            out.push((format!("hekma.label.{key}"), kept));
        }
        out
    }

    /// Observations folded since start.
    pub fn folded(&self) -> u64 {
        self.folded
    }

    /// The keys that folded since the last call (the once-per-flush note).
    pub fn take_folded_keys(&mut self) -> Vec<String> {
        std::mem::take(&mut self.folded_keys).into_iter().collect()
    }
}

/// The recency window over Agent Instances.
#[derive(Clone, Debug, Default)]
pub struct InstanceWindow {
    last_seen: BTreeMap<String, u64>,
    next: u64,
    evicted: u64,
}

impl InstanceWindow {
    /// Mark `instance` observed now; the instances that leave the window
    /// (the least recently observed beyond [`INSTANCE_WINDOW`]).
    pub fn touch(&mut self, instance: &str) -> Vec<String> {
        self.next = self.next.saturating_add(1);
        self.last_seen.insert(instance.to_string(), self.next);
        if self.last_seen.len() <= INSTANCE_WINDOW {
            return Vec::new();
        }
        let mut by_age: Vec<(u64, String)> = self
            .last_seen
            .iter()
            .map(|(name, seq)| (*seq, name.clone()))
            .collect();
        by_age.sort();
        let excess = self.last_seen.len() - INSTANCE_WINDOW;
        let evicted: Vec<String> = by_age.into_iter().take(excess).map(|(_, n)| n).collect();
        for name in &evicted {
            self.last_seen.remove(name);
        }
        self.evicted = self.evicted.saturating_add(evicted.len() as u64);
        evicted
    }

    /// Instances evicted since the last call (the once-per-flush note).
    pub fn take_evicted(&mut self) -> u64 {
        std::mem::take(&mut self.evicted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_allow_listed_keys_project_and_values_past_the_cap_fold() {
        let mut none = LabelProjection::new(Vec::new(), 200);
        assert!(none
            .project(&labels(&[("team", "core"), ("env", "prod")]))
            .is_empty());
        let mut team = LabelProjection::new(vec!["team".into(), "env".into()], 3);
        assert_eq!(
            team.project(&labels(&[("env", "prod"), ("job", "7"), ("team", "a")])),
            labels(&[("hekma.label.team", "a"), ("hekma.label.env", "prod")])
        );
        for value in ["b", "c"] {
            team.project(&labels(&[("team", value)]));
        }
        // The cap is three distinct values: the fourth folds, a seen one keeps.
        assert_eq!(
            team.project(&labels(&[("team", "d")])),
            labels(&[("hekma.label.team", OVERFLOW_VALUE)])
        );
        assert_eq!(
            team.project(&labels(&[("team", "a")])),
            labels(&[("hekma.label.team", "a")])
        );
        assert_eq!(team.folded(), 1);
        assert_eq!(team.take_folded_keys(), vec!["team".to_string()]);
        assert!(team.take_folded_keys().is_empty());
        assert_eq!(depth_str(ControlDepth::Wire), "wire");
    }

    #[test]
    fn the_instance_window_keeps_the_most_recent_instances() {
        let mut window = InstanceWindow::default();
        for n in 0..INSTANCE_WINDOW {
            assert!(window.touch(&format!("i{n}")).is_empty());
        }
        // Re-touching the oldest keeps it; the next new one evicts i1.
        window.touch("i0");
        assert_eq!(window.touch("new"), vec!["i1".to_string()]);
        assert_eq!(window.take_evicted(), 1);
        assert_eq!(window.take_evicted(), 0);
    }
}
