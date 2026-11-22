# Installs or updates ikk-stt for the current user (no administrator
# rights needed). Run it again to update. AGPL-3.0-or-later.
#
#   irm https://raw.githubusercontent.com/iKK001/ikk-stt/main/install.ps1 | iex
#
# What it does: downloads the newest release from GitHub, checks its
# SHA-256 against the value published with it, unpacks it into
# %LOCALAPPDATA%\Programs\ikk-stt, adds a Start menu entry and lists the
# app under Settings > Apps > Installed apps, from where it can be removed.
# Settings and stored keys are not touched.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo    = 'iKK001/ikk-stt'
$asset   = 'ikk-stt-windows-x64.zip'
$exe     = 'ikk-stt.exe'
$name    = 'ikk-stt'
$target  = Join-Path $env:LOCALAPPDATA "Programs\$name"
$base    = "https://github.com/$repo/releases/latest/download"

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "$name needs 64-bit Windows."
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("$name-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $zip = Join-Path $work $asset
    Write-Host "Downloading $name ..."
    Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing

    # The release publishes the zip's SHA-256 next to it.
    $sumFile = Join-Path $work "$asset.sha256"
    Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile $sumFile -UseBasicParsing
    $expected = ((Get-Content $sumFile -Raw).Trim() -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) {
        throw "The download does not match its published SHA-256 (expected $expected, got $actual). Nothing was installed."
    }

    $fresh = Join-Path $work 'app'
    Expand-Archive -Path $zip -DestinationPath $fresh
    if (-not (Test-Path (Join-Path $fresh $exe))) {
        throw "The release does not contain $exe. Nothing was installed."
    }

    # A running copy holds its files: close it first.
    Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($exe)) -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($target, [StringComparison]::OrdinalIgnoreCase) } |
        ForEach-Object {
            Write-Host "Closing the running $name ..."
            $_ | Stop-Process -Force
            $_.WaitForExit(5000) | Out-Null
        }

    if (Test-Path $target) {
        Remove-Item -Path $target -Recurse -Force
    }
    New-Item -ItemType Directory -Path (Split-Path $target) -Force | Out-Null
    Move-Item -Path $fresh -Destination $target

    # Start menu entry.
    $programs = [Environment]::GetFolderPath('Programs')
    $link = Join-Path $programs "$name.lnk"
    $shell = New-Object -ComObject WScript.Shell
    $shortcut = $shell.CreateShortcut($link)
    $shortcut.TargetPath = Join-Path $target $exe
    $shortcut.WorkingDirectory = $target
    $shortcut.Description = 'Dictation for people with impaired speech'
    $shortcut.Save()

    $version = ''
    $versionFile = Join-Path $target 'VERSION.txt'
    if (Test-Path $versionFile) { $version = ' ' + (Get-Content $versionFile -Raw).Trim() }

    # Listed under Settings > Apps > Installed apps (for this user), with
    # the uninstall script that came in the release as its "Uninstall".
    $remover = Join-Path $target 'uninstall.ps1'
    if (Test-Path $remover) {
        $key = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$name"
        New-Item -Path $key -Force | Out-Null
        $quoted = $remover.Replace("'", "''")
        $run = "powershell.exe -NoProfile -ExecutionPolicy Bypass -Command `"& '$quoted'; Read-Host 'Press Enter to close'`""
        $sizeKb = [int](((Get-ChildItem $target -Recurse -File | Measure-Object Length -Sum).Sum) / 1KB)
        Set-ItemProperty -Path $key -Name DisplayName -Value $name
        Set-ItemProperty -Path $key -Name DisplayVersion -Value $version.Trim()
        Set-ItemProperty -Path $key -Name DisplayIcon -Value (Join-Path $target $exe)
        Set-ItemProperty -Path $key -Name InstallLocation -Value $target
        Set-ItemProperty -Path $key -Name UninstallString -Value $run
        Set-ItemProperty -Path $key -Name EstimatedSize -Value $sizeKb -Type DWord
        Set-ItemProperty -Path $key -Name NoModify -Value 1 -Type DWord
        Set-ItemProperty -Path $key -Name NoRepair -Value 1 -Type DWord
    }
    Write-Host ""
    Write-Host "$name$version is installed in $target"
    Write-Host "Start it from the Start menu ($name), or run:"
    Write-Host "  & '$(Join-Path $target $exe)'"
    Write-Host "To remove it: Settings > Apps > Installed apps > $name > Uninstall, or run:"
    Write-Host "  irm https://raw.githubusercontent.com/$repo/main/uninstall.ps1 | iex"
}
finally {
    Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
}
