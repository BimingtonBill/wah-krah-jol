# Sets up the walkable Alftand -> Blackreach demo on a fresh machine:
# checks the toolchain, builds the release converter and engine, converts your own Skyrim SE
# install into a converted asset folder, and writes a launcher next to it.
#
#   pwsh -File tools/demo/setup-demo.ps1 -SkyrimPath "<SkyrimSE>" -OutDir "<converted>"
#
# Nothing is downloaded and nothing is installed: the script prints the install commands for
# anything it is missing. Bethesda data is never copied into the repository - the converter reads
# your install read-only and writes only into -OutDir.
#
# See docs/demo/README.md for the long form.

[CmdletBinding()]
param(
    # Your Skyrim SE install root, or its Data folder (absolute, or relative to the repository).
    # Omit to let the script look for the install itself (Steam registry keys and library folders);
    # it asks when it cannot find one.
    [string]$SkyrimPath,
    # Where the converted assets go. Must not be the Skyrim Data folder, and should live outside
    # this repository (a converted set is tens of GB of Bethesda-derived data and is never committed).
    [string]$OutDir,
    # Worker counts for the conversion. 0 keeps the converter's own defaults
    # (cpu-jobs = all cores, io-jobs = 2).
    [int]$CpuJobs = 0,
    [int]$IoJobs = 0,
    # Skip "cargo build --release" for the converter and the engine.
    [switch]$SkipBuild,
    # Skip the conversion (leaves an existing -OutDir alone) and only write the launcher.
    [switch]$SkipConvert,
    # Print every command instead of running it. Reads nothing but the filesystem.
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repository = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$windowsVariable = Get-Variable -Name IsWindows -ErrorAction SilentlyContinue
$onWindows = if ($windowsVariable) { [bool]$windowsVariable.Value } else { $true }

function Write-Heading([string]$Text) {
    Write-Host ""
    Write-Host "== $Text" -ForegroundColor Cyan
}

function Write-Note([string]$Text) {
    Write-Host "   $Text" -ForegroundColor Gray
}

function Write-Caution([string]$Text) {
    Write-Host "   $Text" -ForegroundColor Yellow
}

# Relative paths on the command line are read as relative to the repository, so the documented
# invocation works whatever directory the shell happens to be in.
function Resolve-RepoPath([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    return [IO.Path]::GetFullPath((Join-Path $repository $Path))
}

function Format-Command([string]$FilePath, [string[]]$Arguments) {
    $quoted = foreach ($argument in $Arguments) {
        if ($argument -match '[\s"]') { '"' + $argument.Replace('"', '""') + '"' } else { $argument }
    }
    (@($FilePath) + $quoted) -join " "
}

# Runs a command and returns its exit code. In -DryRun it prints the command and does not run it.
function Invoke-SetupStep {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$What
    )
    $display = Format-Command $FilePath $Arguments
    if ($DryRun) {
        Write-Host "[dry-run] $What"
        Write-Host "[dry-run] $display"
        return 0
    }
    Write-Host "+ $display"
    # Out-Host keeps the command's own output out of this function's return value.
    & $FilePath @Arguments | Out-Host
    $exitCode = $LASTEXITCODE
    if ($null -eq $exitCode) { $exitCode = 0 }
    return $exitCode
}

function Test-CommandExists([string]$Name) {
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

# ---------------------------------------------------------------- prerequisites

# The MSVC C++ toolchain is needed twice over: rustc links the x86_64-pc-windows-msvc target against
# the Windows SDK, and `cc` build scripts compile C++ (basis_universal, bundled SQLite, and the
# converter's own KTX2 bridge). .cargo/config.toml sets rust-lld.exe as the linker for that target,
# but the MSVC toolchain and SDK are still where the libraries come from.
function Test-MsvcToolchain {
    if (Test-CommandExists "link.exe") {
        return "link.exe is on PATH"
    }
    if (${env:ProgramFiles(x86)}) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
        if (Test-Path -LiteralPath $vswhere) {
            $root = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
            if ($root) { return "Visual Studio C++ tools at $root" }
        }
    }
    if (Test-CommandExists "rustup") {
        $targets = & rustup target list --installed 2>$null
        if ($targets -match "x86_64-pc-windows-msvc") {
            # The target being installed means rustup was set up for MSVC; it does not prove the
            # C++ tools are there, and a build still fails without them. A weak pass.
            return "the x86_64-pc-windows-msvc target is installed (C++ tools unverified)"
        }
    }
    return $null
}

function Test-CxxCompiler {
    foreach ($name in @("cc", "gcc", "clang", "cl.exe")) {
        if (Test-CommandExists $name) { return $name }
    }
    return $null
}

function Assert-Prerequisites {
    Write-Heading "Prerequisites"
    if ($DryRun) {
        # A dry run does not start anything, not even a version probe.
        Write-Host "[dry-run] where.exe cargo; cargo --version"
        if ($onWindows) {
            Write-Host "[dry-run] where.exe link.exe (else vswhere -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64, else rustup target list --installed)"
        } else {
            Write-Host "[dry-run] where.exe cc/gcc/clang"
        }
        Write-Host "[dry-run] (missing tools would be reported here with their install commands)"
        return
    }
    $missing = @()

    if (Test-CommandExists "cargo") {
        $version = (& cargo --version) 2>$null
        Write-Note $(if ($version) { "cargo: $version" } else { "cargo: found" })
    } else {
        $missing += @(
            "cargo (Rust stable) was not found on PATH.",
            "  Install it:   winget install Rustlang.Rustup",
            "  Then:         rustup default stable-msvc",
            "  Or see:       https://rustup.rs"
        )
    }

    if ($onWindows) {
        $msvc = Test-MsvcToolchain
        if ($msvc) {
            Write-Note "MSVC C++ toolchain: $msvc"
        } else {
            $missing += @(
                "The Visual Studio C++ build tools (link.exe) were not found.",
                "  Both the engine and the converter need them: rustc links the MSVC target against",
                "  the Windows SDK, and the build compiles C++ for basis_universal, bundled SQLite",
                "  and the converter's KTX2 bridge.",
                "  Install:      winget install Microsoft.VisualStudio.2022.BuildTools --override `"--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended`"",
                "  Or install `"Visual Studio 2022 Build Tools`" with the `"Desktop development with C++`" workload.",
                "  link.exe only shows up on PATH inside a Developer Command Prompt; rustc finds the",
                "  toolchain by itself, so that alone is not a problem."
            )
        }
    } else {
        $cxx = Test-CxxCompiler
        if ($cxx) {
            Write-Note "C/C++ compiler: $cxx"
        } else {
            $missing += @(
                "No C/C++ compiler (cc/gcc/clang) was found on PATH.",
                "  Install one, for example:  sudo apt install build-essential",
                "  The build compiles C++ for basis_universal, bundled SQLite and the converter bridge."
            )
        }
    }

    if (-not $missing) {
        Write-Note "all present."
        return
    }

    Write-Host ""
    foreach ($line in $missing) { Write-Caution $line }
    throw "Install the tools above and run this script again. Nothing was installed for you."
}

# ---------------------------------------------------------------- Skyrim detection

function Test-SkyrimDataDir([string]$Path) {
    if (-not $Path -or -not (Test-Path -LiteralPath $Path -PathType Container)) { return $false }
    foreach ($entry in (Get-ChildItem -LiteralPath $Path -Filter "*.esm" -File -ErrorAction SilentlyContinue)) {
        if ($entry.Name -ieq "Skyrim.esm") { return $true }
    }
    return $false
}

# Accepts a Skyrim install root, its Data folder, or a Steam root/library, and returns the Data
# folder holding Skyrim.esm - or $null.
function Resolve-SkyrimDataDir([string]$Path) {
    if (-not $Path) { return $null }
    foreach ($candidate in @(
            $Path,
            (Join-Path $Path "Data"),
            (Join-Path $Path "steamapps/common/Skyrim Special Edition/Data"),
            (Join-Path $Path "common/Skyrim Special Edition/Data")
        )) {
        if (Test-SkyrimDataDir $candidate) { return (Resolve-Path -LiteralPath $candidate).Path }
    }
    return $null
}

function Get-SteamLibraries {
    $roots = [System.Collections.Generic.List[string]]::new()
    foreach ($entry in @(
            @{ Hive = "HKCU"; Key = "Software\Valve\Steam"; Value = "SteamPath" },
            @{ Hive = "HKLM"; Key = "SOFTWARE\Valve\Steam"; Value = "InstallPath" },
            @{ Hive = "HKLM"; Key = "SOFTWARE\WOW6432Node\Valve\Steam"; Value = "InstallPath" }
        )) {
        try {
            $value = (Get-ItemProperty -Path "$($entry.Hive):\$($entry.Key)" -Name $entry.Value -ErrorAction Stop).($entry.Value)
            if ($value) { $roots.Add([string]$value) }
        } catch { }
    }
    if (-not $onWindows -and $env:HOME) {
        foreach ($relative in @(".local/share/Steam", ".steam/steam", ".steam/root", "Library/Application Support/Steam")) {
            $candidate = Join-Path $env:HOME $relative
            if (Test-Path -LiteralPath $candidate) { $roots.Add($candidate) }
        }
    }

    $libraries = [System.Collections.Generic.List[string]]::new()
    foreach ($root in $roots) {
        if (-not $libraries.Contains($root)) { $libraries.Add($root) }
        $vdf = Join-Path $root "steamapps/libraryfolders.vdf"
        if (-not (Test-Path -LiteralPath $vdf)) { continue }
        $text = Get-Content -LiteralPath $vdf -Raw
        foreach ($match in [regex]::Matches($text, '"path"\s+"([^"]+)"')) {
            $library = $match.Groups[1].Value -replace '\\\\', '\'
            if (-not $libraries.Contains($library)) { $libraries.Add($library) }
        }
    }
    return $libraries
}

function Find-SkyrimDataDir {
    if ($SkyrimPath) {
        $data = Resolve-SkyrimDataDir (Resolve-RepoPath $SkyrimPath)
        if ($data) { return $data }
        Write-Caution "-SkyrimPath '$SkyrimPath' does not look like a Skyrim SE install (no Skyrim.esm in it or in its Data folder)."
    }

    Write-Note "looking for Skyrim SE (Steam app 489830)..."
    foreach ($library in (Get-SteamLibraries)) {
        $steamapps = Join-Path $library "steamapps"
        $installDir = "Skyrim Special Edition"
        $manifest = Join-Path $steamapps "appmanifest_489830.acf"
        if (Test-Path -LiteralPath $manifest) {
            $match = [regex]::Match((Get-Content -LiteralPath $manifest -Raw), '"installdir"\s+"([^"]+)"')
            if ($match.Success) { $installDir = $match.Groups[1].Value }
        }
        $data = Resolve-SkyrimDataDir (Join-Path $steamapps "common\$installDir")
        if ($data) {
            Write-Note "found $data"
            return $data
        }
    }

    if ($DryRun) {
        Write-Caution "no Skyrim SE install found; a real run would now ask for -SkyrimPath."
        return $null
    }
    if ([Environment]::UserInteractive) {
        $answer = Read-Host "Path to your Skyrim SE install (or its Data folder)"
        $data = Resolve-SkyrimDataDir (Resolve-RepoPath $answer)
        if ($data) { return $data }
        throw "'$answer' does not look like a Skyrim SE install: no Skyrim.esm in it or in its Data folder."
    }
    throw @"
Skyrim SE was not found. Point the script at your own install:

    pwsh -File tools/demo/setup-demo.ps1 -SkyrimPath "<SkyrimSE>" -OutDir "<converted>"

<SkyrimSE> is the folder holding Data\Skyrim.esm, usually
<Steam>\steamapps\common\Skyrim Special Edition. Skyrim Special Edition (2016, with the
Dawnguard, HearthFires and Dragonborn plugins) is what the converter expects; the 2011
original release is not supported.
"@
}

# ---------------------------------------------------------------- conversion

# The converter writes into <output>.staging-<pid>-<nanos> next to the output and renames that
# folder into place at the end, so an interrupted run leaves its staging folder behind. Passing it
# back with --resume-staging continues that run instead of converting everything again.
function Find-ResumeStaging([string]$OutputDir) {
    $parent = Split-Path -Parent $OutputDir
    if (-not $parent) { $parent = "." }
    $name = Split-Path -Leaf $OutputDir
    $staging = Get-ChildItem -LiteralPath $parent -Directory -Filter "$name.staging-*" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($staging) { return $staging.FullName }
    return $null
}

function Write-Launcher([string]$OutputDir) {
    $launcher = Join-Path $OutputDir "play-demo.cmd"
    $playScript = Join-Path $repository "tools/demo/play-demo.ps1"
    $content = @"
@echo off
rem Generated by tools/demo/setup-demo.ps1 - starts the walkable Alftand -> Blackreach demo.
rem Starts on foot outside the Alftand entrance; four load doors (E) lead down to Blackreach.
rem The shortcut straight into Blackreach: change "-Start alftand" to "-Start blackreach".
rem Keep this file and the repository where they are, or rewrite the two paths below.
setlocal
set "REPO=$repository"
set "PS=pwsh"
where pwsh >nul 2>nul || set "PS=powershell"
"%PS%" -NoProfile -ExecutionPolicy Bypass -File "$playScript" -Assets "$OutputDir" -Start alftand %*
"@
    if ($DryRun) {
        Write-Host "[dry-run] would write $launcher"
        Write-Host "[dry-run] ----"
        Write-Host $content
        Write-Host "[dry-run] ----"
        return $launcher
    }
    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
    Set-Content -LiteralPath $launcher -Value $content -Encoding ASCII
    Write-Note "launcher written: $launcher"
    return $launcher
}

# ---------------------------------------------------------------- main

Write-Heading "OpenSkyrim demo setup"
Write-Note "repository: $repository"
if ($DryRun) { Write-Note "-DryRun: printing commands only; nothing will be built, converted or written." }

Assert-Prerequisites

if ($SkipConvert -and -not $OutDir) {
    throw "-SkipConvert still needs -OutDir: that is the folder the launcher goes next to."
}
if (-not $OutDir) {
    throw @"
No -OutDir given. Choose a folder with room for the converted assets (tens of GB):

    pwsh -File tools/demo/setup-demo.ps1 -SkyrimPath "<SkyrimSE>" -OutDir "<converted>"

<converted> must not be your Skyrim Data folder, and it should be outside this repository.
"@
}
$OutDir = Resolve-RepoPath $OutDir
Write-Note "output: $OutDir"

$dataDir = $null
if (-not $SkipConvert) {
    Write-Heading "Skyrim SE"
    $dataDir = Find-SkyrimDataDir
    if ($dataDir) {
        Write-Note "Data folder: $dataDir"
        if ($dataDir -eq $OutDir) {
            throw "-OutDir is the Skyrim Data folder. The output needs a folder of its own."
        }
    }
}

if (-not $SkipBuild -and -not $DryRun) {
    $qualifier = (Split-Path -Qualifier $repository).TrimEnd('\', ':')
    try {
        $free = (Get-PSDrive -Name $qualifier).Free
        if ($free -lt 10GB) {
            Write-Caution "only $([math]::Round($free / 1GB, 1)) GiB free on the repository drive; the release build writes into target\ and wants more room than that."
        }
    } catch { }
}

if (-not $SkipBuild) {
    Write-Heading "Build (release)"
    Write-Note "the first build compiles Bevy and takes many minutes; later builds are incremental."
    Invoke-SetupStep "cargo" @("build", "--release", "-p", "converter", "--bin", "converter") "Build the release converter" | Out-Null
    Invoke-SetupStep "cargo" @("build", "--release", "-p", "engine", "--bin", "engine") "Build the release engine" | Out-Null
}

$converter = if ($onWindows) { Join-Path $repository "target\release\converter.exe" } else { Join-Path $repository "target/release/converter" }
$engine = if ($onWindows) { Join-Path $repository "target\release\engine.exe" } else { Join-Path $repository "target/release/engine" }

if (-not $SkipConvert) {
    Write-Heading "Convert (this is the long step)"
    Write-Note "a cold conversion of the whole install measured ~5 hours on the lead's machine;"
    Write-Note "re-running into an existing -OutDir reuses the cache and takes ~20 minutes."
    if (-not $DryRun -and -not (Test-Path -LiteralPath $converter)) {
        throw "The converter is not built at $converter. Run without -SkipBuild first."
    }

    $arguments = [System.Collections.Generic.List[string]]::new()
    $arguments.Add($(if ($dataDir) { $dataDir } else { "<SkyrimSE>/Data" }))
    $arguments.Add($OutDir)
    if ($CpuJobs -gt 0) { $arguments.Add("--cpu-jobs"); $arguments.Add("$CpuJobs") }
    if ($IoJobs -gt 0) { $arguments.Add("--io-jobs"); $arguments.Add("$IoJobs") }
    $resume = Find-ResumeStaging $OutDir
    if ($resume) {
        Write-Note "found an interrupted run: resuming from $resume"
        $arguments.Add("--resume-staging"); $arguments.Add($resume)
    }
    $report = "$OutDir-conversion-report.json"
    $arguments.Add("--report-json"); $arguments.Add($report)

    # A conversion that hit a warning or a skipped input still writes usable output but exits 1.
    # That is worth reporting rather than hiding, so the run continues and says where to look.
    $exitCode = Invoke-SetupStep $converter $arguments.ToArray() "Convert the Skyrim SE data"
    if ($exitCode -ne 0 -and -not $DryRun) {
        Write-Caution "the converter exited with code $exitCode (warnings or skipped inputs)."
        Write-Caution "Look in $report and $OutDir\conversion-manifest.json."
        Write-Caution "Such a run leaves complete=false, and the engine refuses to start on an"
        Write-Caution "incomplete set. Fix the warning and convert again, or start it with"
        Write-Caution "--allow-incomplete-assets for a look at what did convert."
    }
}

Write-Heading "Launcher"
$launcher = Write-Launcher $OutDir

Write-Heading "Done"
Write-Note "Run the demo:  $launcher"
Write-Note "Or directly:   $engine --assets `"$OutDir`" --demo alftand --walk"
Write-Note "Controls:      mouse look (click the window first), WASD move, Shift run, Space jump,"
Write-Note "               E open a load door, F fly, Esc release the mouse."
Write-Note "Guide:         docs/demo/README.md"
