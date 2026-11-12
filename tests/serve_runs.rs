// HYDRA-UMC-SWARM-SYNC - tests/serve_runs.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// The binary when it DOES start: it listens, answers its health and its stats over a real socket, asks for the shared
// secret when one is set, and stops when it is stopped. The port is a free one the test asks the system for just before
// the start; the test waits for the listener (never a fixed sleep) and always stops the child.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("its address").port()
}

struct Node {
    child: Child,
    port: u16,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(secret: Option<&str>) -> Node {
    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hydra-umc-swarm-sync"));
    command
        .args(["serve", "--addr", "127.0.0.1", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(secret) = secret {
        command.env("SWARM_SYNC_SHARED_SECRET", secret);
    } else {
        command.env_remove("SWARM_SYNC_SHARED_SECRET");
    }
    let child = command.spawn().expect("the binary must run");
    let node = Node { child, port };
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return node;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the node never listened on port {port}");
}

/// One HTTP/1.1 request over a plain socket; returns the status line and the whole answer.
fn get(port: u16, path: &str, authorization: Option<&str>) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(value) = authorization {
        request.push_str(&format!("Authorization: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    let status = answer.lines().next().unwrap_or("").to_string();
    (status, answer)
}

#[test]
fn a_node_that_starts_answers_its_health() {
    let node = start(None);
    let (status, answer) = get(node.port, "/health", None);
    assert!(status.contains("200"), "{answer}");
}

#[test]
fn a_node_that_starts_answers_its_stats_as_json() {
    let node = start(None);
    let (status, answer) = get(node.port, "/stats", None);
    assert!(status.contains("200"), "{answer}");
    let body = answer.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(body.trim_start().starts_with('{'), "{answer}");
}

#[test]
fn a_route_that_does_not_exist_is_a_404_and_the_node_keeps_answering() {
    let node = start(None);
    let (status, _) = get(node.port, "/no/such/route", None);
    assert!(status.contains("404"), "{status}");
    let (status, answer) = get(node.port, "/health", None);
    assert!(status.contains("200"), "{answer}");
}

#[test]
fn with_a_shared_secret_the_state_needs_the_token_and_the_health_stays_open() {
    let node = start(Some("0123456789abcdef-shared-secret"));
    let (status, _) = get(node.port, "/state", None);
    assert!(status.contains("401"), "{status}");
    let (status, _) = get(node.port, "/state", Some("Bearer wrong-token"));
    assert!(status.contains("401"), "{status}");
    let (status, answer) = get(
        node.port,
        "/state",
        Some("Bearer 0123456789abcdef-shared-secret"),
    );
    assert!(status.contains("200"), "{answer}");
    let (status, answer) = get(node.port, "/health", None);
    assert!(status.contains("200"), "{answer}");
}

#[test]
fn a_node_that_is_stopped_stops_listening() {
    let mut node = start(None);
    let port = node.port;
    node.child.kill().expect("kill");
    node.child.wait().expect("wait");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut closed = false;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            closed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        closed,
        "the port {port} was still open after the node stopped"
    );
}
