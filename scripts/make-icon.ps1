<#
.SYNOPSIS
    Draws the SysPilot icon and writes it as a multi-size .ico file.

.DESCRIPTION
    The icon is a gauge: a dial with a needle, on a rounded blue tile. It is
    drawn as vectors at each size (no scaling of a bitmap). Run it again
    after changing the drawing; the result is committed.

.EXAMPLE
    ./scripts/make-icon.ps1
#>
[CmdletBinding()]
param(
    [string] $Path = (Join-Path (Split-Path -Parent $PSScriptRoot) 'crates\ui\assets\syspilot.ico')
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

function New-RoundedTile([single] $size, [single] $radius) {
    $tile = New-Object System.Drawing.Drawing2D.GraphicsPath
    $diameter = 2 * $radius
    $tile.AddArc(0, 0, $diameter, $diameter, 180, 90)
    $tile.AddArc($size - $diameter, 0, $diameter, $diameter, 270, 90)
    $tile.AddArc($size - $diameter, $size - $diameter, $diameter, $diameter, 0, 90)
    $tile.AddArc(0, $size - $diameter, $diameter, $diameter, 90, 90)
    $tile.CloseFigure()
    $tile
}

# One size of the icon, as PNG bytes.
function New-IconImage([int] $size) {
    $bitmap = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.SmoothingMode = 'AntiAlias'
    $graphics.PixelOffsetMode = 'HighQuality'
    $graphics.Clear([System.Drawing.Color]::Transparent)

    $tile = New-RoundedTile $size ($size * 0.22)
    $top = [System.Drawing.Color]::FromArgb(255, 0, 120, 212)
    $bottom = [System.Drawing.Color]::FromArgb(255, 0, 69, 150)
    $area = New-Object System.Drawing.RectangleF(0, 0, $size, $size)
    $fill = New-Object System.Drawing.Drawing2D.LinearGradientBrush($area, $top, $bottom, 90.0)
    $graphics.FillPath($fill, $tile)

    # The dial: three quarters of a circle, open at the bottom.
    $white = [System.Drawing.Color]::White
    $stroke = [Math]::Max(1.6, $size * 0.085)
    $pen = New-Object System.Drawing.Pen($white, $stroke)
    $pen.StartCap = 'Round'
    $pen.EndCap = 'Round'
    $inset = $size * 0.2
    $extent = $size - 2 * $inset
    $graphics.DrawArc($pen, $inset, $inset + $size * 0.04, $extent, $extent, 135, 270)

    # The needle, pointing up and to the right, and its hub.
    $centre = $size / 2
    $hubY = $centre + $size * 0.04
    $angle = -50 * [Math]::PI / 180
    $length = $extent * 0.36
    $graphics.DrawLine($pen, $centre, $hubY, $centre + $length * [Math]::Cos($angle), $hubY + $length * [Math]::Sin($angle))
    $hub = [Math]::Max(1.5, $size * 0.075)
    $graphics.FillEllipse((New-Object System.Drawing.SolidBrush($white)), $centre - $hub, $hubY - $hub, 2 * $hub, 2 * $hub)

    $graphics.Dispose()
    $stream = New-Object System.IO.MemoryStream
    if ($size -ge 256) {
        $bitmap.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    } else {
        # Smaller sizes are stored the classic way, which every reader of
        # .ico files understands: a BITMAPINFOHEADER whose height counts
        # the colour rows and the mask rows, bottom-up BGRA pixels, then a
        # one-bit mask (all zero: the alpha channel decides).
        $writer = New-Object System.IO.BinaryWriter($stream)
        $writer.Write([uint32] 40)
        $writer.Write([int32] $size)
        $writer.Write([int32] (2 * $size))
        $writer.Write([uint16] 1)
        $writer.Write([uint16] 32)
        $writer.Write([uint32] 0)
        $writer.Write([uint32] 0)
        $writer.Write([int32] 0)
        $writer.Write([int32] 0)
        $writer.Write([uint32] 0)
        $writer.Write([uint32] 0)
        for ($y = $size - 1; $y -ge 0; $y--) {
            for ($x = 0; $x -lt $size; $x++) {
                $writer.Write([int32] $bitmap.GetPixel($x, $y).ToArgb())
            }
        }
        $maskRow = [byte[]]::new([int] ([Math]::Ceiling($size / 32.0) * 4))
        for ($y = 0; $y -lt $size; $y++) {
            $writer.Write($maskRow)
        }
        $writer.Flush()
    }
    $bitmap.Dispose()
    , $stream.ToArray()
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$images = foreach ($size in $sizes) { , (New-IconImage $size) }

New-Item -ItemType Directory -Force (Split-Path -Parent $Path) | Out-Null
$file = [System.IO.File]::Create($Path)
$writer = New-Object System.IO.BinaryWriter($file)
# ICONDIR, then one ICONDIRENTRY per image, then the images.
$writer.Write([uint16] 0)
$writer.Write([uint16] 1)
$writer.Write([uint16] $sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($index = 0; $index -lt $sizes.Count; $index++) {
    # A width or height byte of 0 means 256.
    $side = [byte] ($sizes[$index] % 256)
    $writer.Write($side)
    $writer.Write($side)
    $writer.Write([byte] 0)
    $writer.Write([byte] 0)
    $writer.Write([uint16] 1)
    $writer.Write([uint16] 32)
    $writer.Write([uint32] $images[$index].Length)
    $writer.Write([uint32] $offset)
    $offset += $images[$index].Length
}
foreach ($image in $images) {
    $writer.Write([byte[]] $image)
}
$writer.Dispose()
'{0} ({1:N0} bytes, {2} sizes)' -f $Path, (Get-Item $Path).Length, $sizes.Count
