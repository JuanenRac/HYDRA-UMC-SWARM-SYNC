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
    pub value: String,
    pub time: u64,
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

pub enum ReconcileError {
    NoCells,
    /// SWARM-01: a real, "impossible" identity collision surfaced by
    /// `LwwMap::merge_report` - see that error type's own header comment.
    /// Refusing to produce a `ReconcileOutput` at all is the real
    /// "rechazo" (rejection) the situation calls for, rather than
    /// silently reporting a `converged` verdict
    /// that papers over an anomaly the merge itself couldn't resolve.
    ImpossibleConflict(IdentityCollision<String, String>),
    /// SWARM-01: this node's own Lamport clock reached `u64::MAX` while
    /// reconciling - see `ClockOverflowError`'s own header comment for
    /// why this must surface as a real error rather than a silent wrap.
    ClockOverflow,
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
        }
    }
}

#[derive(Serialize)]
pub struct ReconcileOutput {
    pub cells_merged: usize,
    pub converged: bool,
    pub merged_state: BTreeMap<String, String>,
    pub conflicts_resolved: usize,
    pub conflicts: Vec<MergeConflict<String, String>>,
    pub next_local_time: u64,
}

fn build_cell_map(cell: &Cell) -> LwwMap<String, String> {
    let mut map = LwwMap::new();
    for w in &cell.writes {
        map.set(
            w.key.clone(),
            w.value.clone(),
            LamportTime(w.time),
            cell.writer,
        );
    }
    map
}

/// The exact real merge this project's own CLI already runs: merges
/// every cell's map left-to-right AND right-to-left, and reports
/// `converged` (both orders reaching the identical final state - the
/// actual property a real CRDT must have) rather than assuming it.
/// Delegates to `reconcile_with_prior` with a fresh, empty prior state -
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
    maps.extend(scenario.cells.iter().map(build_cell_map));

    let mut conflicts: Vec<MergeConflict<String, String>> = Vec::new();
    // SWARM-01: try_fold (not fold) so a real IdentityCollision reported
    // by merge_report stops reconciliation immediately - propagated via
    // `?` below as ReconcileError::ImpossibleConflict, rather than being
    // silently absorbed into a `converged` verdict that never explains
    // what actually happened. The backward-order merge (used only to
    // cross-check convergence) is never even computed once this fires.
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
    let merged_backward = maps
        .iter()
        .rev()
        .skip(1)
        .fold(maps[maps.len() - 1].clone(), |acc, m| acc.merge(m));

    let forward_snapshot: BTreeMap<String, String> = merged_forward.snapshot();
    let backward_snapshot: BTreeMap<String, String> = merged_backward.snapshot();
    let converged = forward_snapshot == backward_snapshot;

    let mut clock = LamportClock::new();
    if let Some(latest) = merged_forward.max_time() {
        clock
            .observe(latest)
            .map_err(|_| ReconcileError::ClockOverflow)?;
    }
    let next_local_time = clock.tick().map_err(|_| ReconcileError::ClockOverflow)?;

    let output = ReconcileOutput {
        cells_merged: scenario.cells.len(),
        converged,
        merged_state: forward_snapshot,
        conflicts_resolved: conflicts.len(),
        conflicts,
        next_local_time: next_local_time.0,
    };
    Ok((output, merged_forward))
}
