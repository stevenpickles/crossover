<#
.SYNOPSIS
    Stage the release executables into tools\ and build the Chocolatey package.

.DESCRIPTION
    Copies target\release\crossover.exe and crossover-svc.exe next to the
    install scripts (they are embedded in the .nupkg), stamps the version to
    match apps/crossover/Cargo.toml unless overridden, and runs `choco pack`.

    Build the release binaries first:  cargo build --release -p crossover -p crossover-svc

.PARAMETER Version
    Package version. Defaults to the version in apps/crossover/Cargo.toml.
#>
[CmdletBinding()]
param(
    [string]$Version
)

$ErrorActionPreference = 'Stop'

$chocoDir = Split-Path -Parent $PSCommandPath
$toolsDir = Join-Path $chocoDir 'tools'
$repoRoot = Resolve-Path (Join-Path $chocoDir '..\..')
$releaseDir = Join-Path $repoRoot 'target\release'

foreach ($binary in @('crossover.exe', 'crossover-svc.exe')) {
    $source = Join-Path $releaseDir $binary
    if (-not (Test-Path $source)) {
        throw "$binary not found in $releaseDir; run: cargo build --release -p crossover -p crossover-svc"
    }
    Copy-Item -Path $source -Destination $toolsDir -Force
}

if (-not $Version) {
    $cargoToml = Get-Content (Join-Path $repoRoot 'apps\crossover\Cargo.toml') -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') { $Version = $Matches[1] }
    else { throw 'Could not read the version from apps\crossover\Cargo.toml; pass -Version.' }
}

Write-Host "Packing crossover $Version ..."
& choco pack (Join-Path $chocoDir 'crossover.nuspec') --version $Version --outputdirectory $chocoDir
Write-Host ''
Write-Host "Built crossover.$Version.nupkg in $chocoDir"
Write-Host 'Install from here:  choco install crossover -y -s .'
Write-Host "Or push to a feed:  choco push crossover.$Version.nupkg -s <feed-url> --api-key <key>"
