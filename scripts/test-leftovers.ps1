<#
.SYNOPSIS
    Checks that leftovers.ps1 lists a folder's contents and never searches
    outside it.

.DESCRIPTION
    Builds a scratch tree under the temp directory in which an item named
    like the missing folder sits deeper in its parent: the trap that made
    test-package.ps1 walk all of Program Files. Needs nothing else, so CI
    runs it.

.EXAMPLE
    ./scripts/test-leftovers.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'leftovers.ps1')

$failures = @()
function Assert-Equal($Case, $Expected, $Received) {
    if ($Expected -ne $Received) {
        $script:failures += "($Case): expected $Expected | received: $Received"
    }
}

$root = Join-Path ([IO.Path]::GetTempPath()) "SysPilot-Test-leftovers-$PID"
try {
    # The namesake: what Get-ChildItem -Recurse -Path would find.
    New-Item -ItemType Directory -Force (Join-Path $root 'deeper\gone.x') | Out-Null
    Set-Content (Join-Path $root 'deeper\gone.x\inside.txt') 'not in the folder asked for'
    $kept = Join-Path $root 'kept'
    New-Item -ItemType Directory -Force (Join-Path $kept 'sub') | Out-Null
    Set-Content (Join-Path $kept 'sub\a.txt') 'a'
    $hidden = New-Item -Force (Join-Path $kept '.hidden')
    $hidden.Attributes = 'Hidden'
    $empty = New-Item -ItemType Directory -Force (Join-Path $root 'empty')

    # -join: a list compares element-wise in PowerShell, a string does not.
    Assert-Equal 'missing folder' '' ((Get-LeftoverName (Join-Path $root 'gone.x')) -join ', ')
    Assert-Equal 'empty folder' '' ((Get-LeftoverName $empty.FullName) -join ', ')
    Assert-Equal 'folder with items' '.hidden, a.txt, sub' ((Get-LeftoverName $kept | Sort-Object) -join ', ')
    Assert-Equal 'a file in place of the folder' 'a.txt' ((Get-LeftoverName (Join-Path $kept 'sub\a.txt')) -join ', ')
    # Brackets are wildcards to -Path; the name must be taken as it is.
    $bracketed = New-Item -ItemType Directory -Force (Join-Path $root 'with [brackets]')
    Set-Content -LiteralPath (Join-Path $bracketed.FullName 'b.txt') 'b'
    Assert-Equal 'folder with wildcard characters' 'b.txt' ((Get-LeftoverName $bracketed.FullName) -join ', ')
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failures) {
    $failures | ForEach-Object { Write-Error $_ -ErrorAction Continue }
    exit 1
}
'leftovers: all cases passed'
