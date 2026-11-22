<#
.SYNOPSIS
    Removes the per-user DataVivid installation made by install.ps1.
    Your data files are not touched.
#>
$ErrorActionPreference = 'Continue'
$dest = Join-Path $env:LOCALAPPDATA 'Programs\DataVivid'
$exe = Join-Path $dest 'datavivid.exe'

Get-Process datavivid -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe } |
    ForEach-Object { $_.CloseMainWindow() | Out-Null; Start-Sleep -Milliseconds 800; if (-not $_.HasExited) { $_.Kill() } }

if (Test-Path $exe) {
    Start-Process -FilePath $exe -ArgumentList '--unregister' -Wait -WindowStyle Hidden
}

$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'DataVivid.lnk'
Remove-Item $lnk -ErrorAction SilentlyContinue
Remove-Item 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DataVivid' -Recurse -ErrorAction SilentlyContinue
Remove-Item $dest -Recurse -Force -ErrorAction SilentlyContinue
Write-Host 'DataVivid was removed.'
