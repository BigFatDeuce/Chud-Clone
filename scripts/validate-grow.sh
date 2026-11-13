#!/bin/bash
# INDEPENDENT validation of an NTFS volume grown by Chud Clone, using the reference Linux
# tools (no Chud Clone code is involved):
#   - ntfsresize --check     : consistency of the volume and of its allocation map;
#   - ntfsinfo               : expected number of clusters;
#   - ntfs-3g (read-only)    : hashes of every file, compared with the source;
#   - comparison with the same volume grown by ntfsresize itself (free space);
#   - a large file written into the added space (on a copy), then read back.
#
# Usage (Linux or WSL, root): validate-grow.sh <source.img> <clone.img> <partition number>
# Works on image files only (created by the tests: CHUD_EXPORT_DIR).
set -uo pipefail
SRC="${1:?source image}"; DST="${2:?clone image}"; N="${3:?partition number}"
W="$(mktemp -d /var/tmp/chudclone-grow.XXXXXX)"
trap 'mountpoint -q "$W/mnt" 2>/dev/null && umount "$W/mnt"; rm -rf "$W"' EXIT
mkdir -p "$W/mnt"
FAIL=0
ok()  { echo "  OK    $*"; }
bad() { echo "  FAIL  $*"; FAIL=1; }

geom() { # geom <image> <n> -> "start size" in sectors
  sfdisk -d "$1" 2>/dev/null | awk -v n="$2" -F'[ ,=:]+' '/start=/ { p=$1; sub(/.*[^0-9]/, "", p); if (p==n) { for (i=1;i<=NF;i++) { if ($i=="start") s=$(i+1); if ($i=="size") z=$(i+1) } print s, z } }'
}
read -r ss sz <<<"$(geom "$SRC" "$N")"
read -r ds dz <<<"$(geom "$DST" "$N")"
[ -n "${sz:-}" ] && [ -n "${dz:-}" ] || { echo "partition $N not found"; exit 2; }
echo "== Partition $N: source $sz sectors, clone $dz sectors"
dd if="$SRC" of="$W/s.img" bs=512 skip="$ss" count="$sz" status=none
dd if="$DST" of="$W/d.img" bs=512 skip="$ds" count="$dz" status=none

out="$(ntfsresize --check -f "$W/d.img" 2>&1)"; rc=$?
[ $rc -eq 0 ] && ok "ntfsresize --check: no inconsistency" || bad "ntfsresize --check: $(echo "$out" | tail -4)"

clusters() { ntfsinfo -f -m "$1" 2>/dev/null | awk -F: '/Volume Size in Clusters/ {gsub(/ /,"",$2); print $2}'; }
csize() { ntfsinfo -f -m "$1" 2>/dev/null | awk -F: '/Cluster Size/ {gsub(/ /,"",$2); print $2}'; }
cs="$(csize "$W/d.img")"
want=$(( (dz - 1) * 512 / cs ))
got="$(clusters "$W/d.img")"
[ "$got" = "$want" ] && ok "volume size: $got clusters (whole partition)" || bad "volume size: $got clusters, $want expected"
flags="$(ntfsinfo -f -m "$W/d.img" 2>/dev/null | awk -F: '/Volume Flags/ {print $2}')"
echo "$flags" | grep -q DIRTY && ok "volume marked dirty (chkdsk on first Windows start)" || bad "dirty flag missing"

listing() { # hashes of every file of an NTFS image
  ntfs-3g -o ro,force "$1" "$W/mnt" 2>/dev/null || { echo "MOUNT FAILED"; return; }
  (cd "$W/mnt" && find . -type f -print0 | sort -z | xargs -0 -r md5sum)
  umount "$W/mnt"
}
listing "$W/s.img" > "$W/ls.txt"; listing "$W/d.img" > "$W/ld.txt"
count="$(wc -l < "$W/ls.txt")"
if [ "$count" -gt 0 ] && cmp -s "$W/ls.txt" "$W/ld.txt"; then ok "$count file(s), hashes identical to the source"
else bad "files differ: $(diff "$W/ls.txt" "$W/ld.txt" | head -5)"; fi

# Reference: the same volume grown by ntfsresize to the same size.
cp "$W/s.img" "$W/ref.img"; truncate -s $(( dz * 512 )) "$W/ref.img"
if ntfsresize -f -f -s $(( (dz - 1) * 512 )) "$W/ref.img" >"$W/ref.log" 2>&1; then
  free() { ntfs-3g -o ro,force "$1" "$W/mnt" 2>/dev/null && { stat -f -c '%f' "$W/mnt"; umount "$W/mnt"; }; }
  fr="$(free "$W/ref.img")"; fd="$(free "$W/d.img")"; rc_="$(clusters "$W/ref.img")"
  [ "$rc_" = "$got" ] && ok "same number of clusters as ntfsresize ($rc_)" || bad "ntfsresize: $rc_ clusters, Chud Clone: $got"
  [ "$fr" = "$fd" ] && ok "same free space as ntfsresize ($fd blocks)" || bad "free space: ntfsresize $fr, Chud Clone $fd"
else
  bad "reference ntfsresize failed: $(tail -3 "$W/ref.log")"
fi

# Real use of the added space, on a copy (dirty flag cleared by ntfsfix).
cp "$W/d.img" "$W/w.img"
ntfsfix -d "$W/w.img" >/dev/null 2>&1
if ntfs-3g -o rw "$W/w.img" "$W/mnt" 2>/dev/null; then
  avail=$(( $(stat -f -c '%a' "$W/mnt") * $(stat -f -c '%S' "$W/mnt") ))
  fill=$(( avail - 4 * 1024 * 1024 ))
  head -c "$fill" /dev/urandom > "$W/mnt/fill.bin" 2>/dev/null; sync
  h1="$(md5sum < "$W/mnt/fill.bin")"; umount "$W/mnt"
  out="$(ntfsresize --check -f "$W/w.img" 2>&1)" && ok "$(( fill / 1048576 )) MiB file written into the added space; volume still consistent" || bad "after writing: $(echo "$out" | tail -3)"
  ntfs-3g -o ro "$W/w.img" "$W/mnt" && { h2="$(md5sum < "$W/mnt/fill.bin")"; umount "$W/mnt"; }
  [ "$h1" = "${h2:-}" ] && ok "file read back identical" || bad "file read back differs"
else
  bad "read-write mount failed"
fi

[ $FAIL -eq 0 ] && echo "RESULT: growth validated" || echo "RESULT: FAILED"
exit $FAIL
