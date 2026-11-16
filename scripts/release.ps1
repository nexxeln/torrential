# Builds the Windows release zip and its .sha256 the way scripts/release.sh builds the others:
#   scripts/release.ps1   writes dist/torrential-x86_64-pc-windows-msvc.zip and its .sha256
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$target = 'x86_64-pc-windows-msvc'
$asset = "torrential-$target.zip"

cargo build --release --locked -p torrential
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$bin = 'target\release\torrential.exe'
# The static CRT keeps the binary off the Visual C++ runtime, which a fresh Windows may lack.
$imports = & dumpbin /dependents $bin | Out-String
if ($imports -match '(?i)vcruntime|msvcp') {
  throw "$bin links the Visual C++ runtime: $imports"
}

New-Item -ItemType Directory -Force -Path dist | Out-Null
$stage = Join-Path ([IO.Path]::GetTempPath()) "torrential-release-$([guid]::NewGuid())"
New-Item -ItemType Directory -Path $stage | Out-Null
Copy-Item $bin, LICENSE $stage
Remove-Item -Force "dist\$asset" -ErrorAction SilentlyContinue
Compress-Archive -Path "$stage\*" -DestinationPath "dist\$asset"
Remove-Item -Recurse -Force $stage

$hash = (Get-FileHash -Algorithm SHA256 "dist\$asset").Hash.ToLower()
# The same line sha256sum writes, so every release's .sha256 reads alike.
[IO.File]::WriteAllText((Join-Path (Get-Location) "dist\$asset.sha256"), "$hash  $asset`n")
Get-Content "dist\$asset.sha256"
