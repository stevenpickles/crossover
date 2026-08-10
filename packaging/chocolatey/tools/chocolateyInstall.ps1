# Chocolatey runs this elevated on install and on upgrade. On upgrade,
# chocolateyBeforeModify.ps1 from the OLD package has already stopped and
# removed the service (releasing the locked binaries), so here we only lay down
# the files and register + start the service afresh.
$ErrorActionPreference = 'Stop'

$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$installDir = Join-Path $env:ProgramFiles 'Crossover'
$serviceName = 'Crossover'

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
foreach ($binary in @('crossover.exe', 'crossover-svc.exe')) {
    Copy-Item -Path (Join-Path $toolsDir $binary) -Destination $installDir -Force
}

# The service runs crossover-svc.exe as LocalSystem, so it must live in this
# protected location — never a user-writable directory (ADR 0011).
& (Join-Path $installDir 'crossover.exe') service install
Start-Service -Name $serviceName

Write-Host "Crossover installed to $installDir and the service is running."
Write-Host 'Set a role in %LOCALAPPDATA%\Crossover\config.toml (see `crossover config`) so the worker has something to do.'
