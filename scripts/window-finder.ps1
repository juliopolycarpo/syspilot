<#
.SYNOPSIS
    Finds the SysPilot window of a process. Dot-source it; it is not run directly.

.DESCRIPTION
    Process.MainWindowHandle is not reliable for an elevated WinUI process: it
    can return the 50x50 UAC_InputIndicatorOverlayWnd the process also owns.
    This walks the top-level windows of the process instead and picks the
    visible one whose title starts with "SysPilot" (the app sets
    "SysPilot - <page>").

.EXAMPLE
    . (Join-Path $PSScriptRoot 'window-finder.ps1')
    $window = Wait-SysPilotWindow -ProcessId 1234 -TimeoutSeconds 20
#>

# Guarded so that a second dot-source in the same session does not re-add the type.
if (-not ('SysPilot.WindowFinder' -as [type])) {
    Add-Type -Namespace SysPilot -Name WindowFinder -MemberDefinition @'
[DllImport("user32.dll")] static extern bool EnumWindows(EnumWindowsProc enumFunc, IntPtr param);
delegate bool EnumWindowsProc(IntPtr window, IntPtr param);
[DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr window, System.Text.StringBuilder text, int maxCount);
[DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr window);

// Ordinal so that the match does not depend on the culture of the machine.
public static bool IsMainWindowTitle(string title) {
    return title != null && title.StartsWith("SysPilot", StringComparison.Ordinal);
}

// Returns IntPtr.Zero while the process has no visible, titled SysPilot window.
public static IntPtr FindMainWindow(uint pid) {
    IntPtr found = IntPtr.Zero;
    EnumWindows((window, _) => {
        uint windowPid;
        GetWindowThreadProcessId(window, out windowPid);
        if (windowPid != pid || !IsWindowVisible(window)) return true;
        var title = new System.Text.StringBuilder(256);
        GetWindowText(window, title, title.Capacity);
        if (!IsMainWindowTitle(title.ToString())) return true;
        found = window;
        return false;
    }, IntPtr.Zero);
    return found;
}
'@
}

function Wait-SysPilotWindow {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [int] $ProcessId,
        # Seconds to wait for the window to appear and get its title.
        [int] $TimeoutSeconds = 20
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        $window = [SysPilot.WindowFinder]::FindMainWindow([uint32] $ProcessId)
        if ($window -ne [IntPtr]::Zero) { return $window }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    throw "(window): expected process $ProcessId to show a SysPilot window within $TimeoutSeconds seconds | received: none"
}
