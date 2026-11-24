<#
.SYNOPSIS
    Builds SysPilot and its installer, a Windows Installer package under dist\.

.DESCRIPTION
    The package holds the service, the sensor worker, the updater, the app
    with its Windows App SDK runtime files and the licence texts. PawnIO
    modules are added when they have been fetched with
    `cargo xtask fetch-modules`; they stay separate files with their own
    licence text. The PawnIO driver itself is never part of a package.

    The installer is built with the WiX toolset, a .NET tool. When `wix` is
    not on the PATH, the pinned version is installed into target\wix-tool,
    which needs the .NET SDK and, the first time, network access.

    The package is not signed here. A release is signed afterwards with
    scripts\sign-release.ps1, by a key that never reaches the build.

.EXAMPLE
    cargo xtask fetch-modules
    ./scripts/package.ps1

.EXAMPLE
    ./scripts/package.ps1 -SkipBuild -Instance SysPilot-Test.msi
#>
[CmdletBinding()]
param(
    # Assemble from an existing release build instead of building.
    [switch] $SkipBuild,
    # For tests: an instance name starting with SysPilot-Test. The package
    # then installs under that name, next to an installed SysPilot.
    [string] $Instance = 'SysPilot',
    # For tests of updating: the version the package declares instead of the
    # one the executables were built with. Never for a release.
    [string] $PackageVersion,
    # Thumbprint of an Authenticode code-signing certificate in the current
    # user's or the machine's store. When given, SysPilot's own executables
    # and the package are signed with it, so that Windows names a publisher.
    # This is separate from the release signature the updater checks.
    [string] $SigningThumbprint,
    # Timestamp server for the Authenticode signatures, so that they outlive
    # the certificate. Empty for none.
    [string] $TimestampServer = 'http://timestamp.digicert.com'
)

$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$release = Join-Path $repository 'target\x86_64-pc-windows-msvc\release'
# The WiX version the installer source is written for. Version 5 is the last
# one without the maintenance-fee licence terms of later versions.
$wixVersion = '5.0.2'

if ($Instance -ne 'SysPilot' -and -not $Instance.StartsWith('SysPilot-Test')) {
    throw "(instance): expected SysPilot or a name starting with SysPilot-Test | received: $Instance"
}

if (-not $SkipBuild) {
    cargo build --release --workspace --locked --manifest-path (Join-Path $repository 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) {
        throw "(build): expected cargo to exit with 0 | received: $LASTEXITCODE"
    }
}

$executables = 'syspilot.exe', 'syspilot-svc.exe', 'syspilot-sensors.exe', 'syspilot-update.exe'
# The window icon is a file the app's build puts beside it.
foreach ($executable in $executables + 'syspilot.ico') {
    if (-not (Test-Path (Join-Path $release $executable))) {
        throw "(release build): expected $executable in $release | received: no such file"
    }
}

$version = (& (Join-Path $release 'syspilot-svc.exe') --version).Split(' ')[-1]
if ($PackageVersion) {
    if ($Instance -eq 'SysPilot') {
        throw '(package version): expected -PackageVersion only with a test instance | received: the product'
    }
    $version = $PackageVersion
}
$name = if ($Instance -eq 'SysPilot') { "SysPilot-$version-x64" } else { "$Instance-$version-x64" }
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
# Cargo's lock files changed names across versions (.cargo-lock, then
# .cargo-artifact-lock and .cargo-build-lock), hence the wildcard.
$cargoFiles = '*.d', '*.pdb', '*.rlib', '*.exe', '.cargo-*'
Get-ChildItem -Force $release | Where-Object {
    $item = $_
    if ($item.PSIsContainer) {
        $cargoDirectories -notcontains $item.Name
    } else {
        -not ($cargoFiles | Where-Object { $item.Name -like $_ })
    }
} | Copy-Item -Recurse -Destination $stage
# Refuse to package build leftovers, whatever a future cargo calls them.
$leftovers = Get-ChildItem -Force -File $stage | Where-Object { $_.Name -like '.*' }
if ($leftovers) {
    throw "(package): expected no hidden build files in the package | received: $($leftovers.Name -join ', ')"
}
foreach ($executable in $executables) {
    Copy-Item (Join-Path $release $executable) $stage
}

foreach ($document in 'LICENSE', 'THIRD-PARTY.md', 'README.md') {
    Copy-Item (Join-Path $repository $document) $stage
}

$modules = Join-Path $repository 'pawnio-modules'
if (Test-Path $modules) {
    Copy-Item -Recurse $modules (Join-Path $stage 'pawnio-modules')
} else {
    Write-Warning 'No pawnio-modules directory: the package will not read driver-backed sensors. Run `cargo xtask fetch-modules` first.'
}

# Signs one of SysPilot's own files with the Authenticode certificate.
function Add-PublisherSignature([string] $File) {
    $arguments = @{ FilePath = $File; Certificate = $certificate; HashAlgorithm = 'SHA256' }
    if ($TimestampServer) {
        $arguments.TimestampServer = $TimestampServer
    }
    $signature = Set-AuthenticodeSignature @arguments
    # A certificate nobody trusts yet still signs; anything else failed.
    if ($signature.Status -notin 'Valid', 'UnknownError') {
        throw "(signing): expected $File to be signed | received: $($signature.Status), $($signature.StatusMessage)"
    }
    '{0}: signed ({1})' -f (Split-Path -Leaf $File), $signature.Status
}

if ($SigningThumbprint) {
    $certificate = Get-ChildItem Cert:\CurrentUser\My, Cert:\LocalMachine\My -CodeSigningCert |
        Where-Object Thumbprint -eq $SigningThumbprint | Select-Object -First 1
    if (-not $certificate) {
        throw "(signing): expected a code-signing certificate with thumbprint $SigningThumbprint in a personal store | received: none"
    }
    # Only SysPilot's own files. The runtime and the PawnIO modules carry
    # their makers' signatures and must stay byte for byte as they came.
    foreach ($executable in $executables) {
        Add-PublisherSignature (Join-Path $stage $executable)
    }
}

# The WiX toolset: the one on the PATH, or the pinned one under target\.
$wix = (Get-Command wix -ErrorAction SilentlyContinue).Source
if (-not $wix) {
    $tools = Join-Path $repository 'target\wix-tool'
    $wix = Join-Path $tools 'wix.exe'
    if (-not (Test-Path $wix)) {
        $dotnet = (Get-Command dotnet -ErrorAction SilentlyContinue).Source
        if (-not $dotnet) {
            $dotnet = Join-Path $env:ProgramFiles 'dotnet\dotnet.exe'
        }
        if (-not (Test-Path $dotnet)) {
            throw '(wix): expected the WiX toolset or the .NET SDK to install it with | received: neither'
        }
        & $dotnet tool install wix --version $wixVersion --tool-path $tools
        if ($LASTEXITCODE -ne 0) {
            throw "(wix): expected dotnet to install WiX $wixVersion | received: exit code $LASTEXITCODE"
        }
    }
}
# WiX 5 was built for .NET 6; it runs on whichever newer runtime is there.
$env:DOTNET_ROLL_FORWARD = 'Major'
$found = (& $wix --version).Split('+')[0]
if ($found -ne $wixVersion) {
    throw "(wix): expected version $wixVersion | received: $found from $wix"
}
& $wix extension add --global "WixToolset.Util.wixext/$wixVersion"
if ($LASTEXITCODE -ne 0) {
    throw "(wix): expected the Util extension to install | received: exit code $LASTEXITCODE"
}

$package = Join-Path $repository "dist\$name.msi"
if (Test-Path $package) {
    Remove-Item -Force $package
}
& $wix build (Join-Path $repository 'installer\syspilot.wxs') `
    -arch x64 -ext WixToolset.Util.wixext -pdbtype none `
    -d "Stage=$stage" -d "Version=$version" -d "Instance=$Instance" `
    -o $package
if ($LASTEXITCODE -ne 0) {
    throw "(installer): expected wix to build the package | received: exit code $LASTEXITCODE"
}
if ($SigningThumbprint) {
    Add-PublisherSignature $package
}

$hash = (Get-FileHash -Algorithm SHA256 $package).Hash.ToLowerInvariant()
Set-Content -Encoding ascii -Path "$package.sha256" -Value "$hash  $name.msi"

'{0} ({1:N1} MB)' -f $package, ((Get-Item $package).Length / 1MB)
"sha256 $hash"
