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

use std::path::PathBuf;

use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::crdt::LwwMap;
use crate::reconcile::{reconcile_with_prior, Scenario};
use crate::store;

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
pub fn run(server: Server, state_path: Option<PathBuf>, shared_secret: Option<String>) {
    let mut state: LwwMap<String, String> = match &state_path {
        Some(path) => match store::load(path) {
            Ok(map) => map,
            Err(e) => {
                eprintln!("[swarm-sync] fatal: could not load state from {path:?}: {e}");
                return;
            }
        },
        None => LwwMap::new(),
    };

    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_string();

        if path == "/stats" && request.method() == &Method::Get {
            write_json(
                request,
                200,
                &json!({
                    "role": "CRDT swarm state reconciliation",
                    "persistent": state_path.is_some(),
                }),
            );
            continue;
        }
        if path == "/state" && request.method() == &Method::Get {
            if !is_authorized(&request, &shared_secret) {
                write_json(
                    request,
                    401,
                    &json!({"error": "missing or invalid Authorization: Bearer <token>"}),
                );
                continue;
            }
            write_json(request, 200, &json!(state.snapshot()));
            continue;
        }
        if path != "/reconcile" || request.method() != &Method::Post {
            write_json(request, 404, &json!({"error": "not found"}));
            continue;
        }
        if !is_authorized(&request, &shared_secret) {
            write_json(
                request,
                401,
                &json!({"error": "missing or invalid Authorization: Bearer <token>"}),
            );
            continue;
        }

        let mut raw = String::new();
        if let Err(e) = request.as_reader().read_to_string(&mut raw) {
            write_json(
                request,
                400,
                &json!({"error": format!("could not read request body: {e}")}),
            );
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

        match reconcile_with_prior(&scenario, state.clone()) {
            Ok((output, merged)) => {
                // Persist BEFORE responding success - a caller told "200
                // OK, merged" must be able to trust this node's own next
                // /reconcile (or a restart) actually remembers it. A
                // real save failure is a real 500, not a silent "it
                // worked" that quietly didn't.
                if let Some(path) = &state_path {
                    if let Err(e) = store::save(path, &merged) {
                        write_json(
                            request,
                            500,
                            &json!({"error": format!("merged successfully but could not persist state to {path:?}: {e}")}),
                        );
                        continue;
                    }
                }
                state = merged;
                write_json(request, 200, &serde_json::to_value(&output).unwrap());
            }
            Err(e) => write_json(request, 400, &json!({"error": e.to_string()})),
        }
    }
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
        thread::spawn(move || run(server, state_path, shared_secret));
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
        assert!(body.contains("\"converged\":true"));
        assert!(body.contains("\"cells_merged\":2"));
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
        assert_eq!(status, 200);
        assert!(body.contains("\"converged\":true"));
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
}
