@echo off
REM HYDRA-UMC-SWARM-SYNC - build.bat
REM Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
REM GPL-3.0 - see LICENSE
setlocal
cd /d "%~dp0"

echo === HYDRA-UMC-SWARM-SYNC build ===
python bump_version.py
if errorlevel 1 ( echo NATIVE VERSION BUMP FAILED. & pause & exit /b 1 )
python "%~dp0bump_manifest_version.py" --sync
if errorlevel 1 ( echo VERSION SYNCHRONIZATION FAILED. & pause & exit /b 1 )
if errorlevel 1 (
    echo WARNING: could not bump version, continuing build anyway.
)

cargo build --release
if errorlevel 1 (
    echo BUILD FAILED.
    pause
    exit /b 1
)

if not exist build mkdir build
copy /Y target\release\hydra-umc-swarm-sync.exe build\hydra-umc-swarm-sync.exe >nul

echo Build OK: build\hydra-umc-swarm-sync.exe
endlocal
pause
