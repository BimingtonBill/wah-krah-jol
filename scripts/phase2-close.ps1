[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Campaign,
    [string]$BundleUri,
    [switch]$ValidateOnly
)

$ErrorActionPreference = "Stop"
$repository = Split-Path -Parent $PSScriptRoot

function Assert-Phase2Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Phase 2 cannot be closed: $Message" }
}

function Write-Utf8File([string]$Path, [string]$Content) {
    [IO.File]::WriteAllText($Path, $Content, [Text.UTF8Encoding]::new($false))
}

$campaignPath = (Resolve-Path -LiteralPath $Campaign).Path
$reportPath = Join-Path $campaignPath "acceptance-report.json"
Assert-Phase2Condition (Test-Path -LiteralPath $reportPath -PathType Leaf) "acceptance-report.json is missing"

$report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
Assert-Phase2Condition ($report.format_version -eq 2) "acceptance report format must be 2"
Assert-Phase2Condition ($report.verdict -eq "accepted") "verdict is '$($report.verdict)', expected 'accepted'"
Assert-Phase2Condition ([bool]$report.release_ready) "release_ready is false"
Assert-Phase2Condition (@($report.failures).Count -eq 0) "acceptance report contains failures"
Assert-Phase2Condition (@($report.warnings).Count -eq 0) "acceptance report contains warnings"
Assert-Phase2Condition ([bool]$report.preflight_passed) "preflight did not pass"
Assert-Phase2Condition ([bool]$report.quality_passed) "quality gates did not pass"
Assert-Phase2Condition ([bool]$report.robustness_passed) "robustness gates did not pass"
Assert-Phase2Condition ([bool]$report.release_build_passed) "release build did not pass"
Assert-Phase2Condition ([bool]$report.functional_passed) "functional gates did not pass"
Assert-Phase2Condition ([bool]$report.baseline.available) "acceptance baseline is missing"
Assert-Phase2Condition ([bool]$report.baseline.same_hardware) "acceptance baseline is from different hardware"
Assert-Phase2Condition ([bool]$report.visual.passed) "signed visual review did not pass"
Assert-Phase2Condition (-not [bool]$report.metadata.dirty_worktree) "campaign used a dirty worktree"

$expectedScenarios = @($report.expected_scenarios)
$actualScenarios = @($report.scenarios.PSObject.Properties.Name)
Assert-Phase2Condition ($expectedScenarios.Count -eq 11) "all 11 Phase 2 scenarios are required"
foreach ($scenario in $expectedScenarios) {
    Assert-Phase2Condition ($scenario -in $actualScenarios) "scenario '$scenario' is missing"
}

$currentCommit = (& git -C $repository rev-parse --short=12 HEAD 2>$null)
Assert-Phase2Condition ([bool]$currentCommit) "current Git commit could not be read"
Assert-Phase2Condition ($report.metadata.commit -eq $currentCommit) "campaign commit '$($report.metadata.commit)' does not match HEAD '$currentCommit'"
& git -C $repository diff --quiet --ignore-submodules HEAD 2>$null
Assert-Phase2Condition ($LASTEXITCODE -eq 0) "tracked worktree changes are present"

$baselinePath = $report.baseline.path
Assert-Phase2Condition ([bool]$baselinePath) "baseline path is empty"
Assert-Phase2Condition (Test-Path -LiteralPath $baselinePath -PathType Leaf) "baseline file is missing"

$screenshotsPath = Join-Path $campaignPath "screenshots.json"
$visualReviewPath = Join-Path $campaignPath "visual-review.json"
Assert-Phase2Condition (Test-Path -LiteralPath $screenshotsPath -PathType Leaf) "screenshots.json is missing"
Assert-Phase2Condition (Test-Path -LiteralPath $visualReviewPath -PathType Leaf) "visual-review.json is missing"
$screenshots = @(Get-Content -LiteralPath $screenshotsPath -Raw | ConvertFrom-Json)
foreach ($scenario in $expectedScenarios) {
    $entry = @($screenshots | Where-Object { $_.scenario -eq $scenario })
    Assert-Phase2Condition ($entry.Count -eq 1) "screenshot evidence for '$scenario' is missing or duplicated"
    Assert-Phase2Condition ([bool]$entry[0].present) "screenshot for '$scenario' was not captured"
    Assert-Phase2Condition (Test-Path -LiteralPath $entry[0].path -PathType Leaf) "screenshot file for '$scenario' is missing"
    $actualHash = (Get-FileHash -LiteralPath $entry[0].path -Algorithm SHA256).Hash
    Assert-Phase2Condition ($actualHash -eq $entry[0].sha256) "screenshot hash mismatch for '$scenario'"
}

$requiredEvidence = @(
    "acceptance-report.json",
    "acceptance-summary.md",
    "preflight.json",
    "quality-gates.json",
    "robustness.json",
    "functional-results.json",
    "comparison.json",
    "screenshots.json",
    "visual-review.json",
    "performance/medians.json"
)
$evidence = foreach ($relativePath in $requiredEvidence) {
    $path = Join-Path $campaignPath $relativePath
    Assert-Phase2Condition (Test-Path -LiteralPath $path -PathType Leaf) "evidence '$relativePath' is missing"
    $item = Get-Item -LiteralPath $path
    [ordered]@{
        path = $relativePath.Replace('\', '/')
        bytes = $item.Length
        sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    }
}
$manifest = [ordered]@{
    format_version = 1
    generated_at = (Get-Date).ToString("o")
    campaign_commit = $report.metadata.commit
    campaign_hardware = $report.metadata.hardware
    baseline_sha256 = (Get-FileHash -LiteralPath $baselinePath -Algorithm SHA256).Hash
    files = @($evidence)
}
$manifestPath = Join-Path $campaignPath "release-evidence-sha256.json"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8
$manifestHash = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash

if ($ValidateOnly) {
    Write-Host "Phase 2 closure validation passed: $campaignPath"
    Write-Host "Evidence manifest SHA-256: $manifestHash"
    exit 0
}

$bundleReference = if ($BundleUri) { "[$BundleUri]($BundleUri)" } else { "``$campaignPath``" }
$releaseEvidencePath = Join-Path $repository "docs\roadmap\02-release-evidence.md"
$releaseEvidence = @"
# Phase 2 release evidence

Phase 2 was closed from a reproducible target-hardware campaign with verdict exactly `accepted`.
The proprietary converted assets and campaign bundle are not stored in this repository.

- Campaign commit: ``$($report.metadata.commit)``
- Hardware: $($report.metadata.hardware)
- Bundle: $bundleReference
- Evidence manifest SHA-256: ``$manifestHash``
- Baseline SHA-256: ``$($manifest.baseline_sha256)``
- Visual review: signed and passed
- Quality, robustness, release build and functional gates: passed
- Scenarios: $($expectedScenarios -join ', ')

The per-file hashes are recorded in `release-evidence-sha256.json` inside the external bundle.
"@
Write-Utf8File $releaseEvidencePath ($releaseEvidence.TrimEnd() + "`n")

$readmePath = Join-Path $repository "README.md"
$readme = Get-Content -LiteralPath $readmePath -Raw
$pendingRoadmap = '- [ ] **[Phase 2: Core Engine Runtime & Vercidium Renderer (`engine`)](docs/roadmap/02-core-engine.md)** — Runtime, integration, profiling, and acceptance infrastructure implemented; complete real-asset sign-off remains pending.'
$completeRoadmap = '- [x] **[Phase 2: Core Engine Runtime & Vercidium Renderer (`engine`)](docs/roadmap/02-core-engine.md)** — Completed with reproducible real-asset acceptance; see the [release evidence](docs/roadmap/02-release-evidence.md).'
Assert-Phase2Condition ($readme.Contains($pendingRoadmap)) "README Phase 2 pending marker was not found"
Write-Utf8File $readmePath ($readme.Replace($pendingRoadmap, $completeRoadmap))

$corePath = Join-Path $repository "docs\roadmap\02-core-engine.md"
$core = Get-Content -LiteralPath $corePath -Raw
$pendingStatus = '> **Status: Acceptance pending.** Runtime, real-asset closure, HZB/indirect-renderer conformance and automated gates are implemented. The three-repetition target-hardware campaign, approved baseline, and signed visual review remain required before completion.'
$completeStatus = '> **Status: Complete.** Runtime and target-hardware acceptance passed. See [Phase 2 release evidence](02-release-evidence.md) for the external bundle and integrity hashes.'
Assert-Phase2Condition ($core.Contains($pendingStatus)) "core-engine pending status was not found"
Write-Utf8File $corePath ($core.Replace($pendingStatus, $completeStatus))

$planPath = Join-Path $repository "docs\roadmap\02-completion-plan.md"
$plan = Get-Content -LiteralPath $planPath -Raw
$planHeading = "# Plano de conclusão da Phase 2`r`n"
if (-not $plan.Contains($planHeading)) { $planHeading = "# Plano de conclusão da Phase 2`n" }
Assert-Phase2Condition ($plan.Contains($planHeading)) "completion-plan heading was not found"
$planStatus = "$planHeading`n> **Status: Complete.** The accepted campaign and integrity hashes are recorded in [Phase 2 release evidence](02-release-evidence.md).`n"
Write-Utf8File $planPath ($plan.Replace($planHeading, $planStatus))

Write-Host "Phase 2 documentation was closed successfully."
Write-Host "Release evidence: $releaseEvidencePath"
Write-Host "Evidence manifest SHA-256: $manifestHash"
