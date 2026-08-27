@echo off
REM =============================================================================
REM HYDRA-UMC-SWARM-SYNC - run.bat
REM Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
REM GPL-3.0 - see LICENSE
REM =============================================================================
REM HYDRA-UMC-SWARM-SYNC - run.bat
REM Runs the already-built release binary. Run build.bat first.
setlocal
cd /d "%~dp0"

if exist build\hydra-umc-swarm-sync.exe (
    build\hydra-umc-swarm-sync.exe %*
) else if exist target\release\hydra-umc-swarm-sync.exe (
    target\release\hydra-umc-swarm-sync.exe %*
) else (
    echo No compiled binary found. Run build.bat first.
    pause
    exit /b 1
)
endlocal
pause
