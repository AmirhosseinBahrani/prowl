# Installs or upgrades PRowl on Windows.
#
#   irm https://github.com/AmirhosseinBahrani/prowl/releases/latest/download/install.ps1 | iex
#   & ([scriptblock]::Create((irm …/install.ps1))) v0.1.0              # a specific version
#   & ([scriptblock]::Create((irm …/install.ps1))) -NoService          # don't (re)start the service
#
# Installs into %LOCALAPPDATA%\Programs\prowl (override with PROWL_INSTALL_DIR) and adds it to
# your user PATH. No administrator rights needed.
param(
    [string]$Version = "latest",
    [switch]$NoService
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$repo = "AmirhosseinBahrani/prowl"
$target = "x86_64-pc-windows-msvc"  # also runs on Windows on Arm through emulation
$dir = if ($env:PROWL_INSTALL_DIR) { $env:PROWL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\prowl" }

if ($Version -eq "latest") {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -Headers @{ "User-Agent" = "prowl-installer" }
    $Version = $release.tag_name
}
if (-not $Version.StartsWith("v")) { $Version = "v$Version" }

$name = "prowl-$($Version.TrimStart('v'))-$target"
$base = "https://github.com/$repo/releases/download/$Version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "Downloading PRowl $Version ($target)..."
    $zip = Join-Path $tmp "$name.zip"
    Invoke-WebRequest "$base/$name.zip" -OutFile $zip
    $sum = Join-Path $tmp "sum"
    Invoke-WebRequest "$base/$name.zip.sha256" -OutFile $sum
    $expected = ((Get-Content $sum -Raw) -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($expected -ne $actual) { throw "Checksum verification failed for $name.zip" }
    Expand-Archive -Path $zip -DestinationPath $tmp

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    # A running daemon locks its executables; stop it so they can be replaced.
    Get-Process prowld, prowld-service -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    foreach ($bin in "prowl", "prowld", "prowld-service") {
        Copy-Item (Join-Path $tmp "$name\$bin.exe") (Join-Path $dir "$bin.exe") -Force
    }
    Write-Host "Installed PRowl $Version"
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not (($userPath -split ';') -contains $dir)) {
    [Environment]::SetEnvironmentVariable("Path", (($userPath, $dir) -ne "" -join ';'), "User")
    $env:Path = "$env:Path;$dir"
    Write-Host "Added $dir to your PATH (new terminals pick it up)."
}

if (-not $NoService) {
    Write-Host "Starting the background service..."
    & (Join-Path $dir "prowld.exe") service install | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Couldn't start the background service; run 'prowld service install' to see why" }
}

foreach ($tool in "claude", "git") {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        Write-Host "Note: PRowl reviews with '$tool', which isn't on your PATH yet."
    }
}
Write-Host ""
Write-Host "Done. Open PRowl with: prowl"
