# The portal benchmark, in one command: builds the engine, times the portal at the dense doors of
# tools/bench/portal_bench_doors.json (closed, open in view, open behind, open behind a wall) in GPU,
# CPU and frame time, prints the summary and compares it with the previous run.
#
#   pwsh -File tools/bench/portal-bench.ps1 [-Assets %OPENSKYRIM_CONVERTED_DIR%] [-BuildProfile release|quick]
#
# It is a timing run: run it ALONE. Another engine, a build or a conversion on the same machine
# changes the numbers. Results go to local/bench/<date-time>.csv (with <csv>.summary.txt beside it).
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
    # Skip the build and use the engine already built for -BuildProfile.
    [switch]$NoBuild,
    # How long the run may take before it is stopped, in seconds.
    [int]$TimeoutSeconds = 900
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repository = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $repository

$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repository "target" }
$engine = Join-Path $targetDir "$BuildProfile/engine.exe"

if (-not $NoBuild) {
    Write-Host "Building the engine ($BuildProfile)..." -ForegroundColor Cyan
    $buildArgs = if ($BuildProfile -eq "release") { @("build", "--release", "--bin", "engine") } else { @("build", "--profile", "quick", "--bin", "engine") }
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}
if (-not (Test-Path -LiteralPath $engine)) { throw "no engine at $engine" }

$benchDir = Join-Path $repository "local/bench"
New-Item -ItemType Directory -Force -Path $benchDir | Out-Null
# The previous portal bench CSV, if any: the newest one with this bench's header.
$previous = Get-ChildItem -Path $benchDir -Filter "*.csv" -File -ErrorAction SilentlyContinue |
    Where-Object { (Get-Content -LiteralPath $_.FullName -TotalCount 1) -like "door,place,state,*gpu_mean_ms*" } |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1

$stamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$csv = Join-Path $benchDir "portal-bench-$stamp.csv"
$log = Join-Path $benchDir "portal-bench-$stamp.log"

Write-Host "Running the portal bench (alone, please) -> $csv" -ForegroundColor Cyan
$started = Get-Date
$process = Start-Process -FilePath $engine -PassThru -NoNewWindow `
    -ArgumentList @("--assets", "`"$Assets`"", "--portal-bench", "`"$Doors`"", "--bench-out", "`"$csv`"") `
    -RedirectStandardOutput $log -RedirectStandardError "$log.err"
# Without taking the handle now, a process started this way reports no exit code once it has ended.
$null = $process.Handle
if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
    $process.Kill()
    throw "the bench did not finish within $TimeoutSeconds s (log: $log)"
}
$elapsed = (Get-Date) - $started
if (-not (Test-Path -LiteralPath $csv)) { throw "the bench wrote no CSV (exit $($process.ExitCode), log: $log)" }

Write-Host ""
Write-Host ("Portal bench: {0:N0} s, exit {1}" -f $elapsed.TotalSeconds, $process.ExitCode) -ForegroundColor Green
$summary = "$csv.summary.txt"
if (Test-Path -LiteralPath $summary) { Get-Content -LiteralPath $summary | Write-Host }

# Per state: each column averaged over the doors.
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

$states = @("closed", "open-in-view", "open-behind", "open-occluded")
$current = Get-StateMeans $csv
Write-Host ""
Write-Host "Per state, averaged over the doors (ms):"
$states | Where-Object { $current.ContainsKey($_) } | ForEach-Object {
    $v = $current[$_]
    [pscustomobject]@{
        state = $_; doors = $v.doors
        gpu = "{0:N2}" -f $v.gpu_mean_ms; gpu_p95 = "{0:N2}" -f $v.gpu_p95_ms
        opaque = "{0:N2}" -f $v.opaque_gpu_mean_ms; offscreen = "{0:N2}" -f $v.offscreen_opaque_gpu_mean_ms
        cpu = "{0:N2}" -f $v.cpu_mean_ms; cpu_p95 = "{0:N2}" -f $v.cpu_p95_ms
        frame = "{0:N2}" -f $v.frame_mean_ms; frame_p95 = "{0:N2}" -f $v.frame_p95_ms
    }
} | Format-Table -AutoSize | Out-String | Write-Host

if ($previous) {
    $before = Get-StateMeans $previous.FullName
    Write-Host "Change since $($previous.Name) (this run minus that one, ms; negative is faster):"
    $states | Where-Object { $current.ContainsKey($_) -and $before.ContainsKey($_) } | ForEach-Object {
        $now = $current[$_]; $then = $before[$_]
        $row = [ordered]@{ state = $_ }
        foreach ($column in @("gpu_mean_ms", "gpu_p95_ms", "offscreen_opaque_gpu_mean_ms", "cpu_mean_ms", "frame_mean_ms")) {
            $row[$column.Replace("_mean_ms", "").Replace("_ms", "")] = "{0:+0.00;-0.00;0.00}" -f ($now[$column] - $then[$column])
        }
        [pscustomobject]$row
    } | Format-Table -AutoSize | Out-String | Write-Host
} else {
    Write-Host "No previous portal bench CSV in local/bench to compare with."
}
Write-Host "CSV: $csv"
exit $process.ExitCode
