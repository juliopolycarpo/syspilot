<#
.SYNOPSIS
    Captures every page of the app in demo mode, for the visual review.

.DESCRIPTION
    Starts `syspilot --demo` once per page and level, waits for the demo's
    sensors to draw a few seconds of history, saves the window as a PNG
    under target/ui-shots and closes the app. Nothing on the system is
    changed: the demo has no service behind it.

.EXAMPLE
    ./scripts/capture-pages.ps1
    ./scripts/capture-pages.ps1 -Pages home,privacy -Levels advanced -Scenario no-driver
#>
[CmdletBinding()]
param(
    [string[]] $Pages = @('home', 'performance', 'privacy', 'security', 'reliability',
        'look-and-feel', 'sensors', 'system', 'custom-actions', 'history', 'settings'),
    [string[]] $Levels = @('simple', 'advanced'),
    [string] $Scenario = 'healthy',
    [string] $Profile = 'debug',
    # Seconds to let the page settle before the capture.
    [int] $SettleSeconds = 5
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root "target\x86_64-pc-windows-msvc\$Profile\syspilot.exe"
if (-not (Test-Path $app)) {
    throw "(app): expected a built app at $app | received: nothing. Run cargo build -p syspilot-ui first."
}

foreach ($level in $Levels) {
    foreach ($page in $Pages) {
        # These two pages only exist at the Advanced level.
        if ($level -eq 'simple' -and $page -in 'custom-actions', 'history') { continue }
        $process = Start-Process $app -PassThru -ArgumentList '--demo', $Scenario, '--page', $page, '--level', $level
        try {
            Start-Sleep -Seconds $SettleSeconds
            & (Join-Path $PSScriptRoot 'capture-window.ps1') -ProcessId $process.Id `
                -Path "target/ui-shots/$Scenario-$level-$page.png"
        } finally {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        }
    }
}
