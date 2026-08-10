# chocolateyBeforeModify.ps1 has already stopped and removed the service and
# waited for its processes to exit, so the binaries are unlocked; just delete
# the install directory. User data is deliberately left intact: identity and
# the trust store under %LOCALAPPDATA%\Crossover, config and logs under
# ~\.crossover.
$ErrorActionPreference = 'SilentlyContinue'

$installDir = Join-Path $env:ProgramFiles 'Crossover'
if (Test-Path $installDir) {
    Remove-Item -Path $installDir -Recurse -Force
}
# Chocolatey removes its own shims; nothing else to undo here.
