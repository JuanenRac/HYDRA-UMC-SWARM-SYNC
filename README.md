<p align="center">
  <img src="images/HYDRA_UMC_BANNER.svg" alt="HYDRA-UMC-SWARM-SYNC banner" width="100%">
</p>

# ⏱️ HYDRA-UMC-SWARM-SYNC

<p align="center">🇺🇸 <b>English</b> | <a href="README_spa.md">🇪🇸 Español</a> | <a href="README_fra.md">🇫🇷 Français</a> | <a href="README_ita.md">🇮🇹 Italiano</a> | <a href="README_deu.md">🇩🇪 Deutsch</a> | <a href="README_zho.md">🇨🇳 简体中文</a> | <a href="README_jpn.md">🇯🇵 日本語</a></p>

### 📡 Precision Time Protocol (PTP) & Multi-Node Synchronization

<p align="left">
  <img src="https://img.shields.io/badge/Licencia-GPL%203.0-blue.svg" alt="GPL 3.0">
  <img src="https://img.shields.io/badge/Protocol-PTP%20%2F%20IEEE%201588-orange.svg" alt="PTP">
  <img src="https://img.shields.io/badge/Accuracy-<100ns-green.svg" alt="Accuracy">
</p>

---

## 1. 🛠️ TECHNICAL OVERVIEW

**HYDRA-UMC-SWARM-SYNC** is the heartbeat of the distributed factory. It implements a specialized version of PTP (Precision Time Protocol / IEEE 1588) to ensure that every controller in the network shares a perfectly synchronized global clock.

This synchronization is critical for multi-robot coordinated motion, where multiple arms must start and end trajectories at the exact same microsecond to avoid collisions or to perform joint assembly tasks.

### Key Features:
* ⏱️ **Ultra-Precise Sync:** Achieves sub-100ns jitter across the local network.
* 🔄 **Synchronized Start/Stop:** Ensures atomic execution of multi-robot trajectory commands.
* 📡 **Hardware Timestamping:** Leverages CM5 and STM32 hardware timers for maximum accuracy.
* 🛡️ **Network Resilient:** Handles packet jitter and temporary network delays.

---

## 2. 🔄 SYNC HIERARCHY

```mermaid
flowchart TD
    GM["Grandmaster Clock (Orchestrator Node)"] --> S1["HydraNode 1 (Slave)"]
    GM --> S2["HydraNode 2 (Slave)"]
    GM --> S3["HydraNode 3 (Slave)"]
    S1 --> H745_1["STM32H745 Local Clock"]
    S2 --> H745_2["STM32H745 Local Clock"]
    S3 --> H745_3["STM32H745 Local Clock"]
```

---

## 3. 🧱 ARCHITECTURE & DESIGN DECISIONS

* **Why CRDT-based sync, not a single source of truth.** Multiple HYDRA-UMC cells can run semi-autonomously and reconnect later - a CRDT merge strategy converges without a central arbiter deciding whose state 'wins', which a naive last-write-wins approach can't guarantee across a real network partition.
* **Why this is a sibling, not a submodule, of HYDRA-UMC-ORCHESTRATOR.** State reconciliation is a continuous background concern independent of any single orchestration decision - keeping it a separate process means an orchestrator restart doesn't interrupt an in-flight merge.
* **Why the CRDT merge is real today but PTP hardware sync is not.** `src/crdt.rs` implements a real LWW-Element-Map (Last-Writer-Wins Map), a state-based CRDT whose `merge` is provably commutative, associative and idempotent - not just "seems to converge on one example", see that module's own property tests. `src/lamport.rs` backs it with a real Lamport logical clock. PTP (IEEE 1588, sub-100ns hardware timestamping) is a fundamentally different, hardware-dependent problem - it needs real NICs/hardware timers to mean anything, and stays deferred until there's real hardware to validate it against. A logical clock is what the CRDT merge actually needs to resolve conflicts deterministically, and that part is real and tested today.
* **How this fits the rest of the ecosystem.** A sibling service under HYDRA-UMC-ORCHESTRATOR, alongside HYDRA-UMC-PATH-PLANNER-3D, HYDRA-UMC-JOB-DISPATCHER and HYDRA-UMC-NODE-HEALING - keeps every cell's own view of swarm state consistent regardless of which one currently holds the orchestrator role.

---

## 📂 DIRECTORY STRUCTURE

```text
HYDRA-UMC-SWARM-SYNC/
├── src/
│   ├── main.rs       # CLI entry point: loads a scenario, reconciles, prints JSON
│   ├── lamport.rs    # LamportClock - the logical clock behind the CRDT's ordering
│   └── crdt.rs       # LwwMap - the real CRDT: set/get/merge/snapshot
├── scenarios/        # Example JSON scenarios (see BUILD & RUN below)
├── build/            # Compiled binaries (build.sh/build.bat output)
├── Cargo.toml        # Rust package manifest (name, version, deps)
├── bump_version.py   # Odometer-style version bump, run by build.sh/.bat
├── build.sh/.bat     # Bumps version, then `cargo build --release`
├── run.sh/.bat       # Runs the compiled binary
└── README.md
```

Pruned from the original template: `hardware/`, `firmware/`, `os/`, `docs/`,
`images/` and `scripts/` — this is a pure software service (Rust binary)
with no dedicated hardware or firmware of its own, no operating system
image to maintain, and no documentation/media/utility-script content
substantial enough yet to warrant their own folders.

---

## 🔧 BUILD & RUN

A real, tested CRDT merge - not just a skeleton that compiles: it
reconciles a multi-cell JSON scenario and prints the converged state.

```bash
# Windows
build.bat
run.bat scenarios/example.json

# Linux / macOS
./build.sh
./run.sh scenarios/example.json
```

`build.sh`/`build.bat` bump the version in `Cargo.toml` (ecosystem-wide
odometer rule, see `bump_version.py`) and then run `cargo build --release`.
`run.sh`/`run.bat` execute the resulting binary directly, forwarding any
arguments (the scenario path) to it.

A scenario is a JSON file with a `cells` array - each cell has an `id`
(for readability), a `writer` ID, and a list of `writes`
(`{key, value, time}`, `time` being an explicit Lamport timestamp,
replayed rather than generated live - the same "explicit, deterministic
input" pattern HYDRA-UMC-PATH-PLANNER-3D's `seed` uses). Each cell's
writes are folded into its own map, then every cell's map is merged
twice - once left-to-right, once right-to-left - and the result prints
`converged: true` only if both orders produced the identical final
state, which is the actual CRDT property this service depends on, not
just an assumption.

```bash
cargo test   # the Lamport clock, and the CRDT itself - including direct
             # checks that merge is commutative, associative and
             # idempotent (not just "looks right" on one example), a
             # deterministic-tie-break test for truly concurrent writes,
             # and a test that simulates two cells running autonomously
             # and reconciling later, per this README's own rationale
```

---

## 🚀 ROADMAP
* **Phase 1:** Deterministic swarm synchronization over TSN and sub-ms jitter reduction.
* **Phase 2:** 3D Path planning with dynamic obstacle avoidance in multi-robot cells.
* **Phase 3:** Multi-robot job dispatching optimization using real-time resource availability.
* **Phase 4:** Support for wireless PTP sync over Wi-Fi 6 (High-Reliability Mode) and sub-100ns accuracy validation.

---

## 🔗 Related Projects

This project is part of a larger robotics ecosystem by the same author (JuanenRac / Electro Hobby 3D), spanning firmware, control software, AI nodes, and fleet tooling. Worth knowing about, since a request might actually be about one of these rather than this repository.

### Family

**Parent:** **[HYDRA-UMC-ORCHESTRATOR](https://github.com/JuanenRac/HYDRA-UMC-ORCHESTRATOR)** — the integration parent this sync layer keeps consistent.

**Siblings:**
- **[HYDRA-UMC-PATH-PLANNER-3D](https://github.com/JuanenRac/HYDRA-UMC-PATH-PLANNER-3D)** — sibling orchestration service, same parent.
- **[HYDRA-UMC-JOB-DISPATCHER](https://github.com/JuanenRac/HYDRA-UMC-JOB-DISPATCHER)** — sibling orchestration service, same parent.
- **[HYDRA-UMC-NODE-HEALING](https://github.com/JuanenRac/HYDRA-UMC-NODE-HEALING)** — sibling orchestration service, same parent.

### Directly Related (outside the family)

This project has no direct relation outside the Orchestration & Swarm family (per the ecosystem's own relationship map) - see "Rest of the Ecosystem" below for everything else.

### Rest of the Ecosystem

**HYDRA-UMC platform** — the multi-robot micro-factory cell
- **[HYDRA-UMC](https://github.com/JuanenRac/HYDRA-UMC)** — the CM5 + STM32H745 motherboard orchestrating up to 8 robot arms.
- **[HYDRA-UMC-SERVER](https://github.com/JuanenRac/HYDRA-UMC-SERVER)** — the Express/WebSocket backend every control client talks to.
- **[HYDRA-UMC-STUDIO](https://github.com/JuanenRac/HYDRA-UMC-STUDIO)** — web-based control dashboard, multi-robot 3D visualization.
- **[HYDRA-UMC-ANDROID-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-ANDROID-CONTROL)** — Android control app over Wi-Fi/Bluetooth.
- **[HYDRA-UMC-IOS-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-IOS-CONTROL)** — iOS/iPadOS control app built in Flutter.
- **[HYDRA-UMC-SUITE](https://github.com/JuanenRac/HYDRA-UMC-SUITE)** — desktop swarm command center (Python/PySide6).
- **[HYDRA-UMC-EDITOR-URDF](https://github.com/JuanenRac/HYDRA-UMC-EDITOR-URDF)** — desktop URDF model editor for the robot catalog.
- **[HYDRA-UMC-DSI](https://github.com/JuanenRac/HYDRA-UMC-DSI)** — native touch UI for the onboard DSI touchscreen.

**URTC platform** — the tool head controller every HYDRA-UMC robot arm carries
- **[URTC](https://github.com/JuanenRac/URTC)** — CAN bus tool head controller, 25 tool profiles.
- **[URTC-FLASHER](https://github.com/JuanenRac/URTC-FLASHER)** — desktop CAN-OTA + SWD/JTAG flashing tool.
- **[URTC-TESTER](https://github.com/JuanenRac/URTC-TESTER)** — desktop live CAN-bus diagnostic tool.
- **[URTC-WEB-STUDIO](https://github.com/JuanenRac/URTC-WEB-STUDIO)** — browser-based alternative via Web Serial API.

**🎥 Vision AI Node (Hailo-8)**
- [HYDRA-UMC-VISION-NODE](https://github.com/JuanenRac/HYDRA-UMC-VISION-NODE)
- [HYDRA-UMC-VISION-STREAMER](https://github.com/JuanenRac/HYDRA-UMC-VISION-STREAMER)
- [HYDRA-UMC-DETECTION-HEF](https://github.com/JuanenRac/HYDRA-UMC-DETECTION-HEF)
- [HYDRA-UMC-SAFETY-ZONES](https://github.com/JuanenRac/HYDRA-UMC-SAFETY-ZONES)
- [HYDRA-UMC-VISUAL-SERVOING-API](https://github.com/JuanenRac/HYDRA-UMC-VISUAL-SERVOING-API)

**🧠 Cognitive AI Node (Hailo-10)**
- [HYDRA-UMC-COGNITIVE-NODE](https://github.com/JuanenRac/HYDRA-UMC-COGNITIVE-NODE)
- [HYDRA-UMC-VLA-ENGINE](https://github.com/JuanenRac/HYDRA-UMC-VLA-ENGINE)
- [HYDRA-UMC-VOICE-UI](https://github.com/JuanenRac/HYDRA-UMC-VOICE-UI)
- [HYDRA-UMC-SEMANTIC-PLANNER](https://github.com/JuanenRac/HYDRA-UMC-SEMANTIC-PLANNER)
- [HYDRA-UMC-DOCS-QA](https://github.com/JuanenRac/HYDRA-UMC-DOCS-QA)

**🎮 Digital Twin & Simulation**
- [HYDRA-UMC-TWIN](https://github.com/JuanenRac/HYDRA-UMC-TWIN)
- [HYDRA-UMC-PHYSICS-REPLICA](https://github.com/JuanenRac/HYDRA-UMC-PHYSICS-REPLICA)
- [HYDRA-UMC-HIL-BRIDGE](https://github.com/JuanenRac/HYDRA-UMC-HIL-BRIDGE)
- [HYDRA-UMC-SYNTHETIC-DATA-GEN](https://github.com/JuanenRac/HYDRA-UMC-SYNTHETIC-DATA-GEN)

**📊 Data & Analytics**
- [HYDRA-UMC-DATALAKE](https://github.com/JuanenRac/HYDRA-UMC-DATALAKE)
- [HYDRA-UMC-TELEMETRY-COLLECTOR](https://github.com/JuanenRac/HYDRA-UMC-TELEMETRY-COLLECTOR)
- [HYDRA-UMC-ANOMALY-DETECTOR](https://github.com/JuanenRac/HYDRA-UMC-ANOMALY-DETECTOR)
- [HYDRA-UMC-PRODUCTION-REPORTS](https://github.com/JuanenRac/HYDRA-UMC-PRODUCTION-REPORTS)

**🏭 Industrial Gateway**
- [HYDRA-UMC-GATEWAY-INDUSTRIAL](https://github.com/JuanenRac/HYDRA-UMC-GATEWAY-INDUSTRIAL)
- [HYDRA-UMC-OPCUA-SERVER](https://github.com/JuanenRac/HYDRA-UMC-OPCUA-SERVER)
- [HYDRA-UMC-MQTT-BROKER](https://github.com/JuanenRac/HYDRA-UMC-MQTT-BROKER)
- [HYDRA-UMC-MTCONNECT-ADAPTER](https://github.com/JuanenRac/HYDRA-UMC-MTCONNECT-ADAPTER)

**🛠️ Complementary Tools**
- [URTC-SMART-RACK](https://github.com/JuanenRac/URTC-SMART-RACK)
- [URTC-VISION-TOOL](https://github.com/JuanenRac/URTC-VISION-TOOL)
- [HYDRA-UMC-WATCH](https://github.com/JuanenRac/HYDRA-UMC-WATCH)
- [HYDRA-UMC-TOOL-CLI](https://github.com/JuanenRac/HYDRA-UMC-TOOL-CLI)
- [HYDRA-UMC-DASHBOARD-AI](https://github.com/JuanenRac/HYDRA-UMC-DASHBOARD-AI)


## 👤 AUTHOR
**JuanenRac** (Electro Hobby 3D)
📧 electrohobby3d@gmail.com

## 📜 LICENSE
GPL-3.0 - See LICENSE for details.

## 🛠️ BUILD & RUN

Use the non-versioning build check before a release build:

| Action | Windows | Linux / macOS |
|---|---|---|
| Build check (no version or CHANGELOG change) | `build-test.bat` | `./build-test.sh` |
| Run / development (when provided) | `run*.bat` or `dev*.bat` | `./run*.sh` or `./dev*.sh` |

`build-test.bat` and `build-test.sh` compile or validate the project stack without incrementing `hydra-umc.project.json` or modifying `CHANGELOG.md`. They may create normal compiler output only. Existing `build*.bat`, `build*.sh`, `run*` and `dev*` scripts retain their project-specific, versioned or runtime behavior; use them when that behavior is required.