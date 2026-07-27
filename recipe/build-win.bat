@echo on
setlocal EnableDelayedExpansion
:: Wrapper so rattler-build uses cmd.exe (always present). Host PowerShell
:: runs build-win.ps1 (MSVC discovery / Git link scrub / hard-fail).
:: See https://github.com/amirhosseindavoody/reshell/issues/29

cd /d "%SRC_DIR%" || exit /b 1

set "PS1=%RECIPE_DIR%\build-win.ps1"
if not exist "%PS1%" (
  echo ERROR: missing %PS1%
  exit /b 1
)

where powershell >nul 2>&1
if errorlevel 1 (
  where pwsh >nul 2>&1
  if errorlevel 1 (
    echo ERROR: neither powershell.exe nor pwsh.exe found on PATH
    exit /b 1
  )
  pwsh -NoProfile -ExecutionPolicy Bypass -File "%PS1%"
  exit /b %ERRORLEVEL%
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%"
exit /b %ERRORLEVEL%
