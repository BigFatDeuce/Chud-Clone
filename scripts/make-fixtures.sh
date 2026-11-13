#!/bin/bash
# Generates the Chud Clone test disk images with REAL file systems
# (mkntfs / mkfs.vfat / sgdisk / sfdisk), without ever touching a physical disk:
# everything happens in ordinary files.
#
# Run on Linux or WSL, as root (ntfs-3g FUSE mount):
#   wsl -d <distro> -- bash scripts/make-fixtures.sh crates/core/tests/fixtures
# Required packages: ntfs-3g ntfsprogs dosfstools mtools gdisk util-linux python3
set -euo pipefail

OUT="${1:?output folder required}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
W="$(mktemp -d /tmp/chudclone-fx.XXXXXX)"
trap 'mountpoint -q "$W/mnt" 2>/dev/null && umount "$W/mnt"; rm -rf "$W"' EXIT
mkdir -p "$W/mnt"
MIB=$((1024*1024))

# Deterministic content: compressible text plus some reproducible randomness.
gen() { # gen <path> <size in KiB> <seed>
python3 - "$1" "$2" "$3" <<'PY'
import sys, random
path, kib, seed = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rnd = random.Random(seed)
out = bytearray()
i = 0
while len(out) < kib * 1024:
    if i % 16 == 0:
        out += rnd.randbytes(256)
    out += (f"chudclone fixture seed={seed} ligne={i} " + "x" * (i % 37) + "\n").encode()
    i += 1
open(path, "wb").write(out[:kib * 1024])
PY
}

populate_ntfs() { # populate_ntfs <image> <profile> <seed>
  local img="$1" profile="$2" seed="$3"
  ntfs-3g -o rw,big_writes "$img" "$W/mnt"
  case "$profile" in
    windows)
      mkdir -p "$W/mnt/Windows/System32/config" "$W/mnt/Users/Test/Documents" "$W/mnt/Program Files/App" "$W/mnt/tmp"
      gen "$W/mnt/Windows/System32/winload.efi" 900 $((seed+1))
      gen "$W/mnt/Windows/System32/ntoskrnl.exe" 1400 $((seed+2))
      gen "$W/mnt/Windows/System32/config/SYSTEM" 700 $((seed+3))
      gen "$W/mnt/Windows/System32/config/SOFTWARE" 1100 $((seed+4))
      # Small files, every other one deleted: creates fragmentation
      # and holes in the allocation map.
      for i in $(seq 1 120); do gen "$W/mnt/tmp/f$i.bin" $((8 + i % 40)) $((seed+100+i)); done
      for i in $(seq 1 2 120); do rm "$W/mnt/tmp/f$i.bin"; done
      gen "$W/mnt/Users/Test/Documents/rapport.docx" 2300 $((seed+5))
      gen "$W/mnt/Program Files/App/app.exe" 3100 $((seed+6))
      printf 'petit fichier résident dans la MFT\n' > "$W/mnt/Users/Test/note.txt"
      ;;
    recovery)
      mkdir -p "$W/mnt/Recovery/WindowsRE"
      gen "$W/mnt/Recovery/WindowsRE/Winre.wim" 2600 $((seed+1))
      gen "$W/mnt/Recovery/WindowsRE/boot.sdi" 300 $((seed+2))
      printf '<WindowsRE/>\n' > "$W/mnt/Recovery/WindowsRE/ReAgent.xml"
      ;;
    sysres)
      mkdir -p "$W/mnt/Boot"
      gen "$W/mnt/bootmgr" 400 $((seed+1))
      gen "$W/mnt/Boot/BCD" 60 $((seed+2))
      ;;
    data)
      mkdir -p "$W/mnt/Photos" "$W/mnt/Windows/System32"
      gen "$W/mnt/Windows/System32/winload.exe" 500 $((seed+9))
      for i in $(seq 1 40); do gen "$W/mnt/Photos/img$i.jpg" $((50 + i * 7)) $((seed+i)); done
      for i in $(seq 2 3 40); do rm "$W/mnt/Photos/img$i.jpg"; done
      gen "$W/mnt/Photos/video.mp4" 4000 $((seed+77))
      ;;
  esac
  sync
  umount "$W/mnt"
}

mk_ntfs() { # mk_ntfs <image> <size MiB> <label> <start sector> <profile> <seed>
  truncate -s $(( $2 * MIB )) "$1"
  mkntfs -q -F -Q -L "$3" -s 512 -c 4096 -p "$4" -H 255 -S 63 "$1"
  populate_ntfs "$1" "$5" "$6"
}

put() { # put <partition.img> <disk.img> <start sector>
  dd if="$1" of="$2" bs=512 seek="$3" conv=notrunc status=none
}

# ---------------------------------------------------------------------------
# 1. GPT "modern Windows" disk: ESP + MSR + Windows (NTFS) + Recovery
# ---------------------------------------------------------------------------
G="$W/gpt-win.img"
truncate -s $((192 * MIB)) "$G"
sgdisk -Z "$G" >/dev/null 2>&1 || true
sgdisk -a 2048 \
  -U 5A5A5A5A-1111-2222-3333-0123456789AB \
  -n 1:2048:+40M  -t 1:EF00 -c 1:"EFI system partition" -u 1:11111111-AAAA-4AAA-8AAA-000000000001 \
  -n 2:0:+16M     -t 2:0C01 -c 2:"Microsoft reserved partition" -u 2:22222222-AAAA-4AAA-8AAA-000000000002 \
  -n 3:0:+96M     -t 3:0700 -c 3:"Basic data partition" -u 3:33333333-AAAA-4AAA-8AAA-000000000003 \
  -n 4:0:+32M     -t 4:2700 -c 4:"" -u 4:44444444-AAAA-4AAA-8AAA-000000000004 \
  -A 4:set:0 -A 4:set:63 "$G" >/dev/null

# ESP FAT32
truncate -s $((40 * MIB)) "$W/esp.img"
mkfs.vfat -F 32 -s 1 -S 512 -n "SYSTEM" -h 2048 "$W/esp.img" >/dev/null
mmd -i "$W/esp.img" ::/EFI ::/EFI/Microsoft ::/EFI/Microsoft/Boot ::/EFI/Boot ::/EFI/Microsoft/Recovery
gen "$W/bootmgfw.efi" 1500 11; gen "$W/BCD" 40 12; gen "$W/bootx64.efi" 1500 11
mcopy -i "$W/esp.img" "$W/bootmgfw.efi" ::/EFI/Microsoft/Boot/bootmgfw.efi
mcopy -i "$W/esp.img" "$W/BCD" ::/EFI/Microsoft/Boot/BCD
mcopy -i "$W/esp.img" "$W/bootx64.efi" ::/EFI/Boot/bootx64.efi
put "$W/esp.img" "$G" 2048

# MSR: arbitrary non-zero content (it must NOT be required in the clone).
python3 -c "import random,sys; sys.stdout.buffer.write(random.Random(5).randbytes(65536))" \
  | dd of="$G" bs=512 seek=$((2048 + 40*2048)) conv=notrunc status=none

WIN_START=$((2048 + 40*2048 + 16*2048))
mk_ntfs "$W/win.img" 96 "Windows" "$WIN_START" windows 1000
put "$W/win.img" "$G" "$WIN_START"
REC_START=$((WIN_START + 96*2048))
mk_ntfs "$W/rec.img" 32 "Recovery" "$REC_START" recovery 2000
put "$W/rec.img" "$G" "$REC_START"
sgdisk -v "$G" | tail -3

# Copy of the Windows partition marked dirty: used to test the fallback
# to a raw copy when the allocation map cannot be trusted.
cp "$W/win.img" "$W/ntfs-dirty.img"
truncate -s $((96 * MIB)) "$W/ntfs-dirty.img"
ntfsfix "$W/ntfs-dirty.img" >/dev/null

# ---------------------------------------------------------------------------
# 2. MBR "BIOS" disk: System Reserved (active) + NTFS + extended (FAT16 + raw)
# ---------------------------------------------------------------------------
M="$W/mbr-win.img"
truncate -s $((128 * MIB)) "$M"
sfdisk -q "$M" <<'EOF'
label: dos
label-id: 0x1badf00d
start=2048,   size=65536,  type=7, bootable
start=67584,  size=98304,  type=7
start=165888, size=90112,  type=f
start=167936, size=40960,  type=6
start=210944, size=40960,  type=83
EOF
# Dummy boot code (440 bytes) and "boot track" (sectors 1-62).
python3 - "$M" <<'PY'
import sys, random
f = open(sys.argv[1], "r+b")
r = random.Random(42)
f.seek(0); f.write(bytes([0x33, 0xC0, 0x8E, 0xD0]) + r.randbytes(436))
f.seek(512); f.write(r.randbytes(62 * 512))
f.close()
PY
mk_ntfs "$W/sysres.img" 32 "System Reserved" 2048 sysres 3000
put "$W/sysres.img" "$M" 2048
mk_ntfs "$W/data.img" 48 "Windows" 67584 data 4000
put "$W/data.img" "$M" 67584
truncate -s $((20 * MIB)) "$W/fat16.img"
mkfs.vfat -F 16 -s 4 -S 512 -n "FAT16DATA" -h 167936 "$W/fat16.img" >/dev/null
mmd -i "$W/fat16.img" ::/docs
for i in 1 2 3 4 5 6 7 8; do gen "$W/d$i.txt" $((30 + i * 55)) $((500+i)); mcopy -i "$W/fat16.img" "$W/d$i.txt" "::/docs/d$i.txt"; done
mdel -i "$W/fat16.img" ::/docs/d3.txt ::/docs/d6.txt
gen "$W/big.bin" 1900 599; mcopy -i "$W/fat16.img" "$W/big.bin" ::/big.bin
put "$W/fat16.img" "$M" 167936
# Linux-type logical partition with no recognised file system: raw data.
python3 -c "import random,sys; sys.stdout.buffer.write(random.Random(9).randbytes(3*1024*1024))" \
  | dd of="$M" bs=512 seek=210944 conv=notrunc status=none
sfdisk -l "$M" | tail -6

# ---------------------------------------------------------------------------
# Compressed output
# ---------------------------------------------------------------------------
for f in gpt-win mbr-win ntfs-dirty; do
  gzip -9 -c "$W/$f.img" > "$OUT/$f.img.gz"
  printf '%s  %s\n' "$(sha256sum "$W/$f.img" | cut -d' ' -f1)" "$f.img"
done > "$OUT/SHA256SUMS"
cat "$OUT/SHA256SUMS"
ls -la "$OUT"
