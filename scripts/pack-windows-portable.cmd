@echo off
setlocal
cd /d "%~dp0\.."
title Kiri — pack Windows portable zip
echo.
echo Packing Windows portable zip (reuses target\kiri-windows-x86_64)...
echo Repo: %CD%
echo.

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0pack-windows-portable.ps1" %*
set ERR=%ERRORLEVEL%
echo.
if not "%ERR%"=="0" (
  echo FAILED with exit code %ERR%.
  pause
  exit /b %ERR%
)
echo Done. Press any key to close.
pause >nul
exit /b 0
