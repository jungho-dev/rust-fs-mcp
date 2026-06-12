@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem .cmd/build.cmd
rem Force-build the release binary after stopping only processes that use
rem the exact Cargo release executable resolved from cargo metadata.

pushd "%~dp0.." || goto :err

where cargo >nul 2>&1 || (
    echo cargo was not found in PATH.
    goto :err
)

set "TARGET_DIR="
for /f "usebackq delims=" %%I in (`powershell -NoProfile -Command "$json = & cargo metadata --format-version 1 --no-deps; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; ($json | ConvertFrom-Json).target_directory"`) do set "TARGET_DIR=%%I"

if not defined TARGET_DIR (
    echo Failed to resolve Cargo target directory.
    goto :err
)

set "EXE_PATH=%TARGET_DIR%\release\rust-fs-mcp.exe"
echo Release binary: "%EXE_PATH%"

powershell -NoProfile -Command ^
    "$target = [IO.Path]::GetFullPath($env:EXE_PATH);" ^
    "$locked = Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and [IO.Path]::GetFullPath($_.ExecutablePath) -eq $target };" ^
    "foreach ($process in $locked) {" ^
    "    Write-Host ('Stopping PID {0}: {1}' -f $process.ProcessId, $process.ExecutablePath);" ^
    "    Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop;" ^
    "};" ^
    "if (Test-Path -LiteralPath $target) {" ^
    "    $removed = $false;" ^
    "    foreach ($attempt in 1..20) {" ^
    "        try {" ^
    "            Remove-Item -LiteralPath $target -Force -ErrorAction Stop;" ^
    "            $removed = $true;" ^
    "            break;" ^
    "        }" ^
    "        catch {" ^
    "            Start-Sleep -Milliseconds 250;" ^
    "        }" ^
    "    };" ^
    "    if (-not $removed) {" ^
    "        Write-Error ('Failed to remove locked release binary: {0}' -f $target);" ^
    "        exit 1;" ^
    "    }" ^
    "}"

if errorlevel 1 goto :err

echo === cargo build --release ===
cargo build --release
if errorlevel 1 goto :err

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
