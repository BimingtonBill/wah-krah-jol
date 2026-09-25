# The portal benchmark, in one command: builds the engine if it is out of date, times the portal at
# the dense doors of tools/bench/portal_bench_doors.json (closed, open in view, open behind, open
# behind a wall) in GPU, CPU and frame time, prints the summary and compares it with the previous run.
#
#   pwsh -File tools/bench/portal-bench.ps1 [-Assets %OPENSKYRIM_CONVERTED_DIR%] [-BuildProfile release|quick]
#   pwsh -Command "& tools/bench/portal-bench.ps1 -Variants 'default=','depth=--portal-depth-composite' -Repeats 2"
#   (-Command, not -File: -File passes a comma list as one string)
#
# Each variant is "name=extra engine arguments". The variants are run interleaved, -Repeats rounds of
# A B C D, so drift (heat, background tasks) hits them all alike, and the summary gives each state's
# mean over the repeats with the spread (min..max) beside it: a difference smaller than the spread
# is noise.
#
# It is a timing run: run it ALONE, and announce it to the other dev sessions first (message and
# team log). Another engine, a build or a conversion on the same machine changes the numbers.
# Results go to local/bench/portal-bench-<date-time>/<variant>-r<n>.csv (with a .summary.txt each).
# See docs/demo/README.md, "The portal bench".

[CmdletBinding()]
param(
    # The converted asset folder: the one holding skyrim_world.db.
    [string]$Assets = "%OPENSKYRIM_CONVERTED_DIR%",
    # The build profile. Report figures from release; quick only to compare against another quick run.
    [ValidateSet("release", "quick")]
    [string]$BuildProfile = "release",
    # The doors file (tools/bench/portal_bench_doors.py writes it).
    [string]$Doors = "tools/bench/portal_bench_doors.json",
    # The variants to compare, each "name=extra engine arguments" ("default=" for none).
    [string[]]$Variants = @("default="),
    # How many interleaved rounds of the variants to run.
    [ValidateRange(1, 20)]
    [int]$Repeats = 2,
    # How long each state is timed for, in seconds (the engine's default, 1.5, when 0).
    [double]$Seconds = 0,
    # Always build, even when the engine is newer than every change to the engine's sources.
    [switch]$Build,
    # Never build: use the engine already built for -BuildProfile.
    [switch]$NoBuild,
    # How long one run may take before it is stopped, in seconds.
    [int]$TimeoutSeconds = 900
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repository = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $repository

$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repository "target" }
$engine = Join-Path $targetDir "$BuildProfile/engine.exe"

# Is the engine older than the sources it is built from? Committed changes are dated by their last
# commit; uncommitted ones always count as newer.
function Test-EngineStale {
    if (-not (Test-Path -LiteralPath $engine)) { return $true }
    $sources = @("crates", "Cargo.toml", "Cargo.lock")
    $dirty = & git status --porcelain -- @sources
    if ($dirty) { return $true }
    $lastCommit = [int64](& git log -1 --format=%ct -- @sources)
    $built = [DateTimeOffset]::new((Get-Item -LiteralPath $engine).LastWriteTimeUtc).ToUnixTimeSeconds()
    return $built -lt $lastCommit
}

if (-not $NoBuild -and ($Build -or (Test-EngineStale))) {
    Write-Host "Building the engine ($BuildProfile)..." -ForegroundColor Cyan
    $buildArgs = if ($BuildProfile -eq "release") { @("build", "--release", "--bin", "engine") } else { @("build", "--profile", "quick", "--bin", "engine") }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
} elseif (-not $NoBuild) {
    Write-Host "The $BuildProfile engine is up to date with the sources; not building." -ForegroundColor Cyan
}
if (-not (Test-Path -LiteralPath $engine)) { throw "no engine at $engine" }

# "name=args" -> name and argument list.
$variantList = foreach ($spec in $Variants) {
    $name, $rest = $spec -split "=", 2
    if (-not $name) { throw "a variant needs a name: '$spec'" }
    $extra = if ($rest) { @($rest -split "\s+" | Where-Object { $_ }) } else { @() }
    [pscustomobject]@{ name = $name; args = $extra }
}
$names = @($variantList | ForEach-Object name)
if (@($names | Sort-Object -Unique).Count -ne $names.Count) { throw "variant names must differ: $($names -join ', ')" }

$benchRoot = Join-Path $repository "local/bench"
New-Item -ItemType Directory -Force -Path $benchRoot | Out-Null
# The previous portal bench run of the first variant, if any, to compare with.
$previous = Get-ChildItem -Path $benchRoot -Filter "*.csv" -File -Recurse -ErrorAction SilentlyContinue |
    Where-Object { (Get-Content -LiteralPath $_.FullName -TotalCount 1) -like "door,place,state,*gpu_mean_ms*" } |
    Sort-Object LastWriteTime -Descending

$stamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$runDir = Join-Path $benchRoot "portal-bench-$stamp"
New-Item -ItemType Directory -Force -Path $runDir | Out-Null

function Invoke-BenchRun([pscustomobject]$Variant, [int]$Round) {
    $csv = Join-Path $runDir "$($Variant.name)-r$Round.csv"
    $log = Join-Path $runDir "$($Variant.name)-r$Round.log"
    $arguments = @("--assets", "`"$Assets`"", "--portal-bench", "`"$Doors`"", "--bench-out", "`"$csv`"",
        "--run-label", "`"$($Variant.name) round $Round of $Repeats`"") + $Variant.args
    if ($Seconds -gt 0) { $arguments += @("--bench-seconds", "$Seconds") }
    $started = Get-Date
    $process = Start-Process -FilePath $engine -PassThru -NoNewWindow -ArgumentList $arguments `
        -RedirectStandardOutput $log -RedirectStandardError "$log.err"
    # Without taking the handle now, a process started this way reports no exit code once it has ended.
    $null = $process.Handle
    if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
        $process.Kill()
        throw "$($Variant.name) round $Round did not finish within $TimeoutSeconds s (log: $log)"
    }
    if (-not (Test-Path -LiteralPath $csv)) { throw "$($Variant.name) round $Round wrote no CSV (exit $($process.ExitCode), log: $log)" }
    Write-Host ("  {0} r{1}: {2:N0} s, exit {3}" -f $Variant.name, $Round, ((Get-Date) - $started).TotalSeconds, $process.ExitCode)
    return [pscustomobject]@{ variant = $Variant.name; round = $Round; csv = $csv; exit = $process.ExitCode }
}

Write-Host "Running the portal bench (alone, please): $($names -join ', ') x $Repeats, interleaved -> $runDir" -ForegroundColor Cyan
$started = Get-Date
$runs = foreach ($round in 1..$Repeats) {
    foreach ($variant in $variantList) { Invoke-BenchRun $variant $round }
}
Write-Host ("Portal bench: {0:N0} s in all" -f ((Get-Date) - $started).TotalSeconds) -ForegroundColor Green

# Per state: each column averaged over the doors, for one CSV.
$columns = @("gpu_mean_ms", "gpu_p95_ms", "opaque_gpu_mean_ms", "offscreen_opaque_gpu_mean_ms", "cpu_mean_ms", "cpu_p95_ms", "frame_mean_ms", "frame_p95_ms")
function Get-StateMeans([string]$Path) {
    $rows = Import-Csv -LiteralPath $Path
    $means = @{}
    foreach ($group in ($rows | Group-Object state)) {
        $values = @{}
        foreach ($column in $columns) {
            $values[$column] = ($group.Group | ForEach-Object { [double]$_.$column } | Measure-Object -Average).Average
        }
        $values["doors"] = $group.Count
        $means[$group.Name] = $values
    }
    return $means
}

# "mean (min..max)" over the repeats.
function Format-Spread([double[]]$Values) {
    $m = $Values | Measure-Object -Average -Minimum -Maximum
    if ($Values.Count -eq 1) { return "{0:N2}" -f $m.Average }
    return "{0:N2} ({1:N2}..{2:N2})" -f $m.Average, $m.Minimum, $m.Maximum
}

$states = @("closed", "closed-behind", "closed-occluded", "open-in-view", "open-behind", "open-occluded")
$perRun = @{}
foreach ($run in $runs) { $perRun["$($run.variant)|$($run.round)"] = Get-StateMeans $run.csv }

$table = foreach ($name in $names) {
    foreach ($state in $states) {
        $samples = @(1..$Repeats | ForEach-Object { $perRun["$name|$_"] } | Where-Object { $_.ContainsKey($state) } | ForEach-Object { $_[$state] })
        if (-not $samples) { continue }
        [pscustomobject]@{
            variant = $name; state = $state; doors = $samples[0].doors
            gpu = Format-Spread ($samples | ForEach-Object { $_.gpu_mean_ms })
            gpu_p95 = Format-Spread ($samples | ForEach-Object { $_.gpu_p95_ms })
            offscreen = Format-Spread ($samples | ForEach-Object { $_.offscreen_opaque_gpu_mean_ms })
            cpu = Format-Spread ($samples | ForEach-Object { $_.cpu_mean_ms })
            frame = Format-Spread ($samples | ForEach-Object { $_.frame_mean_ms })
            frame_p95 = Format-Spread ($samples | ForEach-Object { $_.frame_p95_ms })
        }
    }
}
$summaryText = "Per variant and state, averaged over the doors, mean over $Repeats repeat(s) with (min..max), ms:`n" +
    ($table | Format-Table -AutoSize | Out-String)
Write-Host ""
Write-Host $summaryText
Set-Content -LiteralPath (Join-Path $runDir "summary.txt") -Value $summaryText

# Change since the previous bench, for the first variant's first round against the newest older CSV.
$older = $previous | Where-Object { $_.FullName -notlike "$runDir*" } | Select-Object -First 1
if ($older) {
    $current = $perRun["$($names[0])|1"]
    $before = Get-StateMeans $older.FullName
    Write-Host "Change since $($older.Name) ($($names[0]) r1 minus that one, ms; one run each, so read it against the spread above):"
    $states | Where-Object { $current.ContainsKey($_) -and $before.ContainsKey($_) } | ForEach-Object {
        $now = $current[$_]; $then = $before[$_]
        $row = [ordered]@{ state = $_ }
        foreach ($column in @("gpu_mean_ms", "gpu_p95_ms", "offscreen_opaque_gpu_mean_ms", "cpu_mean_ms", "frame_mean_ms")) {
            $row[$column.Replace("_mean_ms", "").Replace("_ms", "")] = "{0:+0.00;-0.00;0.00}" -f ($now[$column] - $then[$column])
        }
        [pscustomobject]$row
    } | Format-Table -AutoSize | Out-String | Write-Host
}
Write-Host "Results: $runDir"
$failed = @($runs | Where-Object { $_.exit -ne 0 })
exit $(if ($failed) { $failed[0].exit } else { 0 })
