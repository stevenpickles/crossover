# Chocolatey runs this from the CURRENTLY INSTALLED package before an upgrade or
# an uninstall. It is the hook that releases the file locks: the running service
# holds crossover-svc.exe and its worker holds crossover.exe, so the service is
# removed first (which stops both), and then everything still running out of the
# install directory is closed and waited for — before the new files are written
# or the directory is deleted. A layout editor the user left open holds
# crossover-layout.exe the same way and nothing supervises it (ADR 0019), which
# is why the block below goes by directory rather than by service.
$ErrorActionPreference = 'SilentlyContinue'

$installDir = Join-Path $env:ProgramFiles 'Crossover'
$serviceName = 'Crossover'

if (Get-Service -Name $serviceName -ErrorAction SilentlyContinue) {
    $env:RUST_LOG = 'warn'  # quiet the CLI's startup log during packaging
    $installedCli = Join-Path $installDir 'crossover.exe'
    if (Test-Path $installedCli) {
        & $installedCli service uninstall | Out-Null
    } else {
        Stop-Service -Name $serviceName -Force
        & sc.exe delete $serviceName | Out-Null
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
