<#
.SYNOPSIS
    Install Crossover into Program Files and (by default) register + start the
    background service. Standalone, minimal, and re-runnable for upgrades.

.DESCRIPTION
    Deploys crossover.exe, crossover-svc.exe and crossover-layout.exe to a
    protected location — which matters, because the service runs
    crossover-svc.exe as LocalSystem, so a user-writable copy would be a
    privilege-escalation hole (ADR 0011). The binaries must sit beside this
    script (as shipped in the release zip).

    Idempotent: on a re-run it stops and removes any existing service, and
    closes a running layout editor, so the locked exes can be replaced, then
    re-registers. This same logic is the core of the Chocolatey package under
    packaging/chocolatey.

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
# Everything an install deploys, in one list: the three executables and the
# third-party notice the editor's embedded fonts require to travel with them
# (ADR 0019). One list drives both the "is it here?" check and the copy, so a
# file can never be verified and then not installed.
$deployedFiles = @(
    'crossover.exe',
    'crossover-svc.exe',
    'crossover-layout.exe',
    'THIRD-PARTY-NOTICES.txt'
)
foreach ($file in $deployedFiles) {
    if (-not (Test-Path (Join-Path $sourceDir $file))) {
        throw "$file is not next to this script ($sourceDir); run install.ps1 from the release folder that contains every file it ships."
    }
}

# A running service holds crossover-svc.exe and its worker holds crossover.exe,
# so the service goes first. Removing it stops both, but not instantly, and it
# says nothing about anything else running out of the install directory — the
# block below is what actually waits for the locks to drop.
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    Write-Host 'Removing the existing Crossover service so its binaries can be replaced...'
    $installedCli = Join-Path $InstallDir 'crossover.exe'
    if (Test-Path $installedCli) {
        & $installedCli service uninstall | Out-Null
    } else {
        Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
        & sc.exe delete $ServiceName | Out-Null
    }
}

# --- Release the install directory (identical in install.ps1, uninstall.ps1,
# --- and chocolatey\tools\chocolateyBeforeModify.ps1: these ship separately,
# --- so identical text is the shared form available to them).
#
# Anything still running out of the install directory holds a file there. The
# service stop above is asynchronous, a layout editor the user opened answers
# to nothing at all (ADR 0019), and a hand-started `crossover run` answers to
# nobody either. Processes are selected by executable path rather than by name,
# so a copy someone is developing out of target\release is left alone and any
# future binary in this directory is covered without editing this block. Ask
# first and insist second: the editor is the one Crossover process with a
# person looking at it.
$holders = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
        $path = $null
        try { $path = $_.Path } catch { }
        $path -and $path -like "$InstallDir\*"
    })
if ($holders.Count -gt 0) {
    Write-Host "Closing Crossover processes running from $InstallDir so its files can be replaced..."
    foreach ($holder in $holders) {
        # CloseMainWindow throws on a process that has already exited, which
        # under -ErrorActionPreference Stop would abort the script at the worst
        # moment there is: the service already removed, the files not yet dealt
        # with.
        try { $holder.CloseMainWindow() | Out-Null } catch { }
    }
    $holders | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    $stubborn = @($holders | Where-Object { -not $_.HasExited })
    if ($stubborn.Count -gt 0) {
        # Stop-Process returns before Windows has finished tearing the process
        # down, and a half-dead process still holds its executable open — so
        # the kill is followed by a wait, not by a copy.
        Stop-Process -InputObject $stubborn -Force -ErrorAction SilentlyContinue
        $stubborn | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
}
# --- End of the shared release-the-install-directory block.

Write-Host "Installing Crossover to $InstallDir ..."
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($file in $deployedFiles) {
    Copy-Item -Path (Join-Path $sourceDir $file) -Destination $InstallDir -Force
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
Write-Host 'Set a role in ~\.crossover\config.toml (see `crossover config`) so the worker has something to do.'
Write-Host 'Arrange the two machines'' monitors with: crossover layout'
