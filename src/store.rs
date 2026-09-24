// HYDRA-UMC-SWARM-SYNC - src/store.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
//
// POST
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
/// exact (generation, time, writer) stamp
/// `crdt::LwwMap::entries_with_stamps` reports, so reloading it
/// re-establishes the SAME conflict-resolution state a live map already
/// converged to, not a fresh one.
///
/// `value` is `None` for a real tombstone (a key this node deleted
/// via `LwwMap::remove`) - persisting only present entries would forget
/// every real deletion on restart, reopening the exact resurrection
/// `crdt.rs`'s own header comment describes the first time this node
/// reconnects to a peer that still has the pre-delete value.
#[derive(Serialize, Deserialize)]
struct PersistedEntry {
    key: String,
    value: Option<String>,
    time: u64,
    writer: u64,
    generation: u64,
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
        // restore_entry, not set - this replays each entry's own
        // exact prior stamp (tombstone included), never records a fresh
        // life-phase transition for it. See LwwMap::restore_entry's own
        // doc comment for why that distinction matters.
        map.restore_entry(
            entry.key,
            entry.value,
            LamportTime(entry.time),
            entry.writer,
            entry.generation,
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
            .map(|(key, value, time, writer, generation)| PersistedEntry {
                key,
                value,
                time: time.0,
                writer,
                generation,
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

    #[test]
    fn a_real_deletion_survives_save_then_load_as_a_tombstone() {
        // before tombstones existed, save only ever wrote PRESENT
        // entries, so a real remove() was indistinguishable on disk from
        // a key that had simply never existed - reload lost the
        // deletion outright.
        let dir = std::env::temp_dir().join(format!(
            "swarm-sync-test-{}-{}",
            std::process::id(),
            "tombstone-roundtrip"
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set("x".to_string(), "1".to_string(), LamportTime(1), 1);
        map.remove("x".to_string(), LamportTime(2), 1);
        assert_eq!(
            map.get(&"x".to_string()),
            None,
            "removed before save at all"
        );

        save(&path, &map).expect("save must succeed");
        let reloaded = load(&path).expect("load must succeed");
        assert_eq!(
            reloaded.get(&"x".to_string()),
            None,
            "a real deletion must still be gone after a restart, not resurrected"
        );
        assert!(
            reloaded.is_empty(),
            "a tombstone-only map must still report as empty (len/is_empty never count tombstones)"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reloaded_tombstone_still_beats_a_stale_remote_add_after_restart() {
        // this project's own real motivation: a node deletes a key, restarts (the
        // exact moment this module's own persistence matters), then
        // reconnects to a peer whose own copy is still the stale,
        // pre-delete value. Without generation surviving the restart
        // too, a bare (time, writer) stamp comparison could let that
        // stale remote add win back in - this proves it can't.
        let dir = std::env::temp_dir().join(format!(
            "swarm-sync-test-{}-{}",
            std::process::id(),
            "tombstone-outlives-restart"
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let mut map: LwwMap<String, String> = LwwMap::new();
        map.set("x".to_string(), "1".to_string(), LamportTime(1), 1);
        map.remove("x".to_string(), LamportTime(2), 1);
        save(&path, &map).expect("save must succeed");
        let reloaded = load(&path).expect("load must succeed");

        // The stale peer: still has the pre-delete value, stamped at a
        // LATER raw Lamport time than the local delete (e.g. it kept
        // ticking its clock on unrelated keys while partitioned) - a
        // plain (time, writer) comparison alone would wrongly let this
        // win.
        let mut stale_peer: LwwMap<String, String> = LwwMap::new();
        stale_peer.set(
            "x".to_string(),
            "stale-value".to_string(),
            LamportTime(99),
            2,
        );

        let merged = reloaded.merge(&stale_peer);
        assert_eq!(
            merged.get(&"x".to_string()),
            None,
            "a reloaded tombstone must still beat a stale remote add, even across a restart"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
