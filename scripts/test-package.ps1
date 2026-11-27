<#
.SYNOPSIS
    Installs, updates and removes the installer on this machine, as a test
    instance, and fails if anything is wrong or left behind.

.DESCRIPTION
    Builds two packages of the instance SysPilot-Test.msi from the existing
    release build (the built version and one above it) and goes through what
    a user's machine goes through:

      1. installing over what the 0.1 script installer left;
      2. updating while the app is open, keeping the profile;
      3. removing, which must restore the settings and leave no service,
         file, data, shortcut, preferences or Installed apps entry.

    The test instance has its own folder, service and upgrade code, so an
    installed SysPilot is not touched. The only setting it enforces is a
    scratch registry value under HKLM\Software\SysPilot-Test. Needs an
    elevated PowerShell and a release build (`cargo build --release`).

.EXAMPLE
    ./scripts/test-package.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'leftovers.ps1')
$repository = Split-Path -Parent $PSScriptRoot
$instance = 'SysPilot-Test.msi'
$folder = Join-Path $env:ProgramFiles $instance
$data = Join-Path $env:ProgramData $instance
$preferences = Join-Path $env:LOCALAPPDATA $instance
$shortcut = Join-Path $env:ProgramData "Microsoft\Windows\Start Menu\Programs\$instance.lnk"
$service = Join-Path $folder 'syspilot-svc.exe'
$scriptEntry = "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$instance"
$scratch = 'HKLM:\SOFTWARE\SysPilot-Test\Msi'
$logs = Join-Path $repository 'target\package-test'

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$administrator = [Security.Principal.WindowsBuiltInRole]::Administrator
if (-not ([Security.Principal.WindowsPrincipal] $identity).IsInRole($administrator)) {
    throw '(test environment): expected an elevated PowerShell (the test installs a service) | received: a standard user'
}

# Fails the test unless `$received` is `$expected`.
function Assert-Same([string] $what, $expected, $received) {
    if ($expected -ne $received) {
        throw "($what): expected $expected | received: $received"
    }
    "ok  $what"
}

# Runs Windows Installer without a window and returns its exit code.
function Invoke-Installer([string] $step, [string[]] $arguments) {
    $log = Join-Path $logs "$step.log"
    $all = $arguments + @('/qn', '/norestart', '/l*v', "`"$log`"")
    $process = Start-Process msiexec.exe -ArgumentList $all -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "($step): expected msiexec to exit with 0 | received: $($process.ExitCode), see $log"
    }
    "ok  $step"
}

# Builds a package of the test instance and returns its path.
function New-TestPackage([string] $version) {
    $arguments = @{ SkipBuild = $true; Instance = $instance }
    if ($version) {
        $arguments.PackageVersion = $version
    }
    $output = & (Join-Path $PSScriptRoot 'package.ps1') @arguments
    $package = ($output | Where-Object { $_ -match '\.msi \(' } | Select-Object -Last 1) -replace ' \(.*$'
    if (-not $package -or -not (Test-Path $package)) {
        throw "(package): expected package.ps1 to name the package it built | received: $($output -join '; ')"
    }
    $package
}

# The entries of the test instance in Installed apps.
function Get-InstalledEntries {
    $roots = 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    @(Get-ItemProperty $roots -ErrorAction SilentlyContinue | Where-Object DisplayName -eq $instance)
}

# The processes that run from the installed folder.
function Get-InstalledProcesses {
    $names = 'syspilot', 'syspilot-svc', 'syspilot-sensors', 'syspilot-update', 'syspilot-tune'
    @(Get-Process $names -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$folder\*" })
}

New-Item -ItemType Directory -Force $logs | Out-Null
$built = (& (Join-Path $repository 'target\x86_64-pc-windows-msvc\release\syspilot-svc.exe') --version).Split(' ')[-1]
$parts = $built.Split('.')
$newer = '{0}.{1}.{2}' -f $parts[0], $parts[1], ([int] $parts[2] + 1)
$first = New-TestPackage ''
$second = New-TestPackage $newer
$app = $null

try {
    # 1. What the script installer of 0.1 left: its two scripts, its own
    # Installed apps entry, and files Windows Installer has no record of.
    New-Item -ItemType Directory -Force $folder | Out-Null
    Set-Content (Join-Path $folder 'install.ps1') '# left by the script installer'
    Set-Content (Join-Path $folder 'uninstall.ps1') '# left by the script installer'
    Set-Content (Join-Path $folder 'README.md') 'an older text'
    New-Item -ItemType Directory -Force (Join-Path $folder 'en-us'), (Join-Path $folder 'left-by-0.1') | Out-Null
    Set-Content (Join-Path $folder 'left-by-0.1\a-file.txt') 'a file no package knows'
    New-Item -Force $scriptEntry | Out-Null
    Set-ItemProperty $scriptEntry -Name DisplayName -Value $instance

    Invoke-Installer 'install' @('/i', "`"$first`"")
    Assert-Same 'service after install' 'Running' ([string](Get-Service $instance).Status)
    Assert-Same 'service start type' 'Automatic' ([string](Get-Service $instance).StartType)
    Assert-Same 'start menu shortcut' $true (Test-Path $shortcut)
    Assert-Same 'script installer removed' $false (Test-Path (Join-Path $folder 'install.ps1'))
    Assert-Same 'script uninstaller removed' $false (Test-Path (Join-Path $folder 'uninstall.ps1'))
    $staged = Join-Path $repository "dist\$instance-$built-x64\README.md"
    Assert-Same 'older file replaced' (Get-FileHash $staged).Hash (Get-FileHash (Join-Path $folder 'README.md')).Hash
    # @(): PowerShell hands a single result back as the item, not as a list.
    $entries = @(Get-InstalledEntries)
    Assert-Same 'entries in Installed apps' 1 $entries.Count
    Assert-Same 'installed version' $built $entries[0].DisplayVersion
    Assert-Same 'installed location' "$folder\" $entries[0].InstallLocation

    # 2. An update, with a profile to keep and the app open.
    $profile = Join-Path $logs 'profile.toml'
    Set-Content -Encoding utf8 $profile @'
schema = 1
updates = "automatic"

[[custom]]
id = "custom.package-test"
title = "Package test value"
action = { registry = { scope = "machine", key = 'SOFTWARE\SysPilot-Test\Msi', name = "Kept", data = { dword = 1 } } }
'@
    & $service --instance $instance apply $profile | Out-Null
    Assert-Same 'setting applied' 1 (Get-ItemProperty $scratch).Kept
    New-Item -ItemType Directory -Force $preferences | Out-Null
    Set-Content (Join-Path $preferences 'app.toml') 'level = "advanced"'
    $app = Start-Process (Join-Path $folder 'syspilot.exe') -ArgumentList '--instance', $instance -PassThru
    Start-Sleep -Seconds 8
    if ($app.HasExited) {
        Write-Warning 'The app did not stay open here, so closing it is not tested.'
    }

    Invoke-Installer 'update' @('/i', "`"$second`"")
    Assert-Same 'app closed by the update' $true $app.HasExited
    Assert-Same 'service after update' 'Running' ([string](Get-Service $instance).Status)
    $entries = @(Get-InstalledEntries)
    Assert-Same 'entries in Installed apps after update' 1 $entries.Count
    Assert-Same 'updated version' $newer $entries[0].DisplayVersion
    $kept = (& $service --instance $instance export) -join "`n"
    Assert-Same 'update mode kept' $true ($kept -match 'updates = "automatic"')
    Assert-Same 'custom action kept' $true ($kept -match 'custom\.package-test')
    Assert-Same 'setting still applied' 1 (Get-ItemProperty $scratch).Kept
    Assert-Same 'older package refused' 1603 (Start-Process msiexec.exe -Wait -PassThru -ArgumentList '/i', "`"$first`"", '/qn', '/norestart').ExitCode

    # 3. Removal, again with the app open.
    $app = Start-Process (Join-Path $folder 'syspilot.exe') -ArgumentList '--instance', $instance -PassThru
    Start-Sleep -Seconds 6
    Invoke-Installer 'remove' @('/x', "`"$second`"")
    Assert-Same 'service left' $false ([bool](Get-Service $instance -ErrorAction SilentlyContinue))
    Assert-Same 'service registration left' $false (Test-Path "HKLM:\SYSTEM\CurrentControlSet\Services\$instance")
    Assert-Same 'event log source left' $false (Test-Path "HKLM:\SYSTEM\CurrentControlSet\Services\EventLog\Application\$instance")
    Assert-Same 'processes left' 0 @(Get-InstalledProcesses).Count
    Assert-Same 'files left in the install folder' '' ((Get-LeftoverName $folder) -join ', ')
    Assert-Same 'install folder left' $false (Test-Path $folder)
    Assert-Same 'data folder left' $false (Test-Path $data)
    Assert-Same 'start menu shortcut left' $false (Test-Path $shortcut)
    Assert-Same 'user preferences left' $false (Test-Path $preferences)
    Assert-Same 'entries left in Installed apps' 0 @(Get-InstalledEntries).Count
    Assert-Same 'setting restored' $false (Test-Path $scratch)
    'The package installs, updates and removes cleanly.'
} finally {
    # Whatever a failed step left: remove it without hiding the failure.
    if ($app -and -not $app.HasExited) {
        Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
    }
    foreach ($package in $second, $first) {
        Start-Process msiexec.exe -ArgumentList '/x', "`"$package`"", '/qn', '/norestart' -Wait
    }
    Remove-Item -Recurse -Force $folder, $preferences, $scriptEntry, $scratch -ErrorAction SilentlyContinue
}
