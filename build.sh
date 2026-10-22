#!/usr/bin/env bash
# HYDRA_UMC_SCRIPT_STANDARD_HEADER_BEGIN
# *****************************************************************************
# Project   : HYDRA-UMC-SWARM-SYNC
# Script    : build.sh
# Purpose   : Incremental project build, verification and packaging workflow.
# Author    : JuanenRac (Electro Hobby 3D)
# Email     : electrohobby3d@gmail.com
# Copyright : (C) 2026 JuanenRac
# License   : GPL-3.0 - see LICENSE
# *****************************************************************************
# HYDRA_UMC_SCRIPT_STANDARD_HEADER_END
# HYDRA_UMC_SCRIPT_STANDARD_BANNER_BEGIN
printf '\n*******************************************************************************\n'
printf '%s\n' "* HYDRA-UMC-SWARM-SYNC - build.sh"
printf '%s\n' "* Mode      : INCREMENTAL BUILD"
printf '%s\n' "* Author    : JuanenRac (Electro Hobby 3D)"
printf '%s\n' "* Email     : electrohobby3d@gmail.com"
printf '%s\n' "* Copyright : (C) 2026 JuanenRac"
printf '%s\n' "* License   : GPL-3.0 - see LICENSE"
printf '%s\n' "* ------------------------------------------------------------------------- *"
printf '%s\n' "* 1. Read the project version (building never changes it; use the explicit bump command)."
printf '%s\n' "* 2. Run this project's declared build, verification and packaging commands."
printf '%s\n' "* 3. Report the result and keep an interactive terminal open."
printf '%s\n' "*******************************************************************************"
printf '\n'
# HYDRA_UMC_SCRIPT_STANDARD_BANNER_END
set -euo pipefail
cd "$(dirname "$0")"

# Keep the window open if this was double-clicked (e.g. from a file
# manager) instead of run from an already-open terminal - fires on
# success AND on a `set -e` early exit alike, but only prompts when
# stdin is actually a terminal (never in CI/piped/non-interactive runs).
trap '[ -t 0 ] && read -r -p "Press Enter to close..." _' EXIT

echo "=== HYDRA-UMC-SWARM-SYNC build ==="
# HYDRA_UMC_SCRIPT_STANDARD_VERSION_STEP
printf '%s\n' "[1/3] Reading the project version (building never changes it)..."
HYDRA_UMC_VERSION_BUILT="$(python3 -c 'import json, pathlib, sys; print(json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["version"])' "$(dirname "$0")/hydra-umc.project.json")"
printf '%s\n' "      Building v${HYDRA_UMC_VERSION_BUILT:-unknown} (building never changes the version)"
echo

cargo build --release
cargo test

mkdir -p build
cp -f target/release/hydra-umc-swarm-sync build/hydra-umc-swarm-sync 2>/dev/null || \
    cp -f target/release/hydra-umc-swarm-sync.exe build/hydra-umc-swarm-sync.exe

echo "Build OK: build/hydra-umc-swarm-sync"
printf '%s\n' "Built v${HYDRA_UMC_VERSION_BUILT:-unknown}. Building never changes the version; to raise it run: python tools/hydra_project_tool.py bump"
