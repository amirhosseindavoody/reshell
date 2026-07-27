@echo on
setlocal EnableDelayedExpansion
cd /d "%SRC_DIR%" || exit /b 1

:: ---------------------------------------------------------------------------
:: Git for Windows (and msys/m2) ship usr\bin\link.exe — the unix `link` tool.
:: Cargo's MSVC toolchain looks up bare `link.exe` on PATH; if the unix one
:: wins, you get:  /usr/bin/link: extra operand '...'
:: See: https://github.com/amirhosseindavoody/reshell/issues/29
:: ---------------------------------------------------------------------------

:: 1) Drop Git/msys usr\bin (and similar) from PATH for this build only.
set "NEWPATH="
for %%A in ("%PATH:;=";"%") do (
  set "P=%%~A"
  set "SKIP="
  echo !P! | findstr /I /C:"\Git\usr\bin" /C:"\Git\mingw64\bin" /C:"\Git\mingw32\bin" /C:"\cygwin64\bin" /C:"\cygwin\bin" >nul && set "SKIP=1"
  if not defined SKIP (
    if defined NEWPATH (
      set "NEWPATH=!NEWPATH!;!P!"
    ) else (
      set "NEWPATH=!P!"
    )
  )
)
if defined NEWPATH set "PATH=!NEWPATH!"

:: 2) m2-* packages sometimes install a unix link into the build prefix.
if exist "%BUILD_PREFIX%\Library\usr\bin\link.exe" (
  echo Renaming build-prefix unix link.exe
  ren "%BUILD_PREFIX%\Library\usr\bin\link.exe" link.exe.unix_bak
)

:: 3) Point cargo at MSVC link.exe explicitly when we can find it.
set "MSVC_LINK="
for /f "delims=" %%i in ('where link 2^>nul') do (
  echo %%i | findstr /I /C:"\VC\Tools\MSVC\" /C:"\Microsoft Visual Studio\" >nul
  if not errorlevel 1 (
    if not defined MSVC_LINK set "MSVC_LINK=%%i"
  )
)
if defined MSVC_LINK (
  echo Using MSVC linker: !MSVC_LINK!
  set "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER=!MSVC_LINK!"
  set "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER=!MSVC_LINK!"
) else (
  echo WARNING: MSVC link.exe not on PATH after cleanup.
  echo where link:
  where link 2>nul
  echo If the build fails, install "Visual Studio Build Tools" with the C++ workload,
  echo or ensure the conda vs20xx activation put MSVC on PATH.
)

echo PATH linker candidates:
where link 2>nul

cargo install --locked --no-track --force --path . --root "%PREFIX%\Library" || exit /b 1

if not exist "%PREFIX%\Library\bin\reshell.exe" (
  echo ERROR: reshell.exe missing after cargo install
  dir /s /b "%PREFIX%"
  exit /b 1
)

if exist "%PREFIX%\Library\.crates.toml" del /F /Q "%PREFIX%\Library\.crates.toml"
if exist "%PREFIX%\Library\.crates2.json" del /F /Q "%PREFIX%\Library\.crates2.json"

echo Windows build OK: "%PREFIX%\Library\bin\reshell.exe"
endlocal
exit /b 0
