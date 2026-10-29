<#
.SYNOPSIS
Installs StreamForge from this unpacked package, or removes it again.

.DESCRIPTION
Copies the programs (bin\) and the documents into the installation folder,
for the current user and without administrator rights, and adds a Start
menu shortcut to the desktop app that starts in your home folder. Nothing
else is changed: to use the commands by name, add the bin folder to your
PATH (the script prints how). Installing again first removes what the
previous installation in that folder installed.

  powershell -ExecutionPolicy Bypass -File install.ps1 [-Prefix DIR]
  powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall [-Prefix DIR]

.PARAMETER Prefix
The installation folder. Default: %LOCALAPPDATA%\Programs\StreamForge.

.PARAMETER Uninstall
Removes what this script installed into the folder (it keeps a list there)
and the Start menu shortcut, if that points into the folder. Projects and
recordings stay where you saved them.
#>
[CmdletBinding()]
param(
    [string] $Prefix = (Join-Path $env:LOCALAPPDATA 'Programs\StreamForge'),
    [switch] $Uninstall
)

# Windows PowerShell 5.1 is what every Windows 10 and 11 has; nothing here
# needs a newer one.
Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'

# Relative to the current location, as other commands would read it.
$Prefix = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Prefix).TrimEnd('\')
$bin = Join-Path $Prefix 'bin'
$desktop = Join-Path $bin 'streamforge-desktop.exe'
# Every file this script installed, relative to the prefix, one per line.
$manifest = Join-Path $Prefix 'installed-files.txt'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'StreamForge.lnk'
$shell = New-Object -ComObject WScript.Shell

# The files of an earlier installation in this prefix, from its list.
# Entries that would leave the prefix are skipped.
function Get-InstalledFileList {
    if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) {
        return @()
    }
    @(Get-Content -LiteralPath $manifest | Where-Object {
            $_ -and -not [IO.Path]::IsPathRooted($_) -and $_ -notmatch '(^|[\\/])\.\.([\\/]|$)'
        })
}

function Remove-Installation {
    $files = Get-InstalledFileList
    foreach ($file in $files) {
        $path = Join-Path $Prefix $file
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            try {
                Remove-Item -LiteralPath $path -Force
            } catch {
                throw "cannot remove ${path}: $($_.Exception.Message) (is StreamForge still running?)"
            }
        }
    }
    # The folders the files were in, deepest first, where now empty.
    $folders = @{}
    foreach ($file in $files) {
        $dir = Split-Path -Parent $file
        while ($dir) {
            $folders[$dir] = $true
            $dir = Split-Path -Parent $dir
        }
    }
    foreach ($dir in ($folders.Keys | Sort-Object -Property Length -Descending)) {
        $path = Join-Path $Prefix $dir
        if ((Test-Path -LiteralPath $path -PathType Container) -and -not (Get-ChildItem -LiteralPath $path -Force)) {
            Remove-Item -LiteralPath $path
        }
    }
    if (Test-Path -LiteralPath $manifest) {
        Remove-Item -LiteralPath $manifest
    }
    if ((Test-Path -LiteralPath $shortcut) -and $shell.CreateShortcut($shortcut).TargetPath -eq $desktop) {
        Remove-Item -LiteralPath $shortcut
    }
    if ((Test-Path -LiteralPath $Prefix -PathType Container) -and -not (Get-ChildItem -LiteralPath $Prefix -Force)) {
        Remove-Item -LiteralPath $Prefix
    }
}

if ($Uninstall) {
    Remove-Installation
    Write-Output "StreamForge removed from $Prefix. Projects and recordings stay where you saved them."
    exit 0
}

$here = $PSScriptRoot.TrimEnd('\')
if (-not (Test-Path -LiteralPath (Join-Path $here 'bin\streamforge-desktop.exe') -PathType Leaf)) {
    throw "run install.ps1 from the unpacked package folder (bin\streamforge-desktop.exe is missing next to it)"
}
if ($Prefix -eq $here) {
    throw "the package folder itself cannot be the installation folder; the programs already run from bin\ here"
}

$items = @('bin', 'docs', 'licenses', 'README.md', 'CHANGELOG.md', 'INSTALL.md', 'BUILD-INFO.txt',
    'LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE-OFL-IBM-Plex.txt')
$files = @(foreach ($item in $items) {
        Get-ChildItem -LiteralPath (Join-Path $here $item) -Recurse -File -Force |
            ForEach-Object { $_.FullName.Substring($here.Length + 1) }
    })

Remove-Installation
New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
# The list comes first, so an installation that fails half way can still
# be removed.
[IO.File]::WriteAllLines($manifest, [string[]] $files)
foreach ($file in $files) {
    $target = Join-Path $Prefix $file
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
    Copy-Item -LiteralPath (Join-Path $here $file) -Destination $target -Force
}

$link = $shell.CreateShortcut($shortcut)
$link.TargetPath = $desktop
# As from Linux's application menu: recordings and projects go to the home
# folder unless chosen otherwise.
$link.WorkingDirectory = $env:USERPROFILE
$link.IconLocation = "$desktop,0"
$link.Description = 'StreamForge: scenes, mixer, recording and streaming'
$link.Save()

Write-Output "StreamForge installed into ${Prefix}:"
Write-Output "  programs   $bin"
Write-Output "  shortcut   $shortcut"
Write-Output "  documents  $Prefix (start with docs\user-guide.md)"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$onPath = @("$userPath;$env:Path" -split ';' | Where-Object { $_.TrimEnd('\') -eq $bin })
if (-not $onPath) {
    Write-Output "Note: $bin is not on your PATH. To use the commands by name in new consoles:"
    Write-Output "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$bin', 'User')"
}
