// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/server.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Plain JSON/HTTP surface (`tiny_http`, blocking, no async runtime) -
//! same convention as `HYDRA-UMC-TWIN`'s own `server.rs`. POST /reconcile
//! reaches the exact same `reconcile()` the CLI's own bare invocation
//! already runs against a local scenario file - the scenario travels
//! directly in the JSON request body instead, since a server-side file
//! path only ever made sense for a CLI running on the same machine as
//! the file.

use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::reconcile::{reconcile, Scenario};

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
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

pub fn run(server: Server) {
    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_string();

        if path == "/stats" && request.method() == &Method::Get {
            write_json(
                request,
                200,
                &json!({"role": "CRDT swarm state reconciliation"}),
            );
            continue;
        }
        if path != "/reconcile" || request.method() != &Method::Post {
            write_json(request, 404, &json!({"error": "not found"}));
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

        match reconcile(&scenario) {
            Ok(output) => write_json(request, 200, &serde_json::to_value(&output).unwrap()),
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
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("tiny_http always binds a real IP socket for an http:// server")
            .port();
        thread::spawn(move || run(server));
        port
    }

    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
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
}
