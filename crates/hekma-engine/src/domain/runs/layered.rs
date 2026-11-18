//! The delta-log map behind the Run snapshot (story 24-4, spine AD-25):
//! a publish costs O(changed entries), not O(open Runs).
//!
//! A [`Layered`] map is an immutable shared base plus a short list of
//! shared delta layers (each a small map whose `None` is a tombstone).
//! Cloning one copies a pointer per layer, never the entries. The writer
//! builds the next snapshot by cloning the current one, changing it (the
//! changes collect in one owned layer) and sealing it, which shares that
//! layer. Once the deltas grow past [`rebase_bound`] entries, or past
//! [`MAX_LAYERS`] layers, the seal folds them into a new base: one O(n)
//! copy per ~4096 changed entries at least, so the amortized cost per
//! change stays constant. Readers resolve newest layer first, then the
//! base, through at most [`MAX_LAYERS`] probes.

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

/// The most delta layers a map carries before a seal folds them.
pub(crate) const MAX_LAYERS: usize = 32;

/// The fewest delta entries that trigger a fold.
pub(crate) const MIN_REBASE_ENTRIES: usize = 4096;

/// When a seal folds the deltas into a new base: past
/// `max(4096, base / 32)` delta entries.
pub(crate) fn rebase_bound(base_len: usize) -> usize {
    MIN_REBASE_ENTRIES.max(base_len / 32)
}

/// A map whose clone is O(layers), see the module docs.
#[derive(Debug)]
pub(crate) struct Layered<K, V> {
    base: Arc<HashMap<K, V>>,
    /// Sealed layers, oldest first.
    deltas: Vec<Arc<HashMap<K, Option<V>>>>,
    /// Entries in the sealed layers, counted with repeats.
    delta_entries: usize,
    /// The layer this copy is building (not shared yet).
    pending: HashMap<K, Option<V>>,
}

impl<K, V> Default for Layered<K, V> {
    fn default() -> Self {
        Layered {
            base: Arc::new(HashMap::new()),
            deltas: Vec::new(),
            delta_entries: 0,
            pending: HashMap::new(),
        }
    }
}

impl<K: Clone, V: Clone> Clone for Layered<K, V> {
    fn clone(&self) -> Self {
        Layered {
            base: Arc::clone(&self.base),
            deltas: self.deltas.clone(),
            delta_entries: self.delta_entries,
            pending: self.pending.clone(),
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Layered<K, V> {
    /// A map whose base is `entries` and that has no deltas.
    pub(crate) fn from_map(entries: HashMap<K, V>) -> Self {
        Layered {
            base: Arc::new(entries),
            ..Layered::default()
        }
    }

    /// The value at `key`: the newest layer that names it, else the base.
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_key_value(key).map(|(_, value)| value)
    }

    /// The stored key and value at `key`.
    pub(crate) fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some((k, v)) = self.pending.get_key_value(key) {
            return v.as_ref().map(|v| (k, v));
        }
        for layer in self.deltas.iter().rev() {
            if let Some((k, v)) = layer.get_key_value(key) {
                return v.as_ref().map(|v| (k, v));
            }
        }
        self.base.get_key_value(key)
    }

    /// Whether `key` has a value.
    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get(key).is_some()
    }

    /// Set `key` (in this copy's pending layer).
    pub(crate) fn insert(&mut self, key: K, value: V) {
        self.pending.insert(key, Some(value));
    }

    /// Remove `key`, returning its value.
    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized,
    {
        let previous = self.get(key).cloned();
        if previous.is_some() {
            self.pending.insert(key.to_owned(), None);
        }
        previous
    }

    /// Empty the map (a fresh base).
    pub(crate) fn clear(&mut self) {
        *self = Layered::default();
    }

    /// Every live entry, each key once (newest value wins). O(n).
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        let mut seen: HashSet<&K> = HashSet::new();
        let mut out: Vec<(&K, &V)> = Vec::new();
        let layers = std::iter::once(&self.pending).chain(self.deltas.iter().rev().map(|l| &**l));
        for layer in layers {
            for (k, v) in layer {
                if seen.insert(k) {
                    if let Some(v) = v {
                        out.push((k, v));
                    }
                }
            }
        }
        for (k, v) in self.base.iter() {
            if !seen.contains(k) {
                out.push((k, v));
            }
        }
        out.into_iter()
    }

    /// Every live value.
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// How many live entries. O(n) while deltas are pending, O(1) after a
    /// fold.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn len(&self) -> usize {
        if self.deltas.is_empty() && self.pending.is_empty() {
            self.base.len()
        } else {
            self.iter().count()
        }
    }

    /// Whether the map has no live entry.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Share this copy's pending layer, folding every layer into a new base
    /// when the deltas outgrow [`rebase_bound`] or [`MAX_LAYERS`].
    pub(crate) fn seal(&mut self) {
        if !self.pending.is_empty() {
            let layer = std::mem::take(&mut self.pending);
            self.delta_entries += layer.len();
            self.deltas.push(Arc::new(layer));
        }
        if self.delta_entries > rebase_bound(self.base.len()) || self.deltas.len() > MAX_LAYERS {
            self.fold();
        }
    }

    /// Fold every layer into a new base.
    pub(crate) fn fold(&mut self) {
        let mut base: HashMap<K, V> = (*self.base).clone();
        for layer in self.deltas.drain(..) {
            for (k, v) in layer.iter() {
                match v {
                    Some(v) => {
                        base.insert(k.clone(), v.clone());
                    }
                    None => {
                        base.remove(k);
                    }
                }
            }
        }
        for (k, v) in self.pending.drain() {
            match v {
                Some(v) => {
                    base.insert(k, v);
                }
                None => {
                    base.remove(&k);
                }
            }
        }
        self.base = Arc::new(base);
        self.delta_entries = 0;
    }

    /// Whether two maps share one base (the no-whole-copy pin).
    #[cfg(test)]
    pub(crate) fn shares_base_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.base, &other.base)
    }

    /// How many sealed layers this map carries.
    #[cfg(test)]
    pub(crate) fn layers(&self) -> usize {
        self.deltas.len()
    }
}

impl<K, Q, V> std::ops::Index<&Q> for Layered<K, V>
where
    K: Eq + Hash + Clone + Borrow<Q>,
    Q: Eq + Hash + ?Sized,
    V: Clone,
{
    type Output = V;

    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("no entry for the key")
    }
}

impl<K: Eq + Hash + Clone, V: Clone> FromIterator<(K, V)> for Layered<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Layered::from_map(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(n: u32) -> Layered<u32, u32> {
        (0..n).map(|i| (i, i)).collect()
    }

    #[test]
    fn reads_resolve_newest_layer_first_and_tombstones_hide_the_base() {
        let mut map = seeded(10);
        map.insert(3, 30);
        assert_eq!(map.remove(&4), Some(4));
        assert_eq!(map.remove(&99), None);
        map.seal();
        map.insert(3, 300);
        map.insert(11, 11);
        assert_eq!(map.get(&3), Some(&300));
        assert_eq!(map.get(&4), None);
        assert!(!map.contains_key(&4));
        assert_eq!(map.get_key_value(&11), Some((&11, &11)));
        assert_eq!(map[&5], 5);
        assert_eq!(map.len(), 10);
        let mut values: Vec<u32> = map.values().copied().collect();
        values.sort();
        assert_eq!(values, vec![0, 1, 2, 5, 6, 7, 8, 9, 11, 300]);
        map.seal();
        map.fold();
        assert_eq!(map.layers(), 0);
        assert_eq!(map.len(), 10);
        assert_eq!(map.get(&3), Some(&300));
        map.clear();
        assert!(map.is_empty());
    }

    #[test]
    fn a_publish_shares_the_base_and_only_a_bound_or_layer_count_folds_it() {
        let current = seeded(100_000);
        let mut next = current.clone();
        for i in 0..8 {
            next.insert(i, i + 1);
        }
        next.seal();
        assert!(next.shares_base_with(&current), "no whole-map copy");
        assert_eq!(next.layers(), 1);
        assert_eq!(current.get(&0), Some(&0), "the old view is immutable");
        assert_eq!(next.get(&0), Some(&1));
        // The layer cap folds: the 33rd sealed layer triggers it.
        let mut map = next.clone();
        for round in 0..MAX_LAYERS as u32 {
            map.insert(round, round);
            map.seal();
        }
        assert_eq!(map.layers(), 0, "past {MAX_LAYERS} layers the deltas fold");
        assert!(!map.shares_base_with(&current));
        // The entry bound folds: max(4096, base / 32) entries.
        assert_eq!(rebase_bound(100_000), 4096);
        assert_eq!(rebase_bound(1_000_000), 31_250);
        let mut big = seeded(100_000);
        for i in 0..=4096u32 {
            big.insert(i, 0);
        }
        big.seal();
        assert_eq!(big.layers(), 0);
        assert_eq!(big.len(), 100_000);
    }
}
