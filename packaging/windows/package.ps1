<#
.SYNOPSIS
Builds the StreamForge Windows binaries in release mode, checks them on
this system and packs them (ADR 0020).

.DESCRIPTION
  pwsh packaging/windows/package.ps1 -FFmpeg <dir>

Run from the repository root on Windows, with Rust, the MSVC build tools
and LLVM (for bindgen; LIBCLANG_PATH). <dir> is the output of
packaging/windows/build-ffmpeg.sh: the binaries link its import libraries,
and its DLLs, licences and build information go into the package. Writes
dist\<name>.zip and dist\<name>.zip.sha256 (in sha256sum's format).
#>
param(
    [Parameter(Mandatory = $true)] [string] $FFmpeg
)

Set-StrictMode -Version 3.0
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$FFmpeg = (Resolve-Path -LiteralPath $FFmpeg).Path
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$name = "streamforge-$version-x86_64-windows"
$out = Join-Path (Get-Location) "dist\$name"
$bins = @('streamforge', 'streamforge-engine', 'streamforge-desktop', 'streamforge-mcp')

# Runs a program to its end (also a window program, which a console does
# not wait for) and returns what it printed; fails if it fails.
function Invoke-Program([string] $Program, [string[]] $Arguments, [string] $Stdin = $null) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $Program
    foreach ($argument in $Arguments) {
        $info.ArgumentList.Add($argument)
    }
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.RedirectStandardInput = $true
    $process = [System.Diagnostics.Process]::Start($info)
    if ($Stdin) {
        $process.StandardInput.WriteLine($Stdin)
    }
    $process.StandardInput.Close()
    $stderr = $process.StandardError.ReadToEndAsync()
    $stdout = $process.StandardOutput.ReadToEnd()
    $process.WaitForExit()
    if ($stderr.Result) {
        Write-Host $stderr.Result.TrimEnd()
    }
    if ($process.ExitCode -ne 0) {
        throw "$Program $Arguments exited with $($process.ExitCode): $stdout"
    }
    $stdout.TrimEnd()
}

$env:FFMPEG_DIR = $FFmpeg
cargo build --release --locked -p streamforge-cli -p streamforge-engine -p streamforge-desktop -p streamforge-mcp

if (Test-Path -LiteralPath $out) {
    Remove-Item -LiteralPath $out -Recurse -Force
}
Remove-Item -Path "dist\$name.zip*" -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path "$out\bin" | Out-Null
foreach ($bin in $bins) {
    Copy-Item -LiteralPath "target\release\$bin.exe" -Destination "$out\bin\"
}
Copy-Item -Path "$FFmpeg\bin\*.dll" -Destination "$out\bin\"
Copy-Item -LiteralPath LICENSE-MIT, LICENSE-APACHE, README.md, CHANGELOG.md, packaging\windows\INSTALL.md, packaging\windows\install.ps1 -Destination $out
# The documentation the README and the user guide link to, at the same
# relative paths as in the repository.
Copy-Item -LiteralPath docs -Destination $out -Recurse
# The desktop binary embeds IBM Plex, whose licence must travel with it;
# FFmpeg's build brings its own and those of what is built into it.
Copy-Item -LiteralPath crates\streamforge-desktop\assets\fonts\OFL.txt -Destination "$out\LICENSE-OFL-IBM-Plex.txt"
Copy-Item -LiteralPath "$FFmpeg\licenses" -Destination $out -Recurse

# Every relative link in the user's entry points must resolve inside the
# package (they are read there, not on GitHub).
$broken = @()
foreach ($doc in @('README.md', 'CHANGELOG.md', 'INSTALL.md', 'docs\user-guide.md')) {
    $path = Join-Path $out $doc
    $dir = Split-Path -Parent $path
    foreach ($match in [regex]::Matches((Get-Content -Raw -LiteralPath $path), '\]\(([^)\s]+)\)')) {
        $target = $match.Groups[1].Value -replace '#.*$', ''
        if ($target -and $target -notmatch '^[a-z]+:' -and -not (Test-Path -LiteralPath (Join-Path $dir $target))) {
            $broken += "broken link in ${doc}: $target"
        }
    }
}
if ($broken) {
    throw ($broken -join "`n")
}

# The binaries and DLLs import only each other and libraries every
# Windows 10 (1903 or newer) has: no Visual C++ runtime (the C runtime is
# linked statically, see .cargo/config.toml), nothing a runner happens to
# have. A new import fails here until it is checked and added.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$dumpbin = & $vswhere -latest -products * -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
if (-not $dumpbin) {
    throw 'dumpbin.exe not found'
}
$windows = @('advapi32', 'bcrypt', 'bcryptprimitives', 'combase', 'coremessaging', 'crypt32', 'd3d11',
    'dwmapi', 'dxgi', 'gdi32', 'imm32', 'kernel32', 'mmdevapi', 'msvcrt', 'ncrypt', 'ntdll', 'ole32',
    'oleaut32', 'opengl32', 'secur32', 'setupapi', 'shell32', 'shlwapi', 'uiautomationcore', 'user32',
    'userenv', 'uxtheme', 'ws2_32') | ForEach-Object { "$_.dll" }
$bad = @()
foreach ($file in Get-ChildItem -Path "$out\bin\*" -Include *.exe, *.dll) {
    $imports = @(& $dumpbin /nologo /dependents $file.FullName |
            ForEach-Object { $_.Trim() } | Where-Object { $_ -match '^[^\s]+\.dll$' })
    Write-Host "$($file.Name): $($imports -join ' ')"
    # Every one imports at least kernel32; none means the output was not read.
    if ($imports -notcontains 'kernel32.dll') {
        $bad += "$($file.Name): no imports read from dumpbin"
    }
    foreach ($import in $imports) {
        $bundled = Test-Path -LiteralPath (Join-Path "$out\bin" $import)
        # API sets (api-ms-win-core-*), except the C runtime's.
        $apiSet = $import -match '^api-ms-win-core-'
        # The Visual C++ runtime is refused even if it were bundled.
        $runtime = $import -match '^(vcruntime|msvcp|concrt|vccorlib|ucrtbase)|^api-ms-win-crt-'
        if ($runtime -or -not ($bundled -or $apiSet -or $windows -contains $import)) {
            $bad += "$($file.Name) imports $import"
        }
    }
}
if ($bad) {
    throw ($bad -join "`n")
}

# What this package was built from and with, for bug reports.
$os = Get-CimInstance Win32_OperatingSystem
$info = @(
    "StreamForge $version for Windows (x86_64)"
    "commit: $(git rev-parse HEAD)"
    "rustc: $(rustc --version)"
    "built on: $($os.Caption) $($os.Version)"
    "C runtime: static (+crt-static)"
    "licence: GPL version 3 for the programs as a whole (INSTALL.md)"
    ''
    (Get-Content -Raw -LiteralPath "$FFmpeg\BUILD-INFO.txt").TrimEnd()
)
[IO.File]::WriteAllLines("$out\BUILD-INFO.txt", [string[]] $info)
Get-Content -LiteralPath "$out\BUILD-INFO.txt"

# Smoke test on this system: each binary starts and answers, from the
# package's bin\ (so with its DLLs), and the CLI records with libx264.
$desktopVersion = Invoke-Program "$out\bin\streamforge-desktop.exe" @('--version')
if ($desktopVersion -ne "streamforge-desktop $version") {
    throw "streamforge-desktop --version printed '$desktopVersion'"
}
Invoke-Program "$out\bin\streamforge.exe" @('--version')
Invoke-Program "$out\bin\streamforge.exe" @('status', '--json')
$response = Invoke-Program "$out\bin\streamforge-engine.exe" @() '{"id":1,"command":"get_status"}'
Write-Host $response
if (-not $response.Contains('"id":1,"ok":true')) {
    throw 'the engine did not answer get_status'
}
Invoke-Program "$out\bin\streamforge-engine.exe" @('--version')
Invoke-Program "$out\bin\streamforge-mcp.exe" @('--version')

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("streamforge-smoke-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
$report = Invoke-Program "$out\bin\streamforge.exe" @('--output-dir', $tmp, 'record', '--file-name', 'smoke.mkv', '--duration-ms', '2000', '--video-encoder', 'software')
Write-Host $report
$video = ($report | ConvertFrom-Json).recording.video
$recording = Get-Item -LiteralPath (Join-Path $tmp 'smoke.mkv')
# At least a quarter of the frames: a slow runner may drop some (counted).
if ($video.encoder -ne 'libx264' -or $video.frames_encoded -lt 30 -or $recording.Length -lt 10000) {
    throw "the recording used $($video.encoder) for $($video.frames_encoded) frames, $($recording.Length) bytes"
}

# The installer, as users run it (Windows PowerShell 5.1): into a fresh
# folder with a space, a Start menu shortcut to the installed desktop, and
# out again, leaving only what was not its own.
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'StreamForge.lnk'
$shell = New-Object -ComObject WScript.Shell
$fso = New-Object -ComObject Scripting.FileSystemObject
function Invoke-Installer([string] $Script, [string[]] $Arguments, [switch] $Refused) {
    # The exit code is checked here, also when a refusal is expected.
    $PSNativeCommandUseErrorActionPreference = $false
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Script @Arguments
    if ($Refused -and $LASTEXITCODE -eq 0) {
        throw "$Script $Arguments succeeded, but should have been refused"
    }
    if (-not $Refused -and $LASTEXITCODE -ne 0) {
        throw "$Script $Arguments exited with $LASTEXITCODE"
    }
}
function Assert-Installed([string] $Prefix) {
    $null = Invoke-Program "$Prefix\bin\streamforge.exe" @('--version')
    foreach ($file in @('docs\user-guide.md', 'CHANGELOG.md', 'BUILD-INFO.txt', 'licenses\FFmpeg\COPYING.GPLv3', 'install.ps1', 'installed-files.txt')) {
        if (-not (Test-Path -LiteralPath (Join-Path $Prefix $file))) {
            throw "$file is missing in $Prefix"
        }
    }
    $link = $shell.CreateShortcut($shortcut)
    # The same file however its path is spelled.
    $same = (Test-Path -LiteralPath $link.TargetPath) -and
        $fso.GetFile($link.TargetPath).ShortPath -eq $fso.GetFile("$Prefix\bin\streamforge-desktop.exe").ShortPath
    if (-not $same -or $link.WorkingDirectory -ne $env:USERPROFILE) {
        throw "the shortcut points to '$($link.TargetPath)' in '$($link.WorkingDirectory)'"
    }
}
function Assert-Removed([string] $Prefix) {
    $left = @(Get-ChildItem -LiteralPath $Prefix -Recurse -Force | ForEach-Object { $_.FullName.Substring($Prefix.Length + 1) })
    if ($left.Count -ne 1 -or $left[0] -ne 'notes.txt' -or (Test-Path -LiteralPath $shortcut)) {
        throw "install.ps1 -Uninstall left behind: $($left -join ', ') (shortcut: $(Test-Path -LiteralPath $shortcut))"
    }
}

# The tests use the real Start menu; a shortcut of this machine's own
# installation is put back afterwards.
$saved = Join-Path $tmp 'StreamForge.lnk'
if (Test-Path -LiteralPath $shortcut) {
    Copy-Item -LiteralPath $shortcut -Destination $saved
}
try {
    $prefix = Join-Path $tmp 'with space\StreamForge'
    Invoke-Installer "$out\install.ps1" @('-Prefix', $prefix)
    Assert-Installed $prefix
    # A file of an older version listed in the installation's list is removed
    # when installing again; a file of the user's stays.
    Set-Content -LiteralPath "$prefix\bin\avcodec-61.dll" -Value 'old'
    Add-Content -LiteralPath "$prefix\installed-files.txt" -Value 'bin\avcodec-61.dll'
    Set-Content -LiteralPath "$prefix\notes.txt" -Value 'mine'
    Invoke-Installer "$out\install.ps1" @('-Prefix', $prefix)
    Assert-Installed $prefix
    if (Test-Path -LiteralPath "$prefix\bin\avcodec-61.dll") {
        throw 'installing again kept a file of the previous installation'
    }
    # The installed copy of the script removes its own installation.
    Invoke-Installer "$prefix\install.ps1" @('-Uninstall')
    Assert-Removed $prefix
    # The prefix spelled with its 8.3 short name ("with space" has one where
    # the drive makes them) for installing, the long one for removing, and
    # the other way round.
    $short = $fso.GetFolder($prefix).ShortPath
    if ($short -eq $prefix) {
        Write-Host "no 8.3 names for $prefix; the short spelling is not tested"
    } else {
        Write-Host "short spelling: $short"
        Invoke-Installer "$out\install.ps1" @('-Prefix', $short)
        Assert-Installed $prefix
        Invoke-Installer "$out\install.ps1" @('-Uninstall', '-Prefix', $prefix)
        Assert-Removed $prefix
        Invoke-Installer "$out\install.ps1" @('-Prefix', $prefix)
        Invoke-Installer "$out\install.ps1" @('-Uninstall', '-Prefix', $short)
        Assert-Removed $prefix
    }
    # A folder of the installation replaced by a junction: nothing is removed
    # through it.
    Invoke-Installer "$out\install.ps1" @('-Prefix', $prefix)
    $outside = Join-Path $tmp 'outside'
    New-Item -ItemType Directory -Path $outside | Out-Null
    Set-Content -LiteralPath "$outside\user-guide.md" -Value 'not StreamForge'
    Remove-Item -LiteralPath "$prefix\docs" -Recurse -Force
    New-Item -ItemType Junction -Path "$prefix\docs" -Target $outside | Out-Null
    Invoke-Installer "$out\install.ps1" @('-Uninstall', '-Prefix', $prefix) -Refused
    if (-not (Test-Path -LiteralPath "$outside\user-guide.md")) {
        throw 'install.ps1 -Uninstall removed a file through a junction'
    }
    # Removes the junction, not what it points to.
    (Get-Item -LiteralPath "$prefix\docs" -Force).Delete()
    Invoke-Installer "$out\install.ps1" @('-Uninstall', '-Prefix', $prefix)
    Assert-Removed $prefix
    # Files in the way that StreamForge did not install: refused, untouched.
    $taken = Join-Path $tmp 'taken'
    New-Item -ItemType Directory -Path $taken | Out-Null
    Set-Content -LiteralPath "$taken\README.md" -Value 'mine'
    Invoke-Installer "$out\install.ps1" @('-Prefix', $taken) -Refused
    if ((Get-Content -Raw -LiteralPath "$taken\README.md").Trim() -ne 'mine' -or (Test-Path -LiteralPath "$taken\installed-files.txt") -or (Test-Path -LiteralPath $shortcut)) {
        throw 'install.ps1 changed a folder whose files it refused'
    }
    # Without -Prefix: under %LOCALAPPDATA%\Programs.
    $localAppData = $env:LOCALAPPDATA
    $env:LOCALAPPDATA = Join-Path $tmp 'local'
    try {
        Invoke-Installer "$out\install.ps1" @()
        Assert-Installed (Join-Path $env:LOCALAPPDATA 'Programs\StreamForge')
        Invoke-Installer "$out\install.ps1" @('-Uninstall')
        if ((Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'Programs\StreamForge')) -or (Test-Path -LiteralPath $shortcut)) {
            throw 'install.ps1 -Uninstall left the default installation behind'
        }
    } finally {
        $env:LOCALAPPDATA = $localAppData
    }
} finally {
    if (Test-Path -LiteralPath $saved) {
        Copy-Item -LiteralPath $saved -Destination $shortcut -Force
    } elseif (Test-Path -LiteralPath $shortcut) {
        Remove-Item -LiteralPath $shortcut
    }
}
Remove-Item -LiteralPath $tmp -Recurse -Force

& 7z a -tzip -bso0 "dist\$name.zip" $out
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath "dist\$name.zip").Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path (Get-Location) "dist\$name.zip.sha256"), "$hash  $name.zip`n")
Get-Content -LiteralPath "dist\$name.zip.sha256"
