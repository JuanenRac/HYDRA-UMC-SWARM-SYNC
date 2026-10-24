// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/reconcile.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! The real CRDT reconciliation this project's own CLI already runs,
//! split out into a pure function so a real HTTP caller (server.rs) can
//! reach it too - split out unchanged (behavior-preserving), not
//! rewritten, so both callers run the exact same merge logic.
//!
//! Deliberately still a request/response computation over a scenario
//! handed to it, never a live gossip network between cells - main.rs's
//! own module doc already explains why that transport choice stays
//! deferred (a real network design decision, not forgotten here).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crdt::{IdentityCollision, LwwMap, MergeConflict};
use crate::lamport::{LamportClock, LamportTime};

#[derive(Deserialize)]
pub struct Write {
    pub key: String,
    /// Ignored when `deleted` is true.
    #[serde(default)]
    pub value: String,
    pub time: u64,
    /// The write deletes the key (a tombstone) instead of setting it. The writes of a cell are applied in order, so a set followed by a
    /// delete of the same key is a key that was there and is gone.
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Deserialize)]
pub struct Cell {
    #[allow(dead_code)] // kept in the scenario file for readability, not needed at runtime
    pub id: String,
    pub writer: u64,
    pub writes: Vec<Write>,
}

#[derive(Deserialize)]
pub struct Scenario {
    pub cells: Vec<Cell>,
}

#[derive(Debug)]
pub enum ReconcileError {
    NoCells,
    /// a real, "impossible" identity collision surfaced by
    /// `LwwMap::merge_report` - see that error type's own header comment.
    /// Refusing to produce a `ReconcileOutput` at all is the real
    /// "rechazo" (rejection) the situation calls for, rather than
    /// silently reporting a merged state
    /// that papers over an anomaly the merge itself couldn't resolve.
    ImpossibleConflict(IdentityCollision<String, String>),
    /// this node's own Lamport clock reached `u64::MAX` while
    /// reconciling - see `ClockOverflowError`'s own header comment for
    /// why this must surface as a real error rather than a silent wrap.
    ClockOverflow,
    /// a write carries a Lamport time further ahead than this node accepts
    /// in one step - see `check_stamps`
    StampTooFarAhead {
        key: String,
        time: u64,
        limit: u64,
    },
    /// an entry of a peer carries a generation further ahead than this node accepts for that key - see `entries::check_entries`
    GenerationTooFarAhead {
        key: String,
        generation: u64,
        limit: u64,
    },
    /// an entry that cannot be merged (empty or oversized key, value too long, generation 0, too many entries)
    InvalidEntry {
        key: String,
        reason: String,
    },
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileError::NoCells => write!(f, "scenario has no cells - nothing to reconcile"),
            ReconcileError::ImpossibleConflict(collision) => write!(f, "{collision}"),
            ReconcileError::ClockOverflow => write!(
                f,
                "local Lamport clock reached u64::MAX while reconciling - refusing to wrap causal order"
            ),
            ReconcileError::StampTooFarAhead { key, time, limit } => write!(
                f,
                "write to {key:?} carries Lamport time {time}, past the {limit} this node accepts in one step: a time that far ahead could never be overwritten"
            ),
            ReconcileError::GenerationTooFarAhead { key, generation, limit } => write!(
                f,
                "entry {key:?} carries generation {generation}, past the {limit} this node accepts for it: a generation that far ahead could never be overtaken"
            ),
            ReconcileError::InvalidEntry { key, reason } => write!(f, "entry {key:?} cannot be merged: {reason}"),
        }
    }
}

#[derive(Serialize)]
pub struct ReconcileOutput {
    pub cells_merged: usize,
    pub merged_state: BTreeMap<String, String>,
    pub conflicts_resolved: usize,
    pub conflicts: Vec<MergeConflict<String, String>>,
    pub next_local_time: u64,
}

/// uses `set_checked` (not plain `set`) so two writes in this SAME
/// cell's own `writes` list, for the same key at the identical (time,
/// writer) stamp but with DIFFERENT values, are refused as the real
/// `IdentityCollision` they are - see `set_checked`'s own header comment
/// for exactly why plain `set` silently resolving this by insertion order
/// was a real bug (permuting `cell.writes` could change which value
/// "won", with neither permutation ever surfacing as a conflict, entirely
/// before `reconcile_with_prior`'s own cross-cell merge ever ran).
fn build_cell_map(
    cell: &Cell,
) -> Result<LwwMap<String, String>, IdentityCollision<String, String>> {
    let mut map = LwwMap::new();
    for w in &cell.writes {
        if w.deleted {
            map.remove(w.key.clone(), LamportTime(w.time), cell.writer);
            continue;
        }
        map.set_checked(
            w.key.clone(),
            w.value.clone(),
            LamportTime(w.time),
            cell.writer,
        )?;
    }
    Ok(map)
}

/// How far ahead of the newest time this node already knows a single write
/// may be, unless `--max-time-jump` says otherwise.
pub const DEFAULT_MAX_TIME_JUMP: u64 = 1_000_000;

/// Refuses a write whose Lamport time is more than `max_jump` past `known`
/// (the newest time in this node's state, or 0 when it is empty).
///
/// The time and the writer of a write are whatever the caller says. A write
/// with a time near `u64::MAX` wins every conflict for ever and, because the
/// clock refuses to pass it, can never be overwritten or even followed by an
/// honest write: the key stays captured until somebody edits the state file
/// by hand. Bounding each step keeps every value correctable: whatever one
/// request writes, the next request may go that far again.
pub fn check_stamps(
    scenario: &Scenario,
    known: Option<LamportTime>,
    max_jump: u64,
) -> Result<(), ReconcileError> {
    let limit = known.map_or(0, |t| t.0).saturating_add(max_jump);
    for cell in &scenario.cells {
        for w in &cell.writes {
            if w.time > limit {
                return Err(ReconcileError::StampTooFarAhead {
                    key: w.key.clone(),
                    time: w.time,
                    limit,
                });
            }
        }
    }
    Ok(())
}

/// The exact real merge this project's own CLI already runs: merges
/// every cell's map left to right and reports every conflict it resolved.
/// That the merge is order-independent is a property of the CRDT itself,
/// pinned by the property tests in `crdt.rs`, not something re-checked on
/// every call. Delegates to `reconcile_with_prior` with a fresh, empty prior state -
/// exactly this function's own original, unchanged behavior.
pub fn reconcile(scenario: &Scenario) -> Result<ReconcileOutput, ReconcileError> {
    reconcile_with_prior(scenario, LwwMap::new()).map(|(output, _)| output)
}

/// Real per-node persistence support: merges `scenario`'s own cells AND
/// `prior` - this node's own previously-persisted state (store.rs), or a
/// fresh empty map on that node's first run - into the same real
/// forward/backward convergence check `reconcile` already performs, so a
/// second call against a running server actually remembers what an
/// earlier one converged to instead of starting from a blank slate every
/// time. `prior` merges in exactly like one more real cell (the SAME
/// merge/conflict-resolution code path, no special case), but
/// `cells_merged` still counts only `scenario.cells` - `prior` is
/// already-known state, not a new cell reporting in this request.
/// Returns the real output alongside the new merged map, so the caller
/// (server.rs) can persist it back to disk.
pub fn reconcile_with_prior(
    scenario: &Scenario,
    prior: LwwMap<String, String>,
) -> Result<(ReconcileOutput, LwwMap<String, String>), ReconcileError> {
    if scenario.cells.is_empty() {
        return Err(ReconcileError::NoCells);
    }

    let mut maps: Vec<LwwMap<String, String>> = Vec::with_capacity(scenario.cells.len() + 1);
    maps.push(prior);
    for cell in &scenario.cells {
        maps.push(build_cell_map(cell).map_err(ReconcileError::ImpossibleConflict)?);
    }

    let mut conflicts: Vec<MergeConflict<String, String>> = Vec::new();
    // try_fold (not fold) so a real IdentityCollision reported
    // by merge_report stops reconciliation immediately - propagated via
    // `?` below as ReconcileError::ImpossibleConflict, rather than being
    // silently absorbed into a verdict that never explains what actually
    // happened.
    let merged_forward =
        maps.iter()
            .skip(1)
            .try_fold(maps[0].clone(), |acc, m| -> Result<_, ReconcileError> {
                let (merged, round_conflicts) = acc
                    .merge_report(m)
                    .map_err(ReconcileError::ImpossibleConflict)?;
                conflicts.extend(round_conflicts);
                Ok(merged)
            })?;
    let merged_forward_snapshot: BTreeMap<String, String> = merged_forward.snapshot();

    let mut clock = LamportClock::new();
    if let Some(latest) = merged_forward.max_time() {
        clock
            .observe(latest)
            .map_err(|_| ReconcileError::ClockOverflow)?;
    }
    let next_local_time = clock.tick().map_err(|_| ReconcileError::ClockOverflow)?;

    let output = ReconcileOutput {
        cells_merged: scenario.cells.len(),
        merged_state: merged_forward_snapshot,
        conflicts_resolved: conflicts.len(),
        conflicts,
        next_local_time: next_local_time.0,
    };
    Ok((output, merged_forward))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(id: &str, writer: u64, writes: &[(&str, &str, u64)]) -> Cell {
        Cell {
            id: id.to_string(),
            writer,
            writes: writes
                .iter()
                .map(|(k, v, t)| Write {
                    key: k.to_string(),
                    value: v.to_string(),
                    time: *t,
                    deleted: false,
                })
                .collect(),
        }
    }

    fn scenario(cells: Vec<Cell>) -> Scenario {
        Scenario { cells }
    }

    #[test]
    fn check_stamps_allows_exactly_the_limit_and_refuses_one_past_it() {
        let at_limit = scenario(vec![cell("a", 1, &[("k", "v", 100 + 50)])]);
        assert!(check_stamps(&at_limit, Some(LamportTime(100)), 50).is_ok());
        let past = scenario(vec![cell("a", 1, &[("k", "v", 100 + 51)])]);
        match check_stamps(&past, Some(LamportTime(100)), 50) {
            Err(ReconcileError::StampTooFarAhead { key, time, limit }) => {
                assert_eq!((key.as_str(), time, limit), ("k", 151, 150));
            }
            _ => panic!("a stamp past the limit must be refused"),
        }
    }

    #[test]
    fn check_stamps_measures_from_zero_when_nothing_is_known_yet() {
        let s = scenario(vec![cell("a", 1, &[("k", "v", DEFAULT_MAX_TIME_JUMP + 1)])]);
        assert!(check_stamps(&s, None, DEFAULT_MAX_TIME_JUMP).is_err());
        let ok = scenario(vec![cell("a", 1, &[("k", "v", DEFAULT_MAX_TIME_JUMP)])]);
        assert!(check_stamps(&ok, None, DEFAULT_MAX_TIME_JUMP).is_ok());
    }

    #[test]
    fn check_stamps_inspects_every_cell_not_only_the_first() {
        let s = scenario(vec![
            cell("a", 1, &[("k", "v", 1)]),
            cell("b", 2, &[("far", "v", 9_999_999_999)]),
        ]);
        match check_stamps(&s, Some(LamportTime(0)), 10) {
            Err(ReconcileError::StampTooFarAhead { key, .. }) => assert_eq!(key, "far"),
            _ => panic!("the second cell's runaway stamp must be found"),
        }
    }

    #[test]
    fn check_stamps_does_not_overflow_when_the_known_time_is_near_u64_max() {
        let s = scenario(vec![cell("a", 1, &[("k", "v", u64::MAX)])]);
        assert!(check_stamps(&s, Some(LamportTime(u64::MAX - 1)), u64::MAX).is_ok());
    }

    #[test]
    fn reconcile_without_cells_is_refused() {
        match reconcile(&scenario(vec![])) {
            Err(ReconcileError::NoCells) => {}
            _ => panic!("an empty scenario has nothing to reconcile"),
        }
    }

    #[test]
    fn the_later_stamp_wins_and_the_loser_is_reported_once() {
        let s = scenario(vec![
            cell("a", 1, &[("k", "old", 5)]),
            cell("b", 2, &[("k", "new", 9)]),
        ]);
        let out = reconcile(&s).ok().unwrap();
        assert_eq!(out.merged_state.get("k").map(String::as_str), Some("new"));
        assert_eq!(out.conflicts_resolved, 1);
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(out.cells_merged, 2);
        assert_eq!(
            out.next_local_time, 11,
            "the clock observes the latest stamp (9 -> 10) and then ticks once"
        );
    }

    #[test]
    fn the_same_stamp_from_two_writers_is_broken_by_the_larger_writer_id_in_either_order() {
        let ab = scenario(vec![
            cell("a", 1, &[("k", "from-1", 7)]),
            cell("b", 2, &[("k", "from-2", 7)]),
        ]);
        let ba = scenario(vec![
            cell("b", 2, &[("k", "from-2", 7)]),
            cell("a", 1, &[("k", "from-1", 7)]),
        ]);
        let one = reconcile(&ab).ok().unwrap();
        let two = reconcile(&ba).ok().unwrap();
        assert_eq!(one.merged_state, two.merged_state);
        assert_eq!(
            one.merged_state.get("k").map(String::as_str),
            Some("from-2")
        );
    }

    #[test]
    fn a_prior_state_is_merged_in_and_a_stale_write_does_not_beat_it() {
        let first = scenario(vec![cell("a", 1, &[("k", "fresh", 20)])]);
        let (_, state) = reconcile_with_prior(&first, LwwMap::new()).ok().unwrap();
        let stale = scenario(vec![cell("b", 2, &[("k", "stale", 3), ("other", "x", 4)])]);
        let (out, state) = reconcile_with_prior(&stale, state).ok().unwrap();
        assert_eq!(out.merged_state.get("k").map(String::as_str), Some("fresh"));
        assert_eq!(out.merged_state.get("other").map(String::as_str), Some("x"));
        assert_eq!(state.len(), 2);
        assert!(out.next_local_time > 20);
    }

    #[test]
    fn the_same_stamp_with_two_values_inside_one_cell_is_an_impossible_conflict() {
        let s = scenario(vec![cell("a", 1, &[("k", "one", 4), ("k", "two", 4)])]);
        match reconcile(&s) {
            Err(ReconcileError::ImpossibleConflict(_)) => {}
            _ => panic!("one writer cannot claim two values for the same stamp"),
        }
    }

    #[test]
    fn a_state_at_the_top_of_the_clock_range_is_refused_instead_of_wrapping() {
        let s = scenario(vec![cell("a", 1, &[("k", "v", u64::MAX)])]);
        match reconcile(&s) {
            Err(ReconcileError::ClockOverflow) => {}
            _ => panic!("the local clock must not wrap past u64::MAX"),
        }
    }

    #[test]
    fn every_error_has_a_readable_message() {
        assert!(ReconcileError::NoCells.to_string().contains("no cells"));
        assert!(ReconcileError::ClockOverflow
            .to_string()
            .contains("u64::MAX"));
        let far = ReconcileError::StampTooFarAhead {
            key: "k".into(),
            time: 9,
            limit: 3,
        };
        let text = far.to_string();
        assert!(text.contains("\"k\"") && text.contains('9') && text.contains('3'));
    }
}
