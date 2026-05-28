@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem .cmd/release-local.cmd
rem Build Windows release zip assets locally (x86_64 + aarch64).
rem Mirrors the packaging step of .github/workflows/release.yml so a
rem tag push isn't the first place where the assets are produced.
rem
rem Output (repo root): rust-fs-mcp-<target>.zip + .sha256sum

set BIN=rust-fs-mcp
set TARGETS=x86_64-pc-windows-msvc aarch64-pc-windows-msvc

pushd "%~dp0.." || goto :err

for %%T in (%TARGETS%) do (
    echo === %%T ===
    rustup target add %%T >nul 2>&1
    cargo build --release --locked --target %%T || goto :err

    set OUT=%BIN%-%%T
    set ZIP=!OUT!.zip
    set BINPATH=target\%%T\release\%BIN%.exe

    if not exist "!BINPATH!" (
        echo missing binary: !BINPATH!
        goto :err
    )

    if exist "!ZIP!" del /q "!ZIP!"
    powershell -NoProfile -Command "Compress-Archive -Path '!BINPATH!' -DestinationPath '!ZIP!' -Force -CompressionLevel Optimal" || goto :err
    for /f "tokens=*" %%H in ('powershell -NoProfile -Command "(Get-FileHash -Algorithm SHA256 '!ZIP!').Hash.ToLower()"') do set HASH=%%H
    > "!ZIP!.sha256sum" echo !HASH!  !ZIP!
    echo Built: !ZIP! (!HASH!)
)

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
