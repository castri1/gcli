# Installs gcli from a GitHub release (Windows).
#
#   irm https://raw.githubusercontent.com/castri1/gcli/main/install.ps1 | iex
#
# Environment variables:
#   GCLI_VERSION       release tag to install (default: the latest), e.g. v0.1.0
#   GCLI_INSTALL_DIR   where to put gcli.exe (default: %LOCALAPPDATA%\gcli\bin)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$repo = 'castri1/gcli'
$version = if ($env:GCLI_VERSION) { $env:GCLI_VERSION } else { 'latest' }
$dir = if ($env:GCLI_INSTALL_DIR) { $env:GCLI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'gcli\bin' }

if (-not [Environment]::Is64BitOperatingSystem) { throw 'gcli needs 64-bit Windows' }
# Also runs on Windows on ARM, through x64 emulation.
$target = 'x86_64-pc-windows-msvc'

if ($version -eq 'latest') {
    $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
}

$name = "gcli-$version-$target"
$url = "https://github.com/$repo/releases/download/$version/$name.zip"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
    Write-Host "Downloading gcli $version ($target)..."
    $zip = Join-Path $tmp "$name.zip"
    Invoke-WebRequest $url -OutFile $zip -UseBasicParsing
    Invoke-WebRequest "$url.sha256" -OutFile "$zip.sha256" -UseBasicParsing
    $expected = ((Get-Content "$zip.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) { throw "checksum mismatch for $name.zip" }

    Expand-Archive $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory $dir -Force | Out-Null
    Copy-Item (Join-Path $tmp "$name\gcli.exe") $dir -Force
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$exe = Join-Path $dir 'gcli.exe'
Write-Host "Installed $exe ($(& $exe --version))"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($userPath -split ';') -contains $dir)) {
    $newPath = if ($userPath) { "$userPath;$dir" } else { $dir }
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path += ";$dir"
    Write-Host "Added $dir to your PATH. Other open terminals need a restart to see it."
}

if (-not (Get-Command gcloud -ErrorAction SilentlyContinue)) {
    Write-Host ''
    Write-Host 'Note: signing in to Google Cloud uses the gcloud CLI: https://cloud.google.com/sdk/docs/install'
}
