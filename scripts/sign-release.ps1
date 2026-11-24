<#
.SYNOPSIS
    Signs a SysPilot installer with the release key, so that installed copies
    accept it as an update.

.DESCRIPTION
    An installed SysPilot installs a new version only when the package has a
    signature from a key listed in installer\update-signers. The private half
    of that key stays with the maintainer, in an SSH agent, and never reaches
    the build: a release is built by CI as a draft, signed here, and only
    then published.

    The signature is an OpenSSH file signature (`ssh-keygen -Y sign`) with the
    namespace `syspilot-update`, written next to the package as <package>.sig.
    The agent asks for approval when the key is used.

    With -Tag, the package is downloaded from that release (a draft counts),
    signed, and the signature is uploaded to it. -Publish then takes the
    release out of draft, which is what offers it to every installed copy.

.EXAMPLE
    ./scripts/sign-release.ps1 -Package dist\SysPilot-0.2.0-x64.msi

.EXAMPLE
    ./scripts/sign-release.ps1 -Tag v0.2.0 -Publish
#>
[CmdletBinding(DefaultParameterSetName = 'Package')]
param(
    # A package on this machine.
    [Parameter(Mandatory, ParameterSetName = 'Package')] [string] $Package,
    # A release on GitHub, by its tag.
    [Parameter(Mandatory, ParameterSetName = 'Tag')] [string] $Tag,
    # After signing a release by tag: publish it.
    [Parameter(ParameterSetName = 'Tag')] [switch] $Publish,
    # The name of the signing key in installer\update-signers.
    [string] $Signer = 'syspilot'
)

$ErrorActionPreference = 'Stop'
$repository = Split-Path -Parent $PSScriptRoot
$signers = Join-Path $repository 'installer\update-signers'
$namespace = 'syspilot-update'
# Windows' own OpenSSH: it is the one that talks to the agent's pipe.
$keygen = Join-Path $env:SystemRoot 'System32\OpenSSH\ssh-keygen.exe'
if (-not (Test-Path $keygen)) {
    throw "(ssh-keygen): expected the OpenSSH client of Windows | received: no $keygen"
}

if ($Tag) {
    $downloads = Join-Path $repository "dist\release-$Tag"
    if (Test-Path $downloads) {
        Remove-Item -Recurse -Force $downloads
    }
    gh release download $Tag --pattern '*.msi' --dir $downloads
    if ($LASTEXITCODE -ne 0) {
        throw "(release $Tag): expected gh to download its package | received: exit code $LASTEXITCODE"
    }
    $found = @(Get-ChildItem $downloads -Filter '*.msi')
    if ($found.Count -ne 1) {
        throw "(release $Tag): expected one .msi | received: $($found.Count)"
    }
    $Package = $found[0].FullName
}
$Package = (Resolve-Path $Package).Path

# The public key to sign with, from the list the updater is built with: a
# signature from any other key would be refused by every installed copy.
$line = Get-Content $signers | Where-Object { $_ -match "^$([regex]::Escape($Signer))\s" } | Select-Object -First 1
if ($line -notmatch '(ssh-ed25519 \S+)') {
    throw "(signer): expected a line for $Signer with an ssh-ed25519 key in $signers | received: none"
}
$publicKey = Join-Path ([IO.Path]::GetTempPath()) "syspilot-release-key-$PID.pub"
Set-Content -Encoding ascii -Path $publicKey -Value $Matches[1]

$signature = "$Package.sig"
if (Test-Path $signature) {
    Remove-Item -Force $signature
}
try {
    # With a public key, ssh-keygen asks the agent for the private half.
    & $keygen -Y sign -f $publicKey -n $namespace -O hashalg=sha512 $Package
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $signature)) {
        throw "(signing): expected the agent to sign with the key $Signer | received: exit code $LASTEXITCODE. Is the agent running and the key in it?"
    }
} finally {
    Remove-Item -Force $publicKey
}

# Check it the way a user can, with OpenSSH and the list in the repository.
# ssh-keygen reads the signed file from its standard input.
$verify = Start-Process -FilePath $keygen -NoNewWindow -Wait -PassThru -RedirectStandardInput $Package `
    -ArgumentList '-Y', 'verify', '-f', "`"$signers`"", '-I', $Signer, '-n', $namespace, '-s', "`"$signature`""
if ($verify.ExitCode -ne 0) {
    throw "(signature): expected ssh-keygen to verify $signature | received: exit code $($verify.ExitCode)"
}

# And the way an installed copy will, with the updater itself when it has
# been built: the same code, the same compiled-in keys.
$updater = Join-Path $repository 'target\x86_64-pc-windows-msvc\release\syspilot-update.exe'
if (Test-Path $updater) {
    & $updater verify $Package $signature
    if ($LASTEXITCODE -ne 0) {
        throw "(signature): expected the updater to accept $signature | received: exit code $LASTEXITCODE"
    }
} else {
    Write-Warning 'The updater is not built here, so only ssh-keygen checked the signature.'
}

if ($Tag) {
    gh release upload $Tag $signature --clobber
    if ($LASTEXITCODE -ne 0) {
        throw "(release $Tag): expected gh to upload the signature | received: exit code $LASTEXITCODE"
    }
    if ($Publish) {
        gh release edit $Tag --draft=false
        if ($LASTEXITCODE -ne 0) {
            throw "(release $Tag): expected gh to publish it | received: exit code $LASTEXITCODE"
        }
        "Release $Tag is signed and published."
    } else {
        "Release $Tag is signed. It is offered to installed copies once it is published."
    }
}
$signature
