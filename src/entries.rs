// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/entries.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! The way two nodes exchange state: the full entries of the map, stamps and tombstones included.
//!
//! `GET /state` answers only what is present (a value per key), which is all a reader needs and nothing a peer can merge: the time, the
//! writer and the generation of each entry, and the keys that were deleted, are what decide a conflict. `GET /state?full=1` answers them
//! (`StampedEntry`), and `POST /merge` takes them, so a node can be brought up to date from another one, whatever either has written and
//! deleted in the meantime. Everything that arrives is checked before it touches the map: the size of the entry, the time (no more than
//! `max_time_jump` past what this node already knows) and the generation (no more than `max_generation_jump` past the key's own). Without
//! the last two a peer could write a time or a generation so far ahead that nobody could ever overwrite or resurrect the key again.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crdt::{LwwMap, MergeConflict};
use crate::lamport::{LamportClock, LamportTime};
use crate::reconcile::ReconcileError;

/// The most entries one merge request may carry.
pub const MAX_ENTRIES: usize = 20_000;
/// The longest key and value an entry may have, in bytes.
pub const MAX_KEY_BYTES: usize = 256;
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// How far ahead of the generation this node holds for a key a remote entry may be, unless `--max-generation-jump` says otherwise.
pub const DEFAULT_MAX_GENERATION_JUMP: u64 = 1_000;

/// One entry of the map as a peer exchanges it. `value` is null for a tombstone: the key was deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampedEntry {
    pub key: String,
    pub value: Option<String>,
    pub time: u64,
    pub writer: u64,
    pub generation: u64,
}

#[derive(Debug, Deserialize)]
pub struct MergeRequest {
    pub entries: Vec<StampedEntry>,
}

#[derive(Debug, Serialize)]
pub struct MergeOutput {
    pub entries_received: usize,
    pub conflicts_resolved: usize,
    pub conflicts: Vec<MergeConflict<String, String>>,
    pub next_local_time: u64,
}

fn invalid(key: &str, reason: &str) -> ReconcileError {
    ReconcileError::InvalidEntry {
        key: key.chars().take(64).collect(),
        reason: reason.to_string(),
    }
}

/// Every entry of the map, tombstones included, in key order.
pub fn state_entries(map: &LwwMap<String, String>) -> Vec<StampedEntry> {
    map.entries_with_stamps()
        .into_iter()
        .map(|(key, value, time, writer, generation)| StampedEntry {
            key,
            value,
            time: time.0,
            writer,
            generation,
        })
        .collect()
}

/// Refuses entries this node will not merge. `local` is the node's own map (the generation of a key is measured from what it holds).
pub fn check_entries(
    entries: &[StampedEntry],
    local: &LwwMap<String, String>,
    max_time_jump: u64,
    max_generation_jump: u64,
) -> Result<(), ReconcileError> {
    if entries.is_empty() {
        return Err(ReconcileError::NoCells);
    }
    if entries.len() > MAX_ENTRIES {
        return Err(invalid("", "more entries than one request may carry"));
    }
    let time_limit = local
        .max_time()
        .map_or(0, |t| t.0)
        .saturating_add(max_time_jump);
    for entry in entries {
        if entry.key.is_empty()
            || entry.key.len() > MAX_KEY_BYTES
            || entry.key.chars().any(char::is_control)
        {
            return Err(invalid(
                &entry.key,
                "the key is empty, too long or holds control characters",
            ));
        }
        if entry
            .value
            .as_ref()
            .is_some_and(|v| v.len() > MAX_VALUE_BYTES)
        {
            return Err(invalid(&entry.key, "the value is too long"));
        }
        if entry.generation == 0 {
            return Err(invalid(&entry.key, "the generation starts at 1"));
        }
        if entry.time > time_limit {
            return Err(ReconcileError::StampTooFarAhead {
                key: entry.key.clone(),
                time: entry.time,
                limit: time_limit,
            });
        }
        let generation_limit = local
            .generation_of(&entry.key)
            .saturating_add(max_generation_jump);
        if entry.generation > generation_limit {
            return Err(ReconcileError::GenerationTooFarAhead {
                key: entry.key.clone(),
                generation: entry.generation,
                limit: generation_limit,
            });
        }
    }
    Ok(())
}

/// Merges the entries into `prior` with the exact stamps they carry. The same entry twice is idempotent; two different values under one
/// stamp are the impossible collision the merge refuses.
pub fn merge_entries(
    entries: &[StampedEntry],
    prior: LwwMap<String, String>,
) -> Result<(MergeOutput, LwwMap<String, String>), ReconcileError> {
    let mut remote: LwwMap<String, String> = LwwMap::new();
    // two entries of the request for the same key are two claims about it: the stamp decides, and an identical stamp with another value is a collision
    let mut seen: BTreeMap<&str, &StampedEntry> = BTreeMap::new();
    for entry in entries {
        if let Some(earlier) = seen.get(entry.key.as_str()) {
            if (earlier.generation, earlier.time, earlier.writer)
                == (entry.generation, entry.time, entry.writer)
                && earlier.value != entry.value
            {
                return Err(ReconcileError::ImpossibleConflict(
                    crate::crdt::IdentityCollision {
                        key: entry.key.clone(),
                        time: LamportTime(entry.time),
                        writer: entry.writer,
                        local_value: earlier.value.clone(),
                        remote_value: entry.value.clone(),
                    },
                ));
            }
        }
        seen.insert(entry.key.as_str(), entry);
        remote.restore_entry(
            entry.key.clone(),
            entry.value.clone(),
            LamportTime(entry.time),
            entry.writer,
            entry.generation,
        );
    }
    let (merged, conflicts) = prior
        .merge_report(&remote)
        .map_err(ReconcileError::ImpossibleConflict)?;
    let mut clock = LamportClock::new();
    if let Some(latest) = merged.max_time() {
        clock
            .observe(latest)
            .map_err(|_| ReconcileError::ClockOverflow)?;
    }
    let next = clock.tick().map_err(|_| ReconcileError::ClockOverflow)?;
    Ok((
        MergeOutput {
            entries_received: entries.len(),
            conflicts_resolved: conflicts.len(),
            conflicts,
            next_local_time: next.0,
        },
        merged,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        key: &str,
        value: Option<&str>,
        time: u64,
        writer: u64,
        generation: u64,
    ) -> StampedEntry {
        StampedEntry {
            key: key.to_string(),
            value: value.map(str::to_string),
            time,
            writer,
            generation,
        }
    }

    fn map(entries: &[StampedEntry]) -> LwwMap<String, String> {
        let mut m = LwwMap::new();
        for e in entries {
            m.restore_entry(
                e.key.clone(),
                e.value.clone(),
                LamportTime(e.time),
                e.writer,
                e.generation,
            );
        }
        m
    }

    #[test]
    fn the_state_of_a_map_round_trips_through_its_entries_tombstones_included() {
        let original = map(&[entry("a", Some("1"), 5, 1, 1), entry("b", None, 7, 2, 2)]);
        let entries = state_entries(&original);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1], entry("b", None, 7, 2, 2));
        let (_, merged) = merge_entries(&entries, LwwMap::new()).unwrap();
        assert_eq!(state_entries(&merged), entries);
    }

    #[test]
    fn a_deletion_of_a_peer_reaches_a_node_that_still_holds_the_value() {
        let local = map(&[entry("k", Some("old"), 10, 1, 1)]);
        let peer = vec![entry("k", None, 20, 2, 2)];
        check_entries(&peer, &local, 100, 5).unwrap();
        let (out, merged) = merge_entries(&peer, local).unwrap();
        assert_eq!(out.conflicts_resolved, 1);
        assert_eq!(merged.get(&"k".to_string()), None);
        // and the stale value cannot bring it back
        let stale = vec![entry("k", Some("old"), 10, 1, 1)];
        let (_, again) = merge_entries(&stale, merged).unwrap();
        assert_eq!(again.get(&"k".to_string()), None);
    }

    #[test]
    fn merging_the_same_entries_twice_changes_nothing() {
        let entries = vec![entry("k", Some("v"), 3, 1, 1)];
        let (_, once) = merge_entries(&entries, LwwMap::new()).unwrap();
        let (out, twice) = merge_entries(&entries, once.clone()).unwrap();
        assert_eq!(out.conflicts_resolved, 0);
        assert_eq!(state_entries(&once), state_entries(&twice));
    }

    #[test]
    fn the_same_stamp_with_another_value_is_refused_whether_it_comes_from_the_node_or_the_request()
    {
        let local = map(&[entry("k", Some("a"), 3, 1, 1)]);
        assert!(matches!(
            merge_entries(&[entry("k", Some("b"), 3, 1, 1)], local),
            Err(ReconcileError::ImpossibleConflict(_))
        ));
        let twice = vec![
            entry("k", Some("a"), 3, 1, 1),
            entry("k", Some("b"), 3, 1, 1),
        ];
        assert!(matches!(
            merge_entries(&twice, LwwMap::new()),
            Err(ReconcileError::ImpossibleConflict(_))
        ));
    }

    #[test]
    fn a_time_or_a_generation_too_far_ahead_is_refused() {
        let local = map(&[entry("k", Some("v"), 100, 1, 3)]);
        assert!(check_entries(&[entry("k", Some("v"), 150, 2, 3)], &local, 50, 2).is_ok());
        assert!(matches!(
            check_entries(&[entry("k", Some("v"), 151, 2, 3)], &local, 50, 2),
            Err(ReconcileError::StampTooFarAhead { limit: 150, .. })
        ));
        assert!(check_entries(&[entry("k", Some("v"), 100, 2, 5)], &local, 50, 2).is_ok());
        assert!(matches!(
            check_entries(&[entry("k", Some("v"), 100, 2, 6)], &local, 50, 2),
            Err(ReconcileError::GenerationTooFarAhead { limit: 5, .. })
        ));
        // a key this node never saw is measured from generation 0
        assert!(check_entries(&[entry("new", Some("v"), 1, 2, 2)], &local, 50, 2).is_ok());
        assert!(check_entries(&[entry("new", Some("v"), 1, 2, 3)], &local, 50, 2).is_err());
    }

    #[test]
    fn malformed_entries_are_refused() {
        let local = LwwMap::new();
        for bad in [
            entry("", Some("v"), 1, 1, 1),
            entry(&"k".repeat(MAX_KEY_BYTES + 1), Some("v"), 1, 1, 1),
            entry("line\nbreak", Some("v"), 1, 1, 1),
            entry("k", Some(&"v".repeat(MAX_VALUE_BYTES + 1)), 1, 1, 1),
            entry("k", Some("v"), 1, 1, 0),
        ] {
            assert!(check_entries(&[bad], &local, 100, 5).is_err());
        }
        assert!(matches!(
            check_entries(&[], &local, 100, 5),
            Err(ReconcileError::NoCells)
        ));
        let many: Vec<StampedEntry> = (0..=MAX_ENTRIES)
            .map(|i| entry(&format!("k{i}"), Some("v"), 1, 1, 1))
            .collect();
        assert!(check_entries(&many, &local, 100, 5).is_err());
    }

    #[test]
    fn two_nodes_that_exchange_their_entries_end_up_equal() {
        let a = map(&[
            entry("x", Some("from-a"), 5, 1, 1),
            entry("only-a", Some("1"), 2, 1, 1),
            entry("gone", None, 9, 1, 2),
        ]);
        let b = map(&[
            entry("x", Some("from-b"), 6, 2, 1),
            entry("only-b", Some("2"), 3, 2, 1),
            entry("gone", Some("back?"), 4, 2, 1),
        ]);
        let (_, a_after) = merge_entries(&state_entries(&b), a.clone()).unwrap();
        let (_, b_after) = merge_entries(&state_entries(&a), b.clone()).unwrap();
        assert_eq!(state_entries(&a_after), state_entries(&b_after));
        assert_eq!(a_after.get(&"x".to_string()), Some(&"from-b".to_string()));
        assert_eq!(a_after.get(&"gone".to_string()), None);
        assert_eq!(a_after.get(&"only-b".to_string()), Some(&"2".to_string()));
    }
}
