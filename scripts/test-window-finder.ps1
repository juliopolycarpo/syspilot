<#
.SYNOPSIS
    Checks that window-finder.ps1 picks the SysPilot window and nothing else.

.DESCRIPTION
    Opens real windows in this process, among them a small untitled one like
    the UAC overlay an elevated WinUI process owns, and fails if
    Wait-SysPilotWindow returns anything but the visible "SysPilot - ..." one.
    Needs an interactive desktop, so CI does not run it.

.EXAMPLE
    ./scripts/test-window-finder.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
. (Join-Path $PSScriptRoot 'window-finder.ps1')

$failures = @()
function Assert-Equal($Case, $Expected, $Received) {
    if ($Expected -ne $Received) {
        $script:failures += "($Case): expected $Expected | received: $Received"
    }
}

function New-TestWindow([string] $Title, [bool] $Visible) {
    $form = New-Object System.Windows.Forms.Form
    $form.Text = $Title
    $form.Size = New-Object System.Drawing.Size(50, 50)
    $form.ShowInTaskbar = $false
    # Creating the handle without Show() keeps the window invisible.
    if ($Visible) { $form.Show() } else { [void]$form.Handle }
    $form
}

$forms = @()
try {
    # 1. Title filter, without windows.
    Assert-Equal 'title: "SysPilot - Home"' $true ([SysPilot.WindowFinder]::IsMainWindowTitle('SysPilot - Home'))
    Assert-Equal 'title: empty' $false ([SysPilot.WindowFinder]::IsMainWindowTitle(''))
    Assert-Equal 'title: null' $false ([SysPilot.WindowFinder]::IsMainWindowTitle($null))
    Assert-Equal 'title: other prefix' $false ([SysPilot.WindowFinder]::IsMainWindowTitle('Not SysPilot'))
    Assert-Equal 'title: lower case' $false ([SysPilot.WindowFinder]::IsMainWindowTitle('syspilot'))

    # 2. Only decoys: no window, and the wait names the process and the timeout.
    $forms += New-TestWindow '' $true
    $forms += New-TestWindow 'SysPilot - hidden' $false
    $forms += New-TestWindow 'Other' $true
    Assert-Equal 'only decoys' ([IntPtr]::Zero) ([SysPilot.WindowFinder]::FindMainWindow([uint32]$PID))
    try {
        Wait-SysPilotWindow -ProcessId $PID -TimeoutSeconds 1 | Out-Null
        $failures += "(timeout): expected an error naming process $PID | received: a window"
    } catch {
        Assert-Equal '(timeout) message' $true ($_.Exception.Message -like "(window): expected process $PID to show*within 1 seconds*")
    }

    # 3. The real one among the decoys.
    $main = New-TestWindow 'SysPilot - Home' $true
    $forms += $main
    Assert-Equal 'real window' $main.Handle ([SysPilot.WindowFinder]::FindMainWindow([uint32]$PID))
    Assert-Equal 'wait returns it' $main.Handle (Wait-SysPilotWindow -ProcessId $PID -TimeoutSeconds 1)

    # 4. A process that owns no windows (the idle process, id 0) finds nothing.
    Assert-Equal 'other process' ([IntPtr]::Zero) ([SysPilot.WindowFinder]::FindMainWindow([uint32]0))
} finally {
    foreach ($form in $forms) { $form.Dispose() }
}

if ($failures) {
    $failures | ForEach-Object { Write-Error $_ -ErrorAction Continue }
    exit 1
}
'window-finder: all cases passed'
