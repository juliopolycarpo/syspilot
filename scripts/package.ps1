<#
.SYNOPSIS
    Builds SysPilot and assembles a release folder and zip under dist\.

.DESCRIPTION
    The package holds the service, the sensor worker, the app with its
    Windows App SDK runtime files, the install and uninstall scripts and the
    licence texts. PawnIO modules are added when they have been fetched with
    `cargo xtask fetch-modules`; they stay separate files with their own
    licence text. The PawnIO driver itself is never part of a package.

.EXAMPLE
    cargo xtask fetch-modules
    ./scripts/package.ps1
#>
[CmdletBinding()]
param(
    # Assemble from an existing release build instead of building.
    [switch] $SkipBuild
)

$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$release = Join-Path $repository 'target\x86_64-pc-windows-msvc\release'

if (-not $SkipBuild) {
    cargo build --release --workspace --locked --manifest-path (Join-Path $repository 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) {
        throw "(build): expected cargo to exit with 0 | received: $LASTEXITCODE"
    }
}

$executables = 'syspilot.exe', 'syspilot-svc.exe', 'syspilot-sensors.exe'
# The window icon is a file the app's build puts beside it.
foreach ($executable in $executables + 'syspilot.ico') {
    if (-not (Test-Path (Join-Path $release $executable))) {
        throw "(release build): expected $executable in $release | received: no such file"
    }
}

$version = (& (Join-Path $release 'syspilot-svc.exe') --version).Split(' ')[-1]
$name = "SysPilot-$version-x64"
$stage = Join-Path $repository "dist\$name"
if (Test-Path $stage) {
    Remove-Item -Recurse -Force $stage
}
New-Item -ItemType Directory -Force $stage | Out-Null

# The release directory holds the runtime files next to cargo's own
# output. Everything that is not cargo's is the runtime. A developer may
# have put modules there to try the worker; the package takes them from
# the fetched directory only.
$cargoDirectories = 'build', 'deps', 'examples', 'incremental', '.fingerprint', 'pawnio-modules'
$cargoFiles = '*.d', '*.pdb', '*.rlib', '*.exe', '.cargo-lock'
Get-ChildItem -Force $release | Where-Object {
    $item = $_
    if ($item.PSIsContainer) {
        $cargoDirectories -notcontains $item.Name
    } else {
        -not ($cargoFiles | Where-Object { $item.Name -like $_ })
    }
} | Copy-Item -Recurse -Destination $stage
foreach ($executable in $executables) {
    Copy-Item (Join-Path $release $executable) $stage
}

foreach ($document in 'LICENSE', 'THIRD-PARTY.md', 'README.md') {
    Copy-Item (Join-Path $repository $document) $stage
}
foreach ($script in 'install.ps1', 'uninstall.ps1') {
    Copy-Item (Join-Path $PSScriptRoot $script) $stage
}

$modules = Join-Path $repository 'pawnio-modules'
if (Test-Path $modules) {
    Copy-Item -Recurse $modules (Join-Path $stage 'pawnio-modules')
} else {
    Write-Warning 'No pawnio-modules directory: the package will not read driver-backed sensors. Run `cargo xtask fetch-modules` first.'
}

$archive = Join-Path $repository "dist\$name.zip"
if (Test-Path $archive) {
    Remove-Item -Force $archive
}
Compress-Archive -Path $stage -DestinationPath $archive
$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
Set-Content -Encoding ascii -Path "$archive.sha256" -Value "$hash  $name.zip"

'{0} ({1:N1} MB)' -f $archive, ((Get-Item $archive).Length / 1MB)
"sha256 $hash"
