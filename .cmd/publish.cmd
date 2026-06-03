@echo off
setlocal EnableExtensions EnableDelayedExpansion
rem .cmd/publish.cmd
rem Publish the crate to crates.io.
rem Runs `cargo publish --dry-run --locked` first, then asks for
rem confirmation before the real `cargo publish --locked`.
rem Requires a crates.io token configured via `cargo login`.

pushd "%~dp0.." || goto :err

echo === dry-run ===
cargo publish --dry-run --locked || goto :err

echo.
set CONFIRM=
set /p CONFIRM=Publish to crates.io? [y/N]: 
if /i not "!CONFIRM!"=="y" (
    echo Aborted.
    goto :done
)

echo === publish ===
cargo publish --locked || goto :err
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
