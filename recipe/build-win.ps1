# Requires PowerShell 5+ (Windows). Used by recipe.yaml on win-64 / win-arm64.
# Fixes https://github.com/amirhosseindavoody/reshell/issues/29:
# Git Bash ships usr\bin\link.exe (unix `link`), which shadows MSVC's linker.
#
# MSVC is NOT redistributable via conda. Without Visual Studio Build Tools on
# the host, this script exits before cargo so users see a clear winget hint
# instead of `/usr/bin/link: extra operand`. Prefer the prebuilt win-64 package
# from GitHub Releases when you do not want to install Build Tools.

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $false

Set-Location -LiteralPath $env:SRC_DIR

function Test-MsvcLinkPath {
    param([string]$Path)
    if (-not $Path) { return $false }
    if (-not (Test-Path -LiteralPath $Path)) { return $false }
    # Reject unix/msys link masquerading as MSVC.
    $full = (Resolve-Path -LiteralPath $Path).Path
    if ($full -match '(?i)\\(Git|msys|cygwin|usr)\\') { return $false }
    if ($full -notmatch '(?i)\\VC\\Tools\\MSVC\\|\\Microsoft Visual Studio\\') { return $false }
    return $true
}

function Find-MsvcLink {
    $candidates = [System.Collections.Generic.List[string]]::new()

    # 1) Conda / vs20xx activation often sets this.
    if ($env:VCToolsInstallDir) {
        foreach ($rel in @(
                "bin\Hostx64\x64\link.exe",
                "bin\Hostarm64\arm64\link.exe",
                "bin\Hostx86\x86\link.exe"
            )) {
            $candidates.Add((Join-Path $env:VCToolsInstallDir $rel))
        }
    }

    # 2) vswhere (installed with Visual Studio / Build Tools).
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $vswhere) {
        $installPaths = @()
        foreach ($requires in @(
                "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
            )) {
            $found = & $vswhere -latest -products * -requires $requires -property installationPath 2>$null
            if ($found) { $installPaths += $found }
        }
        # Fallback: any VS install, even without the requires filter.
        if (-not $installPaths) {
            $found = & $vswhere -latest -products * -property installationPath 2>$null
            if ($found) { $installPaths += $found }
        }
        foreach ($installPath in ($installPaths | Select-Object -Unique)) {
            $msvcRoot = Join-Path $installPath "VC\Tools\MSVC"
            if (-not (Test-Path -LiteralPath $msvcRoot)) { continue }
            $verDirs = Get-ChildItem -LiteralPath $msvcRoot -Directory |
                Sort-Object Name -Descending
            foreach ($verDir in $verDirs) {
                foreach ($rel in @(
                        "bin\Hostx64\x64\link.exe",
                        "bin\Hostarm64\arm64\link.exe",
                        "bin\Hostx86\x86\link.exe"
                    )) {
                    $candidates.Add((Join-Path $verDir.FullName $rel))
                }
            }
        }
    }

    # 3) Scan well-known Program Files trees (covers non-vswhere installs).
    foreach ($root in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
        if (-not $root) { continue }
        $vsRoot = Join-Path $root "Microsoft Visual Studio"
        if (-not (Test-Path -LiteralPath $vsRoot)) { continue }
        Get-ChildItem -LiteralPath $vsRoot -Directory -ErrorAction SilentlyContinue | ForEach-Object {
            Get-ChildItem -LiteralPath $_.FullName -Directory -ErrorAction SilentlyContinue | ForEach-Object {
                $msvcRoot = Join-Path $_.FullName "VC\Tools\MSVC"
                if (-not (Test-Path -LiteralPath $msvcRoot)) { return }
                Get-ChildItem -LiteralPath $msvcRoot -Directory -ErrorAction SilentlyContinue |
                    Sort-Object Name -Descending |
                    ForEach-Object {
                        foreach ($rel in @(
                                "bin\Hostx64\x64\link.exe",
                                "bin\Hostarm64\arm64\link.exe",
                                "bin\Hostx86\x86\link.exe"
                            )) {
                            $candidates.Add((Join-Path $_.FullName $rel))
                        }
                    }
            }
        }
    }

    # 4) whatever `where link` finds, but only MSVC paths.
    $whereOut = & where.exe link 2>$null
    foreach ($line in $whereOut) {
        if ($line) { $candidates.Add($line.Trim()) }
    }

    foreach ($c in $candidates) {
        if (Test-MsvcLinkPath $c) {
            return (Resolve-Path -LiteralPath $c).Path
        }
    }
    return $null
}

$msvcLink = Find-MsvcLink
if (-not $msvcLink) {
    Write-Host @"

ERROR: MSVC link.exe was not found.

Windows source builds of reshell need the MSVC linker. Git Bash's unix
link.exe is NOT a substitute (that produces: /usr/bin/link: extra operand).

Easiest fix — install a prebuilt package (no Visual Studio required):

  See README "Windows (prebuilt)" or the windows-client GitHub Release:
  https://github.com/amirhosseindavoody/reshell/releases/tag/windows-client

Or install Build Tools, then retry a source build:

  winget install Microsoft.VisualStudio.BuildTools --accept-package-agreements --accept-source-agreements --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

  pixi global install --force-reinstall --git https://github.com/amirhosseindavoody/reshell.git --branch main reshell

Issue: https://github.com/amirhosseindavoody/reshell/issues/29
"@
    exit 1
}

# Drop Git/msys/cygwin AND conda usr\bin (unix tools) from PATH for this process.
# Conda layout puts Library\usr\bin on PATH; that is where m2-* unix link.exe lives.
$env:PATH = (
    $env:PATH -split ';' |
    Where-Object {
        $_ -and
        ($_ -notmatch '(?i)\\Git\\usr\\bin') -and
        ($_ -notmatch '(?i)\\Git\\mingw(64|32)\\bin') -and
        ($_ -notmatch '(?i)\\cygwin(64)?\\bin') -and
        ($_ -notmatch '(?i)\\msys(64)?\\(usr\\)?bin') -and
        ($_ -notmatch '(?i)\\Library\\usr\\bin') -and
        ($_ -notmatch '(?i)\\Library\\mingw-w64\\bin')
    }
) -join ';'

# Rename any unix link.exe still sitting in build/host prefixes.
foreach ($prefix in @($env:BUILD_PREFIX, $env:PREFIX, $env:CONDA_PREFIX)) {
    if (-not $prefix) { continue }
    $unixLink = Join-Path $prefix "Library\usr\bin\link.exe"
    if (Test-Path -LiteralPath $unixLink) {
        Write-Host "Renaming unix link.exe -> link.exe.unix_bak ($unixLink)"
        Rename-Item -LiteralPath $unixLink -NewName "link.exe.unix_bak" -Force
    }
}

# Force cargo to the real MSVC linker (do not rely on PATH order alone).
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $msvcLink
$env:CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER = $msvcLink
$msvcBin = Split-Path -Parent $msvcLink
$env:PATH = "$msvcBin;$env:PATH"

Write-Host "Using MSVC linker: $msvcLink"
Write-Host "Linker candidates on PATH:"
& where.exe link

cargo install --locked --no-track --force --path . --root "$env:PREFIX\Library"
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$exe = Join-Path $env:PREFIX "Library\bin\reshell.exe"
if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host "ERROR: reshell.exe missing after cargo install"
    Get-ChildItem -LiteralPath $env:PREFIX -Recurse -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
    exit 1
}

Remove-Item -LiteralPath (Join-Path $env:PREFIX "Library\.crates.toml") -ErrorAction SilentlyContinue
Remove-Item -LiteralPath (Join-Path $env:PREFIX "Library\.crates2.json") -ErrorAction SilentlyContinue

Write-Host "Windows build OK: $exe"
exit 0
