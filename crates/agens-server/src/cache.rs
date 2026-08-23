//! A bounded memo of things derived per run.
//!
//! Two of the coordinator's loops keep one: the journal publisher remembers
//! which repository a run belongs to, and ingest remembers the health state a
//! run's journal folds to. Both are pure memoization — every value can be
//! derived again from the control plane — and neither loop is told when a run
//! ends, so an unbounded map grows one entry per run for the life of the
//! process and never gives one back.
//!
//! The bound is least-recently-used rather than a lifecycle hook. A run that
//! ended is exactly the run nothing touches again, so recency evicts it without
//! either loop having to learn about run states, and a value evicted early
//! costs one derivation rather than a wrong answer.

use std::collections::{BTreeMap, HashMap};

/// A capacity-bounded map from run id to a derived value.
pub(crate) struct RunCache<V> {
    capacity: usize,
    /// The value, with the stamp under which `recency` holds this run.
    entries: HashMap<i64, (u64, V)>,
    /// Stamps in order, so the oldest entry is the first one.
    recency: BTreeMap<u64, i64>,
    stamps: u64,
}

impl<V> RunCache<V> {
    /// A cache holding at most `capacity` runs. A capacity of zero is raised to
    /// one: a cache that can hold nothing would derive on every lookup and hide
    /// that it is doing so.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            recency: BTreeMap::new(),
            stamps: 0,
        }
    }

    /// This run's value, deriving and remembering it when it is not held.
    pub(crate) fn get_or_insert_with<F>(&mut self, run_id: i64, derive: F) -> &V
    where
        F: FnOnce() -> V,
    {
        if !self.entries.contains_key(&run_id) {
            self.insert(run_id, derive());
        } else {
            self.touch(run_id);
        }

        &self.entries[&run_id].1
    }

    /// This run's value if it is held, counting the lookup as a use.
    pub(crate) fn get(&mut self, run_id: i64) -> Option<&V> {
        self.touch(run_id);

        self.entries.get(&run_id).map(|(_, value)| value)
    }

    /// Remembers this run's value, evicting the least recently used entry when
    /// the cache is already at its bound.
    pub(crate) fn insert(&mut self, run_id: i64, value: V) {
        if self.entries.contains_key(&run_id) {
            self.touch(run_id);

            if let Some(entry) = self.entries.get_mut(&run_id) {
                entry.1 = value;
            }

            return;
        }

        while self.entries.len() >= self.capacity {
            let Some((&stamp, &oldest)) = self.recency.iter().next() else {
                break;
            };

            self.recency.remove(&stamp);
            self.entries.remove(&oldest);
        }

        let stamp = self.next_stamp();
        self.entries.insert(run_id, (stamp, value));
        self.recency.insert(stamp, run_id);
    }

    /// How many runs are held. The bound is what a test asserts against.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    fn touch(&mut self, run_id: i64) {
        let stamp = self.next_stamp();

        let Some(entry) = self.entries.get_mut(&run_id) else {
            return;
        };

        let previous = std::mem::replace(&mut entry.0, stamp);

        self.recency.remove(&previous);
        self.recency.insert(stamp, run_id);
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamps = self.stamps.wrapping_add(1);

        self.stamps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_never_holds_more_runs_than_its_bound() {
        let mut cache = RunCache::with_capacity(4);

        for run_id in 1..=1_000 {
            cache.insert(run_id, run_id * 10);
            assert!(cache.len() <= 4, "at run {run_id}");
        }

        assert_eq!(cache.len(), 4);
        assert_eq!(cache.get(1_000), Some(&10_000));
        assert_eq!(cache.get(1), None, "the oldest run was given back");
    }

    #[test]
    fn the_run_evicted_is_the_one_nothing_has_touched() {
        let mut cache = RunCache::with_capacity(3);

        cache.insert(1, "one");
        cache.insert(2, "two");
        cache.insert(3, "three");

        // Run 1 is used again, so run 2 becomes the oldest.
        assert_eq!(cache.get(1), Some(&"one"));

        cache.insert(4, "four");

        assert_eq!(cache.get(2), None);
        assert_eq!(cache.get(1), Some(&"one"));
        assert_eq!(cache.get(3), Some(&"three"));
        assert_eq!(cache.get(4), Some(&"four"));
    }

    #[test]
    fn a_value_is_derived_once_and_then_remembered() {
        let mut cache = RunCache::with_capacity(2);
        let mut derivations = 0;

        for _ in 0..5 {
            let value = cache.get_or_insert_with(7, || {
                derivations += 1;
                "seven"
            });

            assert_eq!(*value, "seven");
        }

        assert_eq!(derivations, 1);
    }

    #[test]
    fn re_inserting_a_held_run_replaces_its_value_without_growing_the_cache() {
        let mut cache = RunCache::with_capacity(2);

        cache.insert(1, "first");
        cache.insert(1, "second");

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(1), Some(&"second"));
    }
}
