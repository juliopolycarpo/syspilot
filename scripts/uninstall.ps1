<#
.SYNOPSIS
    Removes SysPilot from the folder this script is in.

.DESCRIPTION
    Restores every setting SysPilot changed, removes the service and its
    data, the Start menu shortcut, the Installed apps entry and the files.
    The PawnIO driver is not SysPilot's and is left alone.

.EXAMPLE
    ./uninstall.ps1
    ./uninstall.ps1 -KeepTweaks
#>
#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    # Leave the settings as they are now instead of restoring them.
    [switch] $KeepTweaks,
    # For tests: the instance name given to install.ps1.
    [string] $Instance = 'SysPilot'
)

$ErrorActionPreference = 'Stop'
$location = $PSScriptRoot
$service = Join-Path $location 'syspilot-svc.exe'
if (-not (Test-Path $service)) {
    throw "(installation): expected syspilot-svc.exe next to this script | received: nothing in $location"
}

Get-Process syspilot -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($location, [StringComparison]::OrdinalIgnoreCase) } |
    Stop-Process -Force

$arguments = @('--instance', $Instance, 'uninstall')
if ($KeepTweaks) {
    $arguments += '--keep-tweaks'
}
$report = & $service @arguments
if ($LASTEXITCODE -ne 0) {
    # The files stay, so that this can be run again.
    throw "(service): expected it to uninstall | received: $report"
}
$report

if ($Instance -eq 'SysPilot') {
    $shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\SysPilot.lnk'
    if (Test-Path $shortcut) {
        Remove-Item -Force $shortcut
    }
    $entry = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SysPilot'
    if (Test-Path $entry) {
        Remove-Item -Recurse -Force $entry
    }
}

# This script is inside the folder it removes; PowerShell has read it
# already, but the folder must not be the current directory.
Set-Location $env:SystemRoot
Remove-Item -Recurse -Force $location
"Removed $location."
