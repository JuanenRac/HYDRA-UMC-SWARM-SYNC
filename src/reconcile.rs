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

use crate::crdt::{LwwMap, MergeConflict};
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
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReconcileError::NoCells => write!(f, "scenario has no cells - nothing to reconcile"),
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
        map.set(w.key.clone(), w.value.clone(), LamportTime(w.time), cell.writer);
    }
    map
}

/// The exact real merge this project's own CLI already runs: merges
/// every cell's map left-to-right AND right-to-left, and reports
/// `converged` (both orders reaching the identical final state - the
/// actual property a real CRDT must have) rather than assuming it.
pub fn reconcile(scenario: &Scenario) -> Result<ReconcileOutput, ReconcileError> {
    if scenario.cells.is_empty() {
        return Err(ReconcileError::NoCells);
    }

    let maps: Vec<LwwMap<String, String>> = scenario.cells.iter().map(build_cell_map).collect();

    let mut conflicts: Vec<MergeConflict<String, String>> = Vec::new();
    let merged_forward = maps.iter().skip(1).fold(maps[0].clone(), |acc, m| {
        let (merged, round_conflicts) = acc.merge_report(m);
        conflicts.extend(round_conflicts);
        merged
    });
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
        clock.observe(latest);
    }
    let next_local_time = clock.tick();

    Ok(ReconcileOutput {
        cells_merged: scenario.cells.len(),
        converged,
        merged_state: forward_snapshot,
        conflicts_resolved: conflicts.len(),
        conflicts,
        next_local_time: next_local_time.0,
    })
}
