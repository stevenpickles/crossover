<#
.SYNOPSIS
    Install Crossover into Program Files and (by default) register + start the
    background service. Standalone, minimal, and re-runnable for upgrades.

.DESCRIPTION
    Deploys crossover.exe and crossover-svc.exe to a protected location — which
    matters, because the service runs crossover-svc.exe as LocalSystem, so a
    user-writable copy would be a privilege-escalation hole (ADR 0011). The
    binaries must sit beside this script (as shipped in the release zip).

    Idempotent: on a re-run it stops and removes any existing service first so
    the locked exe can be replaced, then re-registers. This same logic is the
    core of the Chocolatey package under packaging/chocolatey.

.PARAMETER InstallDir
    Where the binaries go. Default: %ProgramFiles%\Crossover.

.PARAMETER SkipService
    Deploy the files but do not register/start the service (foreground use only).

.PARAMETER SkipPath
    Do not add the install directory to the machine PATH.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:ProgramFiles 'Crossover'),
    [switch]$SkipService,
    [switch]$SkipPath
)

$ErrorActionPreference = 'Stop'
$ServiceName = 'Crossover'
$env:RUST_LOG = 'warn'  # quiet the CLI's startup log so only result lines show

# Re-launch elevated if needed: writing to Program Files and touching the SCM
# both require administrator. Forward the bound parameters so an elevated
# relaunch behaves identically.
$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Administrator rights required; relaunching elevated...'
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    foreach ($entry in $PSBoundParameters.GetEnumerator()) {
        if ($entry.Value -is [switch]) {
            if ($entry.Value.IsPresent) { $argList += "-$($entry.Key)" }
        } else {
            $argList += "-$($entry.Key)"; $argList += "`"$($entry.Value)`""
        }
    }
    Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList $argList
    return
}

$sourceDir = Split-Path -Parent $PSCommandPath
$binaries = @('crossover.exe', 'crossover-svc.exe')
foreach ($binary in $binaries) {
    if (-not (Test-Path (Join-Path $sourceDir $binary))) {
        throw "$binary is not next to this script ($sourceDir); run install.ps1 from the release folder that contains both executables."
    }
}

# Release the file locks before overwriting: a running service holds
# crossover-svc.exe, and its worker holds crossover.exe. Removing the service
# stops both; then wait for the processes to actually exit.
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    Write-Host 'Removing the existing Crossover service so its binaries can be replaced...'
    $installedCli = Join-Path $InstallDir 'crossover.exe'
    if (Test-Path $installedCli) {
        & $installedCli service uninstall | Out-Null
    } else {
        Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
        & sc.exe delete $ServiceName | Out-Null
    }
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Process -Name 'crossover', 'crossover-svc' -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
    }
}

Write-Host "Installing Crossover to $InstallDir ..."
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($binary in $binaries) {
    Copy-Item -Path (Join-Path $sourceDir $binary) -Destination $InstallDir -Force
}

if (-not $SkipPath) {
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($machinePath -split ';') -notcontains $InstallDir) {
        [Environment]::SetEnvironmentVariable('Path', "$machinePath;$InstallDir", 'Machine')
        Write-Host "Added $InstallDir to the machine PATH (new shells will see it)."
    }
}

if ($SkipService) {
    Write-Host 'Files installed. Skipped service registration (-SkipService).'
    Write-Host "Run background operation later with: `"$InstallDir\crossover.exe`" service install"
    return
}

Write-Host 'Registering and starting the Crossover service...'
& (Join-Path $InstallDir 'crossover.exe') service install
Start-Service -Name $ServiceName
Write-Host ''
Write-Host 'Crossover installed and the service is running.'
Write-Host 'Set a role in %LOCALAPPDATA%\Crossover\config.toml (see `crossover config`) so the worker has something to do.'
