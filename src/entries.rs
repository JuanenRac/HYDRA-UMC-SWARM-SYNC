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
    /// Wall-clock milliseconds of the delete, on a tombstone only, and only from a node that keeps a retention window (retention.rs). A node
    /// that does not know the field ignores it, and an entry without it is dated when it is first seen.
    #[serde(
        default,
        rename = "deletedAtMs",
        skip_serializing_if = "Option::is_none"
    )]
    pub deleted_at_ms: Option<u64>,
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
            deleted_at_ms: None,
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
        if let Some(value) = &entry.value {
            crate::typed::check_value(&entry.key, value)
                .map_err(|reason| invalid(&entry.key, &reason))?;
            crate::typed::check_generation(&entry.key, value, entry.generation)
                .map_err(|reason| invalid(&entry.key, &reason))?;
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

/// The entries of a request as a map with the exact stamps they carry.
fn remote_map(entries: &[StampedEntry]) -> Result<LwwMap<String, String>, ReconcileError> {
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
    Ok(remote)
}

/// Merges the entries into `prior` with the exact stamps they carry. The same entry twice is idempotent; two different values under one
/// stamp are the impossible collision the merge refuses.
pub fn merge_entries(
    entries: &[StampedEntry],
    prior: LwwMap<String, String>,
) -> Result<(MergeOutput, LwwMap<String, String>), ReconcileError> {
    let remote = remote_map(entries)?;
    let (merged, conflicts) = prior
        .merge_report(&remote)
        .map_err(ReconcileError::ImpossibleConflict)?;
    let next = next_local_time(&merged)?;
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

/// The time this node stamps its next write with: one past the latest time in `map`, as the clock observes it.
fn next_local_time(map: &LwwMap<String, String>) -> Result<LamportTime, ReconcileError> {
    let mut clock = LamportClock::new();
    if let Some(latest) = map.max_time() {
        clock
            .observe(latest)
            .map_err(|_| ReconcileError::ClockOverflow)?;
    }
    clock.tick().map_err(|_| ReconcileError::ClockOverflow)
}

/// What `merge_entries_in_place` gives back: the output of the merge and the keys whose entry changed.
#[cfg_attr(not(test), allow(dead_code))] // the server switches over to the in-place merge in the next change
pub type InPlaceOutcome = Result<(MergeOutput, Vec<String>), ReconcileError>;

/// The value and the stamp (time, writer, generation) of one entry.
#[cfg_attr(not(test), allow(dead_code))] // the server switches over to the in-place merge in the next change
type EntryState = (Option<String>, u64, u64, u64);

/// `merge_entries` applied to `prior` itself, for the server: no copy of the state is made, and the keys that changed come back with the output
/// so that only those are written down. All or nothing: a refused request (a collision, a clock that would overflow) leaves `prior` as it was.
///
/// A clock at the very end of its range (a time within two of `u64::MAX`) is rare enough to take the copying path, which decides the overflow on
/// the merged map exactly as before.
#[cfg_attr(not(test), allow(dead_code))] // the server switches over to the in-place merge in the next change
pub fn merge_entries_in_place(
    entries: &[StampedEntry],
    prior: &mut LwwMap<String, String>,
) -> InPlaceOutcome {
    let near_the_end = |time: u64| time >= u64::MAX - 1;
    if prior.max_time().is_some_and(|t| near_the_end(t.0))
        || entries.iter().any(|e| near_the_end(e.time))
    {
        let (output, merged) = merge_entries(entries, prior.clone())?;
        let changed = changed_keys(prior, &merged);
        *prior = merged;
        return Ok((output, changed));
    }
    let remote = remote_map(entries)?;
    let (conflicts, changed) = prior
        .merge_report_in_place(&remote)
        .map_err(ReconcileError::ImpossibleConflict)?;
    let next = next_local_time(prior)?;
    Ok((
        MergeOutput {
            entries_received: entries.len(),
            conflicts_resolved: conflicts.len(),
            conflicts,
            next_local_time: next.0,
        },
        changed,
    ))
}

/// The keys whose entry (value or stamp) differs between `before` and `after`.
#[cfg_attr(not(test), allow(dead_code))] // the server switches over to the in-place merge in the next change
fn changed_keys(before: &LwwMap<String, String>, after: &LwwMap<String, String>) -> Vec<String> {
    let known: BTreeMap<String, EntryState> = before
        .entries_with_stamps()
        .into_iter()
        .map(|(key, value, time, writer, generation)| (key, (value, time.0, writer, generation)))
        .collect();
    after
        .entries_with_stamps()
        .into_iter()
        .filter(|(key, value, time, writer, generation)| {
            known.get(key) != Some(&(value.clone(), time.0, *writer, *generation))
        })
        .map(|(key, ..)| key)
        .collect()
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
            deleted_at_ms: None,
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

    // ---- missions and zone reservations: the attempt is the generation, and two orchestrators exchange them over a simulated link ----

    fn mission(id: &str, status: &str, owner: &str, attempt: u64) -> String {
        format!(
            r#"{{"mission_id":"{id}","status":"{status}","owner":"{owner}","attempt":{attempt}}}"#
        )
    }

    /// A link between two in-memory nodes: what one sends is checked and merged by the other, unless the link is cut.
    struct SimulatedLink {
        up: bool,
    }

    impl SimulatedLink {
        fn send(
            &self,
            from: &LwwMap<String, String>,
            to: LwwMap<String, String>,
        ) -> LwwMap<String, String> {
            if !self.up {
                return to;
            }
            let entries = state_entries(from);
            check_entries(&entries, &to, 1_000_000, 1_000).expect("a valid exchange");
            merge_entries(&entries, to).unwrap().1
        }
    }

    fn write_mission(
        map: &mut LwwMap<String, String>,
        record: &str,
        key: &str,
        attempt: u64,
        time: u64,
        writer: u64,
    ) {
        map.restore_entry(
            key.to_string(),
            Some(record.to_string()),
            LamportTime(time),
            writer,
            attempt + 1,
        );
    }

    #[test]
    fn a_newer_attempt_wins_over_an_older_one_whatever_the_clocks_say() {
        // A owns attempt 0 and keeps writing with a LATE clock while it is cut off; B takes the mission over as attempt 1 with an EARLY clock.
        let key = "mission/m-1";
        let mut a = LwwMap::new();
        let mut b = LwwMap::new();
        write_mission(
            &mut a,
            &mission("m-1", "running", "orch-a", 0),
            key,
            0,
            10,
            1,
        );
        let link = SimulatedLink { up: true };
        b = link.send(&a, b);
        let cut = SimulatedLink { up: false };
        write_mission(
            &mut b,
            &mission("m-1", "assigned", "orch-b", 1),
            key,
            1,
            11,
            2,
        );
        write_mission(
            &mut a,
            &mission("m-1", "completed", "orch-a", 0),
            key,
            0,
            900_000,
            1,
        );
        // while the link is cut nothing moves
        assert_eq!(
            cut.send(&a, b.clone()).get(&key.to_string()),
            b.get(&key.to_string())
        );
        // when it comes back, in either order of exchange, both end with attempt 1
        let a_after = link.send(&b, a.clone());
        let b_after = link.send(&a, b.clone());
        assert_eq!(state_entries(&a_after), state_entries(&b_after));
        let kept = a_after.get(&key.to_string()).unwrap();
        assert!(
            kept.contains("orch-b") && kept.contains("\"attempt\":1"),
            "{kept}"
        );
    }

    #[test]
    fn a_record_whose_generation_is_not_its_attempt_plus_one_is_refused_by_a_merge() {
        let local = LwwMap::new();
        let forged = entry(
            "mission/m-1",
            Some(&mission("m-1", "running", "orch-a", 4)),
            5,
            1,
            1, // attempt 4 must carry generation 5
        );
        let why = check_entries(&[forged], &local, 100, 10)
            .unwrap_err()
            .to_string();
        assert!(why.contains("attempt plus one"), "{why}");
        let honest = entry(
            "mission/m-1",
            Some(&mission("m-1", "running", "orch-a", 4)),
            5,
            1,
            5,
        );
        assert!(check_entries(&[honest], &local, 100, 10).is_ok());
        // a tombstone of a mission has no value to check
        assert!(check_entries(&[entry("mission/m-1", None, 6, 1, 2)], &local, 100, 10).is_ok());
    }

    #[test]
    fn a_zone_reservation_taken_over_with_a_higher_attempt_is_kept_by_both_nodes() {
        let key = "zone_reservation/cell-2";
        let reservation = |holder: &str, attempt: u64| {
            format!(
                r#"{{"zone_id":"cell-2","holder":"{holder}","attempt":{attempt},"expires_at_ms":1700000005000}}"#
            )
        };
        let mut a = LwwMap::new();
        let mut b = LwwMap::new();
        write_mission(&mut a, &reservation("arm-1", 0), key, 0, 50, 1);
        write_mission(&mut b, &reservation("arm-2", 1), key, 1, 20, 2);
        let link = SimulatedLink { up: true };
        let a_after = link.send(&b, a.clone());
        let b_after = link.send(&a, b.clone());
        assert_eq!(state_entries(&a_after), state_entries(&b_after));
        assert!(a_after.get(&key.to_string()).unwrap().contains("arm-2"));
    }

    #[test]
    fn the_deletion_time_is_optional_in_both_directions() {
        // an entry from a node that does not send it
        let old: StampedEntry =
            serde_json::from_str(r#"{"key":"k","value":null,"time":3,"writer":1,"generation":2}"#)
                .unwrap();
        assert_eq!(old.deleted_at_ms, None);
        // and it is left out of what such an entry sends back
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("deletedAtMs"), "{text}");
        let with: StampedEntry = serde_json::from_str(
            r#"{"key":"k","value":null,"time":3,"writer":1,"generation":2,"deletedAtMs":1700}"#,
        )
        .unwrap();
        assert_eq!(with.deleted_at_ms, Some(1700));
        assert!(serde_json::to_string(&with)
            .unwrap()
            .contains(r#""deletedAtMs":1700"#));
        // the time does not take part in what the merge decides: the same stamp with and without it is the same entry for the map
        let (_, a) = merge_entries(std::slice::from_ref(&old), LwwMap::new()).unwrap();
        let (_, b) = merge_entries(&[with], LwwMap::new()).unwrap();
        assert_eq!(state_entries(&a), state_entries(&b));
    }
}

#[cfg(test)]
mod in_place_tests {
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
            deleted_at_ms: None,
        }
    }

    fn prior() -> LwwMap<String, String> {
        let mut m = LwwMap::new();
        m.restore_entry(
            "a".to_string(),
            Some("one".to_string()),
            LamportTime(3),
            1,
            1,
        );
        m.restore_entry(
            "b".to_string(),
            Some("two".to_string()),
            LamportTime(4),
            1,
            1,
        );
        m
    }

    type Row = (String, Option<String>, u64, u64, u64);

    fn state(m: &LwwMap<String, String>) -> Vec<Row> {
        m.entries_with_stamps()
            .into_iter()
            .map(|(k, v, t, w, g)| (k, v, t.0, w, g))
            .collect()
    }

    #[test]
    fn in_place_gives_the_same_state_output_and_the_changed_keys() {
        let request = vec![
            entry("a", Some("newer"), 9, 2, 1),
            entry("b", Some("older"), 1, 2, 1),
            entry("c", Some("fresh"), 2, 2, 1),
        ];
        let (copied_output, copied) = merge_entries(&request, prior()).unwrap();
        let mut in_place = prior();
        let (output, changed) = merge_entries_in_place(&request, &mut in_place).unwrap();
        assert_eq!(state(&in_place), state(&copied));
        assert_eq!(output.next_local_time, copied_output.next_local_time);
        assert_eq!(output.conflicts_resolved, copied_output.conflicts_resolved);
        assert_eq!(changed, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn a_collision_leaves_the_state_as_it_was() {
        let request = vec![
            entry("a", Some("newer"), 9, 2, 1),
            entry("b", Some("different"), 4, 1, 1),
        ];
        let mut state_before = prior();
        let before = state(&state_before);
        assert!(merge_entries_in_place(&request, &mut state_before).is_err());
        assert_eq!(state(&state_before), before);
    }

    #[test]
    fn a_clock_near_the_end_of_its_range_gets_the_same_answer_as_the_copying_merge() {
        let request = vec![entry("z", Some("late"), u64::MAX - 1, 2, 1)];
        let copied = merge_entries(&request, prior());
        let mut in_place = prior();
        let before = state(&in_place);
        let outcome = merge_entries_in_place(&request, &mut in_place);
        assert!(copied.is_err());
        assert!(outcome.is_err());
        assert_eq!(
            state(&in_place),
            before,
            "an overflow leaves the state as it was"
        );
    }

    #[test]
    fn a_late_entry_that_is_overwritten_takes_the_copying_path_and_reports_the_changed_key() {
        // the entry at the end of the clock range is replaced by a newer generation with a small time, so the merged map has room for the next time
        let mut late = LwwMap::new();
        late.restore_entry(
            "z".to_string(),
            Some("late".to_string()),
            LamportTime(u64::MAX - 1),
            1,
            1,
        );
        late.restore_entry(
            "y".to_string(),
            Some("kept".to_string()),
            LamportTime(2),
            1,
            1,
        );
        let request = vec![entry("z", None, 5, 2, 2)];
        let (copied_output, copied) = merge_entries(&request, late.clone()).unwrap();
        let (output, changed) = merge_entries_in_place(&request, &mut late).unwrap();
        assert_eq!(state(&late), state(&copied));
        assert_eq!(output.next_local_time, copied_output.next_local_time);
        assert_eq!(changed, vec!["z".to_string()]);
    }

    #[test]
    fn seeded_random_requests_agree_with_the_copying_merge() {
        let mut seed: u64 = 0xfeed_beef;
        let mut next = move |bound: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % bound
        };
        for _ in 0..200 {
            let request: Vec<StampedEntry> = (0..next(6))
                .map(|i| {
                    entry(
                        &format!("k{}", next(5)),
                        Some(&format!("v{i}-{}", next(50))),
                        1 + next(12),
                        2 + next(2),
                        1,
                    )
                })
                .collect();
            let copied = merge_entries(&request, prior());
            let mut in_place = prior();
            let outcome = merge_entries_in_place(&request, &mut in_place);
            match (copied, outcome) {
                (Ok((copied_output, copied_map)), Ok((output, _changed))) => {
                    assert_eq!(state(&in_place), state(&copied_map));
                    assert_eq!(output.next_local_time, copied_output.next_local_time);
                }
                (Err(_), Err(_)) => assert_eq!(state(&in_place), state(&prior())),
                _ => panic!("the two merges disagree about whether the request is refused"),
            }
        }
    }
}
