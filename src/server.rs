// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/server.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Plain JSON/HTTP surface (`tiny_http`, blocking, no async runtime) -
//! same convention as `HYDRA-UMC-TWIN`'s own `server.rs`. POST /reconcile
//! reaches the exact same `reconcile_with_prior()` the CLI's own bare
//! `reconcile()` delegates to - the scenario travels directly in the
//! JSON request body instead, since a server-side file path only ever
//! made sense for a CLI running on the same machine as the file.
//!
//! This server
//! used to be fully stateless - every /reconcile call started from a
//! blank slate and discarded its own result, so a real running instance
//! never remembered a previous call, and a restart never lost anything
//! because there was never anything to lose. `run()` now holds this
//! node's own real, persistent CRDT state in memory (loaded from
//! `state_path` at startup via `store.rs`, or empty on a real first run)
//! and merges every new scenario INTO it, persisting the result back to
//! disk after each call - `POST /reconcile` twice in a row now actually
//! accumulates, and a restart resumes from where this node left off.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::crdt::{LwwMap, MergeConflict};
use crate::entries::{
    check_entries, merge_entries, state_entries, MergeRequest, StampedEntry,
    DEFAULT_MAX_GENERATION_JUMP,
};
use crate::gossip::{self, Peer};
use crate::reconcile::{check_stamps, reconcile_with_prior, ReconcileError, Scenario};
use crate::store;

/// How many threads serve requests at the same time.
const WORKER_THREADS: usize = 4;

/// A reconcile scenario is the state of a few cells; a megabyte is far more
/// than a fleet needs and far less than a body that exhausts the memory of a
/// small board. Anything larger is refused with 413 before it is read.
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// True for the addresses that cannot be reached from another machine. Takes
/// a bare host (`127.0.0.1`, `::1`, `localhost`) or a host with a port
/// (`127.0.0.1:8112`, `[::1]:8112`, `localhost:8112`).
pub fn is_loopback_addr(addr: &str) -> bool {
    if let Ok(socket) = addr.parse::<std::net::SocketAddr>() {
        return socket.ip().is_loopback();
    }
    let host = match addr.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            host
        }
        _ => addr,
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Why this process must not start: a node that accepts other nodes' state
/// from the network without checking who sends it lets anyone on that
/// network rewrite the swarm's view of itself.
pub fn startup_problem(
    addr: &str,
    shared_secret: Option<&str>,
    allow_anonymous: bool,
) -> Option<String> {
    let has_secret = shared_secret.is_some_and(|s| !s.is_empty());
    if is_loopback_addr(addr) || has_secret || allow_anonymous {
        return None;
    }
    Some(format!(
        "refusing to listen on {addr} without SWARM_SYNC_SHARED_SECRET: set it, bind --addr 127.0.0.1, or pass --allow-anonymous to run an open node on purpose"
    ))
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

/// Real gap this closes: `/reconcile` and `/state` had no way at all to
/// tell which node (or anyone else on the same LAN) is talking to them -
/// any caller could inject fabricated cell state into another node's own
/// CRDT, or read it. Opt-in via `shared_secret` (see `SWARM_SYNC_SHARED_SECRET`
/// in main.rs): `None` keeps this server's exact prior behavior (no
/// caller authentication at all, matching the LAN-trust posture this
/// project has documented from the start); `Some` requires a matching
/// `Authorization: Bearer <secret>` header on every `/reconcile` and
/// `/state` request, `/stats` stays open either way (a harmless
/// diagnostic, same reasoning as this ecosystem's other services keeping
/// a bare liveness/status route unauthenticated).
fn is_authorized(request: &tiny_http::Request, shared_secret: &Option<String>) -> bool {
    let Some(secret) = shared_secret else {
        return true;
    };
    let presented = request
        .headers()
        .iter()
        .find(|h| {
            h.field
                .as_str()
                .as_str()
                .eq_ignore_ascii_case("Authorization")
        })
        .map(|h| h.value.as_str());
    match presented.and_then(|v| v.strip_prefix("Bearer ")) {
        Some(token) => constant_time_eq(token.as_bytes(), secret.as_bytes()),
        None => false,
    }
}

/// A naive `==` on the raw token would let an attacker measure how many
/// leading bytes matched via response-time differences (a real, if slow,
/// side channel against a long-lived shared secret) - this always
/// compares every byte of the longer operand regardless of where the
/// first mismatch is, and folds a length mismatch into the same
/// constant-time path rather than short-circuiting on it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_ok = a.len() == b.len();
    let compare_len = a.len().max(b.len());
    let mut diff: u8 = if len_ok { 0 } else { 1 };
    for i in 0..compare_len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

fn write_json(request: tiny_http::Request, status: u16, body: &serde_json::Value) {
    let text = body.to_string();
    let response = Response::from_string(text)
        .with_status_code(status)
        .with_header(json_header());
    let _ = request.respond(response);
}

pub fn bind(addr: &str) -> std::io::Result<Server> {
    Server::http(addr).map_err(std::io::Error::other)
}

/// Real per-node persistence, opt-in: `state_path` given loads/saves this
/// node's own state to that real file (see store.rs); `None` keeps this
/// server's own memory-only behavior for the lifetime of the process (a
/// real, supported mode - a short-lived test/demo instance with nothing
/// worth surviving a restart), same as before this pass, just no longer
/// the only mode.
/// What this node can say about its own synchronization: how many
/// reconciliations it completed, how many conflicts they resolved, when the
/// last one finished and which write won the most recent conflict.
#[derive(Default)]
struct SyncEvidence {
    reconciles: u64,
    /// Merges of stamped entries (from a peer or from `POST /merge`).
    merges: u64,
    conflicts_total: u64,
    last_reconcile_ms: Option<u64>,
    last_conflict: Option<serde_json::Value>,
    /// The most recent conflicts that were resolved, oldest first, at most
    /// `CONFLICT_HISTORY` of them (`GET /conflicts`).
    history: std::collections::VecDeque<serde_json::Value>,
}

/// How many resolved conflicts a node remembers; older ones are forgotten
/// first, `conflictsResolved` keeps counting all of them.
const CONFLICT_HISTORY: usize = 200;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl SyncEvidence {
    /// Remembers every conflict a reconcile or a merge resolved: the key, the
    /// writer whose value was kept and the writer whose value was dropped.
    fn record_conflicts(&mut self, conflicts: &[MergeConflict<String, String>], at_ms: u64) {
        for conflict in conflicts {
            let (winner, loser) = if conflict.kept_remote {
                (conflict.remote_writer, conflict.local_writer)
            } else {
                (conflict.local_writer, conflict.remote_writer)
            };
            let entry = json!({
                "key": conflict.key,
                "winnerWriter": winner,
                "loserWriter": loser,
                "atMs": at_ms,
            });
            self.last_conflict = Some(json!({
                "key": conflict.key,
                "winnerWriter": winner,
                "loserWriter": loser,
            }));
            if self.history.len() == CONFLICT_HISTORY {
                self.history.pop_front();
            }
            self.history.push_back(entry);
        }
    }

    /// The newest `limit` resolved conflicts, newest first.
    fn conflicts_json(&self, limit: usize) -> serde_json::Value {
        let newest: Vec<&serde_json::Value> = self.history.iter().rev().take(limit).collect();
        json!({
            "conflictsResolved": self.conflicts_total,
            "remembered": self.history.len(),
            "conflicts": newest,
        })
    }

    fn to_json(&self, now: u64) -> serde_json::Value {
        json!({
            "reconciles": self.reconciles,
            "merges": self.merges,
            "conflictsResolved": self.conflicts_total,
            "lastReconcileAgeMs": self.last_reconcile_ms.map(|t| now.saturating_sub(t)),
            "lastConflict": self.last_conflict,
        })
    }
}

/// What every worker shares: the node's CRDT state and its sync evidence.
/// A reconcile holds the state lock from the merge until the new state is on
/// disk, so two requests can never persist over each other.
struct Shared {
    state: Mutex<LwwMap<String, String>>,
    evidence: Mutex<SyncEvidence>,
    state_path: Option<PathBuf>,
    shared_secret: Option<String>,
    max_time_jump: u64,
    max_generation_jump: u64,
    gossip: Mutex<Vec<PeerEvidence>>,
}

/// What this node knows of its exchange with one peer.
struct PeerEvidence {
    address: String,
    rounds: u64,
    last_ok_ms: Option<u64>,
    last_error: Option<String>,
    entries_pulled: u64,
    entries_pushed: u64,
}

/// The peers this node asks for their state, and how.
#[derive(Clone)]
pub struct GossipConfig {
    pub peers: Vec<Peer>,
    pub interval: std::time::Duration,
    /// Also send this node's state to each peer (`POST /merge`) after pulling theirs.
    pub push: bool,
}

/// What `run_with` takes besides the socket and the state.
#[derive(Clone)]
pub struct Extra {
    pub max_generation_jump: u64,
    pub gossip: Option<GossipConfig>,
}

impl Default for Extra {
    fn default() -> Self {
        Extra {
            max_generation_jump: DEFAULT_MAX_GENERATION_JUMP,
            gossip: None,
        }
    }
}

/// Serves requests until the listener closes. `initial_state` is what the
/// caller loaded from `state_path` (main.rs reads it before binding, so a
/// state file that cannot be read stops the start instead of leaving a
/// listening process that exits with success). Requests are served by a
/// small pool of workers: a client that uploads slowly holds one worker,
/// not the node. `max_time_jump` bounds how far ahead of the newest time in
/// the state a single write may be (see `reconcile::check_stamps`).
#[cfg(test)]
pub fn run(
    server: Server,
    initial_state: LwwMap<String, String>,
    state_path: Option<PathBuf>,
    shared_secret: Option<String>,
    max_time_jump: u64,
) {
    run_with(
        server,
        initial_state,
        state_path,
        shared_secret,
        max_time_jump,
        Extra::default(),
    );
}

/// `run`, with the bound on generations and the gossip client.
pub fn run_with(
    server: Server,
    initial_state: LwwMap<String, String>,
    state_path: Option<PathBuf>,
    shared_secret: Option<String>,
    max_time_jump: u64,
    extra: Extra,
) {
    let peers: Vec<PeerEvidence> = extra
        .gossip
        .as_ref()
        .map(|g| {
            g.peers
                .iter()
                .map(|p| PeerEvidence {
                    address: format!("{}{}", p.host_port, p.prefix),
                    rounds: 0,
                    last_ok_ms: None,
                    last_error: None,
                    entries_pulled: 0,
                    entries_pushed: 0,
                })
                .collect()
        })
        .unwrap_or_default();
    let shared = Arc::new(Shared {
        state: Mutex::new(initial_state),
        evidence: Mutex::new(SyncEvidence::default()),
        state_path,
        shared_secret,
        max_time_jump,
        max_generation_jump: extra.max_generation_jump,
        gossip: Mutex::new(peers),
    });
    if let Some(config) = extra.gossip {
        let shared = Arc::clone(&shared);
        thread::spawn(move || gossip_loop(&shared, &config));
    }
    let server = Arc::new(server);
    for _ in 1..WORKER_THREADS {
        let server = Arc::clone(&server);
        let shared = Arc::clone(&shared);
        thread::spawn(move || serve(&server, &shared));
    }
    serve(&server, &shared);
}

fn serve(server: &Server, shared: &Shared) {
    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_string();

        if path == "/health" && request.method() == &Method::Get {
            write_json(
                request,
                200,
                &json!({
                    "status": "ok",
                    "name": "HYDRA-UMC-SWARM-SYNC",
                    "version": env!("CARGO_PKG_VERSION"),
                }),
            );
            continue;
        }
        if path == "/stats" && request.method() == &Method::Get {
            let sync = shared.evidence.lock().unwrap().to_json(now_ms());
            write_json(
                request,
                200,
                &json!({
                    "role": "CRDT swarm state reconciliation",
                    "persistent": shared.state_path.is_some(),
                    "sync": sync,
                    "peers": peers_json(shared, now_ms()),
                }),
            );
            continue;
        }
        if path == "/conflicts" && request.method() == &Method::Get {
            if !is_authorized(&request, &shared.shared_secret) {
                write_json(
                    request,
                    401,
                    &json!({"error": "missing or invalid Authorization: Bearer <token>"}),
                );
                continue;
            }
            let limit = request
                .url()
                .split_once('?')
                .and_then(|(_, q)| q.split('&').find_map(|p| p.strip_prefix("limit=")))
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(CONFLICT_HISTORY)
                .clamp(1, CONFLICT_HISTORY);
            let body = shared.evidence.lock().unwrap().conflicts_json(limit);
            write_json(request, 200, &body);
            continue;
        }
        if path == "/state" && request.method() == &Method::Get {
            if !is_authorized(&request, &shared.shared_secret) {
                write_json(
                    request,
                    401,
                    &json!({"error": "missing or invalid Authorization: Bearer <token>"}),
                );
                continue;
            }
            if request
                .url()
                .split_once('?')
                .is_some_and(|(_, q)| q.split('&').any(|p| p == "full=1"))
            {
                // every entry with its stamp, tombstones included: what a peer needs to merge
                let entries = state_entries(&shared.state.lock().unwrap());
                write_json(request, 200, &json!({ "entries": entries }));
                continue;
            }
            let snapshot = shared.state.lock().unwrap().snapshot();
            write_json(request, 200, &json!(snapshot));
            continue;
        }
        if !matches!(path.as_str(), "/reconcile" | "/merge") || request.method() != &Method::Post {
            write_json(request, 404, &json!({"error": "not found"}));
            continue;
        }
        if !is_authorized(&request, &shared.shared_secret) {
            write_json(
                request,
                401,
                &json!({"error": "missing or invalid Authorization: Bearer <token>"}),
            );
            continue;
        }

        // a body that announces more than the limit is refused before any of
        // it is read
        if request
            .body_length()
            .is_some_and(|len| len as u64 > MAX_BODY_BYTES)
        {
            write_json(
                request,
                413,
                &json!({"error": format!("request body is larger than {MAX_BODY_BYTES} bytes")}),
            );
            continue;
        }
        let mut raw = String::new();
        match Read::take(request.as_reader(), MAX_BODY_BYTES + 1).read_to_string(&mut raw) {
            Ok(_) if raw.len() as u64 > MAX_BODY_BYTES => {
                write_json(
                    request,
                    413,
                    &json!({"error": format!("request body is larger than {MAX_BODY_BYTES} bytes")}),
                );
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                write_json(
                    request,
                    400,
                    &json!({"error": format!("could not read request body: {e}")}),
                );
                continue;
            }
        }

        if path == "/merge" {
            let merge: MergeRequest = match serde_json::from_str(&raw) {
                Ok(m) => m,
                Err(e) => {
                    write_json(
                        request,
                        400,
                        &json!({"error": format!("malformed merge JSON: {e}")}),
                    );
                    continue;
                }
            };
            let (status, body) = merge_request(shared, &merge.entries);
            write_json(request, status, &body);
            continue;
        }

        let scenario: Scenario = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                write_json(
                    request,
                    400,
                    &json!({"error": format!("malformed scenario JSON: {e}")}),
                );
                continue;
            }
        };

        let (status, body) = reconcile_request(shared, &scenario);
        write_json(request, status, &body);
    }
}

/// Merges `scenario` into the node's state, persists the result and answers
/// with the status and body to send.
fn reconcile_request(shared: &Shared, scenario: &Scenario) -> (u16, serde_json::Value) {
    let mut state = shared.state.lock().unwrap();
    if let Err(e) = check_stamps(scenario, state.max_time(), shared.max_time_jump) {
        return error_response(&e);
    }
    for cell in &scenario.cells {
        for write in cell.writes.iter().filter(|w| !w.deleted) {
            if let Err(reason) = crate::typed::check_value(&write.key, &write.value) {
                return error_response(&ReconcileError::InvalidEntry {
                    key: write.key.chars().take(64).collect(),
                    reason,
                });
            }
            let required = crate::typed::required_generation(&write.key, &write.value)
                .ok()
                .flatten();
            let problem = match (required, write.generation) {
                (Some(r), Some(g)) if r == g => None,
                (Some(r), Some(g)) => Some(format!(
                    "the generation of a record is its attempt plus one ({r}), not {g}"
                )),
                (Some(r), None) => Some(format!(
                    "a mission or a zone reservation needs \"generation\": {r} (its attempt plus one)"
                )),
                (None, Some(_)) => {
                    Some("generation is only for missions and zone reservations".to_string())
                }
                (None, None) => None,
            };
            if let Some(reason) = problem {
                return error_response(&ReconcileError::InvalidEntry {
                    key: write.key.chars().take(64).collect(),
                    reason,
                });
            }
        }
    }
    match reconcile_with_prior(scenario, state.clone()) {
        Ok((output, merged)) => {
            // Persist BEFORE responding success - a caller told "200
            // OK, merged" must be able to trust this node's own next
            // /reconcile (or a restart) actually remembers it. A
            // real save failure is a real 500, not a silent "it
            // worked" that quietly didn't.
            if let Some(path) = &shared.state_path {
                if let Err(e) = store::save(path, &merged) {
                    return (
                        500,
                        json!({"error": format!("merged successfully but could not persist state to {path:?}: {e}")}),
                    );
                }
            }
            *state = merged;
            drop(state);
            let mut evidence = shared.evidence.lock().unwrap();
            evidence.reconciles += 1;
            evidence.conflicts_total += output.conflicts_resolved as u64;
            evidence.last_reconcile_ms = Some(now_ms());
            let at = now_ms();
            evidence.record_conflicts(&output.conflicts, at);
            (200, serde_json::to_value(&output).unwrap())
        }
        Err(e) => error_response(&e),
    }
}

/// Merges stamped entries (from a peer or `POST /merge`) into the node's state, persists the result and answers with the status and body.
fn merge_request(shared: &Shared, entries: &[StampedEntry]) -> (u16, serde_json::Value) {
    let mut state = shared.state.lock().unwrap();
    if let Err(e) = check_entries(
        entries,
        &state,
        shared.max_time_jump,
        shared.max_generation_jump,
    ) {
        return error_response(&e);
    }
    match merge_entries(entries, state.clone()) {
        Ok((output, merged)) => {
            if let Some(path) = &shared.state_path {
                if let Err(e) = store::save(path, &merged) {
                    return (
                        500,
                        json!({"error": format!("merged successfully but could not persist state to {path:?}: {e}")}),
                    );
                }
            }
            *state = merged;
            drop(state);
            let mut evidence = shared.evidence.lock().unwrap();
            evidence.merges += 1;
            evidence.conflicts_total += output.conflicts_resolved as u64;
            evidence.last_reconcile_ms = Some(now_ms());
            let at = now_ms();
            evidence.record_conflicts(&output.conflicts, at);
            (200, serde_json::to_value(&output).unwrap())
        }
        Err(e) => error_response(&e),
    }
}

fn peers_json(shared: &Shared, now: u64) -> serde_json::Value {
    let peers = shared.gossip.lock().unwrap();
    json!(peers
        .iter()
        .map(|p| json!({
            "peer": p.address,
            "rounds": p.rounds,
            "lastOkAgeMs": p.last_ok_ms.map(|t| now.saturating_sub(t)),
            "lastError": p.last_error,
            "entriesPulled": p.entries_pulled,
            "entriesPushed": p.entries_pushed,
        }))
        .collect::<Vec<_>>())
}

/// One round with every peer: pull their state and merge it, then (if asked) send ours. A peer that cannot be reached or whose state is
/// refused is recorded and the round goes on with the next.
fn gossip_round(shared: &Shared, config: &GossipConfig) {
    let token = shared.shared_secret.as_deref();
    for (index, peer) in config.peers.iter().enumerate() {
        let outcome: Result<(u64, u64), String> = (|| {
            let entries = gossip::pull(peer, token)?;
            let pulled = entries.len() as u64;
            if !entries.is_empty() {
                let (status, body) = merge_request(shared, &entries);
                if status != 200 {
                    return Err(format!(
                        "the state of {} was not merged: {}",
                        peer.host_port,
                        body["error"].as_str().unwrap_or("refused")
                    ));
                }
            }
            let mut pushed = 0;
            if config.push {
                let ours = state_entries(&shared.state.lock().unwrap());
                if !ours.is_empty() {
                    gossip::push(peer, token, &ours)?;
                    pushed = ours.len() as u64;
                }
            }
            Ok((pulled, pushed))
        })();
        let mut peers = shared.gossip.lock().unwrap();
        let evidence = &mut peers[index];
        evidence.rounds += 1;
        match outcome {
            Ok((pulled, pushed)) => {
                evidence.last_ok_ms = Some(now_ms());
                evidence.last_error = None;
                evidence.entries_pulled += pulled;
                evidence.entries_pushed += pushed;
            }
            Err(e) => {
                eprintln!("[swarm-sync] gossip with {}: {e}", evidence.address);
                evidence.last_error = Some(e);
            }
        }
    }
}

fn gossip_loop(shared: &Shared, config: &GossipConfig) {
    loop {
        gossip_round(shared, config);
        thread::sleep(config.interval);
    }
}

/// What the caller did wrong is a 400 (nothing to merge) or a 422 (a write
/// this node refuses); a collision or a clock at its limit is a conflict with
/// the node's own state, a 409.
fn error_response(e: &ReconcileError) -> (u16, serde_json::Value) {
    let status = match e {
        ReconcileError::NoCells => 400,
        ReconcileError::StampTooFarAhead { .. } => 422,
        ReconcileError::GenerationTooFarAhead { .. } => 422,
        ReconcileError::InvalidEntry { .. } => 422,
        ReconcileError::ImpossibleConflict(_) => 400,
        ReconcileError::ClockOverflow => 409,
    };
    (status, json!({"error": e.to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

    fn start_test_server() -> u16 {
        start_test_server_with_state(None)
    }

    fn start_test_server_with_state(state_path: Option<std::path::PathBuf>) -> u16 {
        start_test_server_full(state_path, None)
    }

    fn start_test_server_full(
        state_path: Option<std::path::PathBuf>,
        shared_secret: Option<String>,
    ) -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("tiny_http always binds a real IP socket for an http:// server")
            .port();
        let initial = match &state_path {
            Some(path) => store::load(path).expect("the test state file must load"),
            None => LwwMap::new(),
        };
        thread::spawn(move || {
            run(
                server,
                initial,
                state_path,
                shared_secret,
                crate::reconcile::DEFAULT_MAX_TIME_JUMP,
            )
        });
        port
    }

    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        post_with_auth(port, path, body, None)
    }

    fn post_with_auth(port: u16, path: &str, body: &str, bearer: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let auth_header = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (headers, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status_line = headers.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, resp_body.to_string())
    }

    fn get(port: u16, path: &str) -> (u16, String) {
        get_with_auth(port, path, None)
    }

    fn get_with_auth(port: u16, path: &str, bearer: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let auth_header = bearer
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth_header}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status_line = headers.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, body.to_string())
    }

    #[test]
    fn reconcile_two_cells_no_conflict() {
        let port = start_test_server();
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]},
            {"id": "cell-b", "writer": 2, "writes": [{"key": "y", "value": "2", "time": 1}]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", scenario);
        assert_eq!(status, 200);
        assert!(body.contains("\"cells_merged\":2"));
    }

    #[test]
    fn stats_report_conflicts_the_winning_writer_and_sync_age() {
        let port = start_test_server();
        let (_, before) = get(port, "/stats");
        assert!(before.contains("\"reconciles\":0"), "{before}");
        assert!(before.contains("\"lastReconcileAgeMs\":null"), "{before}");
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "from-a", "time": 5}]},
            {"id": "cell-b", "writer": 2, "writes": [{"key": "x", "value": "from-b", "time": 1}]}
        ]}"#;
        assert_eq!(post(port, "/reconcile", scenario).0, 200);
        let (_, after) = get(port, "/stats");
        let stats: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(stats["sync"]["reconciles"], 1);
        assert_eq!(stats["sync"]["conflictsResolved"], 1);
        assert_eq!(stats["sync"]["lastConflict"]["key"], "x");
        assert_eq!(stats["sync"]["lastConflict"]["winnerWriter"], 1);
        assert_eq!(stats["sync"]["lastConflict"]["loserWriter"], 2);
        assert!(stats["sync"]["lastReconcileAgeMs"].as_u64().is_some());
    }

    #[test]
    fn conflicts_route_lists_every_resolved_conflict_newest_first() {
        let port = start_test_server();
        let (status, empty) = get(port, "/conflicts");
        assert_eq!(status, 200);
        let empty: serde_json::Value = serde_json::from_str(&empty).unwrap();
        assert_eq!(empty["conflicts"].as_array().unwrap().len(), 0);
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "a", "time": 5}, {"key": "y", "value": "a", "time": 1}]},
            {"id": "cell-b", "writer": 2, "writes": [{"key": "x", "value": "b", "time": 1}, {"key": "y", "value": "b", "time": 9}]}
        ]}"#;
        assert_eq!(post(port, "/reconcile", scenario).0, 200);
        let (_, body) = get(port, "/conflicts");
        let history: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(history["conflictsResolved"], 2);
        assert_eq!(history["remembered"], 2);
        let list = history["conflicts"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        let keys: Vec<&str> = list.iter().map(|c| c["key"].as_str().unwrap()).collect();
        assert!(keys.contains(&"x") && keys.contains(&"y"), "{body}");
        for conflict in list {
            assert!(conflict["atMs"].as_u64().is_some());
            let (winner, loser) = match conflict["key"].as_str().unwrap() {
                "x" => (1, 2),
                _ => (2, 1),
            };
            assert_eq!(conflict["winnerWriter"], winner);
            assert_eq!(conflict["loserWriter"], loser);
        }
        let (_, limited) = get(port, "/conflicts?limit=1");
        let limited: serde_json::Value = serde_json::from_str(&limited).unwrap();
        assert_eq!(limited["conflicts"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn conflict_history_is_bounded_and_drops_the_oldest() {
        let mut evidence = SyncEvidence::default();
        for i in 0..(CONFLICT_HISTORY + 5) {
            let conflict = MergeConflict {
                key: format!("k{i}"),
                local_generation: 0,
                local_time: crate::lamport::LamportTime(2),
                local_writer: 1,
                local_value: Some("a".to_string()),
                remote_generation: 0,
                remote_time: crate::lamport::LamportTime(1),
                remote_writer: 2,
                remote_value: Some("b".to_string()),
                kept_remote: false,
            };
            evidence.conflicts_total += 1;
            evidence.record_conflicts(&[conflict], i as u64);
        }
        let body = evidence.conflicts_json(CONFLICT_HISTORY);
        assert_eq!(body["remembered"], CONFLICT_HISTORY);
        assert_eq!(body["conflictsResolved"], CONFLICT_HISTORY + 5);
        assert_eq!(
            body["conflicts"][0]["key"],
            format!("k{}", CONFLICT_HISTORY + 4)
        );
        assert_eq!(body["conflicts"][CONFLICT_HISTORY - 1]["key"], "k5");
    }

    #[test]
    fn conflicts_route_needs_the_shared_secret_when_one_is_set() {
        let port = start_test_server_full(None, Some("conflicts-secret-0123456789".to_string()));
        assert_eq!(get(port, "/conflicts").0, 401);
        assert_eq!(
            get_with_auth(port, "/conflicts", Some("conflicts-secret-0123456789")).0,
            200
        );
    }

    #[test]
    fn reconcile_resolves_a_real_conflict() {
        let port = start_test_server();
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "from-a", "time": 5}]},
            {"id": "cell-b", "writer": 2, "writes": [{"key": "x", "value": "from-b", "time": 1}]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", scenario);
        assert_eq!(status, 200);
        assert!(body.contains("\"conflicts_resolved\":1"));
        assert!(body.contains("from-a"));
    }

    // two writes for the SAME key at the identical (time, writer)
    // stamp but DIFFERENT values, both inside the SAME cell's own
    // `writes` list - before the fix, build_cell_map() used plain `set`
    // and silently kept whichever write happened to be listed first,
    // discarding the other with no trace and never surfacing it to
    // reconcile_with_prior's own cross-cell conflict detection at all.
    // This must now be refused as a real IdentityCollision (400), exactly
    // like the identical situation already is when it happens ACROSS two
    // cells (reconcile_resolves_a_real_conflict's own sibling case, a
    // genuine stamp ORDERING difference, is a normal resolvable conflict
    // - this is the "same stamp, still two different values" case that
    // has no principled winner at all).
    #[test]
    fn reconcile_rejects_a_same_stamp_collision_inside_one_cell() {
        let port = start_test_server();
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [
                {"key": "x", "value": "first", "time": 5},
                {"key": "x", "value": "second", "time": 5}
            ]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", scenario);
        assert_eq!(status, 400);
        assert!(body.contains("impossible conflict"));
        assert!(body.contains("first"));
        assert!(body.contains("second"));
    }

    // this project's own acceptance criterion: PERMUTING those two entries must
    // never produce a different, "conflict-free" result - both orderings
    // must be refused identically, since neither value is a principled
    // winner. Before the fix, this permutation would have silently kept
    // "second" instead of "first" (the plain LWW `set` insertion-order
    // tie-break) and reported 200, converged - the exact bug this closes.
    #[test]
    fn reconcile_rejects_the_same_collision_regardless_of_write_order() {
        let port = start_test_server();
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [
                {"key": "x", "value": "second", "time": 5},
                {"key": "x", "value": "first", "time": 5}
            ]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", scenario);
        assert_eq!(status, 400);
        assert!(body.contains("impossible conflict"));
    }

    // A true idempotent duplicate (identical stamp AND identical value,
    // e.g. a retried write landing twice in the same cell's own log) must
    // still merge cleanly - 's fix must not turn every same-stamp
    // repeat into a false-positive collision, only a genuinely competing
    // one with two different values.
    #[test]
    fn reconcile_still_accepts_a_true_duplicate_write_inside_one_cell() {
        let port = start_test_server();
        let scenario = r#"{"cells": [
            {"id": "cell-a", "writer": 1, "writes": [
                {"key": "x", "value": "same", "time": 5},
                {"key": "x", "value": "same", "time": 5}
            ]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", scenario);
        assert_eq!(status, 200);
        assert!(body.contains("\"conflicts_resolved\":0"));
        assert!(body.contains("same"));
    }

    #[test]
    fn reconcile_rejects_empty_cells() {
        let port = start_test_server();
        let (status, _) = post(port, "/reconcile", r#"{"cells": []}"#);
        assert_eq!(status, 400);
    }

    #[test]
    fn reconcile_rejects_malformed_json() {
        let port = start_test_server();
        let (status, _) = post(port, "/reconcile", "not json");
        assert_eq!(status, 400);
    }

    #[test]
    fn stats() {
        let port = start_test_server();
        let (status, body) = get(port, "/stats");
        assert_eq!(status, 200);
        assert!(body.contains("role"));
    }

    #[test]
    fn unknown_path_is_404() {
        let port = start_test_server();
        let (status, _) = get(port, "/nope");
        assert_eq!(status, 404);
    }

    #[test]
    fn stats_reports_whether_persistence_is_configured() {
        let memory_only_port = start_test_server();
        let (_, memory_only_body) = get(memory_only_port, "/stats");
        assert!(memory_only_body.contains("\"persistent\":false"));

        let dir =
            std::env::temp_dir().join(format!("swarm-sync-server-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("stats.json");
        let persistent_port = start_test_server_with_state(Some(state_path));
        let (_, persistent_body) = get(persistent_port, "/stats");
        assert!(persistent_body.contains("\"persistent\":true"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_state_reports_the_real_current_node_state() {
        let port = start_test_server();
        let (status, body) = get(port, "/state");
        assert_eq!(status, 200);
        assert_eq!(body, "{}");

        post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
        );
        let (_, body) = get(port, "/state");
        assert!(body.contains("\"x\":\"1\""));
    }

    const ROBOT_STATE: &str = r#"{"schema_version": "1.0", "robot_id": "arm-1", "state": "busy", "updated_at_utc": "2024-01-01T00:00:00Z"}"#;

    fn robot_state_scenario(key: &str, value: &str, time: u64) -> String {
        json!({"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": key, "value": value, "time": time}]}]}).to_string()
    }

    #[test]
    fn a_robot_state_is_replicated_as_a_typed_value_and_a_free_string_under_its_key_is_refused() {
        let port = start_test_server();
        let (status, _) = post(
            port,
            "/reconcile",
            &robot_state_scenario("robot_state/arm-1", ROBOT_STATE, 1),
        );
        assert_eq!(status, 200);
        let (_, body) = get(port, "/state");
        assert!(
            body.contains("robot_state/arm-1") && body.contains("busy"),
            "{body}"
        );

        for (key, value) in [
            ("robot_state/arm-1", "estop-cleared"),
            (
                "robot_state/arm-1",
                r#"{"schema_version": "1.0", "robot_id": "arm-1", "state": "sleeping", "updated_at_utc": "2024-01-01T00:00:00Z"}"#,
            ),
            ("robot_state/arm-2", ROBOT_STATE),
        ] {
            let (status, body) = post(port, "/reconcile", &robot_state_scenario(key, value, 2));
            assert_eq!(status, 422, "{key} = {value}: {body}");
            assert!(body.contains("robot state"), "{body}");
        }
        // other keys stay untyped, and nothing the refusals carried reached the map
        assert_eq!(
            post(
                port,
                "/reconcile",
                &robot_state_scenario("robot-1", "estop-cleared", 3)
            )
            .0,
            200
        );
        let (_, body) = get(port, "/state");
        assert!(
            !body.contains("sleeping") && body.contains("estop-cleared"),
            "{body}"
        );
    }

    #[test]
    fn a_robot_state_that_arrives_from_a_peer_is_checked_before_it_is_merged() {
        let port = start_test_server();
        let entry = |value: &str| {
            json!({"entries": [{"key": "robot_state/arm-1", "value": value, "time": 1, "writer": 2, "generation": 1}]}).to_string()
        };
        assert_eq!(post(port, "/merge", &entry("estop-cleared")).0, 422);
        assert_eq!(post(port, "/merge", &entry(ROBOT_STATE)).0, 200);
        let (_, body) = get(port, "/state");
        assert!(body.contains("robot_state/arm-1"), "{body}");
        // a tombstone has no value and is always accepted
        let tombstone = json!({"entries": [{"key": "robot_state/arm-1", "value": null, "time": 2, "writer": 2, "generation": 2}]}).to_string();
        assert_eq!(post(port, "/merge", &tombstone).0, 200);
    }

    // The real point of this whole pass: a SECOND /reconcile call must
    // remember what the first one converged to, not start from a blank
    // slate.
    #[test]
    fn a_second_reconcile_call_accumulates_onto_the_first_instead_of_forgetting_it() {
        let port = start_test_server();
        post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
        );
        post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-b", "writer": 2, "writes": [{"key": "y", "value": "2", "time": 1}]}]}"#,
        );

        let (_, body) = get(port, "/state");
        assert!(
            body.contains("\"x\":\"1\"") && body.contains("\"y\":\"2\""),
            "expected both x (from the first call) and y (from the second) to still be present, got: {body}"
        );
    }

    // A real earlier write must still win a real later conflict, exactly
    // as it would within one single /reconcile call - accumulating state
    // across calls must not weaken the CRDT's own real conflict rule.
    #[test]
    fn a_later_reconcile_call_still_loses_a_real_conflict_against_the_accumulated_state() {
        let port = start_test_server();
        post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "from-a-time-5", "time": 5}]}]}"#,
        );
        let (status, body) = post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-b", "writer": 2, "writes": [{"key": "x", "value": "from-b-time-1", "time": 1}]}]}"#,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"conflicts_resolved\":1"));

        let (_, state_body) = get(port, "/state");
        assert!(state_body.contains("\"x\":\"from-a-time-5\""));
    }

    // The real end-to-end proof this pass exists for: a genuinely
    // SEPARATE server instance, pointed at the same real state file on
    // disk, picks up right where the first one left off - a real restart
    // scenario, not the same in-memory Rust value reused across calls.
    #[test]
    fn a_real_restart_against_the_same_state_file_resumes_where_the_last_one_left_off() {
        let dir = std::env::temp_dir().join(format!(
            "swarm-sync-server-test-restart-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("state.json");

        let first_port = start_test_server_with_state(Some(state_path.clone()));
        let (status, _) = post(
            first_port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
        );
        assert_eq!(status, 200);

        // A genuinely new server, on a new port, loading the SAME real
        // file - as a real restarted process would.
        let second_port = start_test_server_with_state(Some(state_path));
        let (_, body) = get(second_port, "/state");
        assert!(
            body.contains("\"x\":\"1\""),
            "expected the second server to load the first one's own persisted state, got: {body}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Real gap this pass closes: before this, ANY caller on the network
    // could inject fabricated cell state into a node's own CRDT with no
    // way at all to tell who sent it. These lock in the opt-in behavior:
    // unset SWARM_SYNC_SHARED_SECRET keeps every prior test above passing
    // unchanged; set, both /reconcile and /state require a matching
    // Authorization: Bearer <token>.
    #[test]
    fn reconcile_is_open_when_no_shared_secret_is_configured() {
        let port = start_test_server();
        let (status, _) = post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn reconcile_rejects_a_missing_bearer_token_when_a_shared_secret_is_configured() {
        let port = start_test_server_full(None, Some("s3cr3t".to_string()));
        let (status, _) = post(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
        );
        assert_eq!(status, 401);
    }

    #[test]
    fn reconcile_rejects_a_wrong_bearer_token() {
        let port = start_test_server_full(None, Some("s3cr3t".to_string()));
        let (status, _) = post_with_auth(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
            Some("wrong-secret"),
        );
        assert_eq!(status, 401);
    }

    #[test]
    fn reconcile_accepts_the_correct_bearer_token() {
        let port = start_test_server_full(None, Some("s3cr3t".to_string()));
        let (status, body) = post_with_auth(
            port,
            "/reconcile",
            r#"{"cells": [{"id": "cell-a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#,
            Some("s3cr3t"),
        );
        assert_eq!(status, 200, "{body}");
    }

    #[test]
    fn state_is_also_gated_by_the_same_shared_secret() {
        let port = start_test_server_full(None, Some("s3cr3t".to_string()));
        let (unauthed_status, _) = get(port, "/state");
        assert_eq!(unauthed_status, 401);
        let (authed_status, _) = get_with_auth(port, "/state", Some("s3cr3t"));
        assert_eq!(authed_status, 200);
    }

    #[test]
    fn stats_stays_open_regardless_of_shared_secret_configuration() {
        let port = start_test_server_full(None, Some("s3cr3t".to_string()));
        let (status, _) = get(port, "/stats");
        assert_eq!(status, 200);
    }

    #[test]
    fn constant_time_eq_matches_equal_and_rejects_unequal_or_mismatched_length() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn loopback_addresses_are_recognised_and_nothing_else_is() {
        for addr in [
            "127.0.0.1",
            "localhost",
            "::1",
            "[::1]",
            "127.0.0.1:8112",
            "localhost:8112",
            "[::1]:8112",
        ] {
            assert!(is_loopback_addr(addr), "{addr} is loopback");
        }
        for addr in [
            "0.0.0.0",
            "192.168.0.180",
            "::",
            "example.com",
            "",
            "0.0.0.0:8112",
            "[::]:8112",
            "example.com:8112",
        ] {
            assert!(!is_loopback_addr(addr), "{addr} is not loopback");
        }
    }

    #[test]
    fn a_network_address_needs_the_shared_secret_or_an_explicit_opt_out() {
        assert!(startup_problem("127.0.0.1:8112", None, false).is_none());
        assert!(startup_problem("0.0.0.0:8112", None, false).is_some());
        assert!(startup_problem("0.0.0.0:8112", Some(""), false).is_some());
        assert!(startup_problem("0.0.0.0:8112", Some("s3cret"), false).is_none());
        assert!(startup_problem("0.0.0.0:8112", None, true).is_none());
    }

    #[test]
    fn a_body_over_the_limit_is_413_and_the_node_keeps_serving() {
        let port = start_test_server();
        let big = format!(
            "{{\"cells\":[],\"pad\":\"{}\"}}",
            "x".repeat(MAX_BODY_BYTES as usize)
        );
        let (status, body) = post(port, "/reconcile", &big);
        assert_eq!(status, 413, "{body}");
        let (status, _) = get(port, "/stats");
        assert_eq!(status, 200);
    }

    #[test]
    fn a_write_far_ahead_of_the_state_is_refused_so_no_key_can_be_captured_for_good() {
        let port = start_test_server();
        let capture = r#"{"cells": [
            {"id": "evil", "writer": 18446744073709551615,
             "writes": [{"key": "robot-1", "value": "estop-cleared", "time": 18446744073709551613}]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", capture);
        assert_eq!(status, 422, "{body}");
        assert!(body.contains("could never be overwritten"), "{body}");
        let (_, state) = get(port, "/state");
        assert_eq!(state, "{}", "nothing of the refused write may be kept");

        // honest steps still work, and a value written one step ahead can be corrected
        let step = |value: &str, time: u64| {
            format!(
                r#"{{"cells": [{{"id": "a", "writer": 1, "writes": [{{"key": "robot-1", "value": "{value}", "time": {time}}}]}}]}}"#
            )
        };
        assert_eq!(post(port, "/reconcile", &step("ESTOP", 5)).0, 200);
        assert_eq!(post(port, "/reconcile", &step("wrong", 900_000)).0, 200);
        assert_eq!(post(port, "/reconcile", &step("ESTOP", 1_800_000)).0, 200);
        let (_, state) = get(port, "/state");
        assert!(state.contains("ESTOP"), "{state}");
    }

    #[test]
    fn errors_are_mapped_to_the_status_that_says_what_went_wrong() {
        let port = start_test_server();
        assert_eq!(post(port, "/reconcile", r#"{"cells": []}"#).0, 400);
        let collision = r#"{"cells": [
            {"id": "a", "writer": 1, "writes": [
                {"key": "k", "value": "one", "time": 3},
                {"key": "k", "value": "two", "time": 3}]}
        ]}"#;
        let (status, body) = post(port, "/reconcile", collision);
        assert_eq!(status, 400, "{body}");
        assert!(
            body.contains("\"k\"") || body.contains("k"),
            "the answer must say which write collided: {body}"
        );
    }

    #[test]
    fn health_answers_without_a_token() {
        let port = start_test_server_full(None, Some("s3cret".to_string()));
        let (status, body) = get(port, "/health");
        assert_eq!(status, 200);
        assert!(body.contains("HYDRA-UMC-SWARM-SYNC"));
    }

    #[test]
    fn a_slow_upload_does_not_stop_the_other_requests() {
        let port = start_test_server();
        let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
        slow.write_all(b"POST /reconcile HTTP/1.1\r\nHost: x\r\nContent-Length: 500000\r\n\r\n{")
            .unwrap();
        thread::sleep(std::time::Duration::from_millis(200));
        let started = std::time::Instant::now();
        assert_eq!(get(port, "/stats").0, 200);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        drop(slow);
    }

    #[test]
    fn a_body_that_announces_more_than_the_limit_is_refused_without_reading_it() {
        let port = start_test_server();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        stream
            .write_all(
                b"POST /reconcile HTTP/1.1\r\nHost: x\r\nContent-Length: 90000000\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        // the node answers at once but keeps the connection open while it
        // waits for the body it refused: one read is enough
        let mut buf = [0u8; 512];
        let n = stream.read(&mut buf).unwrap_or(0);
        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(raw.starts_with("HTTP/1.1 413"), "{raw}");
    }

    #[test]
    fn a_state_file_in_a_directory_that_does_not_exist_yet_is_created() {
        let dir = std::env::temp_dir().join(format!("swarm-sync-newdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("nested").join("state.json");
        let port = start_test_server_with_state(Some(file.clone()));
        let scenario = r#"{"cells": [{"id": "a", "writer": 1, "writes": [{"key": "x", "value": "1", "time": 1}]}]}"#;
        assert_eq!(post(port, "/reconcile", scenario).0, 200);
        assert!(file.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- the exchange of full entries and the gossip client ----

    fn start_node(shared_secret: Option<String>, extra: Extra) -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("a real IP socket")
            .port();
        thread::spawn(move || {
            run_with(
                server,
                LwwMap::new(),
                None,
                shared_secret,
                crate::reconcile::DEFAULT_MAX_TIME_JUMP,
                extra,
            )
        });
        port
    }

    fn full_entries(port: u16) -> serde_json::Value {
        let (status, body) = get(port, "/state?full=1");
        assert_eq!(status, 200, "{body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["entries"].clone()
    }

    #[test]
    fn full_state_carries_stamps_and_tombstones_and_plain_state_still_does_not() {
        let port = start_test_server();
        let scenario = r#"{"cells":[{"id":"a","writer":7,"writes":[
            {"key":"kept","value":"v","time":3},
            {"key":"gone","value":"x","time":4},
            {"key":"gone","deleted":true,"time":5}]}]}"#;
        assert_eq!(post(port, "/reconcile", scenario).0, 200);
        let entries = full_entries(port);
        let by_key = |k: &str| {
            entries
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["key"] == k)
                .unwrap()
                .clone()
        };
        assert_eq!(by_key("kept")["value"], "v");
        assert_eq!(by_key("kept")["time"], 3);
        assert_eq!(by_key("kept")["writer"], 7);
        assert_eq!(by_key("kept")["generation"], 1);
        assert!(
            by_key("gone")["value"].is_null(),
            "a deleted key is a tombstone, not an absent key"
        );
        assert_eq!(by_key("gone")["generation"], 2);
        let (_, plain) = get(port, "/state");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&plain).unwrap(),
            json!({"kept": "v"})
        );
    }

    #[test]
    fn a_write_can_delete_a_key_and_a_later_stale_add_does_not_bring_it_back() {
        let port = start_test_server();
        let first = r#"{"cells":[{"id":"a","writer":1,"writes":[{"key":"k","value":"v","time":2},{"key":"k","deleted":true,"time":6}]}]}"#;
        assert_eq!(post(port, "/reconcile", first).0, 200);
        let (_, state) = get(port, "/state");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap(),
            json!({})
        );
        let stale =
            r#"{"cells":[{"id":"b","writer":2,"writes":[{"key":"k","value":"old","time":4}]}]}"#;
        assert_eq!(post(port, "/reconcile", stale).0, 200);
        let (_, state) = get(port, "/state");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap(),
            json!({})
        );
    }

    #[test]
    fn post_merge_takes_stamped_entries_and_refuses_what_a_peer_must_not_send() {
        let port = start_test_server();
        let ok = r#"{"entries":[{"key":"k","value":"v","time":5,"writer":2,"generation":1},{"key":"t","value":null,"time":6,"writer":2,"generation":2}]}"#;
        let (status, body) = post(port, "/merge", ok);
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["entries_received"],
            2
        );
        let (_, state) = get(port, "/state");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap(),
            json!({"k": "v"})
        );
        // the same entries again are idempotent
        let (status, body) = post(port, "/merge", ok);
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["conflicts_resolved"],
            0
        );
        // a time or a generation too far ahead, an empty key, generation 0 and an empty list are refused
        for bad in [
            r#"{"entries":[{"key":"k","value":"v","time":99999999,"writer":2,"generation":1}]}"#,
            r#"{"entries":[{"key":"k","value":"v","time":7,"writer":2,"generation":5000}]}"#,
            r#"{"entries":[{"key":"","value":"v","time":7,"writer":2,"generation":1}]}"#,
            r#"{"entries":[{"key":"k","value":"v","time":7,"writer":2,"generation":0}]}"#,
            r#"{"entries":[]}"#,
        ] {
            let (status, body) = post(port, "/merge", bad);
            assert!(status == 422 || status == 400, "{bad} -> {status} {body}");
        }
        assert_eq!(post(port, "/merge", "not json").0, 400);
        // the same stamp claiming another value is the impossible collision
        let clash =
            r#"{"entries":[{"key":"k","value":"other","time":5,"writer":2,"generation":1}]}"#;
        assert_eq!(post(port, "/merge", clash).0, 400);
    }

    #[test]
    fn merge_and_full_state_need_the_shared_secret_when_there_is_one() {
        let secret = "0123456789abcdef".to_string();
        let port = start_node(Some(secret.clone()), Extra::default());
        assert_eq!(get(port, "/state?full=1").0, 401);
        assert_eq!(post(port, "/merge", r#"{"entries":[]}"#).0, 401);
        assert_eq!(get_with_auth(port, "/state?full=1", Some(&secret)).0, 200);
    }

    /// Waits for an observable effect; the limit is only the point where a hang is called a failure, so it is generous for a loaded machine.
    fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        for _ in 0..600 {
            if condition() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    fn gossip_to(peers: &[u16], push: bool) -> Extra {
        Extra {
            max_generation_jump: DEFAULT_MAX_GENERATION_JUMP,
            gossip: Some(GossipConfig {
                peers: peers
                    .iter()
                    .map(|p| gossip::parse_peer(&format!("http://127.0.0.1:{p}")).unwrap())
                    .collect(),
                interval: std::time::Duration::from_millis(50),
                push,
            }),
        }
    }

    #[test]
    fn a_node_that_asks_a_peer_gets_its_writes_and_its_deletions() {
        let secret = "0123456789abcdef".to_string();
        let a = start_node(Some(secret.clone()), Extra::default());
        let scenario = r#"{"cells":[{"id":"a","writer":1,"writes":[
            {"key":"one","value":"1","time":2},
            {"key":"two","value":"2","time":3},
            {"key":"two","deleted":true,"time":4}]}]}"#;
        assert_eq!(
            post_with_auth(a, "/reconcile", scenario, Some(&secret)).0,
            200
        );
        let b = start_node(Some(secret.clone()), gossip_to(&[a], false));
        wait_for("B to hold what A wrote", || {
            let (status, body) = get_with_auth(b, "/state", Some(&secret));
            status == 200
                && serde_json::from_str::<serde_json::Value>(&body).unwrap() == json!({"one": "1"})
        });
        // the tombstone came over too: a stale add of "two" to B does not bring the key back
        let stale =
            r#"{"cells":[{"id":"b","writer":2,"writes":[{"key":"two","value":"old","time":3}]}]}"#;
        assert_eq!(post_with_auth(b, "/reconcile", stale, Some(&secret)).0, 200);
        let (_, body) = get_with_auth(b, "/state", Some(&secret));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            json!({"one": "1"})
        );
        let (_, stats) = get(b, "/stats");
        let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
        assert!(
            stats["peers"][0]["entriesPulled"].as_u64().unwrap() >= 2,
            "{stats}"
        );
        assert!(stats["peers"][0]["lastError"].is_null());
        assert!(stats["sync"]["merges"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn two_nodes_that_ask_each_other_converge_and_push_sends_ours_too() {
        let a = start_node(None, Extra::default());
        let b = start_node(None, gossip_to(&[a], true));
        assert_eq!(post(a, "/reconcile", r#"{"cells":[{"id":"a","writer":1,"writes":[{"key":"from-a","value":"1","time":2}]}]}"#).0, 200);
        assert_eq!(post(b, "/reconcile", r#"{"cells":[{"id":"b","writer":2,"writes":[{"key":"from-b","value":"2","time":3}]}]}"#).0, 200);
        // B pulls A's state and pushes its own, so A ends up with both without asking
        wait_for("A and B to hold both keys", || {
            let state =
                |p: u16| serde_json::from_str::<serde_json::Value>(&get(p, "/state").1).unwrap();
            state(a) == json!({"from-a": "1", "from-b": "2"})
                && state(b) == json!({"from-a": "1", "from-b": "2"})
        });
        assert_eq!(full_entries(a), full_entries(b));
    }

    #[test]
    fn a_peer_that_is_down_or_wrong_is_recorded_and_does_not_stop_the_node() {
        // A port that was only bound and released can be handed to another test's node in the same run, which would then answer as a
        // healthy peer. So the port stays held for the whole test by a listener that accepts and hangs up: a peer that is "wrong"
        // in a way that fails at once and the same way every time.
        let wrong = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = wrong.local_addr().unwrap().port();
        // it lives until the test process ends, which is what keeps the port taken
        std::thread::spawn(move || {
            for stream in wrong.incoming().flatten() {
                drop(stream);
            }
        });
        let node = start_node(None, gossip_to(&[dead], false));
        wait_for("the failure to be recorded", || {
            let (_, stats) = get(node, "/stats");
            let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
            stats["peers"][0]["lastError"].is_string()
        });
        assert_eq!(get(node, "/health").0, 200);
        assert_eq!(
            post(
                node,
                "/reconcile",
                r#"{"cells":[{"id":"a","writer":1,"writes":[{"key":"k","value":"v","time":1}]}]}"#
            )
            .0,
            200
        );
    }

    #[test]
    fn a_mission_is_written_with_its_attempt_plus_one_as_generation_and_the_newer_attempt_wins() {
        let node = start_node(None, Extra::default());
        let rec = |owner: &str, attempt: u64| {
            format!(
                r#"{{"mission_id":"m-1","status":"running","owner":"{owner}","attempt":{attempt}}}"#
            )
        };
        let body = |owner: &str, attempt: u64, generation: &str, time: u64| {
            let value = serde_json::to_string(&rec(owner, attempt)).unwrap();
            format!(
                r#"{{"cells":[{{"id":"c","writer":1,"writes":[{{"key":"mission/m-1","value":{value},"time":{time}{generation}}}]}}]}}"#
            )
        };
        // no generation, or the wrong one, is a 422
        assert_eq!(post(node, "/reconcile", &body("orch-a", 0, "", 5)).0, 422);
        assert_eq!(
            post(
                node,
                "/reconcile",
                &body("orch-a", 0, r#","generation":3"#, 5)
            )
            .0,
            422
        );
        // generation on an ordinary key is refused too
        let plain = r#"{"cells":[{"id":"c","writer":1,"writes":[{"key":"k","value":"v","time":5,"generation":1}]}]}"#;
        assert_eq!(post(node, "/reconcile", plain).0, 422);
        // attempt 0 late in time, then attempt 1 early in time: attempt 1 is what the node keeps
        assert_eq!(
            post(
                node,
                "/reconcile",
                &body("orch-a", 0, r#","generation":1"#, 900)
            )
            .0,
            200
        );
        assert_eq!(
            post(
                node,
                "/reconcile",
                &body("orch-b", 1, r#","generation":2"#, 20)
            )
            .0,
            200
        );
        let (_, state) = get(node, "/state");
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        let kept = state["mission/m-1"].as_str().unwrap();
        assert!(kept.contains("orch-b"), "{kept}");
    }
}
