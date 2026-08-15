# Packaging Crossover (Windows)

Crossover ships two executables that deploy together:

- **`crossover.exe`** — the CLI and the worker (runs as the user).
- **`crossover-svc.exe`** — the background service daemon (runs as LocalSystem).

Both install into a **protected** location (`%ProgramFiles%\Crossover`). This is
not cosmetic: the service runs `crossover-svc.exe` as SYSTEM, so a user-writable
copy would let any user replace it and gain SYSTEM execution. Never install for
service use into a user-writable directory (ADR 0011).

There are two ways to install, sharing the same logic. Start with the script;
adopt Chocolatey when you want versioned, silent install/upgrade/uninstall
across machines.

## Build everything first

```powershell
.\scripts\build.ps1
```

That is the whole build: it runs the CI gate, compiles both executables,
verifies they carry matching build metadata, and writes every deliverable to
`dist\` — the portable archive plus its `.sha256`, the Chocolatey `.nupkg`,
and an `artifacts.json` manifest recording versions, the source commit, and
each artifact's hash. `-SkipChecks` skips the gate; `-SkipChocolatey` stops
after the archive.

The version is derived from the source state rather than typed in: a tagged
commit produces `0.1.0`, anything else produces something that names its
origin, such as `0.1.0-dev.7.gabc1234.dirty`. Run `crossover version` (or
`crossover-svc.exe --version`) to see the full identity of any binary you
find on a machine. NuGet rejects dots in a pre-release label, so the
Chocolatey package for that build is versioned `0.1.0-dev-7-gabc1234-dirty`;
`artifacts.json` carries both forms.

To compile without packaging:

```powershell
cargo build --release -p crossover -p crossover-svc
```

Both land in `target\release\`.

## 1. Minimal: the PowerShell scripts

`windows\install.ps1` and `windows\uninstall.ps1` are standalone and
self-elevating. Ship them in a zip alongside the two `.exe`s (the script expects
the binaries beside it).

```powershell
# From the folder containing install.ps1, crossover.exe, crossover-svc.exe:
.\install.ps1                       # to Program Files, registers + starts the service
.\install.ps1 -SkipService          # deploy only (foreground use)
.\install.ps1 -InstallDir 'D:\Apps\Crossover'
.\uninstall.ps1                     # stop + remove service, delete files
```

Re-running `install.ps1` upgrades in place: it removes the existing service
(releasing the locked binaries), replaces the files, and re-registers.

## 2. Chocolatey (silent install / upgrade / uninstall)

The `chocolatey\` folder is a package whose install scripts reuse the same
logic. The two executables are **embedded** in the `.nupkg` (suited to a local
or private feed).

**Build the package.** `scripts\build.ps1` does this as part of a full run.
To build only the package, from binaries you already have:

```powershell
.\chocolatey\pack.ps1                                   # target\release, Cargo version
.\chocolatey\pack.ps1 -Version 0.1.0 -OutputDirectory ..\dist
```

This produces `crossover.<version>.nupkg` in the output directory
(`chocolatey\` by default).

**Install / upgrade / uninstall** (all silent, all elevated by Chocolatey):

```powershell
choco install crossover -y -s <source>     # <source> = the chocolatey folder, or a feed URL
choco upgrade crossover -y -s <source>
choco uninstall crossover -y
```

`<source>` can be the local `chocolatey\` folder to start, or a private feed
(ProGet / Nexus / Azure Artifacts) once you distribute more widely. Push with:

```powershell
choco push crossover.<version>.nupkg -s <feed-url> --api-key <key>
```

### The upgrade lock, handled

A running service holds `crossover-svc.exe`, and its worker holds
`crossover.exe`, so an upgrade cannot overwrite them directly. Chocolatey runs
`chocolateyBeforeModify.ps1` from the *old* package first, which removes the
service and waits for both processes to exit — releasing the locks — before the
new package lays down files and re-registers. The standalone `install.ps1` does
the same at the top of a re-run.

## Going public later

For the public Chocolatey community repository (rather than a private feed),
two changes apply: fetch the executables from a signed release URL in
`chocolateyInstall.ps1` instead of embedding them, and code-sign the binaries so
SmartScreen does not warn. Neither is needed for personal or internal use.

## After install

The service starts the worker, but the worker needs a role. Create
`~/.crossover/config.toml` (see `crossover config` for an example) with a
`[network]` role and, for seamless mode, a `[seamless] side`. (Config and logs
live under `~/.crossover`; identity and the trust store stay under
`%LOCALAPPDATA%\Crossover`.)
