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

mod clocksync;
mod crdt;
mod entries;
mod gossip;
#[allow(dead_code)] // only the SHA-256 and the HMAC of the signed token are used
mod hmac;
mod lamport;
mod node_writer;
mod reconcile;
mod retention;
mod server;
#[allow(dead_code)]
// the switch and the checks of the canonical copy; this service reads the token of a request only
mod server_identity;
mod service_metrics;
#[cfg(test)]
mod simulation_bench;
mod store;
mod typed;

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
    // The signing key of HYDRA-UMC-SERVER, from a file: with it a /reconcile needs a signed token and writes as the writer of that user. Off unless given.
    let identity_key = match find_flag(args, "--identity-key-file")
        .or_else(|| env::var("HYDRA_UMC_SWARM_SYNC_IDENTITY_KEY_FILE").ok())
        .filter(|path| !path.is_empty())
    {
        Some(path) => match server_identity::load_identity_key(&path) {
            Ok(key) => {
                eprintln!("[swarm-sync] --identity-key-file: a /reconcile needs a signed token of HYDRA-UMC-SERVER and writes as the writer of its user (print it with `writer-id USER`)");
                Some(key)
            }
            Err(e) => {
                eprintln!("[swarm-sync] fatal: --identity-key-file: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
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
    // how a delete and a concurrent add of the same key are resolved; the
    // default keeps a delete ahead of every add of the same life
    match find_flag(args, "--presence").as_deref() {
        None | Some("delete-wins") => crdt::set_add_wins(false),
        Some("add-wins") => crdt::set_add_wins(true),
        Some(other) => {
            eprintln!(
                "[swarm-sync] fatal: --presence must be delete-wins or add-wins, got {other:?}"
            );
            return ExitCode::from(2);
        }
    }
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
    // Dropping old tombstones is off unless a window is given: a node that is away longer than the window may bring deleted keys back (retention.rs).
    let tombstone_retention_ms = match find_flag(args, "--tombstone-retention-ms") {
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) if n >= 60_000 => Some(n),
            _ => {
                eprintln!("[swarm-sync] fatal: --tombstone-retention-ms must be a whole number of at least 60000 (one minute), got {raw:?}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    // Keeping a journal of the changed entries instead of writing the whole state at every merge is off unless a size is given (the journal is folded into a new snapshot past it).
    let journal_max_bytes = match find_flag(args, "--journal-max-bytes") {
        Some(raw) => match raw.parse::<u64>() {
            Ok(n) if n >= 4096 => Some(n),
            _ => {
                eprintln!("[swarm-sync] fatal: --journal-max-bytes must be a whole number of at least 4096, got {raw:?}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    // The measurement of the clock of the peers (and GET /time) is off unless the configuration file turns it on.
    // The same file opens `GET /metrics` with its `metrics` section (off unless enabled).
    let mut metrics = false;
    let clock = match find_flag(args, "--clock-sync-config")
        .or_else(|| env::var("SWARM_SYNC_CLOCK_CONFIG").ok())
        .filter(|p| !p.is_empty())
    {
        None => None,
        Some(path) => {
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    eprintln!("[swarm-sync] fatal: could not read {path}: {e}");
                    return ExitCode::from(2);
                }
            };
            match clocksync::parse_metrics(&text) {
                Ok(on) => metrics = on,
                Err(e) => {
                    eprintln!("[swarm-sync] fatal: {e}");
                    return ExitCode::from(2);
                }
            }
            match clocksync::parse_config(&text) {
                Ok(config) => config.map(|config| server::ClockSync {
                    config,
                    peers: peers.clone(),
                }),
                Err(e) => {
                    eprintln!("[swarm-sync] fatal: {e}");
                    return ExitCode::from(2);
                }
            }
        }
    };
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
            eprintln!(
                "[swarm-sync] POST /reconcile, GET /stats, GET /state, GET /conflicts, GET /health"
            );
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
            if let Some(ms) = tombstone_retention_ms {
                eprintln!("[swarm-sync] tombstones older than {ms} ms are dropped: a node that was away longer than that and still holds a deleted key brings it back");
            }
            if let Some(clock) = &clock {
                eprintln!(
                    "[swarm-sync] clock sync is on: GET /time answers, and {} peer(s) are probed every {} ms (software offset and round trip, not PTP)",
                    clock.peers.len(),
                    clock.config.interval_ms
                );
            }
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
                    clock,
                    metrics,
                    tombstone_retention_ms,
                    identity_key,
                    journal_max_bytes,
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

/// `writer-id USER`: the writer number of a user of HYDRA-UMC-SERVER (what a cell must name when that user's token is used).
fn run_writer_id(args: &[String]) -> ExitCode {
    match args
        .first()
        .filter(|name| !name.is_empty() && !name.starts_with("--"))
    {
        Some(name) => {
            println!("{}", node_writer::writer_for(name));
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("Usage: hydra-umc-swarm-sync writer-id USER");
            ExitCode::from(2)
        }
    }
}

/// `reseal --state-file F --out G --map OLD=USER [--map OLD=USER]...`: writes into G the state of F with the writer OLD of its entries replaced by the writer of USER.
/// F is never changed, and G must not exist.
fn run_reseal(args: &[String]) -> ExitCode {
    let (Some(input), Some(output)) = (find_flag(args, "--state-file"), find_flag(args, "--out"))
    else {
        eprintln!("Usage: hydra-umc-swarm-sync reseal --state-file F --out G --map OLD=USER [--map OLD=USER]...");
        return ExitCode::from(2);
    };
    let mut map: Vec<(u64, String)> = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if arg != "--map" {
            continue;
        }
        let Some((old, user)) = args.get(i + 1).and_then(|pair| pair.split_once('=')) else {
            eprintln!("[swarm-sync] --map needs OLD=USER");
            return ExitCode::from(2);
        };
        let Ok(old) = old.parse::<u64>() else {
            eprintln!("[swarm-sync] --map: {old:?} is not a writer number");
            return ExitCode::from(2);
        };
        map.push((old, user.to_string()));
    }
    if map.is_empty() {
        eprintln!("[swarm-sync] reseal needs at least one --map OLD=USER");
        return ExitCode::from(2);
    }
    if std::path::Path::new(&output).exists() {
        eprintln!("[swarm-sync] {output} exists: reseal never overwrites a file");
        return ExitCode::from(2);
    }
    let text = match fs::read_to_string(&input) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("[swarm-sync] could not read {input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match node_writer::reseal(&text, &map) {
        Ok((resealed, changed)) => match fs::write(&output, resealed) {
            Ok(()) => {
                println!("{changed} entries resealed into {output}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("[swarm-sync] could not write {output}: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("[swarm-sync] {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if args.get(1).map(|s| s.as_str()) == Some("serve") {
        return run_serve(&args[2..]);
    }
    if args.get(1).map(|s| s.as_str()) == Some("writer-id") {
        return run_writer_id(&args[2..]);
    }
    if args.get(1).map(|s| s.as_str()) == Some("reseal") {
        return run_reseal(&args[2..]);
    }

    println!("HYDRA-UMC-SWARM-SYNC v{VERSION}");
    println!("CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.");

    let Some(scenario_path) = args.get(1) else {
        eprintln!("Usage: hydra-umc-swarm-sync <scenario.json>");
        eprintln!(
            "       hydra-umc-swarm-sync serve [--addr ADDR] [--port PORT] [--state-file PATH] [--max-time-jump N] [--max-generation-jump N] [--presence delete-wins|add-wins] [--peer URL]... [--gossip-interval-ms N] [--gossip-push] [--tombstone-retention-ms N] [--clock-sync-config FILE] [--identity-key-file FILE] [--allow-anonymous]\n       hydra-umc-swarm-sync writer-id USER\n       hydra-umc-swarm-sync reseal --state-file F --out G --map OLD=USER"
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
