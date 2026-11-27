<#
.SYNOPSIS
    Checks that checksum.ps1 writes the line `sha256sum -c` reads.

.DESCRIPTION
    Hashes a scratch file of known content and compares the checksum file's
    bytes with what sha256sum itself writes: no carriage return, no byte
    order mark (#76). Needs nothing else, so CI runs it, in PowerShell 7 and
    in Windows PowerShell 5.1.

.EXAMPLE
    ./scripts/test-checksum.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'checksum.ps1')

$failures = @()
function Assert-Equal($Case, $Expected, $Received) {
    if ($Expected -ne $Received) {
        $script:failures += "($Case): expected $Expected | received: $Received"
    }
}

# The bytes as text a failure can show: a carriage return or a byte order
# mark is otherwise invisible.
function Show-Bytes([byte[]] $Bytes) {
    ($Bytes | ForEach-Object {
            # Every matching case runs unless one breaks.
            switch ($_) { 10 { '\n'; break } 13 { '\r'; break } { $_ -lt 32 -or $_ -gt 126 } { '\x{0:x2}' -f $_; break } default { [char] $_ } }
        }) -join ''
}

$root = Join-Path ([IO.Path]::GetTempPath()) "SysPilot-Test-checksum-$PID"
try {
    New-Item -ItemType Directory -Force $root | Out-Null
    # Brackets are wildcards to -Path; the name must be taken as it is.
    $package = Join-Path $root 'SysPilot-[0.0.0]-x64.msi'
    [IO.File]::WriteAllBytes($package, [Text.Encoding]::ASCII.GetBytes('abc'))
    # SHA-256 of "abc", from FIPS 180-2.
    $expected = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'

    Assert-Equal 'returned hash' $expected (Write-Checksum $package)
    $written = [IO.File]::ReadAllBytes("$package.sha256")
    $line = [Text.Encoding]::ASCII.GetBytes("$expected  SysPilot-[0.0.0]-x64.msi`n")
    Assert-Equal 'checksum file' (Show-Bytes $line) (Show-Bytes $written)

    # A relative path is taken from PowerShell's location, and the file name
    # in the line carries no folder.
    Remove-Item -LiteralPath "$package.sha256"
    Push-Location $root
    try {
        Write-Checksum 'SysPilot-[0.0.0]-x64.msi' | Out-Null
    } finally {
        Pop-Location
    }
    Assert-Equal 'checksum file from a relative path' (Show-Bytes $line) (Show-Bytes ([IO.File]::ReadAllBytes("$package.sha256")))
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
}

if ($failures) {
    $failures | ForEach-Object { Write-Error $_ -ErrorAction Continue }
    exit 1
}
'checksum: all cases passed'
