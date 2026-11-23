@echo off
REM Thin PATH entry for project-terminal script runs (see kiri-run-cmd.ps1).
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0kiri-run-cmd.ps1" %*
exit /b %ERRORLEVEL%
