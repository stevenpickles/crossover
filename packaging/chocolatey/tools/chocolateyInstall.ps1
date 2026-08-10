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
# protected location — never a user-writable directory (ADR 0011). Quiet the
# CLI's startup log so only its result lines show in Chocolatey's output.
$env:RUST_LOG = 'warn'
& (Join-Path $installDir 'crossover.exe') service install
Start-Service -Name $serviceName

# Chocolatey shims the exes in tools\ onto PATH for CLI access. The service uses
# this Program Files copy (registered by `service install`); the shims are for
# running `crossover` by hand.
Write-Host "Crossover installed to $installDir; the service is running. Set a role in ~\.crossover\config.toml (see ``crossover config``) so the worker has something to do."
