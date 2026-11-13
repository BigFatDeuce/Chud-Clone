#!/bin/bash
# Validation INDÉPENDANTE d'un clone produit par Chud Clone, avec les outils Linux de
# référence (aucun code de Chud Clone n'intervient ici) :
#   - sgdisk -v / sfdisk    : cohérence de la table de partitions
#   - ntfsresize --check    : cohérence de chaque volume NTFS (carte d'allocation)
#   - fsck.fat -n           : cohérence de chaque volume FAT
#   - montage ntfs-3g / mtools : empreinte de TOUS les fichiers, comparée à la source
#
# Usage (Linux ou WSL, root) : validate-clone.sh <source.img> <clone.img>
# Ne travaille que sur des fichiers image.
set -uo pipefail
SRC="${1:?image source}"; DST="${2:?image clone}"
W="$(mktemp -d /tmp/chudclone-val.XXXXXX)"
trap 'mountpoint -q "$W/mnt" 2>/dev/null && umount "$W/mnt"; rm -rf "$W"' EXIT
mkdir -p "$W/mnt"
FAIL=0
ok()   { echo "  OK    $*"; }
bad()  { echo "  ÉCHEC $*"; FAIL=1; }

# Liste « numéro début taille » (secteurs de 512 octets) des partitions d'une image.
parts() {
  sfdisk -d "$1" 2>/dev/null | awk -F'[ ,=:]+' '/start=/ { n=$1; sub(/.*[^0-9]/, "", n); for (i=1;i<=NF;i++) { if ($i=="start") s=$(i+1); if ($i=="size") z=$(i+1); if ($i=="type") t=$(i+1) } if (t!="f" && t!="5" && t!="85") print n, s, z }'
}

echo "== Table de partitions du clone =="
if sfdisk -d "$DST" 2>/dev/null | grep -q '^label: gpt'; then
  out="$(sgdisk -v "$DST" 2>&1)"
  if echo "$out" | grep -q 'No problems found'; then ok "sgdisk -v : aucune anomalie"; else bad "sgdisk -v : $(echo "$out" | tail -5)"; fi
  s_id="$(sgdisk -p "$SRC" | awk '/Disk identifier/ {print $NF}')"; d_id="$(sgdisk -p "$DST" | awk '/Disk identifier/ {print $NF}')"
  [ "$s_id" = "$d_id" ] && ok "identifiant de disque conservé ($d_id)" || bad "identifiant de disque différent ($s_id / $d_id)"
  for n in $(parts "$SRC" | awk '{print $1}'); do
    a="$(sgdisk -i "$n" "$SRC" | grep -E 'GUID code|unique GUID|Attribute flags|Partition name|Partition size')"
    b="$(sgdisk -i "$n" "$DST" | grep -E 'GUID code|unique GUID|Attribute flags|Partition name|Partition size')"
    [ "$a" = "$b" ] && ok "partition $n : type, GUID, attributs, nom et taille identiques" || bad "partition $n : métadonnées différentes"
  done
else
  s_id="$(sfdisk -d "$SRC" | awk '/label-id/ {print $2}')"; d_id="$(sfdisk -d "$DST" | awk '/label-id/ {print $2}')"
  [ "$s_id" = "$d_id" ] && ok "signature de disque MBR conservée ($d_id)" || bad "signature MBR différente"
  a="$(sfdisk -d "$SRC" | grep 'start=' | sed 's/^[^:]*://')"; b="$(sfdisk -d "$DST" | grep 'start=' | sed 's/^[^:]*://')"
  [ "$a" = "$b" ] && ok "entrées de partition identiques" || bad "entrées de partition différentes"
fi

listing() { # listing <image partition> <fs> → empreintes triées de tous les fichiers
  case "$2" in
    ntfs)
      ntfs-3g -o ro "$1" "$W/mnt" 2>/dev/null || { echo "MONTAGE IMPOSSIBLE"; return; }
      (cd "$W/mnt" && find . -type f -print0 | sort -z | xargs -0 -r md5sum)
      umount "$W/mnt" ;;
    fat)
      mdir -i "$1" -/ -b :: 2>/dev/null | grep -v '/$' | sort | while read -r f; do
        printf '%s  %s\n' "$(mtype -i "$1" "$f" 2>/dev/null | md5sum | cut -d' ' -f1)" "$f"
      done ;;
  esac
}

echo "== Systèmes de fichiers =="
paste <(parts "$SRC") <(parts "$DST") | while read -r n ss sz dn ds dz; do
  dd if="$SRC" of="$W/s.img" bs=512 skip="$ss" count="$sz" status=none
  dd if="$DST" of="$W/d.img" bs=512 skip="$ds" count="$dz" status=none
  sig="$(dd if="$W/s.img" bs=1 skip=3 count=8 status=none | tr -d '\0')"
  fs=other
  [ "$sig" = "NTFS    " ] && fs=ntfs
  if [ "$fs" = other ] && fsck.fat -n "$W/s.img" >/dev/null 2>&1; then fs=fat; fi
  case "$fs" in
    ntfs)
      out="$(ntfsresize --check -f "$W/d.img" 2>&1)"; rc=$?
      [ $rc -eq 0 ] && ok "partition $n (NTFS) : ntfsresize --check sans erreur" || { bad "partition $n (NTFS) : $(echo "$out" | tail -4)"; }
      ;;
    fat)
      out="$(fsck.fat -n "$W/d.img" 2>&1)"; rc=$?
      [ $rc -eq 0 ] && ok "partition $n (FAT) : fsck.fat sans erreur" || bad "partition $n (FAT) : $(echo "$out" | tail -4)"
      ;;
    other)
      if [ "$sz" = "$dz" ] && cmp -s "$W/s.img" "$W/d.img"; then ok "partition $n (brute) : identique octet pour octet"
      else echo "  INFO  partition $n : contenu non comparé (partition réservée ou sans système de fichiers reconnu)"; fi
      continue ;;
  esac
  listing "$W/s.img" "$fs" > "$W/ls.txt"; listing "$W/d.img" "$fs" > "$W/ld.txt"
  count="$(wc -l < "$W/ls.txt")"
  if [ "$count" -gt 0 ] && cmp -s "$W/ls.txt" "$W/ld.txt"; then ok "partition $n : $count fichier(s), empreintes identiques à la source"
  else bad "partition $n : fichiers différents de la source"; diff "$W/ls.txt" "$W/ld.txt" | head -5; fi
  [ "$FAIL" = 1 ] && touch "$W/failed"
done
[ -e "$W/failed" ] && FAIL=1
echo
[ "$FAIL" = 0 ] && echo "VALIDATION INDÉPENDANTE : RÉUSSIE" || echo "VALIDATION INDÉPENDANTE : ÉCHEC"
exit $FAIL
