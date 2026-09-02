[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$appRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\")).Path
$surfaceFile = Join-Path $appRoot "linux-egui/src/lib.rs"
$source = Get-Content -Raw -LiteralPath $surfaceFile
$linuxManifest = Get-Content -Raw -LiteralPath (Join-Path $appRoot "linux-egui/Cargo.toml")
$coreManifest = Get-Content -Raw -LiteralPath (Join-Path $appRoot "crates/openless-core/Cargo.toml")
$coreApi = Get-Content -Raw -LiteralPath (Join-Path $appRoot "crates/openless-core/src/api.rs")
$mainSource = Get-Content -Raw -LiteralPath (Join-Path $appRoot "linux-egui/src/main.rs")

$forbidden = @(
    'pub use openless_core::domains::\*',
    'ActivityStore',
    'CorrectionRuleStore',
    'DictionaryStore',
    'HistoryStore',
    'StylePackStore',
    'HISTORY_CAP',
    'pub use openless_core::\{\s*activity',
    'style_pack_store',
    'selection_voice_intent'
)

$violations = @($forbidden | Where-Object { $source -match $_ })
if ($violations.Count -gt 0) {
    Write-Error "Linux egui public surface exposes core implementation details: $($violations -join ', ')"
    exit 1
}

if ($linuxManifest -match 'legacy-preferences-write' -or $coreManifest -match 'legacy-preferences-write') {
    Write-Error "The legacy whole-document preferences feature must not exist in Core or Linux manifests"
    exit 1
}

if ($mainSource -notmatch 'SingleInstanceBroker::acquire_or_forward' -or
    $mainSource -notmatch 'Fcitx5HotkeyListener::start' -or
    $mainSource -notmatch 'drain_native_events()') {
    Write-Error "Linux eframe production UI must wire single-instance and fcitx5 native events"
    exit 1
}
if ($mainSource -match 'LinuxNativeRuntime::start(backend,s*None,s*None)') {
    Write-Error "Linux eframe production UI must not disable all native adapters"
    exit 1
}

$legacyPreferenceWriters = @(
    'set_preferences',
    'set_preferences_validated',
    'set_preferences_preserving_style',
    'set_preferences_preserving_style_validated'
)
$legacyGate = '#\[cfg\(test\)\]\s*pub\(crate\) fn {0}\s*\('
foreach ($method in $legacyPreferenceWriters) {
    if ($coreApi -notmatch ($legacyGate -f $method)) {
        Write-Error "Legacy preferences writer '$method' must remain crate-private and test-only"
        exit 1
    }
    if ($coreApi -match ("pub\s+fn\s+{0}\s*\(" -f $method)) {
        Write-Error "Legacy preferences writer '$method' is exposed on the public Core facade"
        exit 1
    }
}

Write-Output "Linux egui public surface gate passed (facade/DTO/event/host-interface/fixture only)."
