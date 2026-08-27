#!/usr/bin/env bash
# HYDRA-UMC-SWARM-SYNC - build.sh
# Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
# GPL-3.0 - see LICENSE
set -euo pipefail
python3 "$(dirname "$0")/bump_manifest_version.py" || exit 1
cd "$(dirname "$0")"

# Keep the window open if this was double-clicked (e.g. from a file
# manager) instead of run from an already-open terminal - fires on
# success AND on a `set -e` early exit alike, but only prompts when
# stdin is actually a terminal (never in CI/piped/non-interactive runs).
trap '[ -t 0 ] && read -r -p "Press Enter to close..." _' EXIT

echo "=== HYDRA-UMC-SWARM-SYNC build ==="
python3 bump_version.py || echo "WARNING: could not bump version, continuing build anyway."

cargo build --release

mkdir -p build
cp -f target/release/hydra-umc-swarm-sync build/hydra-umc-swarm-sync 2>/dev/null || \
    cp -f target/release/hydra-umc-swarm-sync.exe build/hydra-umc-swarm-sync.exe

echo "Build OK: build/hydra-umc-swarm-sync"
