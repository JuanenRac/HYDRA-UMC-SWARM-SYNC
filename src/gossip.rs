// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/gossip.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! The anti-entropy client: a node asks each of its peers for its full state every so often (`GET /state?full=1`) and merges what it gets,
//! and, when asked to, sends its own state to them (`POST /merge`). Two nodes that ask each other converge whatever either wrote and
//! deleted while they could not talk.
//!
//! The transport is plain HTTP/1.1 over a TCP stream with a timeout on every step and a cap on what is read: the peers are nodes of the
//! same cell on the same network, and the shared secret (`SWARM_SYNC_SHARED_SECRET`, the one the server checks) is sent as a bearer token.
//! TLS is not spoken here: a peer behind TLS is reached through the tunnel the installation already uses for it. Nothing in this module
//! touches the map; `server.rs` applies what comes back through the same checks as a request from outside.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::entries::StampedEntry;

/// The largest answer read from a peer.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub host_port: String,
    /// The path in front of the routes (empty for a node served at the root).
    pub prefix: String,
}

/// `http://host:port` or `http://host:port/prefix`. Anything else (https, a missing port) is refused.
pub fn parse_peer(text: &str) -> Result<Peer, String> {
    let rest = text.strip_prefix("http://").ok_or_else(|| {
        format!(
            "{text:?} is not an http:// address (a peer behind TLS is reached through a tunnel)"
        )
    })?;
    let (host_port, prefix) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
        None => (rest, ""),
    };
    if host_port.is_empty()
        || !host_port.contains(':')
        || host_port.ends_with(':')
        || host_port.chars().any(|c| c.is_whitespace() || c == '@')
    {
        return Err(format!(
            "{text:?} needs a host and a port, like http://10.0.0.7:8112"
        ));
    }
    Ok(Peer {
        host_port: host_port.to_string(),
        prefix: prefix.to_string(),
    })
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

fn decode_chunked(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("a chunk without a size line")?;
        let size_text =
            std::str::from_utf8(&rest[..end]).map_err(|_| "a chunk size that is not text")?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "a chunk size that is not a number")?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("a chunk cut short".to_string());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

/// Parses the bytes of an HTTP/1.1 response that was read to the end of the connection.
pub fn parse_response(raw: &[u8]) -> Result<Response, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("the answer has no end of headers")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "the headers are not text")?;
    let mut lines = head.lines();
    let status_line = lines.next().ok_or("the answer is empty")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("the status line is not HTTP")?;
    let mut length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let body_raw = &raw[split + 4..];
    let body = if chunked {
        decode_chunked(body_raw)?
    } else if let Some(n) = length {
        if body_raw.len() < n {
            return Err("the body is shorter than it says".to_string());
        }
        body_raw[..n].to_vec()
    } else {
        body_raw.to_vec()
    };
    Ok(Response { status, body })
}

fn exchange(
    peer: &Peer,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> Result<Response, String> {
    let address = peer
        .host_port
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", peer.host_port))?
        .next()
        .ok_or_else(|| format!("{} has no address", peer.host_port))?;
    let mut stream = TcpStream::connect_timeout(&address, STEP_TIMEOUT)
        .map_err(|e| format!("cannot connect to {}: {e}", peer.host_port))?;
    stream.set_read_timeout(Some(STEP_TIMEOUT)).ok();
    stream.set_write_timeout(Some(STEP_TIMEOUT)).ok();
    let mut request = format!("{method} {}{path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n", peer.prefix, peer.host_port);
    if let Some(t) = token {
        request.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot send to {}: {e}", peer.host_port))?;
    if let Some(b) = body {
        stream
            .write_all(b)
            .map_err(|e| format!("cannot send to {}: {e}", peer.host_port))?;
    }
    let mut raw = Vec::new();
    Read::take(&mut stream, MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot read from {}: {e}", peer.host_port))?;
    if raw.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "{} answered more than {MAX_RESPONSE_BYTES} bytes",
            peer.host_port
        ));
    }
    parse_response(&raw)
}

#[derive(serde::Deserialize)]
struct FullState {
    entries: Vec<StampedEntry>,
}

/// The full state of a peer.
pub fn pull(peer: &Peer, token: Option<&str>) -> Result<Vec<StampedEntry>, String> {
    let response = exchange(peer, "GET", "/state?full=1", token, None)?;
    if response.status != 200 {
        return Err(format!(
            "{} answered HTTP {}",
            peer.host_port, response.status
        ));
    }
    let state: FullState = serde_json::from_slice(&response.body)
        .map_err(|e| format!("{} sent a state that is not readable: {e}", peer.host_port))?;
    Ok(state.entries)
}

/// Sends entries to a peer's `POST /merge`.
pub fn push(peer: &Peer, token: Option<&str>, entries: &[StampedEntry]) -> Result<(), String> {
    let body = serde_json::to_vec(&serde_json::json!({ "entries": entries }))
        .map_err(|e| e.to_string())?;
    let response = exchange(peer, "POST", "/merge", token, Some(&body))?;
    if response.status != 200 {
        return Err(format!(
            "{} refused the merge (HTTP {})",
            peer.host_port, response.status
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_are_parsed_and_refused() {
        assert_eq!(
            parse_peer("http://10.0.0.7:8112").unwrap(),
            Peer {
                host_port: "10.0.0.7:8112".into(),
                prefix: "".into()
            }
        );
        assert_eq!(
            parse_peer("http://node:80/swarm/").unwrap().prefix,
            "/swarm"
        );
        for bad in [
            "https://x:1",
            "10.0.0.7:8112",
            "http://",
            "http://host",
            "http://host:",
            "http://u@host:1",
            "ftp://x:1",
        ] {
            assert!(parse_peer(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_response_with_a_length_a_chunked_body_or_none_is_read() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello-extra").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
        let r = parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap();
        assert_eq!(r.body, b"abcde");
        let r = parse_response(b"HTTP/1.1 404 Not Found\r\n\r\nnope").unwrap();
        assert_eq!((r.status, r.body.as_slice()), (404, &b"nope"[..]));
    }

    #[test]
    fn a_broken_response_is_an_error_not_a_panic() {
        for bad in [
            &b""[..],
            b"garbage",
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
        ] {
            assert!(
                parse_response(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
