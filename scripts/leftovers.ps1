<#
.SYNOPSIS
    Lists what is left in a folder. Dot-source it; it is not run directly.

.DESCRIPTION
    `Get-ChildItem -Recurse -Path X` on a folder X that does not exist does
    not fail: it takes the last part of X as a name filter and searches the
    parent folder, recursively, for anything with that name. For an install
    folder under Program Files that is a walk of all of Program Files, which
    takes minutes and reports any namesake as a leftover. -LiteralPath has no
    such fallback.

.EXAMPLE
    . (Join-Path $PSScriptRoot 'leftovers.ps1')
    $left = Get-LeftoverName 'C:\Program Files\SysPilot-Test.msi'
#>

# The names of everything under $Folder, hidden items included; none when
# $Folder does not exist, and its own name when it is a file. A folder that
# cannot be read is named as "unreadable: <path>", so that a check built on
# this says what it could not look at. Never looks outside $Folder.
function Get-LeftoverName([Parameter(Mandatory)] [string] $Folder) {
    if (-not (Test-Path -LiteralPath $Folder)) {
        return @()
    }
    $names = @(Get-ChildItem -LiteralPath $Folder -Recurse -Force -ErrorVariable unreadable -ErrorAction SilentlyContinue |
        ForEach-Object Name)
    $names + @($unreadable | ForEach-Object { "unreadable: $($_.TargetObject)" })
}
