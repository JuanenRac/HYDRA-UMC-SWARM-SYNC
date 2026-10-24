// HYDRA-UMC-SWARM-SYNC - entry point
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// Real CRDT state reconciliation (src/crdt.rs, src/lamport.rs, src/
// reconcile.rs), driven by a JSON scenario file - not yet the real PTP
// (IEEE 1588) hardware clock sync or a live gossip network between
// cells. Why: PTP needs real hardware timers/NICs to mean anything
// (sub-100ns jitter is not a software concept), and choosing a gossip
// transport is a real network design decision - both stay deferred until
// there's real hardware/network infrastructure to build them against.
// What this DOES prove for real: the CRDT merge
// itself is commutative, associative and idempotent (see src/crdt.rs's
// own property tests), the property the README's "Why CRDT-based sync,
// not a single source of truth" architecture decision depends on. The
// CLI merges a concrete multi-cell scenario and reports every conflict
// it resolved.
//
// `serve` (new) reaches that exact same reconcile() function over a
// real HTTP API - still a request/response computation over a scenario
// handed to it, not the deferred live gossip network itself.

mod crdt;
mod entries;
mod gossip;
mod lamport;
mod reconcile;
mod server;
mod store;

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
    // Real per-node persistence, opt-in (see server.rs's own module doc
    // for the full "why"): unset, this node stays exactly the memory-only
    // behavior it always had; set, its own real CRDT state survives a
    // restart at this real file.
    let state_path = find_flag(args, "--state-file")
        .or_else(|| env::var("SWARM_SYNC_STATE_FILE").ok())
        .map(std::path::PathBuf::from);
    // Opt-in mutual authentication between nodes (see server.rs's own
    // is_authorized() for the full rationale) - unset keeps this node's
    // exact prior behavior (any caller on the LAN can reconcile/read
    // state), matching this project's documented LAN-trust posture; set,
    // every /reconcile and /state call must carry a matching
    // Authorization: Bearer <token>. Env var only, deliberately no CLI
    // flag - a shared secret belongs in the environment/systemd unit,
    // never in a process listing any local user can read via `ps`.
    let shared_secret = env::var("SWARM_SYNC_SHARED_SECRET")
        .ok()
        .filter(|s| !s.is_empty());

    let allow_anonymous = args.iter().any(|a| a == "--allow-anonymous");
    let max_time_jump = match find_flag(args, "--max-time-jump") {
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                eprintln!(
                    "[swarm-sync] fatal: --max-time-jump must be a whole number, got {raw:?}"
                );
                return ExitCode::from(2);
            }
        },
        None => reconcile::DEFAULT_MAX_TIME_JUMP,
    };
    let max_generation_jump = match find_flag(args, "--max-generation-jump") {
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) if n >= 1 => n,
            _ => {
                eprintln!("[swarm-sync] fatal: --max-generation-jump must be a whole number of at least 1, got {raw:?}");
                return ExitCode::from(2);
            }
        },
        None => entries::DEFAULT_MAX_GENERATION_JUMP,
    };
    // the peers to ask for their state (--peer http://host:port, repeatable)
    let mut peers = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--peer" {
            let Some(text) = args.get(i + 1) else {
                eprintln!("[swarm-sync] fatal: --peer needs an address like http://10.0.0.7:8112");
                return ExitCode::from(2);
            };
            match gossip::parse_peer(text) {
                Ok(peer) => peers.push(peer),
                Err(e) => {
                    eprintln!("[swarm-sync] fatal: --peer: {e}");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let gossip_interval_ms = match find_flag(args, "--gossip-interval-ms") {
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) if n >= 1_000 => n,
            _ => {
                eprintln!("[swarm-sync] fatal: --gossip-interval-ms must be a whole number of at least 1000, got {raw:?}");
                return ExitCode::from(2);
            }
        },
        None => 10_000,
    };
    let gossip_push = args.iter().any(|a| a == "--gossip-push");
    if !peers.is_empty() && shared_secret.is_none() {
        eprintln!("[swarm-sync] warning: --peer without SWARM_SYNC_SHARED_SECRET: the state of this node is asked for and sent with no authentication");
    }
    if let Some(problem) =
        server::startup_problem(&bind_addr, shared_secret.as_deref(), allow_anonymous)
    {
        eprintln!("[swarm-sync] fatal: {problem}");
        return ExitCode::from(2);
    }

    // The state is read BEFORE the port is bound: a state file that cannot be
    // read is a failed start (exit status 2), not a listening process that
    // quietly exits with success a moment later.
    let initial_state = match &state_path {
        Some(path) => match store::load(path) {
            Ok(map) => map,
            Err(e) => {
                eprintln!("[swarm-sync] fatal: could not load state from {path:?}: {e}");
                return ExitCode::from(2);
            }
        },
        None => crdt::LwwMap::new(),
    };

    match server::bind(&bind_addr) {
        Ok(bound) => {
            eprintln!("[swarm-sync] HTTP API listening on {bind_addr}");
            eprintln!("[swarm-sync] POST /reconcile, GET /stats, GET /state, GET /health");
            match &state_path {
                Some(path) => eprintln!("[swarm-sync] persisting this node's own state to {path:?}"),
                None => eprintln!(
                    "[swarm-sync] --state-file not set: memory-only, this node's state does NOT survive a restart"
                ),
            }
            match &shared_secret {
                Some(_) => eprintln!("[swarm-sync] SWARM_SYNC_SHARED_SECRET set: /reconcile and /state require a matching Authorization: Bearer <token>"),
                None => eprintln!("[swarm-sync] SWARM_SYNC_SHARED_SECRET not set: /reconcile and /state have NO caller authentication (LAN-trust mode)"),
            }
            eprintln!("[swarm-sync] POST /merge and GET /state?full=1 exchange the full entries with a peer");
            let gossip = if peers.is_empty() {
                None
            } else {
                eprintln!(
                    "[swarm-sync] asking {} peer(s) for their state every {} ms{}",
                    peers.len(),
                    gossip_interval_ms,
                    if gossip_push { " and sending ours" } else { "" }
                );
                Some(server::GossipConfig {
                    peers,
                    interval: std::time::Duration::from_millis(gossip_interval_ms),
                    push: gossip_push,
                })
            };
            server::run_with(
                bound,
                initial_state,
                state_path,
                shared_secret,
                max_time_jump,
                server::Extra {
                    max_generation_jump,
                    gossip,
                },
            );
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
        eprintln!(
            "       hydra-umc-swarm-sync serve [--addr ADDR] [--port PORT] [--state-file PATH] [--max-time-jump N] [--max-generation-jump N] [--peer URL]... [--gossip-interval-ms N] [--gossip-push] [--allow-anonymous]"
        );
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

    match serde_json::to_string_pretty(&output) {
        Ok(json) => println!("{json}"),
        Err(e) => {
            eprintln!("[swarm-sync] could not serialize result: {e}");
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}
