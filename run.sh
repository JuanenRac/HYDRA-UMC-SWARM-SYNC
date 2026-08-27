#!/usr/bin/env bash
# =============================================================================
# HYDRA-UMC-SWARM-SYNC - run.sh
# Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
# GPL-3.0 - see LICENSE
# =============================================================================
set -uo pipefail  # no -e: we need to reach the trap below even if the binary exits non-zero
cd "$(dirname "$0")"

# Keep the window open if this was double-clicked instead of run from an
# already-open terminal - only prompts when stdin is actually a terminal
# (never in CI/piped/non-interactive runs). Not `exec`ing the binary
# below (a deliberate change from a plain passthrough) is what lets this
# trap still run once the process itself exits.
trap '[ -t 0 ] && read -r -p "Press Enter to close..." _' EXIT

if [ -x build/hydra-umc-swarm-sync ]; then
    build/hydra-umc-swarm-sync "$@"
    exit $?
elif [ -x target/release/hydra-umc-swarm-sync ]; then
    target/release/hydra-umc-swarm-sync "$@"
    exit $?
elif [ -x build/hydra-umc-swarm-sync.exe ]; then
    build/hydra-umc-swarm-sync.exe "$@"
    exit $?
elif [ -x target/release/hydra-umc-swarm-sync.exe ]; then
    target/release/hydra-umc-swarm-sync.exe "$@"
    exit $?
else
    echo "No compiled binary found. Run build.sh first." >&2
    exit 1
fi
