@echo off
setlocal EnableExtensions EnableDelayedExpansion

set "REL_DIR=%~dp0..\target\release"
set "EXE_NAME=rust-fs-mcp.exe"

rem 타임스탬프 생성 (정렬 가능한 형식)
for /f %%i in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd_HHmmss"') do set "TS=%%i"

rem 기존 exe를 삭제하지 않고 타임스탬프 붙여 백업 (실행 중이어도 rename 가능)
if exist "%REL_DIR%\%EXE_NAME%" ren "%REL_DIR%\%EXE_NAME%" "%EXE_NAME%.%TS%.old" >nul 2>&1

cd /d "%~dp0.."
cargo build --release
pause