#!/bin/bash
# Validation INDÉPENDANTE d'un volume NTFS agrandi par Chud Clone, avec les outils Linux
# de référence (aucun code de Chud Clone n'intervient) :
#   - ntfsresize --check     : cohérence du volume et de sa carte d'allocation ;
#   - ntfsinfo               : nombre de clusters attendu ;
#   - ntfs-3g (lecture)      : empreintes de tous les fichiers, comparées à la source ;
#   - comparaison avec le même volume agrandi par ntfsresize lui-même (espace libre) ;
#   - écriture d'un gros fichier dans l'espace ajouté (sur une copie), relu ensuite.
#
# Usage (Linux ou WSL, root) : validate-grow.sh <source.img> <clone.img> <n° de partition>
# Ne travaille que sur des fichiers image (créés par les tests : CHUD_EXPORT_DIR).
set -uo pipefail
SRC="${1:?image source}"; DST="${2:?image clone}"; N="${3:?numéro de partition}"
W="$(mktemp -d /var/tmp/chudclone-grow.XXXXXX)"
trap 'mountpoint -q "$W/mnt" 2>/dev/null && umount "$W/mnt"; rm -rf "$W"' EXIT
mkdir -p "$W/mnt"
FAIL=0
ok()  { echo "  OK    $*"; }
bad() { echo "  ÉCHEC $*"; FAIL=1; }

geom() { # geom <image> <n> → "début taille" en secteurs
  sfdisk -d "$1" 2>/dev/null | awk -v n="$2" -F'[ ,=:]+' '/start=/ { p=$1; sub(/.*[^0-9]/, "", p); if (p==n) { for (i=1;i<=NF;i++) { if ($i=="start") s=$(i+1); if ($i=="size") z=$(i+1) } print s, z } }'
}
read -r ss sz <<<"$(geom "$SRC" "$N")"
read -r ds dz <<<"$(geom "$DST" "$N")"
[ -n "${sz:-}" ] && [ -n "${dz:-}" ] || { echo "partition $N introuvable"; exit 2; }
echo "== Partition $N : source $sz secteurs, clone $dz secteurs"
dd if="$SRC" of="$W/s.img" bs=512 skip="$ss" count="$sz" status=none
dd if="$DST" of="$W/d.img" bs=512 skip="$ds" count="$dz" status=none

out="$(ntfsresize --check -f "$W/d.img" 2>&1)"; rc=$?
[ $rc -eq 0 ] && ok "ntfsresize --check : aucune incohérence" || bad "ntfsresize --check : $(echo "$out" | tail -4)"

clusters() { ntfsinfo -f -m "$1" 2>/dev/null | awk -F: '/Volume Size in Clusters/ {gsub(/ /,"",$2); print $2}'; }
csize() { ntfsinfo -f -m "$1" 2>/dev/null | awk -F: '/Cluster Size/ {gsub(/ /,"",$2); print $2}'; }
cs="$(csize "$W/d.img")"
want=$(( (dz - 1) * 512 / cs ))
got="$(clusters "$W/d.img")"
[ "$got" = "$want" ] && ok "taille du volume : $got clusters (toute la partition)" || bad "taille du volume : $got clusters, $want attendus"
flags="$(ntfsinfo -f -m "$W/d.img" 2>/dev/null | awk -F: '/Volume Flags/ {print $2}')"
echo "$flags" | grep -q DIRTY && ok "volume marqué « à vérifier » (chkdsk au premier démarrage de Windows)" || bad "indicateur « à vérifier » absent"

listing() { # empreintes de tous les fichiers d'une image NTFS
  ntfs-3g -o ro,force "$1" "$W/mnt" 2>/dev/null || { echo "MONTAGE IMPOSSIBLE"; return; }
  (cd "$W/mnt" && find . -type f -print0 | sort -z | xargs -0 -r md5sum)
  umount "$W/mnt"
}
listing "$W/s.img" > "$W/ls.txt"; listing "$W/d.img" > "$W/ld.txt"
count="$(wc -l < "$W/ls.txt")"
if [ "$count" -gt 0 ] && cmp -s "$W/ls.txt" "$W/ld.txt"; then ok "$count fichier(s), empreintes identiques à la source"
else bad "fichiers différents : $(diff "$W/ls.txt" "$W/ld.txt" | head -5)"; fi

# Référence : le même volume agrandi par ntfsresize à la même taille.
cp "$W/s.img" "$W/ref.img"; truncate -s $(( dz * 512 )) "$W/ref.img"
if ntfsresize -f -f -s $(( (dz - 1) * 512 )) "$W/ref.img" >"$W/ref.log" 2>&1; then
  free() { ntfs-3g -o ro,force "$1" "$W/mnt" 2>/dev/null && { stat -f -c '%f' "$W/mnt"; umount "$W/mnt"; }; }
  fr="$(free "$W/ref.img")"; fd="$(free "$W/d.img")"; rc_="$(clusters "$W/ref.img")"
  [ "$rc_" = "$got" ] && ok "même nombre de clusters que ntfsresize ($rc_)" || bad "ntfsresize : $rc_ clusters, Chud Clone : $got"
  [ "$fr" = "$fd" ] && ok "même espace libre que ntfsresize ($fd blocs)" || bad "espace libre : ntfsresize $fr, Chud Clone $fd"
else
  bad "ntfsresize de référence impossible : $(tail -3 "$W/ref.log")"
fi

# Utilisation réelle de l'espace ajouté, sur une copie (indicateur levé par ntfsfix).
cp "$W/d.img" "$W/w.img"
ntfsfix -d "$W/w.img" >/dev/null 2>&1
if ntfs-3g -o rw "$W/w.img" "$W/mnt" 2>/dev/null; then
  avail=$(( $(stat -f -c '%a' "$W/mnt") * $(stat -f -c '%S' "$W/mnt") ))
  fill=$(( avail - 4 * 1024 * 1024 ))
  head -c "$fill" /dev/urandom > "$W/mnt/remplissage.bin" 2>/dev/null; sync
  h1="$(md5sum < "$W/mnt/remplissage.bin")"; umount "$W/mnt"
  out="$(ntfsresize --check -f "$W/w.img" 2>&1)" && ok "fichier de $(( fill / 1048576 )) Mio écrit dans l'espace ajouté ; volume toujours cohérent" || bad "après écriture : $(echo "$out" | tail -3)"
  ntfs-3g -o ro "$W/w.img" "$W/mnt" && { h2="$(md5sum < "$W/mnt/remplissage.bin")"; umount "$W/mnt"; }
  [ "$h1" = "${h2:-}" ] && ok "fichier relu identique" || bad "fichier relu différent"
else
  bad "montage en écriture impossible"
fi

[ $FAIL -eq 0 ] && echo "RÉSULTAT : agrandissement validé" || echo "RÉSULTAT : ÉCHEC"
exit $FAIL
