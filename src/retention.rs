// HYDRA-UMC-SWARM-SYNC - retention.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// When a deletion happened, for dropping old tombstones (`--tombstone-retention-ms`, off by default).
//
// A tombstone is what stops a peer that never heard of a delete from bringing the key back, so it can only be
// dropped once no peer can still hold an older live copy. Nothing in the swarm can prove that, so the operator
// states it: "a node that is away longer than this window is not trusted any more". The window is counted from
// the wall-clock time of the delete (`deletedAtMs`), which travels with the tombstone, so every node counts the
// same age instead of each starting its own clock when the tombstone reaches it.
//
// The time lives beside the map, never inside the stamp: the CRDT order of two entries (generation, time, writer)
// is untouched, so the merge laws and every node's idea of which entry wins are exactly what they were. A
// tombstone without a time (written by a node that does not send one, or loaded from a file saved before) is dated
// when this node first sees it, which only keeps it longer. When two nodes report different times for the same
// tombstone the earliest is kept, so the nodes agree after one exchange.
//
// Risk, stated plainly: a node that was away longer than the window and still holds a live copy of a key that
// was deleted in the meantime brings that key back to life when it reconnects, because the tombstone that would
// have beaten it is gone. Choose a window much longer than the longest outage you accept. The times are wall-clock
// milliseconds, so the clocks of the nodes should agree to well within the window.

use std::collections::{BTreeMap, HashMap};

use crate::crdt::LwwMap;
use crate::entries::StampedEntry;
use crate::lamport::LamportTime;

type Stamp = (u64, u64, u64); // generation, logical time, writer

/// A tombstone that `LwwMap::purge_tombstones` may drop: key, generation, time, writer.
pub type Expired = (String, u64, LamportTime, u64);

#[derive(Debug, Default)]
pub struct DeletedAt {
    /// key -> (stamp of the tombstone the time belongs to, wall-clock ms of the delete)
    at: BTreeMap<String, (Stamp, u64)>,
}

impl DeletedAt {
    /// Brings the table in line with `state` after a change: every tombstone has a time, a time that belongs to an older tombstone of
    /// the key (or to a key that is live again) is forgotten. `hints` are the entries that arrived with the change, whose `deletedAtMs`
    /// is used for a tombstone with the same stamp; a time in the future is taken as `now`.
    pub fn observe(&mut self, state: &LwwMap<String, String>, hints: &[StampedEntry], now: u64) {
        let hinted: HashMap<(&str, Stamp), u64> = hints
            .iter()
            .filter(|e| e.value.is_none())
            .filter_map(|e| {
                e.deleted_at_ms.map(|at| {
                    (
                        (e.key.as_str(), (e.generation, e.time, e.writer)),
                        at.min(now),
                    )
                })
            })
            .collect();
        let mut next = BTreeMap::new();
        for (key, value, time, writer, generation) in state.entries_with_stamps() {
            if value.is_some() {
                continue;
            }
            let stamp = (generation, time.0, writer);
            let known = self
                .at
                .get(&key)
                .filter(|(s, _)| *s == stamp)
                .map(|(_, at)| *at);
            let hint = hinted.get(&(key.as_str(), stamp)).copied();
            let at = match (known, hint) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) | (None, Some(a)) => a,
                (None, None) => now,
            };
            next.insert(key, (stamp, at));
        }
        self.at = next;
    }

    /// Puts `deletedAtMs` on the tombstones among `entries` that this table dates (the others are sent as they are).
    pub fn attach(&self, entries: &mut [StampedEntry]) {
        for entry in entries.iter_mut().filter(|e| e.value.is_none()) {
            if let Some((stamp, at)) = self.at.get(&entry.key) {
                if *stamp == (entry.generation, entry.time, entry.writer) {
                    entry.deleted_at_ms = Some(*at);
                }
            }
        }
    }

    /// The tombstones of `state` that were deleted more than `retention_ms` before `now`.
    pub fn expired(
        &self,
        state: &LwwMap<String, String>,
        now: u64,
        retention_ms: u64,
    ) -> Vec<Expired> {
        state
            .tombstone_stamps()
            .into_iter()
            .filter(|(key, generation, time, writer)| {
                self.at.get(key).is_some_and(|(stamp, at)| {
                    *stamp == (*generation, time.0, *writer)
                        && now.saturating_sub(*at) > retention_ms
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub fn time_of(&self, key: &str) -> Option<u64> {
        self.at.get(key).map(|(_, at)| *at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with_tombstone() -> LwwMap<String, String> {
        let mut m = LwwMap::new();
        m.set("gone".to_string(), "v".to_string(), LamportTime(1), 1);
        m.remove("gone".to_string(), LamportTime(2), 1);
        m.set("kept".to_string(), "k".to_string(), LamportTime(3), 1);
        m
    }

    fn tomb(key: &str, time: u64, generation: u64, at: Option<u64>) -> StampedEntry {
        StampedEntry {
            key: key.to_string(),
            value: None,
            time,
            writer: 1,
            generation,
            deleted_at_ms: at,
        }
    }

    #[test]
    fn a_tombstone_without_a_time_is_dated_when_it_is_first_seen() {
        let mut table = DeletedAt::default();
        let state = map_with_tombstone();
        table.observe(&state, &[], 5_000);
        assert_eq!(table.time_of("gone"), Some(5_000));
        assert_eq!(
            table.time_of("kept"),
            None,
            "a live key has no deletion time"
        );
        table.observe(&state, &[], 9_000);
        assert_eq!(
            table.time_of("gone"),
            Some(5_000),
            "seeing it again does not date it again"
        );
    }

    #[test]
    fn the_time_that_arrives_with_the_tombstone_is_used_and_the_earliest_wins() {
        let mut table = DeletedAt::default();
        let state = map_with_tombstone();
        table.observe(&state, &[tomb("gone", 2, 2, Some(1_000))], 5_000);
        assert_eq!(table.time_of("gone"), Some(1_000));
        table.observe(&state, &[tomb("gone", 2, 2, Some(3_000))], 6_000);
        assert_eq!(
            table.time_of("gone"),
            Some(1_000),
            "a later report does not move it"
        );
        table.observe(&state, &[tomb("gone", 2, 2, Some(400))], 7_000);
        assert_eq!(table.time_of("gone"), Some(400), "an earlier report does");
    }

    #[test]
    fn a_time_in_the_future_or_for_another_tombstone_is_not_trusted() {
        let mut table = DeletedAt::default();
        let state = map_with_tombstone();
        table.observe(&state, &[tomb("gone", 2, 2, Some(99_000))], 5_000);
        assert_eq!(
            table.time_of("gone"),
            Some(5_000),
            "a future time is taken as now"
        );
        let mut other = DeletedAt::default();
        other.observe(&state, &[tomb("gone", 9, 2, Some(10))], 5_000);
        assert_eq!(
            other.time_of("gone"),
            Some(5_000),
            "a hint for another stamp is ignored"
        );
        let mut live = DeletedAt::default();
        let mut hint = tomb("gone", 2, 2, Some(10));
        hint.value = Some("x".to_string());
        live.observe(&state, &[hint], 5_000);
        assert_eq!(
            live.time_of("gone"),
            Some(5_000),
            "only a tombstone entry carries a deletion time"
        );
    }

    #[test]
    fn a_key_that_is_live_again_or_deleted_again_forgets_the_old_time() {
        let mut table = DeletedAt::default();
        let mut state = map_with_tombstone();
        table.observe(&state, &[], 1_000);
        state.set("gone".to_string(), "back".to_string(), LamportTime(10), 1);
        table.observe(&state, &[], 2_000);
        assert_eq!(table.time_of("gone"), None);
        state.remove("gone".to_string(), LamportTime(11), 1);
        table.observe(&state, &[], 3_000);
        assert_eq!(
            table.time_of("gone"),
            Some(3_000),
            "a new delete is dated anew"
        );
    }

    #[test]
    fn expired_is_strictly_older_than_the_window() {
        let mut table = DeletedAt::default();
        let state = map_with_tombstone();
        table.observe(&state, &[], 1_000);
        assert!(
            table.expired(&state, 1_000 + 500, 500).is_empty(),
            "exactly the window is not over it"
        );
        let expired = table.expired(&state, 1_000 + 501, 500);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, "gone");
        assert!(
            table.expired(&state, 0, 500).is_empty(),
            "a clock that went back drops nothing"
        );
        // a tombstone the table does not date is never dropped
        assert!(DeletedAt::default().expired(&state, u64::MAX, 1).is_empty());
    }

    #[test]
    fn attach_dates_only_the_tombstones_it_knows() {
        let mut table = DeletedAt::default();
        let state = map_with_tombstone();
        table.observe(&state, &[], 1_234);
        let mut entries = crate::entries::state_entries(&state);
        table.attach(&mut entries);
        let gone = entries.iter().find(|e| e.key == "gone").unwrap();
        assert_eq!(gone.deleted_at_ms, Some(1_234));
        assert_eq!(
            entries
                .iter()
                .find(|e| e.key == "kept")
                .unwrap()
                .deleted_at_ms,
            None
        );
    }
}
