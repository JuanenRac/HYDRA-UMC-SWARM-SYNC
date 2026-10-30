# HYDRA-UMC-SWARM-SYNC — CLI Reference

`hydra-umc-swarm-sync` is a single Rust binary (`src/main.rs`), positional-
only, single-purpose: real CRDT (LWW-map) state reconciliation
(`src/crdt.rs`, `src/lamport.rs`) driven by a JSON scenario file — not yet
real PTP (IEEE 1588) hardware clock sync or a live gossip network between
cells (both need real hardware/network decisions this v0 defers, see the
module doc at the top of `src/main.rs`). It merges every cell's writes left to
right and reports every conflict it resolved. That merging in a different
order gives the identical final state is a property of the CRDT itself,
pinned by the property tests in `src/crdt.rs`, not re-checked on every call.
Every example below was captured from a
real, built release binary run against the repo's own real
`scenarios/example.json` fixture — the output shown is real, not
illustrative.

## Usage

```
$ ./run.sh <scenario.json>
```

`run.sh` execs the built binary (`build/hydra-umc-swarm-sync` if present,
else `target/release/hydra-umc-swarm-sync`) and forwards all arguments
unchanged. The examples below invoke the release binary directly, which is
equivalent.

Every invocation — including bare, no-argument invocation — first prints
identity/version and role, then the command's own output:

```
$ hydra-umc-swarm-sync
HYDRA-UMC-SWARM-SYNC v0.1.3
CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.
Usage: hydra-umc-swarm-sync <scenario.json>
       hydra-umc-swarm-sync serve [--addr ADDR] [--port PORT] [--state-file PATH] [--max-time-jump N] [--max-generation-jump N] [--peer URL]... [--gossip-interval-ms N] [--gossip-push] [--allow-anonymous]
See scenarios/example.json for the expected format.
```

Bare invocation exits `0` — printing identity and usage is a valid
no-argument invocation, not a failure.

## `<scenario.json>`

A scenario is a list of `cells`, each with a `writer` id and a list of
`writes` (`key`, `value`, Lamport `time`). The repo's own
`scenarios/example.json` — two cells, one genuine key conflict between
them (`cell-a-node-2` is written by both cells at different Lamport times):

```json
{
  "cells": [
    {
      "id": "cell-a",
      "writer": 1,
      "writes": [
        { "key": "cell-a-node-1", "value": "ok", "time": 1 },
        { "key": "cell-a-node-1", "value": "degraded", "time": 4 },
        { "key": "cell-a-node-2", "value": "ok", "time": 2 }
      ]
    },
    {
      "id": "cell-b",
      "writer": 2,
      "writes": [
        { "key": "cell-b-node-1", "value": "ok", "time": 1 },
        { "key": "cell-a-node-2", "value": "unhealthy", "time": 3 }
      ]
    }
  ]
}
```

```
$ hydra-umc-swarm-sync scenarios/example.json
HYDRA-UMC-SWARM-SYNC v0.1.3
CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.
{
  "cells_merged": 2,
  "merged_state": {
    "cell-a-node-1": "degraded",
    "cell-a-node-2": "unhealthy",
    "cell-b-node-1": "ok"
  },
  "conflicts_resolved": 1,
  "conflicts": [
    {
      "key": "cell-a-node-2",
      "local_time": 2,
      "local_writer": 1,
      "local_value": "ok",
      "remote_time": 3,
      "remote_writer": 2,
      "remote_value": "unhealthy",
      "kept_remote": true
    }
  ],
  "next_local_time": 6
}
```

Exits `0`. Notable real behavior this demonstrates:

- **`conflicts`** — every genuine key collision resolved during the merge,
  with full visibility into which write won and why: `cell-a-node-2` was
  written `"ok"` at Lamport time `2` by cell-a and `"unhealthy"` at time
  `3` by cell-b; the later timestamp (`kept_remote: true`) wins.
- **`next_local_time`** — what a real node's own Lamport clock would tick
  to immediately after reconciling, having first observed the latest time
  learned from the merge (`4`, from `cell-a-node-1`'s second write) — so
  its very next local write is provably ordered after everything the
  swarm just taught it.

### Error paths

**Missing scenario file** (the operating system's own error text follows
`could not read`; exit `1`):

```
$ hydra-umc-swarm-sync scenarios/does-not-exist.json
HYDRA-UMC-SWARM-SYNC v0.1.3
CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.
[swarm-sync] could not read scenarios/does-not-exist.json: No such file or directory (os error 2)
```

**Malformed scenario JSON** (exit `1`):

```
$ echo '{not valid' > malformed.json
$ hydra-umc-swarm-sync malformed.json
HYDRA-UMC-SWARM-SYNC v0.1.3
CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.
[swarm-sync] could not parse malformed.json: key must be a string at line 1 column 2
```

**No cells in the scenario** — an honest refusal rather than reporting an
empty merge as success (exit `1`):

```
$ echo '{"cells": []}' > empty.json
$ hydra-umc-swarm-sync empty.json
HYDRA-UMC-SWARM-SYNC v0.1.3
CRDT swarm state reconciliation service: merges every HydraNode cell's view of swarm state into one convergent, order-independent result.
[swarm-sync] scenario has no cells - nothing to reconcile
```

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | ok — the scenario merged (or bare/no-argument usage output) |
| `1` | missing/unreadable/malformed scenario file, or a scenario with zero cells or an impossible conflict |
| `2` | `serve` could not start: the address/port could not be bound, a network address without `SWARM_SYNC_SHARED_SECRET`, a `--max-time-jump` that is not a number, or a `--state-file` that exists and cannot be read (the state is read before the port is bound, so a node never announces a listener it will not keep) |

## `serve` — JSON/HTTP API

`serve` reaches the exact same `reconcile_with_prior()` function the bare
CLI invocation's own `reconcile()` delegates to, over a real `tiny_http`
server (`src/server.rs`, blocking, no async runtime — same convention as
`HYDRA-UMC-TWIN`'s own `server.rs`). The scenario travels in the JSON
request body instead of a local file path, since a server-side file path
only ever made sense for a CLI running on the same machine as the file.
This is still a request/response computation over a scenario handed to
it — not the live gossip network between cells the module doc at the top
of `src/main.rs` documents as a deliberately deferred design decision.

```
$ hydra-umc-swarm-sync serve --port 8112 --state-file /opt/hydra-umc/swarm-sync/data/state.json
[swarm-sync] HTTP API listening on 127.0.0.1:8112
[swarm-sync] POST /reconcile, GET /stats, GET /state, GET /conflicts, GET /health
[swarm-sync] persisting this node's own state to "/opt/hydra-umc/swarm-sync/data/state.json"
```

`--addr` (default `127.0.0.1`) and `--port` (default `8112`) are both
optional. **Listening on the network needs a secret:** with an `--addr` that is
not loopback, `serve` refuses to start (`[swarm-sync] fatal: ...`, exit `2`)
unless `SWARM_SYNC_SHARED_SECRET` is set, because a node that merges what any
machine on the network sends it lets that machine rewrite the swarm's view of
itself; `--allow-anonymous` runs an open node on purpose, for a lab. A request
body over 1 MiB is refused with `413` before it is read (a body that announces more than that is refused without reading any of it), requests are served by four workers so a slow upload holds one of them and not the node, and the state file (its directory is created if missing) is
written to a temp file, forced to disk and renamed over the real one, so a
power cut cannot leave a truncated state. **`--state-file PATH`** (or the `SWARM_SYNC_STATE_FILE` env
var) is real, opt-in per-node persistence (`src/store.rs`) — this server
used to be fully stateless, discarding every `/reconcile` result instead
of remembering it. Unset, this node stays exactly the memory-only behavior it always
had for the lifetime of the process — a real, supported mode for a
short-lived test/demo instance with nothing worth surviving a restart.
Set, this node's own real CRDT state is loaded from that file at
startup (or starts empty on a genuine first run) and is what every
`/reconcile` call merges its own scenario into — not a blank slate every
time — persisting the new merged result back to the same real file
after each successful call, atomically (a real crash-safe temp-file-then-
rename, never a half-written state file). The process keeps running
until killed; there is no separate "stop" command.

| Route | Method | Behavior |
|-------|--------|----------|
| `/reconcile` | `POST` | Body is a scenario JSON (same shape as `<scenario.json>` above), merged into this node's own current state (empty if `--state-file` is unset, or nothing has been reconciled yet). Returns `200` with the same `ReconcileOutput` JSON the CLI prints on success; `400` with `{"error": "..."}` on a malformed body, a scenario with zero cells or an impossible conflict inside it; `409` when the node's own Lamport clock cannot advance; `413` for a body over 1 MiB; `422` when a write carries a Lamport `time` more than `--max-time-jump` (default 1000000) past the newest time already in the state; or `500` if the scenario merged successfully but `--state-file` itself could not be written. |
| `/stats` | `GET` | Returns `200` with `{"role": "CRDT swarm state reconciliation", "persistent": true\|false, "sync": {...}}` — `persistent` reflects whether `--state-file` is configured. Open, whatever the secret. |
| `/health` | `GET` | Returns `200` with `{"status": "ok", "name": "HYDRA-UMC-SWARM-SYNC", "version": ...}`. Open, whatever the secret, for a watchdog. |
| `/conflicts` | `GET` | Returns `200` with `{"conflictsResolved": n, "remembered": m, "conflicts": [{"key", "winnerWriter", "loserWriter", "atMs"}, ...]}`: the conflicts this node resolved while it reconciled or merged, newest first, so an operator can see which write lost and whose it was (`/stats` only shows the last one). A node remembers the most recent 200 (`remembered`); `conflictsResolved` keeps counting all of them. `?limit=N` returns only the newest N (1 to 200). It names keys, so it needs the shared secret like `/state`. |
| `/state` | `GET` | Returns `200` with this node's own current merged state as a plain `{"key": "value", ...}` object (no stamps/history) — real, direct visibility into what every past `/reconcile` call has accumulated. |
| anything else | any | Returns `404` with `{"error": "not found"}`. |

Real, captured output against the repo's own `scenarios/example.json`,
with `--state-file` configured:

```
$ curl -s -X POST http://127.0.0.1:8112/reconcile -d @scenarios/example.json
{"cells_merged":2,"conflicts":[{"kept_remote":true,"key":"cell-a-node-2","local_time":2,"local_value":"ok","local_writer":1,"remote_time":3,"remote_value":"unhealthy","remote_writer":2}],"conflicts_resolved":1,"merged_state":{"cell-a-node-1":"degraded","cell-a-node-2":"unhealthy","cell-b-node-1":"ok"},"next_local_time":6}
$ curl -s http://127.0.0.1:8112/stats
{"persistent":true,"role":"CRDT swarm state reconciliation"}
$ curl -s http://127.0.0.1:8112/state
{"cell-a-node-1":"degraded","cell-a-node-2":"unhealthy","cell-b-node-1":"ok"}
```

Field content is identical to the CLI's own output above; the key order
differs (alphabetical here) because the HTTP path serializes through a
generic `serde_json::Value` rather than the CLI's direct
`to_string_pretty(&ReconcileOutput)` call — both are the same real
`reconcile()`/`reconcile_with_prior()` result, just serialized two
different ways. A genuinely restarted process pointed at the same real
`--state-file` reports the exact same `/state` a second later, with zero
extra `/reconcile` calls needed to rebuild it — verified for real against
an actual killed-and-relaunched binary, not just the in-process tests
above.

**Why a write cannot be far ahead.** The Lamport `time` and the `writer` of a write
are whatever the caller says. A write with a time near `u64::MAX` would win every
conflict for ever and, because the clock refuses to pass it, could never be
overwritten: the key would stay captured until somebody edited the state file by
hand. Each request may therefore move a time at most `--max-time-jump` past the
newest time in the state (0 when it is empty); whatever one request writes, the next
one can still correct. A client that stamps its writes with wall-clock milliseconds
must raise the flag. The secret is one for every node: it proves the caller knows
it, not which node it is.

`systemd/hydra-umc-swarm-sync.service` runs this exact `serve` mode as a
loopback-only unit on the CM5 (`HYDRA-UMC-OS/provisioning/install_swarm_sync.sh`),
with `--state-file` pointed at a real, durable path.

## Not yet wired in

Neither a live gossip network between cells nor real PTP (IEEE 1588)
hardware clock sync exist yet — see the module doc at the top of
`src/main.rs` for why both are deferred (PTP needs real hardware
timers/NICs to mean anything; a gossip transport is a real network design
decision). What this CLI/HTTP API does prove for real is the CRDT merge
property itself — commutative, associative, and idempotent — on a
concrete multi-cell scenario, whether driven by a local file or a network
request.

## Typed values: the state of a robot

The map keeps strings, but what is written under the key `robot_state/<robot_id>` is a `RobotState` of the SDK (`robot-state.schema.json`, vendored under `schemas/`): `schema_version` `1.0`, `robot_id`, `state` (`idle`, `busy`, `estop`, `fault` or `offline`), `updated_at_utc` and, optionally, `source`, `mission_id`, `battery_percent` and a `pose`. Before a value touches the map, in `POST /reconcile` and in `POST /merge` alike, it must be JSON, pass the schema (Draft 2020-12, formats checked), carry the `robot_id` of its key and, when it has a pose, a quaternion of unit length. Otherwise the whole request is a `422` naming the key and the reason, and nothing is merged. A deletion (a tombstone) is always accepted and every key that does not start with `robot_state/` stays untyped. The missions are not typed yet.

## Exchanging state with other nodes

| Flag | Meaning |
| --- | --- |
| `--peer http://host:port` | A node to ask for its state; repeat for each one. Only `http://` is spoken (a peer behind TLS is reached through a tunnel). The node sends `SWARM_SYNC_SHARED_SECRET` as the bearer token, so the peers must share it |
| `--gossip-interval-ms N` | Pause between two rounds, at least 1000 (default 10000) |
| `--gossip-push` | Also send this node's state to each peer (`POST /merge`) after pulling theirs |
| `--max-generation-jump N` | How far past the generation this node holds for a key an incoming entry may be (default 1000) |

Routes: `GET /state?full=1` answers `{"entries": [{"key", "value" (null for a deleted key), "time", "writer", "generation"}]}`; `POST /merge` takes `{"entries": [...]}` of that shape and answers `entries_received`, `conflicts_resolved`, `conflicts` (each with both generations) and `next_local_time`; an entry with a time or a generation too far ahead, an empty or oversized key, a value over 64 KiB or generation 0 is refused with `422`, the same stamp claiming two values with `400`. A `Write` of a `/reconcile` scenario may carry `"deleted": true` (its `value` is then ignored). `GET /stats` adds `peers` (rounds, last success age, last error, entries pulled and pushed per peer) and `sync.merges`.
