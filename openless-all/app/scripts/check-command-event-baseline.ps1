[CmdletBinding()]
param(
    [string]$BaselinePath,
    [string]$TauriLibPath,
    [string]$CoreEventsPath
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if ([string]::IsNullOrWhiteSpace($BaselinePath)) {
    $BaselinePath = Join-Path $scriptRoot "../../../docs/linux-egui-command-event-baseline.json"
}
if ([string]::IsNullOrWhiteSpace($TauriLibPath)) {
    $TauriLibPath = Join-Path $scriptRoot "../src-tauri/src/lib.rs"
}
if ([string]::IsNullOrWhiteSpace($CoreEventsPath)) {
    $CoreEventsPath = Join-Path $scriptRoot "../crates/openless-core/src/events.rs"
}

if (-not (Test-Path -LiteralPath $BaselinePath)) {
    throw "baseline file not found: $BaselinePath"
}
if (-not (Test-Path -LiteralPath $TauriLibPath)) {
    throw "Tauri lib.rs not found: $TauriLibPath"
}
if (-not (Test-Path -LiteralPath $CoreEventsPath)) {
    throw "core events.rs not found: $CoreEventsPath"
}

$baseline = Get-Content -LiteralPath $BaselinePath -Raw | ConvertFrom-Json
$expected = @($baseline.commands | Sort-Object -Unique)
$source = Get-Content -LiteralPath $TauriLibPath -Raw
$actual = @(
    [regex]::Matches(
        $source,
        '(?m)^\s*(?:(?:\$crate::)?(?:commands|coding_agent::commands)::|\$crate::)([A-Za-z0-9_]+),'
    ) | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
)

$missing = @($expected | Where-Object { $_ -notin $actual })
$added = @($actual | Where-Object { $_ -notin $expected })
$duplicateCount = @($baseline.commands).Count - $expected.Count

if ($baseline.counts.tauriCommandsObserved -ne $expected.Count) {
    throw "baseline count mismatch: counts.tauriCommandsObserved=$($baseline.counts.tauriCommandsObserved), commands=$($expected.Count)"
}
if ($duplicateCount -ne 0) {
    throw "baseline contains duplicate command names: $duplicateCount"
}
if ($missing.Count -gt 0 -or $added.Count -gt 0) {
    if ($missing.Count -gt 0) {
        Write-Error ("commands missing from source: " + ($missing -join ", "))
    }
    if ($added.Count -gt 0) {
        Write-Error ("commands missing from baseline: " + ($added -join ", "))
    }
    exit 1
}

$legacyEvents = @($baseline.events | Sort-Object -Unique)
if ($baseline.counts.legacyEventsObserved -ne $legacyEvents.Count) {
    throw "baseline count mismatch: counts.legacyEventsObserved=$($baseline.counts.legacyEventsObserved), events=$($legacyEvents.Count)"
}
if (@($baseline.events).Count -ne $legacyEvents.Count) {
    throw "baseline contains duplicate legacy event names"
}

$ownedEvents = @(
    @($baseline.eventOwnership.coreSemantic.PSObject.Properties.Name)
    @($baseline.eventOwnership.tauriHost.PSObject.Properties.Name)
    @($baseline.eventOwnership.migrationRequired.PSObject.Properties.Name)
) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
$ownedUnique = @($ownedEvents | Sort-Object -Unique)
$unclassified = @($legacyEvents | Where-Object { $_ -notin $ownedUnique })
$unknownOwned = @($ownedUnique | Where-Object { $_ -notin $legacyEvents })
if ($ownedEvents.Count -ne $ownedUnique.Count) {
    throw "legacy event ownership contains duplicate classifications"
}
if ($unclassified.Count -gt 0 -or $unknownOwned.Count -gt 0) {
    if ($unclassified.Count -gt 0) {
        Write-Error ("legacy events without ownership: " + ($unclassified -join ", "))
    }
    if ($unknownOwned.Count -gt 0) {
        Write-Error ("owned events missing from baseline: " + ($unknownOwned -join ", "))
    }
    exit 1
}

$coreSource = Get-Content -LiteralPath $CoreEventsPath -Raw
$enumMatch = [regex]::Match(
    $coreSource,
    '(?s)pub enum BackendEventKind\s*\{(?<body>.*?)\n\}'
)
if (-not $enumMatch.Success) {
    throw "BackendEventKind enum not found in $CoreEventsPath"
}
$coreActual = @(
    [regex]::Matches($enumMatch.Groups['body'].Value, '(?m)^\s*([A-Z][A-Za-z0-9]+)(?:\(|,)') |
        ForEach-Object {
            ([regex]::Replace($_.Groups[1].Value, '(?<!^)([A-Z])', '_$1')).ToLowerInvariant()
        } |
        Sort-Object -Unique
)
$coreExpected = @($baseline.coreEventKinds | Sort-Object -Unique)
$coreMissing = @($coreExpected | Where-Object { $_ -notin $coreActual })
$coreAdded = @($coreActual | Where-Object { $_ -notin $coreExpected })
if ($baseline.counts.coreEventKindsDefined -ne $coreExpected.Count) {
    throw "baseline count mismatch: counts.coreEventKindsDefined=$($baseline.counts.coreEventKindsDefined), coreEventKinds=$($coreExpected.Count)"
}
if (@($baseline.coreEventKinds).Count -ne $coreExpected.Count) {
    throw "baseline contains duplicate core event kinds"
}
if ($coreMissing.Count -gt 0 -or $coreAdded.Count -gt 0) {
    if ($coreMissing.Count -gt 0) {
        Write-Error ("core event kinds missing from source: " + ($coreMissing -join ", "))
    }
    if ($coreAdded.Count -gt 0) {
        Write-Error ("core event kinds missing from baseline: " + ($coreAdded -join ", "))
    }
    exit 1
}

Write-Output "command/event baseline passed ($($expected.Count) commands; $(@($baseline.events).Count) legacy events; $(@($baseline.coreEventKinds).Count) core event kinds)."
