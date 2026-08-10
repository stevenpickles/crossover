<#
.SYNOPSIS
    Stop and remove the Crossover service and delete the installed binaries.

.DESCRIPTION
    The mirror of install.ps1: removes the service (which stops the daemon and
    its worker), waits for the processes to exit so the files unlock, deletes
    the install directory, and removes it from the machine PATH.

.PARAMETER InstallDir
    Where Crossover was installed. Default: %ProgramFiles%\Crossover.
#>
[CmdletBinding()]
param(
    [string]$InstallDir = (Join-Path $env:ProgramFiles 'Crossover')
)

$ErrorActionPreference = 'Stop'
$ServiceName = 'Crossover'

$identity = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'Administrator rights required; relaunching elevated...'
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"")
    foreach ($entry in $PSBoundParameters.GetEnumerator()) {
        $argList += "-$($entry.Key)"; $argList += "`"$($entry.Value)`""
    }
    Start-Process -FilePath 'powershell.exe' -Verb RunAs -ArgumentList $argList
    return
}

if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
    Write-Host 'Removing the Crossover service...'
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

if (Test-Path $InstallDir) {
    Write-Host "Deleting $InstallDir ..."
    Remove-Item -Path $InstallDir -Recurse -Force
}

$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';') -contains $InstallDir) {
    $trimmed = ($machinePath -split ';' | Where-Object { $_ -ne $InstallDir }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $trimmed, 'Machine')
    Write-Host "Removed $InstallDir from the machine PATH."
}

Write-Host 'Crossover uninstalled. Your identity, trust store, and config under %LOCALAPPDATA%\Crossover are left intact.'
