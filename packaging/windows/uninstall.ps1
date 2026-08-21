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
$env:RUST_LOG = 'warn'  # quiet the CLI's startup log so only result lines show

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

Write-Host 'Crossover uninstalled. Your identity and trust store under %LOCALAPPDATA%\Crossover, and your config and logs under ~\.crossover, are left intact.'
