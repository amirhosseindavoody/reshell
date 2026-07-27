# Requires PowerShell 5+ (Windows). Used by recipe.yaml on win-64 / win-arm64.
# Fixes https://github.com/amirhosseindavoody/reshell/issues/29:
# Git Bash ships usr\bin\link.exe (unix `link`), which shadows MSVC's linker.

$ErrorActionPreference = "Stop"
$PSNativeCommandUseErrorActionPreference = $true

Set-Location -LiteralPath $env:SRC_DIR

function Find-MsvcLink {
    # 1) Conda / vs20xx activation often sets this.
    if ($env:VCToolsInstallDir) {
        foreach ($rel in @(
                "bin\Hostx64\x64\link.exe",
                "bin\Hostarm64\arm64\link.exe",
                "bin\Hostx86\x86\link.exe"
            )) {
            $candidate = Join-Path $env:VCToolsInstallDir $rel
            if (Test-Path -LiteralPath $candidate) {
                return (Resolve-Path -LiteralPath $candidate).Path
            }
        }
    }

    # 2) vswhere (installed with Visual Studio / Build Tools).
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path -LiteralPath $vswhere) {
        $installPath = & $vswhere -latest -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath 2>$null
        if (-not $installPath) {
            $installPath = & $vswhere -latest -products * `
                -requires Microsoft.VisualStudio.Component.VC.Tools.ARM64 `
                -property installationPath 2>$null
        }
        if ($installPath) {
            $msvcRoot = Join-Path $installPath "VC\Tools\MSVC"
            if (Test-Path -LiteralPath $msvcRoot) {
                $verDir = Get-ChildItem -LiteralPath $msvcRoot -Directory |
                    Sort-Object Name -Descending |
                    Select-Object -First 1
                if ($verDir) {
                    foreach ($rel in @(
                            "bin\Hostx64\x64\link.exe",
                            "bin\Hostarm64\arm64\link.exe",
                            "bin\Hostx86\x86\link.exe"
                        )) {
                        $candidate = Join-Path $verDir.FullName $rel
                        if (Test-Path -LiteralPath $candidate) {
                            return (Resolve-Path -LiteralPath $candidate).Path
                        }
                    }
                }
            }
        }
    }

    # 3) whatever `where link` finds, but only MSVC paths.
    $whereOut = & where.exe link 2>$null
    foreach ($line in $whereOut) {
        if ($line -match '\\VC\\Tools\\MSVC\\|\\Microsoft Visual Studio\\') {
            return $line.Trim()
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

Install Build Tools, then retry:

  winget install Microsoft.VisualStudio.BuildTools --accept-package-agreements --accept-source-agreements --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

  pixi global install --force-reinstall --git https://github.com/amirhosseindavoody/reshell.git --branch main reshell

Issue: https://github.com/amirhosseindavoody/reshell/issues/29
"@
    exit 1
}

# Drop Git/msys unix link directories from PATH for this process.
$env:PATH = (
    $env:PATH -split ';' |
    Where-Object {
        $_ -and
        ($_ -notmatch '(?i)\\Git\\usr\\bin$') -and
        ($_ -notmatch '(?i)\\Git\\mingw(64|32)\\bin$') -and
        ($_ -notmatch '(?i)\\cygwin(64)?\\bin$')
    }
) -join ';'

# m2-* packages may install a unix link into the build prefix.
$unixLink = Join-Path $env:BUILD_PREFIX "Library\usr\bin\link.exe"
if (Test-Path -LiteralPath $unixLink) {
    Write-Host "Renaming build-prefix unix link.exe -> link.exe.unix_bak"
    Rename-Item -LiteralPath $unixLink -NewName "link.exe.unix_bak" -Force
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
