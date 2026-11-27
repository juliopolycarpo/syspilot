<#
.SYNOPSIS
    Operates a running SysPilot window the way assistive technology does.

.DESCRIPTION
    Finds a control by its accessible name through UI Automation and
    toggles, invokes or selects it. Used to review flows that need a click
    (confirmations, level switches) and, as a side effect, to check that
    the controls can be reached by name at all. Meant for `syspilot --demo`.

.EXAMPLE
    ./scripts/drive-app.ps1 -ProcessId 1234 -Name 'Turn off widgets' -Action Toggle
    ./scripts/drive-app.ps1 -ProcessId 1234 -List
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [int] $ProcessId,
    # Accessible name of the control.
    [string] $Name,
    [ValidateSet('Toggle', 'Invoke', 'Select', 'Expand')] [string] $Action = 'Invoke',
    # Lists the named controls of the window instead of acting.
    [switch] $List,
    # Lists the controls a keyboard user can reach that have no accessible
    # name, instead of acting. Nothing listed means none.
    [switch] $Unnamed,
    # Seconds to wait for the window to appear.
    [int] $TimeoutSeconds = 20
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
. (Join-Path $PSScriptRoot 'window-finder.ps1')
$automation = [System.Windows.Automation.AutomationElement]
# Not Process.MainWindowHandle: for an elevated process it can be an overlay window.
$window = Wait-SysPilotWindow -ProcessId $ProcessId -TimeoutSeconds $TimeoutSeconds
$root = $automation::FromHandle($window)
$everything = [System.Windows.Automation.Condition]::TrueCondition
$scope = [System.Windows.Automation.TreeScope]::Descendants

if ($List) {
    $root.FindAll($scope, $everything) |
        Where-Object { $_.Current.Name } |
        ForEach-Object { '{0,-14} {1}' -f $_.Current.ControlType.ProgrammaticName.Replace('ControlType.', ''), $_.Current.Name }
    return
}

if ($Unnamed) {
    $root.FindAll($scope, $everything) |
        Where-Object { $_.Current.IsKeyboardFocusable -and -not $_.Current.Name } |
        ForEach-Object { '{0} (id "{1}")' -f $_.Current.ControlType.ProgrammaticName.Replace('ControlType.', ''), $_.Current.AutomationId }
    return
}

$byName = New-Object System.Windows.Automation.PropertyCondition($automation::NameProperty, $Name)
$patterns = @{
    Toggle = [System.Windows.Automation.TogglePattern]::Pattern
    Invoke = [System.Windows.Automation.InvokePattern]::Pattern
    Select = [System.Windows.Automation.SelectionItemPattern]::Pattern
    Expand = [System.Windows.Automation.ExpandCollapsePattern]::Pattern
}
# Several elements can share a name (a row and the switch inside it); the
# one that supports the action is the one meant.
$target = $root.FindAll($scope, $byName) | Where-Object {
    $pattern = $null
    $_.TryGetCurrentPattern($patterns[$Action], [ref] $pattern)
} | Select-Object -First 1
if (-not $target) {
    throw "(control): expected a control named '$Name' that supports $Action | received: none"
}
$pattern = $target.GetCurrentPattern($patterns[$Action])
switch ($Action) {
    'Toggle' { $pattern.Toggle() }
    'Invoke' { $pattern.Invoke() }
    'Select' { $pattern.Select() }
    'Expand' { $pattern.Expand() }
}
"$Action '$Name'"
