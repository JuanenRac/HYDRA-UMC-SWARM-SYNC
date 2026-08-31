// HYDRA-UMC-SWARM-SYNC - entry point
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// Real CRDT state reconciliation (src/crdt.rs, src/lamport.rs, src/
// reconcile.rs), driven by a JSON scenario file - not yet the real PTP
// (IEEE 1588) hardware clock sync or a live gossip network between
// cells. Why: PTP needs real hardware timers/NICs to mean anything
// (sub-100ns jitter is not a software concept), and choosing a gossip
// transport is a real network design decision - both stay deferred, see
// mejoras_futuras.txt. What this DOES prove for real: the CRDT merge
// itself is commutative, associative and idempotent (see src/crdt.rs's
// own property tests), and this CLI demonstrates that same convergence
// on a concrete multi-cell scenario - merging in a different order
// still produces the identical final state, which is the actual
// property the README's "Why CRDT-based sync, not a single source of
// truth" architecture decision depends on.
//
// `serve` (new) reaches that exact same reconcile() function over a
// real HTTP API - still a request/response computation over a scenario
// handed to it, not the deferred live gossip network itself.

mod crdt;
mod lamport;
mod reconcile;
mod server;

use reconcile::{reconcile, Scenario};
use std::env;
use std::fs;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn find_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn run_serve(args: &[String]) -> ExitCode {
    let addr = find_flag(args, "--addr").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = find_flag(args, "--port").unwrap_or_else(|| "8112".to_string());
    let bind_addr = format!("{addr}:{port}");

    match server::bind(&bind_addr) {
        Ok(bound) => {
            eprintln!("[swarm-sync] HTTP API listening on {bind_addr}");
            eprintln!("[swarm-sync] POST /reconcile, GET /stats");
            server::run(bound);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[swarm-sync] fatal: could not start HTTP server on {bind_addr}: {e}");
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("serve") {
        return run_serve(&args[2..]);
    }

    println!("HYDRA-UMC-SWARM-SYNC v{VERSION}");
    println!("CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.");

    let Some(scenario_path) = args.get(1) else {
        eprintln!("Usage: hydra-umc-swarm-sync <scenario.json>");
        eprintln!("       hydra-umc-swarm-sync serve [--addr ADDR] [--port PORT]");
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

    let output = match reconcile(&scenario) {
        Ok(output) => output,
        Err(e) => {
            eprintln!("[swarm-sync] {e}");
            return ExitCode::FAILURE;
        }
    };
    let converged = output.converged;

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
