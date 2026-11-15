@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem .cmd/fp-report.cmd
rem Collect the antivirus false-positive evidence bundle for the release binary.
rem Writes fp-report\fp-report.md under the cargo target folder plus a copy of the exe to attach.
rem Arguments pass through to fp-report.ps1, such as -ExePath with the detected file.

set "PS_EXE=powershell"
where pwsh >nul 2>&1 && set "PS_EXE=pwsh"

"%PS_EXE%" -NoProfile -File "%~dp0fp-report.ps1" %*
if errorlevel 1 goto :err

echo Done.
pause
endlocal
exit /b 0

:err
echo FAILED
echo If PowerShell reported that running scripts is disabled, allow local scripts for this user:
echo   Set-ExecutionPolicy -Scope CurrentUser RemoteSigned
pause
endlocal
exit /b 1
