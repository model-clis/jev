$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'Unsupported platform: Windows x64 is required.' }
$repo = 'https://github.com/model-clis/jev'
$version = if ($env:JEV_VERSION) { $env:JEV_VERSION } else { 'latest' }
if ($version -eq 'latest') { $base = "$repo/releases/latest/download" }
elseif ($version -match '^v\d{4}\.[1-9]\d*\.\d+$') { $base = "$repo/releases/download/$version" }
else { throw 'JEV_VERSION must be a complete vYYYY.MDD.REV tag.' }
$installDir = if ($env:JEV_INSTALL_DIR) { $env:JEV_INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
$asset = 'jev-windows-x86_64.exe'
$tempDir = Join-Path ([IO.Path]::GetTempPath()) ('jev-install-' + [Guid]::NewGuid().ToString('N'))

try {
  [IO.Directory]::CreateDirectory($tempDir) | Out-Null
  $binary = Join-Path $tempDir $asset
  $checksum = "$binary.sha256"
  Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $binary
  Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile $checksum
  $expected = ((Get-Content -LiteralPath $checksum -TotalCount 1) -split '\s+')[0]
  if ($expected -notmatch '^[0-9a-fA-F]{64}$') { throw 'Invalid SHA256 file.' }
  $actual = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash
  if ($actual -ne $expected) { throw 'SHA256 verification failed.' }
  [IO.Directory]::CreateDirectory($installDir) | Out-Null
  $destination = Join-Path $installDir 'jev.exe'
  $stage = Join-Path $installDir ('.jev-' + [Guid]::NewGuid().ToString('N') + '.exe')
  Copy-Item -LiteralPath $binary -Destination $stage
  Move-Item -LiteralPath $stage -Destination $destination -Force
  Write-Host "Installed jev to $destination"
  if (($env:PATH -split ';') -notcontains $installDir) { Write-Host "Add $installDir to PATH." }
} finally {
  if (Test-Path -LiteralPath $tempDir) { Remove-Item -LiteralPath $tempDir -Recurse -Force }
}
