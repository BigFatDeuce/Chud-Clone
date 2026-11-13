<#
.SYNOPSIS
  Builds the Chud Clone WinPE rescue key (ISO image, and optionally writes it to a USB key).

.DESCRIPTION
  Cloning the disk Windows is running from, from that same Windows, is refused: the copy would not
  be consistent. The rescue key boots a minimal Windows (WinPE) that runs entirely in memory; the
  internal disks are at rest there and can be cloned cleanly.

  The graphical interface (WebView2) does not work in WinPE: the key opens a text menu that drives
  chud-clone-cli.exe (list, inspect, SMART, dry run, clone), with the same checks as the
  application. Reports are saved on the key (ChudClone-reports folder).

  Steps:
    1. checks the prerequisites: ADMINISTRATOR prompt, Windows ADK + WinPE add-on;
    2. builds chud-clone-cli.exe (unless -Cli);
    3. "copype amd64" into a new work folder;
    4. mounts boot.wim (DISM), copies the program and the menu, sets the keyboard, unmounts;
    5. produces the ISO image (MakeWinPEMedia /ISO);
    6. with -UsbDrive only: writes the USB key (MakeWinPEMedia /UFD), after checks (removable USB
       disk, neither system nor boot disk, reasonable size) and a typed confirmation.
       THE KEY IS COMPLETELY ERASED.

  Prerequisites (free, from Microsoft): "Windows ADK" and its "Windows PE add-on",
  https://learn.microsoft.com/windows-hardware/get-started/adk-install

.PARAMETER Output
  Path of the ISO image to produce (default: dist\ChudClone-Rescue.iso).

.PARAMETER UsbDrive
  Letter of a USB key to turn into a rescue key (e.g. "E:"). It will be ERASED.

.PARAMETER Language
  Language of the rescue menu and of the messages: en (default) or fr. It can be switched from the
  menu.

.PARAMETER Keyboard
  Keyboard layout in WinPE (default: en-US, or fr-FR when -Language fr).

.PARAMETER Cli
  chud-clone-cli.exe to embed (default: built by this script).

.PARAMETER WorkDir
  Work folder (default: %TEMP%\ChudClone-winpe-<date>). It must not exist.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\make-winpe.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\make-winpe.ps1 -UsbDrive E: -Keyboard en-GB
#>
[CmdletBinding()]
param(
  [string]$Output,
  [string]$UsbDrive,
  [ValidateSet('en', 'fr')][string]$Language = 'en',
  [ValidateSet('en-US', 'en-GB', 'fr-FR', 'fr-BE', 'fr-CH', 'fr-CA', 'de-DE', 'es-ES', 'it-IT')][string]$Keyboard,
  [string]$Cli,
  [string]$WorkDir = (Join-Path $env:TEMP ("ChudClone-winpe-" + (Get-Date -Format 'yyyyMMdd-HHmmss')))
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $Output) { $Output = Join-Path $root 'dist\ChudClone-Rescue.iso' }
if (-not $Keyboard) { $Keyboard = if ($Language -eq 'fr') { 'fr-FR' } else { 'en-US' } }
$locales = @{
  'en-US' = '0409:00000409'; 'en-GB' = '0809:00000809'; 'fr-FR' = '040c:0000040c'; 'fr-BE' = '080c:0000080c'
  'fr-CH' = '100c:0000100c'; 'fr-CA' = '0c0c:00011009'; 'de-DE' = '0407:00000407'; 'es-ES' = '040a:0000040a'; 'it-IT' = '0410:00000410'
}

function Say($m, $c = 'Gray') { Write-Host $m -ForegroundColor $c }
function Fail($m) { throw "FAILED: $m" }

# --- 1. Prerequisites ---------------------------------------------------------------
$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { Fail 'run this script from an ADMINISTRATOR PowerShell prompt (DISM requires it).' }

$kits = $null
foreach ($k in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows Kits\Installed Roots', 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots') {
  try { $kits = (Get-ItemProperty $k -ErrorAction Stop).KitsRoot10; if ($kits) { break } } catch { }
}
$adk = if ($kits) { Join-Path $kits 'Assessment and Deployment Kit' } else { $null }
$dandi = if ($adk) { Join-Path $adk 'Deployment Tools\DandISetEnv.bat' } else { $null }
$pe = if ($adk) { Join-Path $adk 'Windows Preinstallation Environment' } else { $null }
if (-not $dandi -or -not (Test-Path $dandi)) {
  Fail "Windows ADK not found. Install ""Windows ADK"" then the ""Windows PE add-on"":`n  https://learn.microsoft.com/windows-hardware/get-started/adk-install"
}
if (-not (Test-Path (Join-Path $pe 'amd64\en-us\winpe.wim'))) {
  Fail "Windows PE add-on missing ($pe). Install ""Windows PE add-on for the ADK""."
}

# USB key checks BEFORE any long work.
$usb = $null
if ($UsbDrive) {
  $letter = $UsbDrive.TrimEnd(':', '\').ToUpper()
  if ($letter -notmatch '^[A-Z]$') { Fail "invalid drive letter: $UsbDrive" }
  $part = Get-Partition -DriveLetter $letter -ErrorAction SilentlyContinue
  if (-not $part) { Fail "no volume $($letter): found" }
  $usb = Get-Disk -Number $part.DiskNumber
  $sysDisk = (Get-Partition -DriveLetter $env:SystemDrive.TrimEnd(':')).DiskNumber
  if ($usb.BusType -ne 'USB') { Fail "$($letter): is not on a USB disk (bus: $($usb.BusType)). Refused." }
  if ($usb.IsSystem -or $usb.IsBoot -or $usb.Number -eq $sysDisk) { Fail "$($letter): is on the system disk. Refused." }
  if ($usb.Size -gt 256GB) { Fail ("$($letter): is on a {0:N0} GB disk: too large for a rescue key, refused as a precaution." -f ($usb.Size / 1GB)) }
  if ($usb.Size -lt 1GB) { Fail "$($letter): is too small (1 GB minimum)." }
}

# --- 2. Program to embed --------------------------------------------------------------
if (-not $Cli) {
  Say 'Building chud-clone-cli.exe (release)...' Cyan
  $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
  if (-not (Test-Path $cargo)) { $cargo = 'cargo' }
  & $cargo build --release -p chud-clone-cli --manifest-path (Join-Path $root 'Cargo.toml')
  if ($LASTEXITCODE -ne 0) { Fail 'build failed' }
  $Cli = Join-Path $root 'target\release\chud-clone-cli.exe'
}
if (-not (Test-Path $Cli)) { Fail "program not found: $Cli" }
$bytes = [IO.File]::ReadAllText($Cli, [Text.Encoding]::ASCII)
if ($bytes -match 'VCRUNTIME140\.dll') { Fail 'chud-clone-cli.exe depends on VCRUNTIME140.dll, which WinPE lacks (build it from this repository: build.rs links it statically).' }

# --- 3. WinPE environment ---------------------------------------------------------------
if (Test-Path $WorkDir) { Fail "the work folder already exists: $WorkDir" }
function Invoke-Adk([string]$cmdline) {
  & cmd.exe /c "call `"$dandi`" >nul && $cmdline"
  if ($LASTEXITCODE -ne 0) { Fail "ADK command failed: $cmdline" }
}
Say "Creating the WinPE environment in $WorkDir..." Cyan
Invoke-Adk "copype amd64 `"$WorkDir`""

$mount = Join-Path $WorkDir 'mount'
$wim = Join-Path $WorkDir 'media\sources\boot.wim'
$mounted = $false
try {
  Say 'Mounting boot.wim...' Cyan
  & dism.exe /Mount-Image "/ImageFile:$wim" /Index:1 "/MountDir:$mount" | Out-Host
  if ($LASTEXITCODE -ne 0) { Fail 'cannot mount boot.wim' }
  $mounted = $true

  if (-not (Test-Path (Join-Path $mount 'Windows\System32\ucrtbase.dll'))) {
    Say 'Warning: ucrtbase.dll is missing from this WinPE image; chud-clone-cli.exe may not start.' Yellow
  }
  $dest = Join-Path $mount 'ChudClone'
  New-Item -ItemType Directory -Force $dest | Out-Null
  Copy-Item $Cli (Join-Path $dest 'chud-clone-cli.exe')
  # Menu: CRLF line endings, UTF-8 without BOM (chcp 65001 in the script).
  $menu = [IO.File]::ReadAllText((Join-Path $root 'packaging\winpe\menu.cmd'), [Text.Encoding]::UTF8)
  $menu = ($menu -replace "`r`n", "`n") -replace "`n", "`r`n"
  [IO.File]::WriteAllText((Join-Path $dest 'menu.cmd'), $menu, (New-Object Text.UTF8Encoding $false))
  foreach ($l in 'en', 'fr') { Copy-Item (Join-Path $root "packaging\winpe\menu-$l.txt") $dest }
  [IO.File]::WriteAllText((Join-Path $dest 'lang.txt'), $Language, [Text.Encoding]::ASCII)
  foreach ($f in 'LICENSE') { if (Test-Path (Join-Path $root $f)) { Copy-Item (Join-Path $root $f) $dest } }
  [IO.File]::WriteAllText((Join-Path $mount 'Windows\System32\startnet.cmd'), "wpeinit`r`ncall X:\ChudClone\menu.cmd`r`n", [Text.Encoding]::ASCII)

  Say "Keyboard: $Keyboard" Cyan
  & dism.exe "/Image:$mount" "/Set-InputLocale:$($locales[$Keyboard])" | Out-Host
  if ($LASTEXITCODE -ne 0) { Say 'Could not set the keyboard; the US layout will be used.' Yellow }

  Say 'Unmounting and saving boot.wim...' Cyan
  & dism.exe /Unmount-Image "/MountDir:$mount" /Commit | Out-Host
  if ($LASTEXITCODE -ne 0) { Fail 'cannot save boot.wim' }
  $mounted = $false
}
finally {
  if ($mounted) { & dism.exe /Unmount-Image "/MountDir:$mount" /Discard | Out-Null }
}

# Marker at the root of the media: the menu recognises the key by it and stores reports there.
$readme = @"
Chud Clone rescue key (WinPE).
Boot the computer from this key (BIOS/UEFI boot menu, often F12, F11, F8 or Esc).
The ISO image can also be copied to a Ventoy key and started from its menu.
A menu opens: list disks, inspect, SMART, dry run, then clone.
Clone reports are saved in the ChudClone-reports folder of this key (with Ventoy: at the
root of the Ventoy key).
If Secure Boot refuses the key (WinPE is signed by Microsoft), check the BIOS date and UEFI mode.
"@
[IO.File]::WriteAllText((Join-Path $WorkDir 'media\ChudClone-rescue.txt'), $readme.Replace("`n", "`r`n"), (New-Object Text.UTF8Encoding $true))

# --- 5. ISO image -------------------------------------------------------------------------
New-Item -ItemType Directory -Force (Split-Path -Parent $Output) | Out-Null
if (Test-Path $Output) { Remove-Item -Force $Output }
Say "Creating the ISO image $Output..." Cyan
Invoke-Adk "MakeWinPEMedia /ISO /F `"$WorkDir`" `"$Output`""
Say ("ISO ready: {0} ({1:N0} MB)" -f $Output, ((Get-Item $Output).Length / 1MB)) Green

# --- 6. USB key (optional, destructive) ----------------------------------------------------
if ($usb) {
  Say ''
  Say ("USB KEY: {0}: - {1} - {2:N1} GB - disk {3}" -f $letter, $usb.FriendlyName, ($usb.Size / 1GB), $usb.Number) Yellow
  Say 'ALL its content will be ERASED.' Yellow
  $answer = Read-Host "To confirm, type ERASE $letter"
  if ($answer -cne "ERASE $letter") { Say 'Key writing cancelled (the ISO is still available).' Yellow; return }
  Invoke-Adk "MakeWinPEMedia /UFD /F `"$WorkDir`" $($letter):"
  Say "Rescue key ready on $($letter):" Green
}
Say "Work folder (can be deleted): $WorkDir"
