<#
.SYNOPSIS
  Valide la voie d'écriture « disque » de Chud Clone sur des DISQUES VIRTUELS (VHDX) uniquement.

.DESCRIPTION
  Le moteur est validé sur des fichiers image. Ce script éprouve en plus la voie réservée aux
  disques (\\.\PhysicalDriveN : verrouillage des volumes, effacement de la table, écriture non
  tamponnée, relecture de la table par Windows), sans jamais toucher un disque réel :

    1. crée deux fichiers VHDX dans un dossier de travail et les attache ;
    2. vérifie, pour chacun, que le disque Windows obtenu est bien ce fichier
       (type de bus « File Backed Virtual » + chemin du fichier) ;
    3. écrit l'image de test GPT (Windows UEFI factice) sur le VHDX « source » ;
       puis, comme pour un disque Windows branché en externe, monte sa partition Windows avec
       une lettre, y place des fichiers d'échange et d'hibernation inactifs, et la clone vers un
       fichier image (pas pris pour un disque système, copie intelligente, démarrage retrouvé) ;
    4. clone le VHDX source vers le VHDX destination (plus grand), avec vérification complète
       et agrandissement automatique de la partition Windows (NTFS) ;
    5. détache la source (comme on débranche l'ancien disque), remet la destination en ligne,
       puis contrôle le résultat avec les outils de Windows : partitions, chkdsk en lecture seule,
       empreintes de tous les fichiers comparées à celles de la source ; sur la partition
       agrandie (VHDX de test uniquement), « chkdsk /f » doit ne trouver aucune erreur ;
    6. détache tout ; les VHDX sont supprimés sauf avec -Keep.

  Chaque clonage passe --only-virtual au moteur : il refuse toute destination qui n'est pas une
  image ou un disque virtuel. Le script s'arrête au moindre doute sur l'identité d'un disque.

  Nécessite une invite PowerShell ADMINISTRATEUR (attacher un VHDX et écrire dessus l'exigent).

.PARAMETER Cli
  Chemin de chud-clone-cli.exe (défaut : target\release, sinon target\debug).

.PARAMETER WorkDir
  Dossier de travail (défaut : %TEMP%\ChudClone-vhd).

.PARAMETER Keep
  Conserver les VHDX et les rapports à la fin.

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
# Les programmes appelés (chud-clone-cli) écrivent en UTF-8 : sans cela, PowerShell 5.1 décode
# leur sortie avec la page de code OEM et les accents sont illisibles.
[Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
$root = Split-Path -Parent $PSScriptRoot

function Say($m, $c = 'Gray') { Write-Host $m -ForegroundColor $c }
function Fail($m) { throw "ÉCHEC : $m" }

# ------------------------------------------------------------------ Préalables
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  Say 'Ce script doit être lancé depuis une invite PowerShell ouverte en administrateur.' Yellow
  exit 2
}
if (-not $Cli) {
  $Cli = @('target\release\chud-clone-cli.exe', 'target\debug\chud-clone-cli.exe') |
    ForEach-Object { Join-Path $root $_ } | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Cli -or -not (Test-Path $Cli)) { Fail 'chud-clone-cli.exe introuvable (compilez avec « cargo build --release »).' }

New-Item -ItemType Directory -Force $WorkDir | Out-Null
$WorkDir = (Resolve-Path $WorkDir).Path
$reports = Join-Path $WorkDir 'rapports'
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
$mnt = Join-Path $WorkDir 'montages'
$attached = @()
$accessPaths = @()

function New-Vhdx($path, $mb) {
  if (Test-Path $path) { Remove-Item -Force $path }
  $script = Join-Path $WorkDir 'diskpart.txt'
  "create vdisk file=`"$path`" maximum=$mb type=expandable" | Set-Content -Encoding ascii $script
  $o = & diskpart.exe /s $script 2>&1
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path $path)) { Fail "création de $path impossible :`n$o" }
}

# Attache un VHDX et renvoie le numéro de disque Windows, après avoir vérifié que c'est bien lui.
function Mount-Checked($path) {
  Mount-DiskImage -ImagePath $path -StorageType VHDX -Access ReadWrite -NoDriveLetter | Out-Null
  $script:attached += $path
  $n = $null
  for ($i = 0; $i -lt 20 -and $null -eq $n; $i++) {
    $n = (Get-DiskImage -ImagePath $path).Number
    if ($null -eq $n) { Start-Sleep -Milliseconds 500 }
  }
  if ($null -eq $n) { Fail "numéro de disque de $path introuvable" }
  Assert-OurVhd $n $path
  return [int]$n
}

# Triple contrôle d'identité avant toute écriture sur le disque n.
function Assert-OurVhd([int]$n, $path) {
  $d = Get-Disk -Number $n
  if ($d.BusType -ne 'File Backed Virtual') { Fail "le disque $n n'est pas un disque virtuel (bus : $($d.BusType)). Arrêt." }
  if ($d.Location -and ($d.Location -ne $path)) { Fail "le disque $n correspond à « $($d.Location) », pas à $path. Arrêt." }
  if ($d.IsSystem -or $d.IsBoot) { Fail "le disque $n est un disque système. Arrêt." }
  $img = Get-DiskImage -ImagePath $path
  if ($img.Number -ne $n) { Fail "incohérence : $path est attaché comme disque $($img.Number), pas $n. Arrêt." }
  # PowerShell 5.1 : ConvertFrom-Json transmet un tableau JSON comme UN seul objet ; on le
  # range d'abord dans une variable pour que le filtre voie chaque disque séparément.
  $all = & $Cli list --json | Out-String | ConvertFrom-Json
  $listed = @($all) | Where-Object { $_.id -eq "disk:$n" }
  if (@($listed).Count -ne 1) { Fail "Chud Clone ne liste pas le disque $n (ou le liste plusieurs fois). Arrêt." }
  if (-not $listed -or $listed.media -ne 'virtual') { Fail "Chud Clone ne voit pas le disque $n comme virtuel. Arrêt." }
}

function Invoke-Clone($from, $to, $label, [string[]]$extra = @()) {
  Say "`n=== $label" Cyan
  $cliArgs = @('clone', $from, $to, '--confirm-erase', '--only-virtual', '--verify', 'full', '--reports', $reports, '--json') + $extra
  $json = & $Cli @cliArgs
  $code = $LASTEXITCODE
  $rep = $null
  try { $rep = ($json | Out-String) | ConvertFrom-Json } catch { }
  if ($rep) {
    Say ("Résultat : {0}" -f $rep.outcome.status) $(if ($rep.outcome.status -eq 'success_verified') { 'Green' } else { 'Red' })
    $rep.outcome.reasons | ForEach-Object { Say "  · $_" }
  }
  if ($code -ne 0 -or -not $rep -or $rep.outcome.status -ne 'success_verified') { Fail "$label (code $code)" }
  return $rep
}

function Add-Mounts([int]$n, $tag) {
  $res = @{}
  foreach ($p in Get-Partition -DiskNumber $n) {
    if ($p.GptType -ne '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}') { continue } # données de base uniquement
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
  Say "Chud Clone — validation sur disques virtuels`nOutil : $Cli`nDossier : $WorkDir" Cyan
  New-Vhdx $srcVhd 256
  New-Vhdx $dstVhd 512
  $s = Mount-Checked $srcVhd
  $d = Mount-Checked $dstVhd
  Say "VHDX source      : disque $s ($srcVhd)"
  Say "VHDX destination : disque $d ($dstVhd)"

  # 1. Image de test -> VHDX source (écriture par la voie « disque »).
  Assert-OurVhd $s $srcVhd
  # Sans agrandissement : la source doit rester un volume « propre », comme un vrai disque système.
  Invoke-Clone "image:$fixture" "disk:$s" 'Image de test → VHDX source' @('--no-grow') | Out-Null
  Start-Sleep -Seconds 2

  # 1 bis. Disque Windows branché en externe : sa partition Windows reçoit une lettre (volume
  # monté, utilisable dans l'Explorateur) et porte des fichiers d'échange et d'hibernation
  # INACTIFS, comme un disque retiré d'un autre PC. Chud Clone ne doit pas le prendre pour un
  # disque système, et doit le cloner vers un fichier image en copie intelligente : volumes
  # verrouillés et démontés, puis analysés propres.
  Say "`n=== Disque Windows externe (volume monté) → fichier image" Cyan
  $winPart = Get-Partition -DiskNumber $s | Where-Object { $_.GptType -eq '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' } |
    Sort-Object Size -Descending | Select-Object -First 1
  if (-not $winPart) { Fail 'partition Windows introuvable sur la source' }
  Add-PartitionAccessPath -DiskNumber $s -PartitionNumber $winPart.PartitionNumber -AssignDriveLetter
  Start-Sleep -Seconds 2
  $letter = (Get-Partition -DiskNumber $s -PartitionNumber $winPart.PartitionNumber).DriveLetter
  if (-not $letter -or $letter -eq [char]0) { Fail 'aucune lettre attribuée à la partition Windows de la source' }
  Say "Partition Windows de la source montée en ${letter}:"
  foreach ($f in 'pagefile.sys', 'hiberfil.sys', 'swapfile.sys') {
    $fs = [IO.File]::Create("${letter}:\$f")
    try { $fs.SetLength(1MB) } finally { $fs.Dispose() }
  }
  $all = & $Cli list --json | Out-String | ConvertFrom-Json
  $listed = @($all) | Where-Object { $_.id -eq "disk:$s" }
  if ($listed.is_system) { Fail "le disque externe $s est pris pour un disque système (fichiers d'échange inactifs)" }
  Say 'Non pris pour un disque système : correct' Green
  $extImg = Join-Path $WorkDir 'externe.img'
  if (Test-Path $extImg) { Remove-Item -Force $extImg }
  $prep = & $Cli plan "disk:$s" "image:$extImg" --json | Out-String | ConvertFrom-Json
  $ntfs = @($prep.plan.partitions | Where-Object { $_.fs -eq 'ntfs' })
  if (-not $ntfs.Count) { Fail "aperçu : aucune partition NTFS dans le plan" }
  foreach ($p in $ntfs) {
    if ($p.method -ne 'smart') { Fail "aperçu : partition $($p.number) en copie « $($p.method) » ($($p.method_reason))" }
  }
  Say 'Aperçu (volume monté) : copie intelligente prévue' Green
  $rep = Invoke-Clone "disk:$s" "image:$extImg" 'Disque externe → fichier image'
  foreach ($p in @($rep.plan.partitions | Where-Object { $_.fs -eq 'ntfs' })) {
    if ($p.method -ne 'smart') { Fail "clonage : partition $($p.number) en copie « $($p.method) » ($($p.method_reason))" }
  }
  $insp = & $Cli inspect "image:$extImg" --json | Out-String | ConvertFrom-Json
  if ($insp.boot.style -ne 'windows_uefi') { Fail "image : démarrage Windows UEFI non retrouvé ($($insp.boot.style))" }
  Say 'Image : copie intelligente vérifiée, démarrage Windows UEFI retrouvé' Green
  Remove-PartitionAccessPath -DiskNumber $s -PartitionNumber $winPart.PartitionNumber -AccessPath "${letter}:\"
  if (-not $Keep) { Remove-Item -Force $extImg -ErrorAction SilentlyContinue }
  Start-Sleep -Seconds 1

  # Empreintes de référence, lues sur la source telle que Windows la voit.
  $srcMounts = Add-Mounts $s 'src'
  if ($srcMounts.Count -eq 0) { Fail 'aucune partition de données visible sur la source' }
  $ref = @{}
  foreach ($k in $srcMounts.Keys) { $ref[$k] = Get-Tree $srcMounts[$k] }
  Remove-Mounts

  # 2. VHDX source -> VHDX destination (plus grand).
  Assert-OurVhd $s $srcVhd
  Assert-OurVhd $d $dstVhd
  $rep = Invoke-Clone "disk:$s" "disk:$d" 'VHDX source → VHDX destination'
  $grown = $null
  if ($rep.outcome.grow -and $rep.outcome.grow.done) {
    $grown = [int]$rep.outcome.grow.number
    Say ("Partition {0} agrandie : {1:N0} → {2:N0} octets" -f $grown, $rep.outcome.grow.summary.old_size, $rep.outcome.grow.summary.new_size) Green
  } else { Fail 'la partition Windows aurait dû être agrandie (destination deux fois plus grande)' }

  # 3. Comme après un vrai clonage : on « débranche » la source, la destination revient en ligne.
  Dismount-DiskImage -ImagePath $srcVhd | Out-Null
  $script:attached = @($script:attached | Where-Object { $_ -ne $srcVhd })
  Start-Sleep -Seconds 1
  Assert-OurVhd $d $dstVhd
  $disk = Get-Disk -Number $d
  if ($disk.IsOffline) { Set-Disk -Number $d -IsOffline $false }
  Update-Disk -Number $d

  Say "`n=== Contrôles par Windows" Cyan
  $parts = Get-Partition -DiskNumber $d
  $parts | Format-Table PartitionNumber, Type, @{n = 'Taille (Mo)'; e = { [math]::Round($_.Size / 1MB) } }, Offset -AutoSize | Out-String | Write-Host
  if ($disk.PartitionStyle -ne 'GPT') { Fail "style de partition inattendu : $($disk.PartitionStyle)" }
  if (@($parts).Count -ne 4) { Fail "4 partitions attendues, $(@($parts).Count) trouvées" }

  $dstMounts = Add-Mounts $d 'dst'
  foreach ($k in $ref.Keys) {
    if (-not $dstMounts.ContainsKey($k)) { Fail "partition $k absente ou non montable sur la destination" }
    $dir = $dstMounts[$k]
    $o = & chkdsk.exe $dir.TrimEnd('\') 2>&1
    if ($LASTEXITCODE -ne 0) { Fail "chkdsk signale un problème sur la partition $k :`n$($o | Out-String)" }
    Say "chkdsk (lecture seule), partition $k : aucun problème" Green
    if ($k -eq $grown) {
      # fsutil exige un chemin de volume (lettre ou \\?\Volume{GUID}), pas un dossier de montage.
      $guid = @((Get-Partition -DiskNumber $d -PartitionNumber $k).AccessPaths | Where-Object { $_ -like '\\?\Volume*' })[0]
      if ($guid) {
        $q = & fsutil.exe dirty query $guid.TrimEnd('\') 2>&1
        Say ("Avant réparation, fsutil dirty query : " + ($q | Out-String).Trim())
      }
      # VHDX de test uniquement : réparation autorisée, elle ne doit rien trouver à corriger.
      $o = & chkdsk.exe $dir.TrimEnd('\') /f /x 2>&1
      $code = $LASTEXITCODE
      $o | Out-String | Write-Host
      if ($code -ne 0) { Fail "chkdsk /f a dû corriger la partition agrandie $k (code $code)" }
      Say "chkdsk /f, partition agrandie $k : aucune erreur à corriger" Green
      $p = Get-Partition -DiskNumber $d -PartitionNumber $k
      $v = $p | Get-Volume
      Say ("Volume {0} : {1:N0} octets, {2:N0} libres (partition de {3:N0} octets)" -f $k, $v.Size, $v.SizeRemaining, $p.Size)
      if ($v.Size -lt $p.Size - 1MB) { Fail "le volume n'occupe pas toute la partition agrandie" }
    }
    $got = Get-Tree $dir
    $diff = @($ref[$k].Keys | Where-Object { $got[$_] -ne $ref[$k][$_] }) + @($got.Keys | Where-Object { -not $ref[$k].ContainsKey($_) })
    if ($diff.Count) { Fail "partition $k : fichiers différents : $($diff -join ', ')" }
    Say ("Partition {0} : {1} fichiers, empreintes SHA-256 identiques à la source" -f $k, $ref[$k].Count) Green
  }
  Remove-Mounts

  $insp = & $Cli inspect "disk:$d" --json | ConvertFrom-Json
  Say ("Démarrage du clone : {0}" -f $insp.boot.style)
  if ($insp.boot.style -ne 'windows_uefi') { Fail 'éléments de démarrage Windows UEFI non retrouvés sur le clone' }

  $ok = $true
  Say "`nVALIDATION SUR DISQUES VIRTUELS : RÉUSSIE" Green
  Say "Rapports : $reports"
}
catch {
  Say "`n$($_.Exception.Message)" Red
  Say 'VALIDATION SUR DISQUES VIRTUELS : ÉCHOUÉE' Red
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
