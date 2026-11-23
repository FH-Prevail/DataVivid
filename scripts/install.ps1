<#
.SYNOPSIS
    Installs DataVivid for the current user (no admin rights needed).

.DESCRIPTION
    - uses -Exe, or datavivid.exe next to this script (release zip),
      or builds the release exe from the repository
    - copies it to %LOCALAPPDATA%\Programs\DataVivid
    - adds a Start menu shortcut and an entry in Settings > Apps
    - registers DataVivid for its file types ("Open with", Default apps)
    - optionally opens Settings > Default apps so you can make it the default

.EXAMPLE
    pwsh -File scripts\install.ps1
    pwsh -File scripts\install.ps1 -Exe .\datavivid.exe -NoSettings
#>
param(
    [string]$Exe,
    [switch]$NoSettings
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

# A release download ships datavivid.exe next to this script.
if (-not $Exe -and (Test-Path (Join-Path $PSScriptRoot 'datavivid.exe'))) {
    $Exe = Join-Path $PSScriptRoot 'datavivid.exe'
}
if (-not $Exe) {
    Push-Location $repo
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
        $targetDir = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
        $Exe = Join-Path $targetDir 'release\datavivid.exe'
    } finally { Pop-Location }
}
if (-not (Test-Path $Exe)) { throw "Executable not found: $Exe" }

$dest = Join-Path $env:LOCALAPPDATA 'Programs\DataVivid'
New-Item -ItemType Directory -Force $dest | Out-Null
$target = Join-Path $dest 'datavivid.exe'

# Stop a running copy so the file can be replaced.
Get-Process datavivid -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $target } |
    ForEach-Object { $_.CloseMainWindow() | Out-Null; Start-Sleep -Milliseconds 800; if (-not $_.HasExited) { $_.Kill() } }

Copy-Item $Exe $target -Force
Copy-Item (Join-Path $PSScriptRoot 'uninstall.ps1') (Join-Path $dest 'uninstall.ps1') -Force
Unblock-File $target -ErrorAction SilentlyContinue  # downloaded exe: drop the "from the internet" mark
$version = (Get-Item $target).VersionInfo.ProductVersion
if (-not $version) { $version = 'unknown' }

# Start menu shortcut
$programs = [Environment]::GetFolderPath('Programs')
$shell = New-Object -ComObject WScript.Shell
$lnk = $shell.CreateShortcut((Join-Path $programs 'DataVivid.lnk'))
$lnk.TargetPath = $target
$lnk.WorkingDirectory = $dest
$lnk.IconLocation = "$target,0"
$lnk.Description = 'View, clean and convert tabular data'
$lnk.Save()

# Settings > Apps entry (per user)
$un = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\DataVivid'
New-Item -Path $un -Force | Out-Null
$uninstallCmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$dest\uninstall.ps1`""
Set-ItemProperty $un -Name DisplayName -Value 'DataVivid'
Set-ItemProperty $un -Name DisplayVersion -Value $version
Set-ItemProperty $un -Name Publisher -Value 'Prevail Team, FH Steyr'
Set-ItemProperty $un -Name DisplayIcon -Value "$target,0"
Set-ItemProperty $un -Name InstallLocation -Value $dest
Set-ItemProperty $un -Name UninstallString -Value $uninstallCmd
Set-ItemProperty $un -Name NoModify -Value 1 -Type DWord
Set-ItemProperty $un -Name NoRepair -Value 1 -Type DWord
$size = [int]((Get-Item $target).Length / 1KB)
Set-ItemProperty $un -Name EstimatedSize -Value $size -Type DWord

# File types (HKCU only)
$p = Start-Process -FilePath $target -ArgumentList '--register' -Wait -PassThru -WindowStyle Hidden
if ($p.ExitCode -ne 0) { throw "datavivid --register failed with exit code $($p.ExitCode)" }

Write-Host "DataVivid $version installed to $dest"
Write-Host "It now appears in Explorer's 'Open with' menu for CSV, TSV, Parquet, JSON, Arrow, Avro, Excel, ODS and SQLite files."
if (-not $NoSettings) {
    Write-Host "Opening Settings > Default apps: choose DataVivid for the file types you want it to open by default."
    Start-Process 'ms-settings:defaultapps?registeredAppUser=DataVivid'
}
