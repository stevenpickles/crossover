# Chocolatey runs this from the CURRENTLY INSTALLED package before an upgrade or
# an uninstall. It is the hook that releases the file locks: the running service
# holds crossover-svc.exe and its worker holds crossover.exe, so we remove the
# service (which stops both) and wait for the processes to exit before the new
# files are written or the directory is deleted.
$ErrorActionPreference = 'SilentlyContinue'

$installDir = Join-Path $env:ProgramFiles 'Crossover'
$serviceName = 'Crossover'

if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    $installedCli = Join-Path $installDir 'crossover.exe'
    if (Test-Path $installedCli) {
        & $installedCli service uninstall | Out-Null
    } else {
        Stop-Service -Name $serviceName -Force
        & sc.exe delete $serviceName | Out-Null
    }
    $deadline = (Get-Date).AddSeconds(15)
    while ((Get-Process -Name 'crossover', 'crossover-svc' -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 300
    }
}
