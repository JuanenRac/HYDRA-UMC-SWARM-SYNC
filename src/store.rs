// HYDRA-UMC-SWARM-SYNC - src/store.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// Found in an ecosystem-wide software-improvements audit: POST
// /reconcile was fully stateless - every call merged only the scenario
// in that one request body and discarded the result, so a real running
// server had no memory of a previous /reconcile call, and a restart lost
// nothing because there was never anything to lose. This is the real
// per-node persistence that closes that gap: one real JSON file on disk
// holds this node's own current LwwMap<String, String>, survives a
// restart, and is what server.rs merges every new /reconcile scenario
// into (see reconcile::reconcile_with_prior) instead of starting from a
// blank slate every single call.
//
// Deliberately a plain JSON file, not a database - this project's own
// real unit of state (one node's own CRDT map) is small, and every
// sibling repo in this ecosystem that only needs simple durability
// (HYDRA-UMC-TWIN's own workspace state, for one) already uses the same
// real crash-safe write pattern this module reuses.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crdt::LwwMap;
use crate::lamport::LamportTime;

/// The real, on-disk shape of one persisted entry - key/value plus the
/// exact stamp (time, writer) `crdt::LwwMap::entries_with_stamps` reports,
/// so reloading it re-establishes the SAME conflict-resolution state a
/// live map already converged to, not a fresh one.
#[derive(Serialize, Deserialize)]
struct PersistedEntry {
    key: String,
    value: String,
    time: u64,
    writer: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    entries: Vec<PersistedEntry>,
}

/// Loads this node's own previously-persisted state from `path`. A
/// missing file is a real, honest "this node has never persisted
/// anything yet" - its first real run, or a fresh deployment - and
/// returns an empty map rather than an error; every other I/O or parse
/// failure is real and propagated, since silently discarding a real
/// file that failed to load would be worse than refusing to start.
pub fn load(path: &Path) -> io::Result<LwwMap<String, String>> {
    let mut map = LwwMap::new();
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(map),
        Err(e) => return Err(e),
    };
    let state: PersistedState =
        serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    for entry in state.entries {
        map.set(
            entry.key,
            entry.value,
            LamportTime(entry.time),
            entry.writer,
        );
    }
    Ok(map)
}

/// Persists `map`'s own complete real state to `path`, atomically: written
/// to a real sibling temp file first, then renamed over the real
/// destination, so a write interrupted partway (process killed, disk
/// full, power loss) never leaves `path` itself truncated or corrupt -
/// it is either the complete new state or the untouched previous one,
/// never in between. Same real crash-safe pattern this ecosystem already
/// uses everywhere durability matters (see e.g. HYDRA-UMC-SERVER's own
/// writeFileAtomic).
pub fn save(path: &Path, map: &LwwMap<String, String>) -> io::Result<()> {
    let state = PersistedState {
        entries: map
            .entries_with_stamps()
            .into_iter()
            .map(|(key, value, time, writer)| PersistedEntry {
                key,
                value,
                time: time.0,
                writer,
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&state).map_err(io::Error::other)?;

    let tmp_path = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json"),
        std::process::id()
    ));
    fs::write(&tmp_path, json)?;
    fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_of_a_missing_file_is_a_real_empty_map_not_an_error() {
        let dir = std::env::temp_dir().join(format!("swarm-sync-test-{}", std::process::id()));
        let path = dir.join("does-not-exist.json");
        let map = load(&path).expect("a missing state file must load as empty, not error");
        assert!(map.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_every_entry_and_its_real_stamp() {
        let dir = std::env::temp_dir().join(format!(
            "swarm-sync-test-{}-{}",
            std::process::id(),
            "roundtrip"
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set("x".to_string(), "1".to_string(), LamportTime(5), 1);
        map.set("y".to_string(), "2".to_string(), LamportTime(3), 2);

        save(&path, &map).expect("save must succeed");
        let reloaded = load(&path).expect("load must succeed");

        assert_eq!(reloaded.get(&"x".to_string()), Some(&"1".to_string()));
        assert_eq!(reloaded.get(&"y".to_string()), Some(&"2".to_string()));

        // The real point: a reloaded entry keeps ITS OWN real stamp, so a
        // write that would have lost a real conflict before the restart
        // still loses it after - a write from writer=99 at an EARLIER
        // logical time than x's own real stamp (5) must still lose.
        let mut after_reload = reloaded;
        after_reload.set(
            "x".to_string(),
            "should-lose".to_string(),
            LamportTime(2),
            99,
        );
        assert_eq!(
            after_reload.get(&"x".to_string()),
            Some(&"1".to_string()),
            "a reloaded entry's real stamp must still resolve conflicts correctly, not get reset"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!(
            "swarm-sync-test-{}-{}",
            std::process::id(),
            "atomic"
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set("a".to_string(), "1".to_string(), LamportTime(1), 1);
        save(&path, &map).expect("save must succeed");

        let leftover: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != path)
            .collect();
        assert!(
            leftover.is_empty(),
            "expected no leftover temp file, found: {leftover:?}"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
