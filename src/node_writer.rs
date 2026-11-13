// =============================================================================
// HYDRA-UMC-SWARM-SYNC - src/node_writer.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The writer of a node, fixed by its credential.
//!
//! The version stamp of every entry names the node that wrote it (`writer`), and a write that is ahead in time wins. When the node that writes presents a
//! token that HYDRA-UMC-SERVER signed (`--identity-key-file`), the writer is not whatever the client says: it is the number made from the user name in the
//! token, `writer_for(username)`, and a `/reconcile` whose cells name another writer is refused. A node can therefore not write as another one, and a
//! stolen shared secret alone no longer buys the writer of someone else.
//!
//! `reseal` is the migration for the data written before: it rewrites the writer of the entries of a state file from an old number to the one of a user
//! name, into a new file (the old one is never touched).

use serde_json::Value;

const WRITER_DOMAIN: &str = "hydra-umc-swarm-sync-writer-v1:";

/// The writer number of a user name: the first eight bytes of SHA-256 over a fixed prefix and the name.
pub fn writer_for(username: &str) -> u64 {
    let digest = crate::hmac::sha256(format!("{WRITER_DOMAIN}{username}").as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// Rewrites, in the text of a state file, every entry whose writer is `from` to the writer of `username`, for each pair of `map`. Returns the new text and how
/// many entries changed. A state that is not the shape of the store, or a map that names a user twice or an old number twice, is an error.
pub fn reseal(text: &str, map: &[(u64, String)]) -> Result<(String, usize), String> {
    for (i, (from, name)) in map.iter().enumerate() {
        if name.is_empty() {
            return Err("a user name must not be empty".into());
        }
        if map[..i].iter().any(|(other, _)| other == from) {
            return Err(format!("the writer {from} is mapped twice"));
        }
    }
    let mut state: Value =
        serde_json::from_str(text).map_err(|e| format!("the state is not valid JSON: {e}"))?;
    let entries = state
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .ok_or("the state has no `entries` list")?;
    let mut changed = 0usize;
    for entry in entries.iter_mut() {
        let Some(writer) = entry.get("writer").and_then(Value::as_u64) else {
            return Err("an entry has no numeric writer".into());
        };
        if let Some((_, name)) = map.iter().find(|(from, _)| *from == writer) {
            entry["writer"] = Value::from(writer_for(name));
            changed += 1;
        }
    }
    let out = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    Ok((out, changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_writer_of_a_name_is_stable_and_distinct() {
        assert_eq!(writer_for("node-a"), writer_for("node-a"));
        assert_ne!(writer_for("node-a"), writer_for("node-b"));
        assert_ne!(writer_for("node-a"), writer_for("Node-a"));
    }

    #[test]
    fn reseal_changes_only_the_mapped_writers() {
        let text = r#"{"entries":[
            {"key":"a","value":"1","time":1,"writer":7,"generation":1},
            {"key":"b","value":null,"time":2,"writer":8,"generation":1},
            {"key":"c","value":"3","time":3,"writer":7,"generation":2}]}"#;
        let (out, changed) = reseal(text, &[(7, "node-a".to_string())]).unwrap();
        assert_eq!(changed, 2);
        let state: Value = serde_json::from_str(&out).unwrap();
        let writers: Vec<u64> = state["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["writer"].as_u64().unwrap())
            .collect();
        assert_eq!(writers, vec![writer_for("node-a"), 8, writer_for("node-a")]);
        assert_eq!(state["entries"][1]["value"], Value::Null);
        assert_eq!(state["entries"][2]["generation"], 2);
    }

    #[test]
    fn reseal_refuses_what_it_cannot_read() {
        assert!(reseal("not json", &[]).is_err());
        assert!(reseal("{}", &[]).unwrap_err().contains("entries"));
        assert!(reseal(r#"{"entries":[{"key":"a"}]}"#, &[])
            .unwrap_err()
            .contains("writer"));
        assert!(
            reseal(r#"{"entries":[]}"#, &[(1, "a".into()), (1, "b".into())])
                .unwrap_err()
                .contains("twice")
        );
        assert!(reseal(r#"{"entries":[]}"#, &[(1, String::new())]).is_err());
    }
}
