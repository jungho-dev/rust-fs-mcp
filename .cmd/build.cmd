@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem .cmd/build.cmd
rem Build the release binary, then publish it to <target>\release\rust-fs-mcp.exe
rem even when .cargo/config.toml builds under a target-triple subdirectory.

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

rem Stop any rust-fs-mcp.exe running from this target tree so cargo can relink
rem and the publish copy below is not blocked by a file lock, then clear the
rem previous published binary if it is still present.
powershell -NoProfile -Command ^
    "$root = [IO.Path]::GetFullPath($env:TARGET_DIR);" ^
    "$running = Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and ([IO.Path]::GetFullPath($_.ExecutablePath)).StartsWith($root, [StringComparison]::OrdinalIgnoreCase) -and [IO.Path]::GetFileName($_.ExecutablePath) -eq 'rust-fs-mcp.exe' };" ^
    "foreach ($process in $running) {" ^
    "    Write-Host ('Stopping PID {0}: {1}' -f $process.ProcessId, $process.ExecutablePath);" ^
    "    Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop;" ^
    "};" ^
    "$dest = [IO.Path]::Combine($root, 'release', 'rust-fs-mcp.exe');" ^
    "if (Test-Path -LiteralPath $dest) {" ^
    "    $removed = $false;" ^
    "    foreach ($attempt in 1..20) {" ^
    "        try {" ^
    "            Remove-Item -LiteralPath $dest -Force -ErrorAction Stop;" ^
    "            $removed = $true;" ^
    "            break;" ^
    "        }" ^
    "        catch {" ^
    "            Start-Sleep -Milliseconds 250;" ^
    "        }" ^
    "    };" ^
    "    if (-not $removed) {" ^
    "        Write-Error ('Failed to remove locked release binary: {0}' -f $dest);" ^
    "        exit 1;" ^
    "    }" ^
    "}"

if errorlevel 1 goto :err

echo === cargo build --release ===
cargo build --release
if errorlevel 1 goto :err

rem Cargo writes the artifact under <target>\<triple>\release when .cargo/config
rem pins a build target. Publish the freshest rust-fs-mcp.exe up to
rem <target>\release so the binary always lands directly there.
powershell -NoProfile -Command ^
    "$root = [IO.Path]::GetFullPath($env:TARGET_DIR);" ^
    "$dest = [IO.Path]::Combine($root, 'release', 'rust-fs-mcp.exe');" ^
    "$src = Get-ChildItem -LiteralPath $root -Recurse -Filter 'rust-fs-mcp.exe' -File -ErrorAction SilentlyContinue |" ^
    "    Where-Object { $_.DirectoryName -match '\\release$' -and [IO.Path]::GetFullPath($_.FullName) -ne $dest } |" ^
    "    Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1;" ^
    "if ($null -eq $src) {" ^
    "    if (Test-Path -LiteralPath $dest) { exit 0; };" ^
    "    Write-Error 'Build artifact rust-fs-mcp.exe was not found.';" ^
    "    exit 1;" ^
    "};" ^
    "$destDir = Split-Path -Parent $dest;" ^
    "if (-not (Test-Path -LiteralPath $destDir)) {" ^
    "    New-Item -ItemType Directory -Path $destDir -Force | Out-Null;" ^
    "};" ^
    "Copy-Item -LiteralPath $src.FullName -Destination $dest -Force;" ^
    "Write-Host ('Published {0} -> {1}' -f $src.FullName, $dest);"

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
