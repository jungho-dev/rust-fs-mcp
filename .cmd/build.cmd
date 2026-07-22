@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem .cmd/build.cmd
rem Zero-downtime release deploy to <target>\release\rust-fs-mcp.exe.
rem A running exe cannot be deleted or overwritten on Windows, but it can be renamed:
rem the current binary moves aside as rust-fs-mcp.exe.stale-<n>, live servers keep
rem executing the renamed image, and cargo links a fresh binary onto the original path.
rem Stale images are best-effort deleted on the next run once their processes exit.

pushd "%~dp0.." || goto :err

where cargo >nul 2>&1 || (
    echo cargo was not found in PATH.
    goto :err
)

rem Cargo resolves the target dir as CARGO_TARGET_DIR > config build.target-dir > .\target.
rem This project pins neither build.target-dir nor build.target (.cargo/config.toml), so
rem this replaces the powershell + cargo metadata spawn that resolved the same answer.
if defined CARGO_TARGET_DIR (set "TARGET_DIR=%CARGO_TARGET_DIR%") else set "TARGET_DIR=%CD%\target"
set "RELEASE_DIR=%TARGET_DIR%\release"
set "EXE_PATH=%RELEASE_DIR%\rust-fs-mcp.exe"
echo Release binary: "%EXE_PATH%"

rem Clear stale images from earlier deploys; ones still executing fail silently and stay.
del /f /q "%RELEASE_DIR%\rust-fs-mcp.exe.stale-*" >nul 2>&1

rem Move the current binary aside so the link step never hits a file lock. This also
rem removes the link output, so cargo always relinks and EXE_PATH is fresh on success.
if exist "%EXE_PATH%" (
    ren "%EXE_PATH%" "rust-fs-mcp.exe.stale-%RANDOM%%RANDOM%" || (
        echo Failed to move the current binary aside: "%EXE_PATH%"
        goto :err
    )
)

echo === cargo build --release ===
cargo build --release
if errorlevel 1 goto :err

rem Safety net for a future pinned build.target: pull a <triple>\release artifact up.
rem At most one triple directory exists in practice; the last match wins.
if not exist "%EXE_PATH%" (
    if not exist "%RELEASE_DIR%" mkdir "%RELEASE_DIR%"
    for /d %%D in ("%TARGET_DIR%\*") do (
        if exist "%%D\release\rust-fs-mcp.exe" copy /y "%%D\release\rust-fs-mcp.exe" "%EXE_PATH%" >nul
    )
)

if not exist "%EXE_PATH%" (
    echo Build completed without the expected binary: "%EXE_PATH%"
    goto :err
)

for %%I in ("%EXE_PATH%") do echo Built: "%%~fI" ^(%%~zI bytes^)

popd
echo Done.
pause
endlocal
exit /b 0

:err
echo FAILED
popd
pause
endlocal
exit /b 1
