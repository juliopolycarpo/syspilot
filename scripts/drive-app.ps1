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
    [switch] $Unnamed
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
Add-Type -Namespace SysPilot -Name DriveWin -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc enumFunc, IntPtr param);
public delegate bool EnumWindowsProc(IntPtr window, IntPtr param);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr window, System.Text.StringBuilder text, int maxCount);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
public static IntPtr FindMainWindow(uint pid) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((window, _) => {
        uint windowPid;
        GetWindowThreadProcessId(window, out windowPid);
        if (windowPid == pid && IsWindowVisible(window)) {
            var title = new System.Text.StringBuilder(256);
            GetWindowText(window, title, 256);
            if (title.ToString().StartsWith("SysPilot")) {
                found = window;
                return false;
            }
        }
        return true;
    }, IntPtr.Zero);
    return found;
}
'@
$automation = [System.Windows.Automation.AutomationElement]
$window = [SysPilot.DriveWin]::FindMainWindow([uint32]$ProcessId)
if ($window -eq [IntPtr]::Zero) {
    $window = (Get-Process -Id $ProcessId).MainWindowHandle
}
if ($window -eq [IntPtr]::Zero) {
    throw "(window): expected process $ProcessId to have a window | received: none"
}
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
