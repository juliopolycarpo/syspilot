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
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
'@
. (Join-Path $PSScriptRoot 'window-finder.ps1')

# Per-monitor aware (-4), so the rectangle is in physical pixels.
[void][SysPilot.Native]::SetProcessDpiAwarenessContext([IntPtr]::new(-4))

# Not Process.MainWindowHandle: for an elevated process it can be an overlay window.
$window = Wait-SysPilotWindow -ProcessId $ProcessId -TimeoutSeconds $TimeoutSeconds

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
