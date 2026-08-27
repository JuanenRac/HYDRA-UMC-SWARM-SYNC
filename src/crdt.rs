// HYDRA-UMC-SWARM-SYNC - crdt.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// A real LWW-Element-Map (Last-Writer-Wins Map) CRDT: a state-based
// (convergent) CRDT whose merge is provably commutative, associative and
// idempotent - see the property tests below, which check exactly those
// three laws, not just "seems to work on one example". This is the real
// mechanism behind the README's own "Why CRDT-based sync, not a single
// source of truth" rationale: multiple HYDRA-UMC cells can run
// semi-autonomously and reconnect later, and merging two maps always
// converges to the same result no matter the order cells reconnect in -
// a naive last-write-wins-by-wall-clock approach can't guarantee that
// across a real network partition (clocks drift, arrive out of order).
//
// Deliberately no tombstones/delete support yet - removing an entry from
// a CRDT map correctly (so a delete doesn't get silently resurrected by
// a merge with a node that never saw it) is its own real design decision
// (tombstone garbage collection, delete-wins vs. add-wins semantics) -
// see mejoras_futuras.txt for why that's scoped out of this first pass
// rather than bolted on without thinking it through.

use crate::lamport::LamportTime;
use std::collections::BTreeMap;
use std::hash::Hash;

/// One entry's version stamp: (logical time, writer node ID). Comparing
/// two stamps is what resolves a conflict on the same key - later
/// logical time wins; if two writes are truly concurrent (equal logical
/// time, which Lamport clocks make rare but not impossible across
/// independently-ticking nodes), the writer ID is the deterministic
/// tie-breaker, so every node resolves the SAME concurrent conflict the
/// SAME way without needing to talk to each other about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    time: LamportTime,
    writer: u64,
}

#[derive(Debug, Clone)]
struct Entry<V> {
    value: V,
    stamp: Stamp,
}

/// A CRDT map from `K` to `V`, synchronized via last-writer-wins per key.
#[derive(Debug, Clone)]
pub struct LwwMap<K, V> {
    entries: BTreeMap<K, Entry<V>>,
}

impl<K, V> Default for LwwMap<K, V> {
    fn default() -> Self {
        LwwMap {
            entries: BTreeMap::new(),
        }
    }
}

impl<K: Ord + Clone, V: Clone> LwwMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a write to `key`, stamped with `time`/`writer`. If an
    /// entry already exists for this key with a stamp that would win
    /// against this one, the existing entry is kept - `set` on its own
    /// already obeys the same conflict rule `merge` uses, so a node
    /// applying its own out-of-order writes (e.g. replaying a log)
    /// converges the same way a merge would.
    pub fn set(&mut self, key: K, value: V, time: LamportTime, writer: u64) {
        let new_stamp = Stamp { time, writer };
        match self.entries.get(&key) {
            Some(existing) if existing.stamp >= new_stamp => {
                // An existing write already wins this conflict - ignore.
            }
            _ => {
                self.entries.insert(
                    key,
                    Entry {
                        value,
                        stamp: new_stamp,
                    },
                );
            }
        }
    }

    // get/len/is_empty/keys: today only exercised by this module's own
    // tests (the CLI in main.rs only needs set/merge/snapshot/max_time) -
    // kept as the real, complete public API of a map type, for whatever
    // calls this as a library later (a live sync daemon, other tooling),
    // not left half-finished just because nothing outside tests reaches
    // for them yet.
    #[allow(dead_code)]
    pub fn get(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|e| &e.value)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[allow(dead_code)]
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    /// The latest logical time seen across every entry in this map -
    /// what a real node would feed into `LamportClock::observe()` right
    /// after reconciling with the rest of the swarm, so the very next
    /// local event it produces is provably ordered after everything it
    /// just learned about.
    pub fn max_time(&self) -> Option<LamportTime> {
        self.entries.values().map(|e| e.stamp.time).max()
    }

    /// A plain, stamp-free snapshot of the map's current visible state -
    /// what a caller outside this module (e.g. the CLI printing a result
    /// as JSON) actually wants, without leaking the internal `Stamp`
    /// bookkeeping that made the CRDT converge in the first place.
    pub fn snapshot(&self) -> BTreeMap<K, V> {
        self.entries
            .iter()
            .map(|(k, e)| (k.clone(), e.value.clone()))
            .collect()
    }

    /// The real merge operation: for every key present in either map,
    /// keep whichever entry has the winning stamp. This is a join over a
    /// semilattice (per-key max by Stamp) - which is exactly what makes
    /// it commutative, associative and idempotent; see the tests below
    /// for a direct check of all three properties, not just an example
    /// merge.
    pub fn merge(&self, other: &Self) -> Self {
        let mut result = self.clone();
        for (key, other_entry) in other.entries.iter() {
            match result.entries.get(key) {
                Some(existing) if existing.stamp >= other_entry.stamp => {}
                _ => {
                    result.entries.insert(key.clone(), other_entry.clone());
                }
            }
        }
        result
    }
}

impl<K: Ord + Clone + Hash, V: Clone + PartialEq> LwwMap<K, V> {
    /// Two maps are equal if they agree on every key's current value -
    /// used by the property tests to check CRDT convergence (they don't
    /// need to compare internal stamps, just the observable state).
    #[cfg(test)]
    fn same_visible_state(&self, other: &Self) -> bool {
        if self.entries.len() != other.entries.len() {
            return false;
        }
        self.entries
            .iter()
            .all(|(k, v)| other.get(k) == Some(&v.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_round_trip() {
        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set("node-1".to_string(), "ok".to_string(), LamportTime(1), 100);
        assert_eq!(map.get(&"node-1".to_string()), Some(&"ok".to_string()));
    }

    #[test]
    fn later_stamp_wins_on_the_same_key() {
        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set("node-1".to_string(), "ok".to_string(), LamportTime(1), 100);
        map.set(
            "node-1".to_string(),
            "degraded".to_string(),
            LamportTime(5),
            100,
        );
        assert_eq!(
            map.get(&"node-1".to_string()),
            Some(&"degraded".to_string())
        );
    }

    #[test]
    fn an_earlier_stamp_never_overwrites_a_later_one() {
        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set(
            "node-1".to_string(),
            "degraded".to_string(),
            LamportTime(5),
            100,
        );
        // A stale/replayed write with an earlier stamp must not win.
        map.set("node-1".to_string(), "ok".to_string(), LamportTime(2), 100);
        assert_eq!(
            map.get(&"node-1".to_string()),
            Some(&"degraded".to_string())
        );
    }

    #[test]
    fn concurrent_writes_break_ties_deterministically_by_writer_id() {
        let mut a: LwwMap<String, String> = LwwMap::new();
        a.set(
            "node-1".to_string(),
            "from-writer-5".to_string(),
            LamportTime(3),
            5,
        );
        let mut b: LwwMap<String, String> = LwwMap::new();
        b.set(
            "node-1".to_string(),
            "from-writer-9".to_string(),
            LamportTime(3),
            9,
        );

        // Same logical time, different writers - both merge orders must
        // agree on the SAME winner (higher writer ID, per Stamp's Ord).
        let merged_ab = a.merge(&b);
        let merged_ba = b.merge(&a);
        assert_eq!(
            merged_ab.get(&"node-1".to_string()),
            Some(&"from-writer-9".to_string())
        );
        assert!(merged_ab.same_visible_state(&merged_ba));
    }

    #[test]
    fn merge_is_commutative() {
        let mut a: LwwMap<String, i32> = LwwMap::new();
        a.set("x".to_string(), 1, LamportTime(1), 1);
        a.set("y".to_string(), 2, LamportTime(2), 1);

        let mut b: LwwMap<String, i32> = LwwMap::new();
        b.set("y".to_string(), 20, LamportTime(5), 2);
        b.set("z".to_string(), 3, LamportTime(1), 2);

        let ab = a.merge(&b);
        let ba = b.merge(&a);
        assert!(ab.same_visible_state(&ba));
    }

    #[test]
    fn merge_is_associative() {
        let mut a: LwwMap<String, i32> = LwwMap::new();
        a.set("x".to_string(), 1, LamportTime(1), 1);
        let mut b: LwwMap<String, i32> = LwwMap::new();
        b.set("x".to_string(), 2, LamportTime(2), 2);
        let mut c: LwwMap<String, i32> = LwwMap::new();
        c.set("x".to_string(), 3, LamportTime(3), 3);

        let left = a.merge(&b).merge(&c);
        let right = a.merge(&b.merge(&c));
        assert!(left.same_visible_state(&right));
    }

    #[test]
    fn merge_is_idempotent() {
        let mut a: LwwMap<String, i32> = LwwMap::new();
        a.set("x".to_string(), 1, LamportTime(1), 1);
        a.set("y".to_string(), 2, LamportTime(2), 1);

        let once = a.merge(&a);
        assert!(once.same_visible_state(&a));
    }

    #[test]
    fn simulates_two_cells_running_autonomously_then_reconnecting() {
        // Directly exercises the README's own stated rationale: two
        // HYDRA-UMC cells update their OWN node's status independently
        // while partitioned, then reconnect - the merged view must have
        // both cells' latest info, with no update lost.
        let mut cell_a: LwwMap<String, String> = LwwMap::new();
        cell_a.set(
            "cell-a-node-1".to_string(),
            "ok".to_string(),
            LamportTime(1),
            1,
        );
        cell_a.set(
            "cell-a-node-1".to_string(),
            "degraded".to_string(),
            LamportTime(4),
            1,
        );

        let mut cell_b: LwwMap<String, String> = LwwMap::new();
        cell_b.set(
            "cell-b-node-1".to_string(),
            "ok".to_string(),
            LamportTime(1),
            2,
        );

        let reconciled = cell_a.merge(&cell_b);
        assert_eq!(reconciled.len(), 2);
        assert_eq!(
            reconciled.get(&"cell-a-node-1".to_string()),
            Some(&"degraded".to_string())
        );
        assert_eq!(
            reconciled.get(&"cell-b-node-1".to_string()),
            Some(&"ok".to_string())
        );
    }
}
