#Requires -Version 5
<#
.SYNOPSIS
    Installs the release build of audioman.

.DESCRIPTION
    Stops any running audioman instance (Windows locks a running .exe so it
    can't be overwritten), then copies the release binary — and the prism.dll
    it needs beside it at runtime — to the user's software folder.

    Run a release build first:  cargo build --release
#>

$ErrorActionPreference = 'Stop'

$SourceDir = Join-Path $PSScriptRoot 'target\release'
$SourceExe = Join-Path $SourceDir 'audioman.exe'
$SourceDll = Join-Path $SourceDir 'prism.dll'

$DestDir = 'C:\Users\nitropc\stuff\software\audioman'
$DestExe = Join-Path $DestDir 'audioman.exe'
$DestDll = Join-Path $DestDir 'prism.dll'

if (-not (Test-Path $SourceExe)) {
    throw "Release build not found at $SourceExe. Run 'cargo build --release' first."
}

# Stop any running instance so the exe isn't locked while we overwrite it.
$running = Get-Process -Name 'audioman' -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "Stopping running audioman (PID $($running.Id -join ', '))..."
    $running | Stop-Process -Force
    $running | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
}

# Ensure the destination folder exists.
if (-not (Test-Path $DestDir)) {
    New-Item -ItemType Directory -Path $DestDir -Force | Out-Null
}

Copy-Item -Path $SourceExe -Destination $DestExe -Force
Write-Host "Copied audioman.exe -> $DestExe"

# prism.dll must sit next to the exe at runtime (build.rs drops it in target/release).
if (Test-Path $SourceDll) {
    Copy-Item -Path $SourceDll -Destination $DestDll -Force
    Write-Host "Copied prism.dll    -> $DestDll"
} else {
    Write-Warning "prism.dll not found at $SourceDll - audioman.exe won't run without it beside the exe."
}

# Launch the freshly installed build (working dir = its folder so prism.dll resolves).
Start-Process -FilePath $DestExe -WorkingDirectory $DestDir
Write-Host "Started $DestExe"

Write-Host "Done."
