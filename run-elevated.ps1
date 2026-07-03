# Launch the WinSearch GUI with administrator rights (needed to read the NTFS
# MFT / USN journal). Builds release first if the binary is missing.
$ErrorActionPreference = "Stop"
$exe = Join-Path $PSScriptRoot "target\release\WinSearch.exe"
if (-not (Test-Path $exe)) {
    Write-Host "Building release..." -ForegroundColor Cyan
    cargo build --release
}
Write-Host "Launching WinSearch (elevated)..." -ForegroundColor Green
Start-Process $exe -Verb RunAs
