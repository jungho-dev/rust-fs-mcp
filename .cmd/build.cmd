```bat
@echo off
setlocal EnableExtensions DisableDelayedExpansion

rem .cmd\build.cmd
rem Build rust-fs-mcp and publish it to:
rem   <target>\release\rust-fs-mcp.exe
rem
rem The actual Cargo artifact path is read from Cargo JSON output, so this also
rem works when .cargo\config.toml pins a target triple.

set "APP_NAME=rust-fs-mcp"
set "EXE_NAME=%APP_NAME%.exe"
set "PROJECT_DIR=%~dp0.."

set "TARGET_DIR="
set "SOURCE_EXE="
set "PUBLISHED_EXE="
set "PROJECT_PUSHED="

call :main
set "EXIT_CODE=%ERRORLEVEL%"

endlocal & exit /b %EXIT_CODE%


:main
pushd "%PROJECT_DIR%" || (
    echo Failed to enter project directory: "%PROJECT_DIR%"
    exit /b 1
)
set "PROJECT_PUSHED=1"

call :require_command cargo || goto :failed
call :resolve_target_dir || goto :failed

set "PUBLISHED_EXE=%TARGET_DIR%\release\%EXE_NAME%"

echo Target directory : "%TARGET_DIR%"
echo Published binary: "%PUBLISHED_EXE%"

call :unlock_target_tree || goto :failed
call :build_release || goto :failed
call :publish_binary || goto :failed
call :verify_binary || goto :failed

echo Done.
set "RESULT=0"
goto :cleanup


:failed
echo FAILED
set "RESULT=1"


:cleanup
if defined PROJECT_PUSHED popd
pause
exit /b %RESULT%


:require_command
where "%~1" >nul 2>&1 || (
    echo %~1 was not found in PATH.
    exit /b 1
)

exit /b 0


:resolve_target_dir
set "TARGET_DIR="

for /f "usebackq delims=" %%I in (`powershell -NoProfile -Command ^
  "$ErrorActionPreference = 'Stop';" ^
  "$metadata = & cargo metadata --format-version 1 --no-deps;" ^
  "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE };" ^
  "[Console]::Out.WriteLine(($metadata | ConvertFrom-Json).target_directory)"`) do (
  set "TARGET_DIR=%%I"
)

if not defined TARGET_DIR (
  echo Failed to resolve Cargo target directory.
  exit /b 1
)

exit /b 0


:unlock_target_tree
rem Stop only rust-fs-mcp.exe processes running under this project's target tree.
rem Then remove the previous published executable, retrying briefly for delayed
rem Windows file-lock release.

powershell -NoProfile -Command ^
  "$ErrorActionPreference = 'Stop';" ^
  "$root = [IO.Path]::GetFullPath($env:TARGET_DIR).TrimEnd([IO.Path]::DirectorySeparatorChar);" ^
  "$prefix = $root + [IO.Path]::DirectorySeparatorChar;" ^
  "$exeName = $env:EXE_NAME;" ^
  "$running = Get-CimInstance Win32_Process | Where-Object {" ^
  "    $path = $_.ExecutablePath;" ^
  "    if (-not $path) { return $false };" ^
  "    try {" ^
  "        $fullPath = [IO.Path]::GetFullPath($path);" ^
  "        $fullPath.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -and" ^
  "        [string]::Equals([IO.Path]::GetFileName($fullPath), $exeName, [StringComparison]::OrdinalIgnoreCase)" ^
  "    } catch {" ^
  "        $false" ^
  "    }" ^
  "};" ^
  "foreach ($process in $running) {" ^
  "    Write-Host ('Stopping PID {0}: {1}' -f $process.ProcessId, $process.ExecutablePath);" ^
  "    Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop;" ^
  "};" ^
  "$published = Join-Path $root ('release\' + $exeName);" ^
  "if (Test-Path -LiteralPath $published) {" ^
  "    for ($attempt = 1; $attempt -le 20; $attempt++) {" ^
  "        try {" ^
  "            Remove-Item -LiteralPath $published -Force -ErrorAction Stop;" ^
  "            break;" ^
  "        } catch {" ^
  "            if ($attempt -eq 20) { throw };" ^
  "            Start-Sleep -Milliseconds 250;" ^
  "        }" ^
  "    }" ^
  "}"

if errorlevel 1 exit /b 1

exit /b 0


:build_release
set "SOURCE_EXE="
set "ARTIFACT_FILE=%TEMP%\%APP_NAME%-artifact-%RANDOM%-%RANDOM%.txt"

echo === cargo build --release ===

rem Capture only the executable path on stdout.
rem Compiler diagnostics remain visible through stderr.
powershell -NoProfile -Command ^
  "$ErrorActionPreference = 'Stop';" ^
  "$artifact = $null;" ^
  "& cargo build --release --message-format=json | ForEach-Object {" ^
  "    try {" ^
  "        $message = $_ | ConvertFrom-Json -ErrorAction Stop;" ^
  "    } catch {" ^
  "        return" ^
  "    };" ^
  "    if ($message.reason -eq 'compiler-message' -and $message.message.rendered) {" ^
  "        [Console]::Error.Write($message.message.rendered)" ^
  "    };" ^
  "    if (" ^
  "        $message.reason -eq 'compiler-artifact' -and" ^
  "        $message.target.name -eq $env:APP_NAME -and" ^
  "        $message.target.kind -contains 'bin' -and" ^
  "        $message.executable" ^
  "    ) {" ^
  "        $artifact = $message.executable" ^
  "    }" ^
  "};" ^
  "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE };" ^
  "if (-not $artifact -or -not (Test-Path -LiteralPath $artifact -PathType Leaf)) {" ^
  "    throw ('Cargo did not produce ' + $env:EXE_NAME)" ^
  "};" ^
  "[Console]::Out.WriteLine($artifact)" ^
  1>"%ARTIFACT_FILE%"

set "BUILD_EXIT=%ERRORLEVEL%"

if not "%BUILD_EXIT%"=="0" (
    if exist "%ARTIFACT_FILE%" del /q "%ARTIFACT_FILE%" >nul 2>&1
    exit /b %BUILD_EXIT%
)

set /p "SOURCE_EXE="<"%ARTIFACT_FILE%"
del /q "%ARTIFACT_FILE%" >nul 2>&1

if not defined SOURCE_EXE (
    echo Cargo completed without reporting "%EXE_NAME%".
    exit /b 1
)

if not exist "%SOURCE_EXE%" (
    echo Cargo reported a missing artifact: "%SOURCE_EXE%"
    exit /b 1
)

echo Build artifact   : "%SOURCE_EXE%"
exit /b 0


:publish_binary
rem When Cargo already built directly to <target>\release, no copy is needed.
if /i "%SOURCE_EXE%"=="%PUBLISHED_EXE%" (
    echo Publish skipped: artifact is already at the publish path.
    exit /b 0
)

powershell -NoProfile -Command ^
  "$ErrorActionPreference = 'Stop';" ^
  "$source = [IO.Path]::GetFullPath($env:SOURCE_EXE);" ^
  "$destination = [IO.Path]::GetFullPath($env:PUBLISHED_EXE);" ^
  "if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {" ^
  "    throw ('Build artifact was not found: ' + $source)" ^
  "};" ^
  "$destinationDir = Split-Path -Parent $destination;" ^
  "New-Item -ItemType Directory -Path $destinationDir -Force | Out-Null;" ^
  "Copy-Item -LiteralPath $source -Destination $destination -Force -ErrorAction Stop;" ^
  "Write-Host ('Published {0} -> {1}' -f $source, $destination)"

if errorlevel 1 exit /b 1

exit /b 0


:verify_binary
if not exist "%PUBLISHED_EXE%" (
  echo Build completed without the expected binary: "%PUBLISHED_EXE%"
  exit /b 1
)

for %%I in ("%PUBLISHED_EXE%") do (
  echo Built: "%%~fI" ^(%%~zI bytes^)
)

exit /b 0
```
