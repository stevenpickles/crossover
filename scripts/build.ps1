<#
.SYNOPSIS
    One command: verify, build, and package every Crossover deliverable.

.DESCRIPTION
    The local equivalent of a release run. In order:

      1. the CI gate  - cargo fmt --check, clippy (warnings denied),
                        cargo test --workspace, cargo deny (if installed)
      2. the build     - cargo build --release --locked --workspace
      3. verification  - both executables are x64 PE files carrying the same
                        embedded build version and Git commit as the source
                        they were built from
      4. the artifacts - a portable .zip (+ .sha256), the Chocolatey .nupkg,
                        and artifacts.json describing all of them

    The version is never invented here: it is read back out of the binaries,
    which stamp it in apps/build_identity.rs. A build made from uncommitted
    edits says so - '...dirty' in the version - rather than quietly passing
    for a release.

.PARAMETER Version
    Override the package version. Defaults to what the built binaries report.
    A pre-release label containing dots is converted for NuGet's benefit
    (0.1.0-dev.7.gabc1234 becomes 0.1.0-dev-7-gabc1234); the exact build
    version is recorded in artifacts.json either way.

.PARAMETER OutputDirectory
    Where the deliverables land. Defaults to dist\ at the repository root.

.PARAMETER SkipChecks
    Skip step 1. For iterating; artifacts.json records that the gate did not
    run, so a skipped check can never be mistaken for a passed one.

.PARAMETER SkipChocolatey
    Build the portable archive only. Implied when Chocolatey is not installed
    would be a silent downgrade, so that case is an error instead.

.EXAMPLE
    .\scripts\build.ps1
    Everything, from a clean gate to a .nupkg.

.EXAMPLE
    .\scripts\build.ps1 -SkipChecks -SkipChocolatey
    The fast loop: build and zip, nothing else.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$OutputDirectory,
    [switch]$SkipChecks,
    [switch]$SkipChocolatey
)

$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repoRoot 'dist' }
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
$releaseDirectory = Join-Path $repoRoot 'target\release'
$binaries = @('crossover.exe', 'crossover-svc.exe')
$startedAt = Get-Date

function Write-Step([string]$Message) {
    Write-Host ''
    Write-Host "==> $Message" -ForegroundColor Cyan
}

function Invoke-Native([string]$Command, [string[]]$Arguments) {
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command $($Arguments -join ' ') failed with exit code $LASTEXITCODE."
    }
}

function Test-Tool([string]$Name) {
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

# The PE machine field, read straight out of the header. A wrong-architecture
# binary is otherwise indistinguishable from a right one until someone runs it.
function Get-PeMachine([string]$Path) {
    $stream = [System.IO.File]::OpenRead($Path)
    $reader = [System.IO.BinaryReader]::new($stream)
    try {
        if ($stream.Length -lt 0x40) { throw "$Path is truncated (no DOS header)." }
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw "$Path is not a PE executable (no MZ header)." }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadUInt32()
        if ($peOffset -gt ($stream.Length - 6)) { throw "$Path is truncated (PE header past end of file)." }
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) { throw "$Path is not a PE executable (no PE signature)." }
        return $reader.ReadUInt16()
    } finally {
        $reader.Dispose()
        $stream.Dispose()
    }
}

# NuGet accepts only alphanumerics and hyphens in a pre-release label, while
# the build version uses dots (0.1.0-dev.7.gabc1234). Rewrite the label and
# leave the x.y.z core untouched.
function ConvertTo-NuGetVersion([string]$BuildVersion) {
    $parts = $BuildVersion -split '-', 2
    if ($parts.Count -eq 1) { return $parts[0] }
    $label = ($parts[1] -replace '[^0-9A-Za-z-]', '-') -replace '-+', '-'
    return "$($parts[0])-$($label.Trim('-'))"
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Get-GitText([string[]]$Arguments) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    try {
        $output = & git -C $repoRoot @Arguments
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($exitCode -ne 0) { return $null }
    return ([string]($output -join "`n")).Trim()
}

if (-not (Test-Tool 'cargo')) {
    throw 'cargo was not found on PATH. Install the toolchain from https://rustup.rs and reopen the shell.'
}

# ---------------------------------------------------------------- 1. the gate

$dependencyAudit = 'skipped (-SkipChecks)'
if ($SkipChecks) {
    Write-Step 'Skipping the CI gate (-SkipChecks)'
} else {
    Write-Step 'Checking formatting'
    Invoke-Native 'cargo' @('fmt', '--check')

    Write-Step 'Linting (warnings denied)'
    Invoke-Native 'cargo' @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')

    Write-Step 'Running the test suite'
    Invoke-Native 'cargo' @('test', '--workspace')

    if (Test-Tool 'cargo-deny') {
        Write-Step 'Auditing dependencies (advisories, licenses, sources)'
        Invoke-Native 'cargo' @('deny', '--all-features', 'check')
        $dependencyAudit = 'passed'
    } else {
        $dependencyAudit = 'not run (cargo-deny is not installed: cargo install cargo-deny)'
        Write-Warning "Dependency audit skipped - $dependencyAudit"
    }
}

# --------------------------------------------------------------- 2. the build

Write-Step 'Building release binaries'
# --locked: the artifact is built from the dependency versions in the
# lockfile, not whatever resolves today. If this fails, run a plain
# `cargo build` first and commit the lockfile change.
Invoke-Native 'cargo' @('build', '--release', '--locked', '--workspace')

# -------------------------------------------------------- 3. what got built

Write-Step 'Verifying the built binaries'
foreach ($binary in $binaries) {
    $path = Join-Path $releaseDirectory $binary
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Expected binary is missing after the build: $path"
    }
    $machine = Get-PeMachine $path
    if ($machine -ne 0x8664) {
        throw ('{0} has PE machine 0x{1:X4}; expected 0x8664 (x64).' -f $path, $machine)
    }
}

# The build version comes from the binaries themselves, so the artifacts can
# never be labelled with a version the code does not carry.
$reportJson = & (Join-Path $releaseDirectory 'crossover.exe') version --json
if ($LASTEXITCODE -ne 0) {
    throw "crossover.exe version --json failed with exit code $LASTEXITCODE."
}
$report = ($reportJson -join "`n") | ConvertFrom-Json
$buildVersion = [string]$report.version

# Both executables ship together and are upgraded together; a mismatch means
# one of them is stale, which is exactly the bug that is impossible to see
# once they are installed side by side.
foreach ($binary in $binaries) {
    $path = Join-Path $releaseDirectory $binary
    $info = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($path)
    if ([string]::IsNullOrWhiteSpace($info.ProductVersion)) {
        throw "$binary carries no version resource; the build script did not stamp it."
    }
    if ($info.ProductVersion -ne $buildVersion) {
        throw "$binary embeds version $($info.ProductVersion) but crossover.exe reports $buildVersion."
    }
    if ($info.Comments -match 'Git commit: (?<commit>[0-9a-f]{40})') {
        if ($Matches.commit -ne $report.git_commit) {
            throw "$binary was built from commit $($Matches.commit), but crossover.exe reports $($report.git_commit)."
        }
    }
}

$headCommit = Get-GitText @('rev-parse', 'HEAD')
if ($headCommit -and $report.git_commit -and $headCommit -ne $report.git_commit) {
    throw "The binaries were built from $($report.git_commit) but HEAD is now $headCommit. Re-run the build."
}
if ($report.dirty) {
    Write-Warning 'Built from a dirty worktree - these artifacts are not reproducible from a commit.'
}

if (-not $Version) { $Version = ConvertTo-NuGetVersion $buildVersion }
Write-Host "Build version:   $buildVersion"
Write-Host "Package version: $Version"

# ----------------------------------------------------------- 4. the artifacts

Write-Step 'Assembling the portable archive'
$packageName = "crossover-$Version-windows-x64"
$stagingDirectory = Join-Path $OutputDirectory $packageName
$archive = Join-Path $OutputDirectory "$packageName.zip"

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
Remove-Item -LiteralPath $stagingDirectory -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $archive -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "$archive.sha256" -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $stagingDirectory | Out-Null

# install.ps1 expects the executables beside it (packaging\README.md), which
# is exactly the layout of this archive.
$staged = @(
    @{ Source = Join-Path $releaseDirectory 'crossover.exe'; Name = 'crossover.exe' },
    @{ Source = Join-Path $releaseDirectory 'crossover-svc.exe'; Name = 'crossover-svc.exe' },
    @{ Source = Join-Path $repoRoot 'packaging\windows\install.ps1'; Name = 'install.ps1' },
    @{ Source = Join-Path $repoRoot 'packaging\windows\uninstall.ps1'; Name = 'uninstall.ps1' },
    @{ Source = Join-Path $repoRoot 'packaging\README.md'; Name = 'INSTALL.md' },
    @{ Source = Join-Path $repoRoot 'README.md'; Name = 'README.md' },
    @{ Source = Join-Path $repoRoot 'LICENSE'; Name = 'LICENSE' }
)
foreach ($item in $staged) {
    if (-not (Test-Path -LiteralPath $item.Source -PathType Leaf)) {
        throw "Required archive file is missing: $($item.Source)"
    }
    Copy-Item -LiteralPath $item.Source -Destination (Join-Path $stagingDirectory $item.Name) -Force
}

# The identity report travels with the artifacts, minus the one field that
# describes the build machine rather than the build.
$buildReport = $report | Select-Object -Property * -ExcludeProperty executable
$buildReportJson = $buildReport | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText(
    (Join-Path $stagingDirectory 'build-info.json'),
    "$buildReportJson`n",
    [System.Text.UTF8Encoding]::new($false))

try {
    Compress-Archive -Path $stagingDirectory -DestinationPath $archive -CompressionLevel Optimal
} finally {
    Remove-Item -LiteralPath $stagingDirectory -Recurse -Force -ErrorAction SilentlyContinue
}
$archiveHash = Get-Sha256 $archive
[System.IO.File]::WriteAllText(
    "$archive.sha256",
    "$archiveHash  $packageName.zip`n",
    [System.Text.ASCIIEncoding]::new())
Write-Host "Portable archive: $archive"

$packagePath = $null
$chocolateyVersion = $null
if ($SkipChocolatey) {
    Write-Step 'Skipping the Chocolatey package (-SkipChocolatey)'
} else {
    if (-not (Test-Tool 'choco')) {
        throw 'choco was not found on PATH. Install Chocolatey (https://chocolatey.org/install), or pass -SkipChocolatey to build the portable archive only.'
    }
    Write-Step 'Building the Chocolatey package'
    $packagePath = & (Join-Path $repoRoot 'packaging\chocolatey\pack.ps1') `
        -Version $Version `
        -BinariesDirectory $releaseDirectory `
        -OutputDirectory $OutputDirectory |
        Select-Object -Last 1
    if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
        throw "Chocolatey packaging did not produce the expected package: $packagePath"
    }
    $chocolateyVersion = (& choco --version | Select-Object -First 1).Trim()
}

Write-Step 'Writing the artifact manifest'
$artifacts = @(
    [ordered]@{ kind = 'portable-archive'; file = Split-Path -Leaf $archive; sha256 = $archiveHash },
    [ordered]@{ kind = 'portable-checksum'; file = Split-Path -Leaf "$archive.sha256"; sha256 = (Get-Sha256 "$archive.sha256") }
)
if ($packagePath) {
    $artifacts += [ordered]@{
        kind   = 'chocolatey-package'
        file   = Split-Path -Leaf $packagePath
        sha256 = Get-Sha256 $packagePath
    }
}

$manifest = [ordered]@{
    schemaVersion   = 1
    packageVersion  = $Version
    buildVersion    = $buildVersion
    build           = $buildReport
    checks          = if ($SkipChecks) { 'skipped (-SkipChecks)' } else { 'passed' }
    dependencyAudit = $dependencyAudit
    tools           = [ordered]@{ chocolatey = $chocolateyVersion }
    artifacts       = $artifacts
}
$manifestPath = Join-Path $OutputDirectory 'artifacts.json'
$manifestJson = $manifest | ConvertTo-Json -Depth 6
[System.IO.File]::WriteAllText($manifestPath, "$manifestJson`n", [System.Text.UTF8Encoding]::new($false))

Write-Host ''
Write-Host "Done in $([int]((Get-Date) - $startedAt).TotalSeconds)s. Artifacts in $OutputDirectory :" -ForegroundColor Green
foreach ($artifact in $artifacts) {
    Write-Host ("  {0,-20} {1}" -f $artifact.kind, $artifact.file)
}
Write-Host ("  {0,-20} {1}" -f 'manifest', (Split-Path -Leaf $manifestPath))
