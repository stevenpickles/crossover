<#
.SYNOPSIS
    Stage the built executables into tools\ and build the Chocolatey package.

.DESCRIPTION
    Copies crossover.exe, crossover-svc.exe and crossover-layout.exe next to
    the install scripts (they are embedded in the .nupkg), stamps the package
    version, and runs `choco pack`.

    Usually invoked by scripts\build.ps1, which builds the binaries, derives
    the version from what they actually embed, and packages every artifact in
    one pass. Run it directly when you only want the .nupkg from binaries you
    already have.

    Build the release binaries first:
        cargo build --release -p crossover -p crossover-svc -p crossover-layout

.PARAMETER Version
    Package version. Defaults to the version in apps\crossover\Cargo.toml.
    Must be a version NuGet accepts: dots are not allowed in the pre-release
    label, so scripts\build.ps1 passes 0.2.0-dev-319-gabc1234 rather than the
    binaries' own 0.2.0-dev.319.gabc1234.

.PARAMETER BinariesDirectory
    Where to find the executables. Defaults to target\release.

.PARAMETER OutputDirectory
    Where to write the .nupkg. Defaults to this folder.

.OUTPUTS
    The full path of the package that was built.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$BinariesDirectory,
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'

$chocoDir = Split-Path -Parent $PSCommandPath
$toolsDir = Join-Path $chocoDir 'tools'
$repoRoot = (Resolve-Path (Join-Path $chocoDir '..\..')).Path

if (-not $BinariesDirectory) { $BinariesDirectory = Join-Path $repoRoot 'target\release' }
if (-not $OutputDirectory) { $OutputDirectory = $chocoDir }
$BinariesDirectory = [System.IO.Path]::GetFullPath($BinariesDirectory)
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)

foreach ($binary in @('crossover.exe', 'crossover-svc.exe', 'crossover-layout.exe')) {
    $source = Join-Path $BinariesDirectory $binary
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "$binary not found in $BinariesDirectory; run: cargo build --release -p crossover -p crossover-svc -p crossover-layout"
    }
    Copy-Item -LiteralPath $source -Destination $toolsDir -Force
}

# The notice that third-party material embedded in the binaries requires to
# travel with them (today: the editor's Go fonts, ADR 0019), copied verbatim
# from its one committed source. tools\ is what the nuspec packs, so the notice
# both ships and installs from there.
Copy-Item -LiteralPath (Join-Path $repoRoot 'packaging\THIRD-PARTY-NOTICES.txt') `
    -Destination $toolsDir -Force

if (-not $Version) {
    $cargoToml = Get-Content (Join-Path $repoRoot 'apps\crossover\Cargo.toml') -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') { $Version = $Matches[1] }
    else { throw 'Could not read the version from apps\crossover\Cargo.toml; pass -Version.' }
}
# NuGet's pre-release label is alphanumerics and hyphens only. Catching it
# here keeps the failure legible instead of surfacing as a choco parse error.
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z-]+)?$') {
    throw "Package version '$Version' is not a NuGet-compatible version (x.y.z or x.y.z-label, label without dots)."
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

Write-Host "Packing crossover $Version ..."
# 'Continue' for the duration: choco writes to stderr, which would become a
# terminating error under 'Stop' as soon as this script's output is
# redirected. The exit code is the verdict.
$previousPreference = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
try {
    & choco pack (Join-Path $chocoDir 'crossover.nuspec') --version $Version --outputdirectory $OutputDirectory | Out-Host
} finally {
    $ErrorActionPreference = $previousPreference
}
if ($LASTEXITCODE -ne 0) {
    throw "choco pack failed with exit code $LASTEXITCODE."
}

$packagePath = Join-Path $OutputDirectory "crossover.$Version.nupkg"
if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
    throw "choco pack reported success but $packagePath does not exist."
}

# Chocolatey ignores pre-release versions unless asked for them, and every
# build that is not a tagged release has a pre-release label. Without --pre
# the install fails with "crossover was not found with the source(s) listed",
# which reads like a broken package rather than a filtered one.
$prerelease = if ($Version -match '-') { ' --pre' } else { '' }

Write-Host ''
Write-Host "Built $packagePath"
Write-Host "Install from here:  choco upgrade crossover -y$prerelease -s `"$OutputDirectory`""
Write-Host "Or push to a feed:  choco push crossover.$Version.nupkg -s <feed-url> --api-key <key>"
Write-Output $packagePath
