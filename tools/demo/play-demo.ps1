# Starts one of the walkable demos against a converted asset folder: the Alftand -> Blackreach
# descent, or the Riverwood village.
#
#   pwsh -File tools/demo/play-demo.ps1 -Assets "<converted>" [-Start alftand|blackreach|riverwood]
#
# The engine binary is looked up relative to this repository, so the script works from a checkout
# anywhere. Build it first ("cargo build --release -p engine --bin engine"), or let
# tools/demo/setup-demo.ps1 do the whole setup. See docs/demo/README.md.
#
# The engine runs in the foreground, so its log stays in this console: that is what you read when
# something does not look right. Close the window (or Ctrl+C) to stop it.

[CmdletBinding()]
param(
    # The converted asset folder: the one holding skyrim_world.db (see tools/demo/setup-demo.ps1).
    [Parameter(Mandatory = $true)]
    [string]$Assets,
    # Where to start: on foot outside the Alftand entrance in the Pale, straight inside
    # Blackreach, or on the Helgen road south-west of Riverwood.
    [ValidateSet("alftand", "blackreach", "riverwood")]
    [string]$Start = "alftand",
    # Fly instead of walking (the old free camera: WASD, Space up, Shift down, Ctrl fast).
    [switch]$Fly,
    # Scripted run instead of an interactive one: walks the route, screenshots every place and door,
    # then holds W in Blackreach. Writes its log and images into this folder.
    [string]$Tour,
    # Render the camera poses in this JSON file to PNGs and exit (docs/design/reference-shots.md).
    [string]$Shots,
    # Where a -Shots run writes its images; defaults to a folder named after the shots file.
    [string]$ShotsOut,
    # Reach of the terrain-only ring, in cells beyond the full-detail grid (engine default 8).
    [int]$TerrainRadius = 0,
    # Print the command instead of running it.
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repository = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$windowsVariable = Get-Variable -Name IsWindows -ErrorAction SilentlyContinue
$onWindows = if ($windowsVariable) { [bool]$windowsVariable.Value } else { $true }

function Write-Note([string]$Text) {
    Write-Host "   $Text" -ForegroundColor Gray
}

function Resolve-RepoPath([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    return [IO.Path]::GetFullPath((Join-Path $repository $Path))
}

$engineRelative = if ($onWindows) { "target/release/engine.exe" } else { "target/release/engine" }
$engine = Join-Path $repository $engineRelative
if (-not (Test-Path -LiteralPath $engine)) {
    throw @"
The engine is not built at $engine

Build it:      cargo build --release -p engine --bin engine
Or set up:     pwsh -File tools/demo/setup-demo.ps1 -SkyrimPath "<SkyrimSE>" -OutDir "<converted>"
"@
}

$Assets = Resolve-RepoPath $Assets
if (-not (Test-Path -LiteralPath $Assets -PathType Container)) {
    throw "The asset folder '$Assets' does not exist."
}
if (-not (Test-Path -LiteralPath (Join-Path $Assets "skyrim_world.db"))) {
    Write-Note "'$Assets' has no skyrim_world.db - it does not look like a converted asset folder."
    Write-Note "Convert yours with tools/demo/setup-demo.ps1."
}

$arguments = [System.Collections.Generic.List[string]]::new()
$arguments.Add("--assets"); $arguments.Add($Assets)
$arguments.Add("--demo"); $arguments.Add($Start)
# Walking is opt-in (--walk); without it the engine uses the free-flight camera, which is what -Fly
# asks for. There is no --fly flag.
if (-not $Fly) { $arguments.Add("--walk") }
if ($Tour) { $arguments.Add("--demo-tour"); $arguments.Add((Resolve-RepoPath $Tour)) }
if ($Shots) { $arguments.Add("--shots"); $arguments.Add((Resolve-RepoPath $Shots)) }
if ($ShotsOut) { $arguments.Add("--shots-out"); $arguments.Add((Resolve-RepoPath $ShotsOut)) }
if ($TerrainRadius -gt 0) { $arguments.Add("--terrain-radius"); $arguments.Add("$TerrainRadius") }

$display = (@($engine) + $arguments) -join " "

if ($DryRun) {
    Write-Host "[dry-run] $display"
    return
}

Write-Note "engine:  $engine"
Write-Note "assets:  $Assets"
Write-Note "start:   $Start"
Write-Note "controls: mouse look (click the window first), WASD move, Shift run, Space jump,"
Write-Note "          E open a load door, F fly, Esc release the mouse."
Write-Host ""
Write-Host "+ $display"

& $engine @($arguments)
exit $LASTEXITCODE
