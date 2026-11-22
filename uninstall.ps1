# Removes ikk-stt for the current user: the program folder and its Start
# menu entry. Settings and stored keys stay (see the last lines printed).
# AGPL-3.0-or-later.
#
#   irm https://raw.githubusercontent.com/iKK001/ikk-stt/main/uninstall.ps1 | iex

$ErrorActionPreference = 'Stop'

$exe    = 'ikk-stt.exe'
$name   = 'ikk-stt'
$target = Join-Path $env:LOCALAPPDATA "Programs\$name"

Get-Process -Name ([IO.Path]::GetFileNameWithoutExtension($exe)) -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($target, [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        Write-Host "Closing the running $name ..."
        $_ | Stop-Process -Force
        $_.WaitForExit(5000) | Out-Null
    }

if (Test-Path $target) {
    Remove-Item -Path $target -Recurse -Force
    Write-Host "Removed $target"
} else {
    Write-Host "$name is not installed in $target"
}

$link = Join-Path ([Environment]::GetFolderPath('Programs')) "$name.lnk"
if (Test-Path $link) {
    Remove-Item -Path $link -Force
    Write-Host "Removed the Start menu entry"
}

Write-Host ""
Write-Host "Your settings were left in place. To remove them as well, delete:"
Write-Host "  $(Join-Path $env:APPDATA 'ikk\ikk-stt')"
Write-Host "  $(Join-Path $env:LOCALAPPDATA 'ikk\ikk-stt')"
Write-Host "Stored API keys are in the Windows Credential Manager (Control Panel > Credential Manager)."
