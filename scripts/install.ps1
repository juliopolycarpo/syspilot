<#
.SYNOPSIS
    Installs or updates SysPilot from the folder this script is in.

.DESCRIPTION
    Copies the files to Program Files, registers and starts the service, and
    adds a Start menu shortcut and an entry under Installed apps. Running it
    again over an existing installation updates it and keeps every setting.

    Nothing is downloaded. The PawnIO driver is not installed: the app shows
    where to get it and works without it.

.EXAMPLE
    ./install.ps1
#>
#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    # Where to install. The product must be under Program Files: the service
    # refuses to run from a place a standard user could write to.
    [string] $Destination = (Join-Path $env:ProgramFiles 'SysPilot'),
    # For tests: an instance name starting with SysPilot-Test. A test
    # instance gets no shortcut and no Installed apps entry.
    [string] $Instance = 'SysPilot'
)

$ErrorActionPreference = 'Stop'
$source = $PSScriptRoot
$product = $Instance -eq 'SysPilot'
$service = Join-Path $source 'syspilot-svc.exe'
if (-not (Test-Path $service)) {
    throw "(package): expected syspilot-svc.exe next to this script | received: nothing in $source"
}

# An update: the files cannot be replaced while they run. Stopping the
# service leaves every setting as it is.
if (Get-Service -Name $Instance -ErrorAction SilentlyContinue) {
    Stop-Service -Name $Instance -Force
}
Get-Process syspilot, syspilot-sensors -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($Destination, [StringComparison]::OrdinalIgnoreCase) } |
    Stop-Process -Force

New-Item -ItemType Directory -Force $Destination | Out-Null
Get-ChildItem -Force $source |
    Where-Object { $_.Name -ne 'install.ps1' } |
    Copy-Item -Recurse -Force -Destination $Destination

$installed = Join-Path $Destination 'syspilot-svc.exe'
$report = & $installed --instance $Instance install
if ($LASTEXITCODE -ne 0) {
    throw "(service): expected it to install | received: $report"
}
$report

if ($product) {
    $shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\SysPilot.lnk'
    $link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
    $link.TargetPath = Join-Path $Destination 'syspilot.exe'
    $link.WorkingDirectory = $Destination
    $link.Description = 'Keep this PC the way you set it up'
    $link.Save()

    $version = (& $installed --version).Split(' ')[-1]
    $size = (Get-ChildItem -Recurse -File $Destination | Measure-Object -Sum Length).Sum
    $entry = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SysPilot'
    New-Item -Force $entry | Out-Null
    $uninstall = 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "{0}"' -f (Join-Path $Destination 'uninstall.ps1')
    Set-ItemProperty $entry -Name DisplayName -Value 'SysPilot'
    Set-ItemProperty $entry -Name DisplayVersion -Value $version
    Set-ItemProperty $entry -Name Publisher -Value 'SysPilot contributors'
    Set-ItemProperty $entry -Name InstallLocation -Value $Destination
    Set-ItemProperty $entry -Name DisplayIcon -Value (Join-Path $Destination 'syspilot.exe')
    Set-ItemProperty $entry -Name UninstallString -Value $uninstall
    Set-ItemProperty $entry -Name EstimatedSize -Type DWord -Value ([int]($size / 1KB))
    Set-ItemProperty $entry -Name NoModify -Type DWord -Value 1
    Set-ItemProperty $entry -Name NoRepair -Type DWord -Value 1
    'SysPilot is in the Start menu.'
}
