@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0Capture-Crash.ps1" %*
echo.
pause
