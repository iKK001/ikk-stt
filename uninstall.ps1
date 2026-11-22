# Removes Bettina (ikk-stt) for the current user: the program folder, its Start
# menu entry and its line under Settings > Apps > Installed apps (whose
# "Uninstall" runs this script). Settings and stored keys stay (see the
# last lines printed).
# AGPL-3.0-or-later.
#
#   irm https://raw.githubusercontent.com/iKK001/ikk-stt/main/uninstall.ps1 | iex

$ErrorActionPreference = 'Stop'

$exe    = 'ikk-stt.exe'
$name   = 'ikk-stt'
$shown  = 'Bettina'
$target = Join-Path $env:LOCALAPPDATA "Programs\$name"

Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($exe)) -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($target, [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        Write-Host "Closing the running $shown ..."
        $_ | Stop-Process -Force
        $_.WaitForExit(5000) | Out-Null
    }

if (Test-Path $target) {
    Remove-Item -Path $target -Recurse -Force
    Write-Host "Removed $target"
} else {
    Write-Host "$shown is not installed in $target"
}

# The Start menu entry, under the shown name or the project's (older versions).
foreach ($entry in @($shown, $name)) {
    $link = Join-Path ([Environment]::GetFolderPath('Programs')) "$entry.lnk"
    if (Test-Path $link) {
        Remove-Item -Path $link -Force
        Write-Host "Removed the Start menu entry"
    }
}

$key = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$name"
if (Test-Path $key) {
    Remove-Item -Path $key -Recurse -Force
    Write-Host "Removed the entry in Installed apps"
}

Write-Host ""
Write-Host "Your settings were left in place. To remove them as well, delete:"
Write-Host "  $(Join-Path $env:APPDATA 'ikk\ikk-stt')"
Write-Host "  $(Join-Path $env:LOCALAPPDATA 'ikk\ikk-stt')"
Write-Host "Your stored API keys were left in place too (encrypted). To remove them, delete:"
Write-Host "  $(Join-Path $env:APPDATA 'ch.ikk\ikk-stt')"
