<#
.SYNOPSIS
    Writes the checksum file published next to a package. Dot-source it; it
    is not run directly.

.DESCRIPTION
    The file holds one line in the format `sha256sum -c` reads: the hash, two
    spaces, the file name and a line feed. Set-Content ends the line with a
    carriage return and a line feed, and sha256sum then looks for a file whose
    name ends in a carriage return (#76).

.EXAMPLE
    . (Join-Path $PSScriptRoot 'checksum.ps1')
    $hash = Write-Checksum 'dist\SysPilot-0.4.0-x64.msi'
#>

# Writes "<sha256>  <name>" and a line feed to "$File.sha256", in ASCII with
# no byte order mark, and returns the hash in lower case.
function Write-Checksum([Parameter(Mandatory)] [string] $File) {
    $item = Get-Item -LiteralPath $File
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $item.FullName).Hash.ToLowerInvariant()
    # A full path: .NET resolves a relative one against the process's
    # directory, not PowerShell's current location.
    [IO.File]::WriteAllText("$($item.FullName).sha256", "$hash  $($item.Name)`n", [Text.Encoding]::ASCII)
    $hash
}
