# Changelog

All notable work on **HYDRA-UMC-SWARM-SYNC** is summarized here, newest first. Full
session-by-session detail (including dates) lives in a private,
unpublished internal log - this file is public, so it intentionally
omits calendar dates.

## Versioning scheme

`Cargo.toml`'s `version` field is bumped automatically by
`bump_version.py`, run from `build.sh`/`build.bat` before every real
release build (`cargo build --release`).

It follows the ecosystem-wide base-10 "odometer" rule rather than
semantic-versioning judgment calls:

- `PATCH` +1 on every build
- when `PATCH` would exceed 9, it resets to 0 and `MINOR` +1 instead (e.g. `0.0.9` -> `0.1.0`, never `0.0.10`)
- the same carry cascades into `MAJOR` if `MINOR` would exceed 9

---

## [0.0.2] - Real CRDT state reconciliation (LWW-Element-Map + Lamport clock)

- **`src/lamport.rs`** - a real Lamport logical clock: `tick()` for a
  local event, `observe(remote)` for the standard Lamport rule on
  receiving a remote timestamp (jump to one past whichever is later).
  This is what backs the CRDT's ordering - not the README's PTP (IEEE
  1588) hardware sync, which needs real NICs/hardware timers to mean
  anything and stays deferred (see `mejoras_futuras.txt`).
- **`src/crdt.rs`** - a real LWW-Element-Map: `set`/`get`/`merge`, where
  every entry carries a `(LamportTime, writer_id)` stamp and the higher
  stamp wins a conflict, `writer_id` breaking a true tie deterministically
  (every node resolves the same concurrent conflict the same way without
  coordinating). `merge` is a genuine join over a semilattice (per-key
  max by stamp) - proven, not assumed, by the property tests: merge is
  commutative, associative and idempotent, checked directly rather than
  just exercised on one example. No tombstones/delete support yet - see
  `mejoras_futuras.txt` for why that's its own real design decision, not
  bolted on without thinking it through.
- **`src/main.rs`** - now a real CLI: loads a multi-cell JSON scenario,
  builds one map per cell from its writes, merges every cell's map both
  left-to-right and right-to-left, and reports `converged: true` only if
  both orders produced the identical final state - the actual CRDT
  property this service depends on, demonstrated on a concrete scenario,
  not just claimed. Also folds the merged state's latest logical time
  into a fresh `LamportClock` and reports `next_local_time` - what a real
  node's very next local write would be stamped with right after
  reconciling, the same mechanism a live daemon would use.
- Added `serde`/`serde_json` as the crate's first real dependencies (for
  scenario I/O) - still no async runtime, no network transport.
- Verified for real: `cargo build`/`cargo build --release` clean; 11
  `cargo test` cases, including direct checks of all three CRDT laws
  (commutativity, associativity, idempotence), a deterministic-tie-break
  test for truly concurrent writes, and a test that simulates exactly the
  README's own rationale - two cells updating their own node's status
  independently while partitioned, then reconciling with no update lost.
  Additionally smoke-tested the compiled release binary end-to-end
  against `scenarios/example.json` - a real cross-cell conflict (two
  cells writing the same key) resolved correctly by timestamp, printed as
  valid JSON with `converged: true`.
- What's still not real, on purpose - see `mejoras_futuras.txt`: PTP
  (IEEE 1588) hardware clock sync, a live gossip/network transport
  between cells (this is a CLI over a JSON scenario file today, not a
  network service), and tombstone/delete support for the CRDT map.

## [0.0.1] - Initial scaffolding

- **`src/main.rs`** - minimal real entry point (prints identity/version/role, exits 0). No swarm-sync logic yet - CRDT-based state reconciliation across multiple HYDRA-UMC cells lands in a later pass.
- **`Cargo.toml`** - crate metadata, no runtime dependencies yet.
- **`build.sh` / `build.bat`**, **`run.sh` / `run.bat`** - `cargo build --release` and run the resulting binary.
