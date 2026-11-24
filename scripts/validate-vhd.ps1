<#
.SYNOPSIS
  Validates the Chud Clone "disk" write path on VIRTUAL DISKS (VHDX) only.

.DESCRIPTION
  The engine is validated on image files. This script additionally exercises the path reserved
  for disks (\\.\PhysicalDriveN: volume locking, table wipe, unbuffered writes, table re-read
  by Windows), without ever touching a real disk:

    1. creates two VHDX files in a working folder and attaches them;
    2. checks, for each one, that the resulting Windows disk really is that file
       (bus type "File Backed Virtual" + file path);
    3. writes the GPT test image (simulated UEFI Windows) to the "source" VHDX;
       then, as for a Windows disk connected externally, mounts its Windows partition with
       a drive letter, places inactive page and hibernation files on it, and clones it to an
       image file (not treated as a system disk, smart copy, boot configuration found);
    4. clones the source VHDX to the (larger) destination VHDX, with full verification
       and automatic growth of the Windows (NTFS) partition;
    5. detaches the source (as when the old disk is unplugged), brings the destination online,
       then checks the result with the Windows tools: partitions, read-only chkdsk,
       hashes of every file compared with those of the source; on the grown partition
       (test VHDX only), "chkdsk /f" must find no error;
    6. detaches everything; the VHDX files are deleted unless -Keep is given.

  Every clone passes --only-virtual to the engine: it refuses any destination that is not an
  image or a virtual disk. The script stops at the slightest doubt about a disk's identity.

  Requires an ADMINISTRATOR PowerShell prompt (attaching a VHDX and writing to it require it).

.PARAMETER Cli
  Path of chud-clone-cli.exe (default: target\release, otherwise target\debug).

.PARAMETER WorkDir
  Working folder (default: %TEMP%\ChudClone-vhd).

.PARAMETER Keep
  Keep the VHDX files and the reports at the end.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\validate-vhd.ps1
#>
[CmdletBinding()]
param(
  [string]$Cli,
  [string]$WorkDir = (Join-Path $env:TEMP 'ChudClone-vhd'),
  [switch]$Keep
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
# chud-clone-cli writes UTF-8: without this, PowerShell 5.1 decodes its output with the OEM
# code page and non-ASCII characters are garbled.
[Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
# Engine messages in English, whatever the Windows language.
$env:CHUD_LANG = 'en'
$root = Split-Path -Parent $PSScriptRoot

function Say($m, $c = 'Gray') { Write-Host $m -ForegroundColor $c }
function Fail($m) { throw "FAILED: $m" }

# ------------------------------------------------------------------ Prerequisites
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  Say 'This script must be run from a PowerShell prompt opened as administrator.' Yellow
  exit 2
}
if (-not $Cli) {
  $Cli = @('target\release\chud-clone-cli.exe', 'target\debug\chud-clone-cli.exe') |
    ForEach-Object { Join-Path $root $_ } | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Cli -or -not (Test-Path $Cli)) { Fail 'chud-clone-cli.exe not found (build it with "cargo build --release").' }

New-Item -ItemType Directory -Force $WorkDir | Out-Null
$WorkDir = (Resolve-Path $WorkDir).Path
$reports = Join-Path $WorkDir 'reports'
$fixture = Join-Path $WorkDir 'gpt-win.img'
if (-not (Test-Path $fixture)) {
  $gz = Join-Path $root 'crates\core\tests\fixtures\gpt-win.img.gz'
  $in = [IO.File]::OpenRead($gz)
  try {
    $z = New-Object IO.Compression.GZipStream($in, [IO.Compression.CompressionMode]::Decompress)
    $out = [IO.File]::Create($fixture)
    try { $z.CopyTo($out) } finally { $out.Dispose(); $z.Dispose() }
  } finally { $in.Dispose() }
}

$srcVhd = Join-Path $WorkDir 'source.vhdx'
$dstVhd = Join-Path $WorkDir 'destination.vhdx'
$mnt = Join-Path $WorkDir 'mounts'
$attached = @()
$accessPaths = @()

function New-Vhdx($path, $mb) {
  if (Test-Path $path) { Remove-Item -Force $path }
  $script = Join-Path $WorkDir 'diskpart.txt'
  "create vdisk file=`"$path`" maximum=$mb type=expandable" | Set-Content -Encoding ascii $script
  $o = & diskpart.exe /s $script 2>&1
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path $path)) { Fail "cannot create $path`:`n$o" }
}

# Attaches a VHDX and returns its Windows disk number, after checking that it really is that file.
function Mount-Checked($path) {
  Mount-DiskImage -ImagePath $path -StorageType VHDX -Access ReadWrite -NoDriveLetter | Out-Null
  $script:attached += $path
  $n = $null
  for ($i = 0; $i -lt 20 -and $null -eq $n; $i++) {
    $n = (Get-DiskImage -ImagePath $path).Number
    if ($null -eq $n) { Start-Sleep -Milliseconds 500 }
  }
  if ($null -eq $n) { Fail "disk number of $path not found" }
  Assert-OurVhd $n $path
  return [int]$n
}

# Triple identity check before any write to disk n.
function Assert-OurVhd([int]$n, $path) {
  $d = Get-Disk -Number $n
  if ($d.BusType -ne 'File Backed Virtual') { Fail "disk $n is not a virtual disk (bus: $($d.BusType)). Stopping." }
  if ($d.Location -and ($d.Location -ne $path)) { Fail "disk $n maps to '$($d.Location)', not to $path. Stopping." }
  if ($d.IsSystem -or $d.IsBoot) { Fail "disk $n is a system disk. Stopping." }
  $img = Get-DiskImage -ImagePath $path
  if ($img.Number -ne $n) { Fail "mismatch: $path is attached as disk $($img.Number), not $n. Stopping." }
  # PowerShell 5.1: ConvertFrom-Json passes a JSON array down the pipeline as ONE object; it is
  # stored in a variable first so that the filter sees each disk separately.
  $all = & $Cli list --json | Out-String | ConvertFrom-Json
  $listed = @($all) | Where-Object { $_.id -eq "disk:$n" }
  if (@($listed).Count -ne 1) { Fail "Chud Clone does not list disk $n (or lists it more than once). Stopping." }
  if (-not $listed -or $listed.media -ne 'virtual') { Fail "Chud Clone does not see disk $n as virtual. Stopping." }
}

function Invoke-Clone($from, $to, $label, [string[]]$extra = @()) {
  Say "`n=== $label" Cyan
  # --no-boot-prep: the test image's Windows is simulated (its registry is not a real
  # hive), so preparing it to boot elsewhere can only end in a warning. That step is
  # covered by the engine's tests; this script checks the copy itself.
  $cliArgs = @('clone', $from, $to, '--confirm-erase', '--only-virtual', '--verify', 'full', '--no-boot-prep', '--reports', $reports, '--json') + $extra
  $json = & $Cli @cliArgs
  $code = $LASTEXITCODE
  $rep = $null
  try { $rep = ($json | Out-String) | ConvertFrom-Json } catch { }
  if ($rep) {
    Say ("Result: {0}" -f $rep.outcome.status) $(if ($rep.outcome.status -eq 'success_verified') { 'Green' } else { 'Red' })
    $rep.outcome.reasons | ForEach-Object { Say "  - $_" }
  }
  if ($code -ne 0 -or -not $rep -or $rep.outcome.status -ne 'success_verified') { Fail "$label (exit code $code)" }
  return $rep
}

function Add-Mounts([int]$n, $tag) {
  $res = @{}
  foreach ($p in Get-Partition -DiskNumber $n) {
    if ($p.GptType -ne '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}') { continue } # basic data only
    $dir = Join-Path $mnt "$tag-p$($p.PartitionNumber)"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Add-PartitionAccessPath -DiskNumber $n -PartitionNumber $p.PartitionNumber -AccessPath "$dir\"
    $script:accessPaths += , @($n, $p.PartitionNumber, "$dir\")
    $res[$p.PartitionNumber] = $dir
  }
  return $res
}

function Get-Tree($dir) {
  $h = @{}
  Get-ChildItem -LiteralPath $dir -Recurse -File -Force -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -notmatch '\\System Volume Information\\' } |
    ForEach-Object { $h[$_.FullName.Substring($dir.Length)] = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash }
  return $h
}

function Remove-Mounts {
  foreach ($a in $script:accessPaths) {
    try { Remove-PartitionAccessPath -DiskNumber $a[0] -PartitionNumber $a[1] -AccessPath $a[2] -ErrorAction Stop } catch { }
  }
  $script:accessPaths = @()
}

$ok = $false
try {
  Say "Chud Clone - validation on virtual disks`nCLI: $Cli`nFolder: $WorkDir" Cyan
  New-Vhdx $srcVhd 256
  New-Vhdx $dstVhd 512
  $s = Mount-Checked $srcVhd
  $d = Mount-Checked $dstVhd
  Say "Source VHDX      : disk $s ($srcVhd)"
  Say "Destination VHDX : disk $d ($dstVhd)"

  # 1. Test image -> source VHDX (written through the "disk" path).
  Assert-OurVhd $s $srcVhd
  # No growth: the source must stay a "clean" volume, like a real system disk.
  Invoke-Clone "image:$fixture" "disk:$s" 'Test image -> source VHDX' @('--no-grow') | Out-Null
  Start-Sleep -Seconds 2

  # 1b. Windows disk connected externally: its Windows partition gets a drive letter (mounted
  # volume, usable in Explorer) and holds INACTIVE page and hibernation files, like a disk
  # taken out of another PC. Chud Clone must not treat it as a system disk, and must clone it
  # to an image file with a smart copy: volumes locked and dismounted, then analysed as clean.
  Say "`n=== External Windows disk (mounted volume) -> image file" Cyan
  $winPart = Get-Partition -DiskNumber $s | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' } |
    Sort-Object Size -Descending | Select-Object -First 1
  if (-not $winPart) { Fail 'Windows partition not found on the source' }
  Add-PartitionAccessPath -DiskNumber $s -PartitionNumber $winPart.PartitionNumber -AssignDriveLetter
  Start-Sleep -Seconds 2
  $letter = (Get-Partition -DiskNumber $s -PartitionNumber $winPart.PartitionNumber).DriveLetter
  if (-not $letter -or $letter -eq [char]0) { Fail 'no drive letter assigned to the Windows partition of the source' }
  Say "Windows partition of the source mounted as ${letter}:"
  foreach ($f in 'pagefile.sys', 'hiberfil.sys', 'swapfile.sys') {
    $fs = [IO.File]::Create("${letter}:\$f")
    try { $fs.SetLength(1MB) } finally { $fs.Dispose() }
  }
  $all = & $Cli list --json | Out-String | ConvertFrom-Json
  $listed = @($all) | Where-Object { $_.id -eq "disk:$s" }
  if ($listed.is_system) { Fail "external disk $s is treated as a system disk (inactive page files)" }
  Say 'Not treated as a system disk: correct' Green
  $extImg = Join-Path $WorkDir 'external.img'
  if (Test-Path $extImg) { Remove-Item -Force $extImg }
  $prep = & $Cli plan "disk:$s" "image:$extImg" --json | Out-String | ConvertFrom-Json
  $ntfs = @($prep.plan.partitions | Where-Object { $_.fs -eq 'ntfs' })
  if (-not $ntfs.Count) { Fail 'preview: no NTFS partition in the plan' }
  foreach ($p in $ntfs) {
    if ($p.method -ne 'smart') { Fail "preview: partition $($p.number) uses '$($p.method)' copy ($($p.method_reason))" }
  }
  Say 'Preview (mounted volume): smart copy planned' Green
  $rep = Invoke-Clone "disk:$s" "image:$extImg" 'External disk -> image file'
  foreach ($p in @($rep.plan.partitions | Where-Object { $_.fs -eq 'ntfs' })) {
    if ($p.method -ne 'smart') { Fail "clone: partition $($p.number) uses '$($p.method)' copy ($($p.method_reason))" }
  }
  $insp = & $Cli inspect "image:$extImg" --json | Out-String | ConvertFrom-Json
  if ($insp.boot.style -ne 'windows_uefi') { Fail "image: UEFI Windows boot not found ($($insp.boot.style))" }
  Say 'Image: smart copy verified, UEFI Windows boot found' Green
  Remove-PartitionAccessPath -DiskNumber $s -PartitionNumber $winPart.PartitionNumber -AccessPath "${letter}:\"
  if (-not $Keep) { Remove-Item -Force $extImg -ErrorAction SilentlyContinue }
  Start-Sleep -Seconds 1

  # Reference hashes, read from the source as Windows sees it.
  $srcMounts = Add-Mounts $s 'src'
  if ($srcMounts.Count -eq 0) { Fail 'no data partition visible on the source' }
  $ref = @{}
  foreach ($k in $srcMounts.Keys) { $ref[$k] = Get-Tree $srcMounts[$k] }
  Remove-Mounts

  # 2. Source VHDX -> destination VHDX (larger).
  Assert-OurVhd $s $srcVhd
  Assert-OurVhd $d $dstVhd
  $rep = Invoke-Clone "disk:$s" "disk:$d" 'Source VHDX -> destination VHDX'
  $grown = $null
  if ($rep.outcome.grow -and $rep.outcome.grow.done) {
    $grown = [int]$rep.outcome.grow.number
    Say ("Partition {0} grown: {1:N0} -> {2:N0} bytes" -f $grown, $rep.outcome.grow.summary.old_size, $rep.outcome.grow.summary.new_size) Green
  } else { Fail 'the Windows partition should have been grown (destination twice as large)' }

  # 3. As after a real clone: the source is "unplugged", the destination comes back online.
  Dismount-DiskImage -ImagePath $srcVhd | Out-Null
  $script:attached = @($script:attached | Where-Object { $_ -ne $srcVhd })
  Start-Sleep -Seconds 1
  Assert-OurVhd $d $dstVhd
  $disk = Get-Disk -Number $d
  if ($disk.IsOffline) { Set-Disk -Number $d -IsOffline $false }
  Update-Disk -Number $d

  Say "`n=== Checks by Windows" Cyan
  $parts = Get-Partition -DiskNumber $d
  $parts | Format-Table PartitionNumber, Type, @{n = 'Size (MB)'; e = { [math]::Round($_.Size / 1MB) } }, Offset -AutoSize | Out-String | Write-Host
  if ($disk.PartitionStyle -ne 'GPT') { Fail "unexpected partition style: $($disk.PartitionStyle)" }
  if (@($parts).Count -ne 4) { Fail "4 partitions expected, $(@($parts).Count) found" }

  $dstMounts = Add-Mounts $d 'dst'
  foreach ($k in $ref.Keys) {
    if (-not $dstMounts.ContainsKey($k)) { Fail "partition $k missing or not mountable on the destination" }
    $dir = $dstMounts[$k]
    $o = & chkdsk.exe $dir.TrimEnd('\') 2>&1
    if ($LASTEXITCODE -ne 0) { Fail "chkdsk reports a problem on partition $k`:`n$($o | Out-String)" }
    Say "chkdsk (read-only), partition $k`: no problem" Green
    if ($k -eq $grown) {
      # fsutil requires a volume path (letter or \\?\Volume{GUID}), not a mount folder.
      $guid = @((Get-Partition -DiskNumber $d -PartitionNumber $k).AccessPaths | Where-Object { $_ -like '\\?\Volume*' })[0]
      if ($guid) {
        $q = & fsutil.exe dirty query $guid.TrimEnd('\') 2>&1
        Say ("Before repair, fsutil dirty query: " + ($q | Out-String).Trim())
      }
      # Test VHDX only: repair is allowed, and it must find nothing to fix.
      $o = & chkdsk.exe $dir.TrimEnd('\') /f /x 2>&1
      $code = $LASTEXITCODE
      $o | Out-String | Write-Host
      if ($code -ne 0) { Fail "chkdsk /f had to fix the grown partition $k (exit code $code)" }
      Say "chkdsk /f, grown partition $k`: no error to fix" Green
      $p = Get-Partition -DiskNumber $d -PartitionNumber $k
      $v = $p | Get-Volume
      Say ("Volume {0}: {1:N0} bytes, {2:N0} free (partition of {3:N0} bytes)" -f $k, $v.Size, $v.SizeRemaining, $p.Size)
      if ($v.Size -lt $p.Size - 1MB) { Fail 'the volume does not fill the whole grown partition' }
    }
    $got = Get-Tree $dir
    $diff = @($ref[$k].Keys | Where-Object { $got[$_] -ne $ref[$k][$_] }) + @($got.Keys | Where-Object { -not $ref[$k].ContainsKey($_) })
    if ($diff.Count) { Fail "partition $k`: files differ: $($diff -join ', ')" }
    Say ("Partition {0}: {1} files, SHA-256 hashes identical to the source" -f $k, $ref[$k].Count) Green
  }
  Remove-Mounts

  $insp = & $Cli inspect "disk:$d" --json | ConvertFrom-Json
  Say ("Clone boot configuration: {0}" -f $insp.boot.style)
  if ($insp.boot.style -ne 'windows_uefi') { Fail 'UEFI Windows boot files not found on the clone' }

  $ok = $true
  Say "`nVALIDATION ON VIRTUAL DISKS: PASSED" Green
  Say "Reports: $reports"
}
catch {
  Say "`n$($_.Exception.Message)" Red
  Say 'VALIDATION ON VIRTUAL DISKS: FAILED' Red
}
finally {
  Remove-Mounts
  foreach ($p in $script:attached) { try { Dismount-DiskImage -ImagePath $p | Out-Null } catch { } }
  if (-not $Keep) {
    foreach ($p in @($srcVhd, $dstVhd)) { if (Test-Path $p) { Remove-Item -Force $p -ErrorAction SilentlyContinue } }
    if (Test-Path $mnt) { Remove-Item -Recurse -Force $mnt -ErrorAction SilentlyContinue }
  }
}
if ($ok) { exit 0 } else { exit 1 }
