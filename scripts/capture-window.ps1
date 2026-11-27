<#
.SYNOPSIS
    Saves a PNG of a process's main window.

.DESCRIPTION
    Uses PrintWindow with full-content rendering, so the window is captured
    as composed (including WinUI content) even when other windows cover it.
    The capture is in physical pixels at the window's current DPI.

.EXAMPLE
    ./scripts/capture-window.ps1 -ProcessId 1234 -Path target/ui-shots/home.png
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [int] $ProcessId,
    [Parameter(Mandatory)] [string] $Path,
    # Seconds to wait for the window to appear.
    [int] $TimeoutSeconds = 20
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -Namespace SysPilot -Name Native -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out RECT rect);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr window, IntPtr dc, uint flags);
[DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr context);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc enumFunc, IntPtr param);
public delegate bool EnumWindowsProc(IntPtr window, IntPtr param);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr window, System.Text.StringBuilder text, int maxCount);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
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

# Per-monitor aware (-4), so the rectangle is in physical pixels.
[void][SysPilot.Native]::SetProcessDpiAwarenessContext([IntPtr]::new(-4))

$deadline = (Get-Date).AddSeconds($TimeoutSeconds)
do {
    # Elevated WinUI processes create a 50x50 UAC_InputIndicatorOverlayWnd that
    # Process.MainWindowHandle can pick instead of the SysPilot window.
    $window = [SysPilot.Native]::FindMainWindow([uint32]$ProcessId)
    if ($window -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 100
} while ((Get-Date) -lt $deadline)
if ($window -eq [IntPtr]::Zero) {
    throw "(window): expected process $ProcessId to show a window within $TimeoutSeconds seconds | received: none"
}

$rect = New-Object SysPilot.Native+RECT
[void][SysPilot.Native]::GetWindowRect($window, [ref] $rect)
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) {
    throw "(window): expected a visible window | received: ${width}x${height}"
}

$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
try {
    $dc = $graphics.GetHdc()
    # 2 = PW_RENDERFULLCONTENT: ask the compositor for the real content.
    $printed = [SysPilot.Native]::PrintWindow($window, $dc, 2)
    $graphics.ReleaseHdc($dc)
    if (-not $printed) { throw "(window): PrintWindow failed for process $ProcessId" }
    $directory = Split-Path -Parent $Path
    if ($directory) { New-Item -ItemType Directory -Force -Path $directory | Out-Null }
    $bitmap.Save((Join-Path (Get-Location) $Path), [System.Drawing.Imaging.ImageFormat]::Png)
} finally {
    $graphics.Dispose()
    $bitmap.Dispose()
}
"$Path (${width}x${height})"
