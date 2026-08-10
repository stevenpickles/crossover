# chocolateyBeforeModify.ps1 has already stopped and removed the service and
# waited for its processes to exit, so the binaries are unlocked; just delete
# the install directory. User data under %LOCALAPPDATA%\Crossover (identity,
# trust store, config) is deliberately left intact.
$ErrorActionPreference = 'SilentlyContinue'

$installDir = Join-Path $env:ProgramFiles 'Crossover'
if (Test-Path $installDir) {
    Remove-Item -Path $installDir -Recurse -Force
}

# Undo the PATH entry chocolateyInstall added.
$machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
if (($machinePath -split ';') -contains $installDir) {
    $trimmed = ($machinePath -split ';' | Where-Object { $_ -ne $installDir }) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $trimmed, 'Machine')
}
