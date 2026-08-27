// HYDRA-UMC-SWARM-SYNC - entry point
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// Real CRDT state reconciliation (src/crdt.rs, src/lamport.rs), driven
// by a JSON scenario file - not yet the real PTP (IEEE 1588) hardware
// clock sync or a live gossip network between cells. Why: PTP needs real
// hardware timers/NICs to mean anything (sub-100ns jitter is not a
// software concept), and choosing a gossip transport is a real network
// design decision - both stay deferred, see mejoras_futuras.txt. What
// this DOES prove for real: the CRDT merge itself is commutative,
// associative and idempotent (see src/crdt.rs's own property tests),
// and this CLI demonstrates that same convergence on a concrete
// multi-cell scenario - merging in a different order still produces the
// identical final state, which is the actual property the README's "Why
// CRDT-based sync, not a single source of truth" architecture decision
// depends on.

mod crdt;
mod lamport;

use crdt::LwwMap;
use lamport::{LamportClock, LamportTime};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct Write {
    key: String,
    value: String,
    time: u64,
}

#[derive(Deserialize)]
struct Cell {
    #[allow(dead_code)] // kept in the scenario file for readability, not needed at runtime
    id: String,
    writer: u64,
    writes: Vec<Write>,
}

#[derive(Deserialize)]
struct Scenario {
    cells: Vec<Cell>,
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

fn main() -> ExitCode {
    println!("HYDRA-UMC-SWARM-SYNC v{VERSION}");
    println!("CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.");

    let args: Vec<String> = env::args().collect();
    let Some(scenario_path) = args.get(1) else {
        eprintln!("Usage: hydra-umc-swarm-sync <scenario.json>");
        eprintln!("See scenarios/example.json for the expected format.");
        return ExitCode::SUCCESS;
    };

    let raw = match fs::read_to_string(scenario_path) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("[swarm-sync] could not read {scenario_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let scenario: Scenario = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[swarm-sync] could not parse {scenario_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    if scenario.cells.is_empty() {
        eprintln!("[swarm-sync] scenario has no cells - nothing to reconcile");
        return ExitCode::FAILURE;
    }

    let maps: Vec<LwwMap<String, String>> = scenario.cells.iter().map(build_cell_map).collect();

    // Merge left-to-right, then right-to-left - if the CRDT is real,
    // both orders must converge to the identical final state. This is
    // the actual claim being demonstrated, not just "it ran".
    let merged_forward = maps
        .iter()
        .skip(1)
        .fold(maps[0].clone(), |acc, m| acc.merge(m));
    let merged_backward = maps
        .iter()
        .rev()
        .skip(1)
        .fold(maps[maps.len() - 1].clone(), |acc, m| acc.merge(m));

    let forward_snapshot: BTreeMap<String, String> = merged_forward.snapshot();
    let backward_snapshot: BTreeMap<String, String> = merged_backward.snapshot();
    let converged = forward_snapshot == backward_snapshot;

    // What a real node would do the instant after reconciling: fold the
    // newly-learned state into its own Lamport clock, so its very next
    // local write is provably ordered after everything the swarm just
    // taught it - this is the actual mechanism, not a demo stand-in for it.
    let mut clock = LamportClock::new();
    if let Some(latest) = merged_forward.max_time() {
        clock.observe(latest);
    }
    let next_local_time = clock.tick();

    let output = serde_json::json!({
        "cells_merged": scenario.cells.len(),
        "converged": converged,
        "merged_state": forward_snapshot,
        "next_local_time": next_local_time.0,
    });

    match serde_json::to_string_pretty(&output) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("[swarm-sync] could not serialize result: {e}");
            return ExitCode::FAILURE;
        }
    }

    if !converged {
        eprintln!("[swarm-sync] WARNING: merge order changed the result - this would be a real CRDT bug, not expected");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
