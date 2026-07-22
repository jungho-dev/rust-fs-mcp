@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem .cmd/publish.cmd
rem Publish the crate to crates.io.
rem Runs `cargo publish --dry-run --locked` first, then asks for confirmation
rem before the real publish. The real publish skips the verify build with
rem --no-verify because the dry-run just verified the identical package.
rem Requires a crates.io token configured via `cargo login`.

pushd "%~dp0.." || goto :err

echo === dry-run ===
cargo publish --dry-run --locked --allow-dirty || goto :err

echo.
set CONFIRM=
set /p CONFIRM=Publish to crates.io? [y/N]: 
if /i not "!CONFIRM!"=="y" (
    echo Aborted.
    goto :done
)

echo === publish ===
cargo publish --locked --allow-dirty --no-verify || goto :err
echo Done.

:done
popd
pause
endlocal
exit /b 0

:err
echo FAILED
popd
pause
endlocal
exit /b 1
