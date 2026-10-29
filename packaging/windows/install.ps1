<#
.SYNOPSIS
Installs StreamForge from this unpacked package, or removes it again.

.DESCRIPTION
Copies the programs (bin\), the documents and this script into the
installation folder, for the current user and without administrator
rights, and adds a Start menu shortcut to the desktop app that starts in
your home folder. Nothing else is changed: to use the commands by name,
add the bin folder to your PATH (the script prints how). Installing again
first removes what the previous installation in that folder installed.

  powershell -ExecutionPolicy Bypass -File install.ps1 [-Prefix DIR]
  powershell -ExecutionPolicy Bypass -File install.ps1 -Uninstall [-Prefix DIR]

.PARAMETER Prefix
The installation folder. Default: %LOCALAPPDATA%\Programs\StreamForge, or,
for the copy of this script in an installation, that installation.

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

# Every file this script installed, relative to the prefix, one per line.
$manifestName = 'installed-files.txt'
if (-not $PSBoundParameters.ContainsKey('Prefix') -and (Test-Path -LiteralPath (Join-Path $PSScriptRoot $manifestName))) {
    $Prefix = $PSScriptRoot
}
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'StreamForge.lnk'
$shell = New-Object -ComObject WScript.Shell

if (-not ('StreamForgeInstall.Native' -as [type])) {
    Add-Type -Namespace StreamForgeInstall -Name Native -MemberDefinition @'
[DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
public static extern uint GetLongPathName(string shortPath, System.Text.StringBuilder longPath, uint size);
'@
}

# An existing path with long names instead of 8.3 short ones (such as
# RUNNER~1), as the shortcut reports its target; others stay as they are.
function Get-LongPath([string] $Path) {
    $buffer = New-Object System.Text.StringBuilder 32767
    $length = [StreamForgeInstall.Native]::GetLongPathName($Path, $buffer, [uint32] $buffer.Capacity)
    if ($length -eq 0 -or $length -ge $buffer.Capacity) {
        return $Path
    }
    $buffer.ToString()
}

# The prefix and the paths in it; again once the prefix exists, since only
# an existing path has its long name.
function Set-Prefix([string] $Path) {
    # Relative to the current location, as other commands would read it.
    $full = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path).TrimEnd('\')
    $script:Prefix = Get-LongPath $full
    $script:bin = Join-Path $script:Prefix 'bin'
    $script:desktop = Join-Path $script:bin 'streamforge-desktop.exe'
    $script:manifest = Join-Path $script:Prefix $manifestName
}

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
    # The folders the files are in.
    $folders = @{}
    foreach ($file in $files) {
        $dir = Split-Path -Parent $file
        while ($dir) {
            $folders[$dir] = $true
            $dir = Split-Path -Parent $dir
        }
    }
    # A folder replaced by a link (a junction) would lead out of the
    # prefix; nothing is removed through one.
    foreach ($dir in $folders.Keys) {
        $item = Get-Item -LiteralPath (Join-Path $Prefix $dir) -Force -ErrorAction SilentlyContinue
        if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "$($item.FullName) is a link, which install.ps1 does not follow; remove it by hand and run it again"
        }
    }
    if ((Test-Path -LiteralPath $shortcut) -and (Get-LongPath ($shell.CreateShortcut($shortcut).TargetPath)) -eq $desktop) {
        Remove-Item -LiteralPath $shortcut
    }
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
    # Deepest first, where now empty.
    foreach ($dir in ($folders.Keys | Sort-Object -Property Length -Descending)) {
        $path = Join-Path $Prefix $dir
        if ((Test-Path -LiteralPath $path -PathType Container) -and -not (Get-ChildItem -LiteralPath $path -Force)) {
            Remove-Item -LiteralPath $path
        }
    }
    if (Test-Path -LiteralPath $manifest) {
        Remove-Item -LiteralPath $manifest
    }
    if ((Test-Path -LiteralPath $Prefix -PathType Container) -and -not (Get-ChildItem -LiteralPath $Prefix -Force)) {
        Remove-Item -LiteralPath $Prefix
    }
}

Set-Prefix $Prefix

if ($Uninstall) {
    Remove-Installation
    Write-Output "StreamForge removed from $Prefix. Projects and recordings stay where you saved them."
    exit 0
}

$here = Get-LongPath $PSScriptRoot.TrimEnd('\')
if (-not (Test-Path -LiteralPath (Join-Path $here 'bin\streamforge-desktop.exe') -PathType Leaf)) {
    throw "run install.ps1 from the unpacked package folder (bin\streamforge-desktop.exe is missing next to it)"
}
if ($Prefix -eq $here) {
    throw "install.ps1 installs from the unpacked package folder into another one; to remove this installation, add -Uninstall"
}

$items = @('bin', 'docs', 'licenses', 'README.md', 'CHANGELOG.md', 'INSTALL.md', 'BUILD-INFO.txt',
    'LICENSE-MIT', 'LICENSE-APACHE', 'LICENSE-OFL-IBM-Plex.txt', 'install.ps1')
$files = @(foreach ($item in $items) {
        Get-ChildItem -LiteralPath (Join-Path $here $item) -Recurse -File -Force |
            ForEach-Object { $_.FullName.Substring($here.Length + 1) }
    })

# Files in the prefix that no earlier installation there listed are
# someone else's: refused, rather than overwritten now and removed later.
$ours = @{}
foreach ($file in Get-InstalledFileList) {
    $ours[$file] = $true
}
$foreign = @($files | Where-Object { -not $ours.ContainsKey($_) -and (Test-Path -LiteralPath (Join-Path $Prefix $_)) })
if ($foreign) {
    throw "$Prefix has files StreamForge did not install, such as $($foreign[0]); choose another folder with -Prefix, or move them"
}

Remove-Installation
New-Item -ItemType Directory -Force -Path $Prefix | Out-Null
Set-Prefix $Prefix
# The list comes first, so an installation that fails half way can still
# be removed (every path in it was free before).
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
Write-Output "  uninstall  powershell -ExecutionPolicy Bypass -File `"$Prefix\install.ps1`" -Uninstall"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$onPath = @("$userPath;$env:Path" -split ';' | Where-Object { $_ -and (Get-LongPath ($_.TrimEnd('\'))) -eq $bin })
if (-not $onPath) {
    # Inside the single-quoted literal, a quote is written twice.
    $quoted = $bin -replace "'", "''"
    Write-Output "Note: $bin is not on your PATH. To use the commands by name in new consoles:"
    Write-Output "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$quoted', 'User')"
}
