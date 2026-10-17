# Builds release binaries for one Windows target and packages them into dist\:
#   prowl-<version>-<target>.zip (+ .sha256)
# Usage: packaging\package.ps1 -Target x86_64-pc-windows-msvc -Version 0.1.0
param(
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$Version
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist"
$name = "prowl-$Version-$Target"
$stage = Join-Path $dist $name

Push-Location $root
cargo build --release --locked --target $Target -p prowld -p prowl
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
foreach ($bin in @("prowl", "prowld", "prowld-service")) {
    Copy-Item "target\$Target\release\$bin.exe" $stage
}
Copy-Item README.md $stage
$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $stage -DestinationPath $zip
$hash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
Set-Content -Path "$zip.sha256" -Value "$hash  $name.zip" -NoNewline
Pop-Location
Write-Host "packaged $name into $dist"
