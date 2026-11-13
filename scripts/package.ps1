<#
.SYNOPSIS
  Builds the Chud Clone deliverables in dist\:
    - ChudClone-<version>-portable.zip  (exe + portable.flag: data next to the program)
    - ChudClone-<version>-setup.exe     (Inno Setup: Program Files, shortcuts, uninstaller)
    - SHA256SUMS-<version>.txt

.DESCRIPTION
  Touches no disk: compiles, copies files and compresses.
  Requirements: Rust (cargo), Node.js (third-party notice), Inno Setup 6.

.PARAMETER NoBuild
  Reuse the binaries already in target\release.
#>
[CmdletBinding()]
param([switch]$NoBuild)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Chud Clone $version" -ForegroundColor Cyan

if (-not $NoBuild) {
  & cargo build --release -p chud-clone-app -p chud-clone-cli
  if ($LASTEXITCODE -ne 0) { throw 'build failed' }
}
foreach ($exe in 'chud-clone.exe', 'chud-clone-cli.exe') {
  if (-not (Test-Path "target\release\$exe")) { throw "target\release\$exe not found" }
}

$dist = Join-Path $root 'dist'
$stage = Join-Path $dist 'stage'
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force "$stage\licenses" | Out-Null

Copy-Item target\release\chud-clone.exe, target\release\chud-clone-cli.exe $stage
Copy-Item packaging\README.txt $stage
Copy-Item LICENSE "$stage\licenses\Chud-Clone-MIT.txt"
Copy-Item licenses\*.txt "$stage\licenses\"
& node scripts\third-party.js "$stage\licenses\Third-party-components.txt"
if ($LASTEXITCODE -ne 0) { throw 'third-party notice failed' }

# --- Portable version
$portName = "ChudClone-$version-portable"
$port = Join-Path $dist $portName
if (Test-Path $port) { Remove-Item -Recurse -Force $port }
Copy-Item -Recurse $stage $port
Set-Content -Encoding ascii -Path "$port\portable.flag" -Value 'This file enables portable mode: settings and reports are kept in ChudClone-data, next to the program.'
$zip = Join-Path $dist "$portName.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $port -DestinationPath $zip
Write-Host "Portable : $zip" -ForegroundColor Green

# --- Installer
$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
  Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 (ISCC.exe) not found' }
& $iscc /Q "/DAppVersion=$version" "/DSourceDir=$stage" packaging\installer.iss
if ($LASTEXITCODE -ne 0) { throw 'Inno Setup failed' }
$setup = Join-Path $dist "ChudClone-$version-setup.exe"
Write-Host "Installer: $setup" -ForegroundColor Green

Write-Host "`nSHA-256 checksums:" -ForegroundColor Cyan
Get-FileHash -Algorithm SHA256 $zip, $setup, "$port\chud-clone.exe", "$port\chud-clone-cli.exe" |
  ForEach-Object { '{0}  {1}' -f $_.Hash.ToLower(), (Split-Path -Leaf $_.Path) } |
  Tee-Object -FilePath (Join-Path $dist "SHA256SUMS-$version.txt")
