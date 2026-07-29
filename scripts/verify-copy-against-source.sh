#!/usr/bin/env bash
# Compare a whole-disk copy with the disk it was taken from, file by file.
#
# This is the acceptance test no synthetic fixture can replace: it reads the
# real source filesystem and the real copy and asks the only question that
# matters — is every file that should be there, there, and byte-identical?
#
# It answers three things at once:
#   * files MISSING from the copy   → truly removed (what exclusions promise)
#   * files present but ALL ZEROS   → Stage-1 "ghosts" (data gone, entry left)
#   * files present and DIFFERENT   → a defect; nothing else explains it
#
# Everything is read-only, twice over: the source device is set read-only with
# `blockdev --setro`, its container is opened `--readonly`, and the filesystem
# is mounted `ro,norecovery` so not even a journal replay can write to it.
#
# The two filesystems are NEVER mounted at the same time: a copy carries the
# source's btrfs UUID, and two mounted filesystems sharing one UUID is how the
# kernel ends up reading the wrong device.
#
# Usage (WSL2 / any Linux, as root):
#
#   # 1. attach the source disk to WSL from an ELEVATED PowerShell:
#   #    wsl --mount \\.\PHYSICALDRIVE3 --bare
#   # 2. then, inside WSL:
#   sudo bash scripts/verify-copy-against-source.sh /dev/sde /mnt/e/OwnBackup/copy.qcow2
#
#   Add --fast to compare sizes and mtimes only (no hashing).
#   Add --part N to compare partition N instead of hunting for the btrfs one
#   (ext4, NTFS and xfs partitions are compared the same way).
set -euo pipefail

FAST=0
ARGS=()
PART=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --fast) FAST=1 ;;
        --part) PART="$2"; shift ;;
        *) ARGS+=("$1") ;;
    esac
    shift
done
if [[ ${#ARGS[@]} -lt 2 ]]; then
    sed -n '2,30p' "$0"
    exit 2
fi
SRC_DISK="${ARGS[0]}"
COPY="${ARGS[1]}"
WORK="${ARGS[2]:-/tmp/obk-verify}"

[[ $EUID -eq 0 ]] || { echo "нужен root (mount/cryptsetup)"; exit 1; }
[[ -b $SRC_DISK ]] || { echo "$SRC_DISK — не блочное устройство"; exit 1; }
[[ -f $COPY ]] || { echo "$COPY — файла копии нет"; exit 1; }

MNT_SRC="$WORK/mnt-src"
MNT_COPY="$WORK/mnt-copy"
RAW="$WORK/copy.raw"
mkdir -p "$MNT_SRC" "$MNT_COPY"

LOOP=""
cleanup() {
    umount "$MNT_SRC" 2>/dev/null || true
    umount "$MNT_COPY" 2>/dev/null || true
    [[ -n $LOOP ]] && losetup -d "$LOOP" 2>/dev/null || true
    cryptsetup close obk_src 2>/dev/null || true
}
trap cleanup EXIT

# dm-crypt and btrfs are modules in the WSL2 kernel; mount/cryptsetup would
# pull them in anyway, but a missing module is much clearer said out loud than
# met as "unknown filesystem type".
modprobe dm_crypt 2>/dev/null || true
modprobe btrfs 2>/dev/null || true

# --- Phase 0: the source disk becomes physically read-only ------------------
echo "== источник: $SRC_DISK (только чтение)"
blockdev --setro "$SRC_DISK"
for p in "$SRC_DISK"?*; do [[ -b $p ]] && blockdev --setro "$p" || true; done

SRC_PART=""
if [[ -n $PART ]]; then
    # An explicit partition: LUKS if it is a container, plain otherwise.
    cand="${SRC_DISK}${PART}"
    [[ -b $cand ]] || { echo "$cand не найден"; exit 1; }
    if cryptsetup isLuks "$cand" 2>/dev/null; then
        SRC_PART="$cand"
    else
        SRC_FS="$cand"
        echo "   раздел: $SRC_FS ($(blkid -o value -s TYPE "$SRC_FS" 2>/dev/null || echo '?'))"
    fi
else
for p in "$SRC_DISK"?*; do
    [[ -b $p ]] || continue
    if cryptsetup isLuks "$p" 2>/dev/null; then SRC_PART="$p"; break; fi
done
fi
if [[ -n $SRC_PART ]]; then
    echo "   LUKS-раздел: $SRC_PART ($(blockdev --getsize64 "$SRC_PART" 2>/dev/null | numfmt --to=iec 2>/dev/null || echo '?'))"
    echo "   сейчас будет запрошен пароль от контейнера — открывается ТОЛЬКО НА ЧТЕНИЕ"
    cryptsetup open --readonly "$SRC_PART" obk_src
    SRC_FS=/dev/mapper/obk_src
elif [[ -z ${SRC_FS:-} ]]; then
    # No container and no explicit choice: take a btrfs partition.
    for p in "$SRC_DISK"?*; do
        [[ -b $p ]] || continue
        if blkid -o value -s TYPE "$p" 2>/dev/null | grep -q btrfs; then SRC_FS="$p"; fi
    done
    [[ -n ${SRC_FS:-} ]] || { echo "на диске нет ни LUKS, ни btrfs; укажите --part N"; exit 1; }
    echo "   btrfs-раздел: $SRC_FS"
fi

# --- Phase 1: manifest of the source ----------------------------------------
# btrfs wants subvolid=5 to see every subvolume from one mount; ext/xfs/NTFS
# want no such thing, and `norecovery` is spelled differently or not at all.
mount_ro() { # $1 = device/image, $2 = mountpoint, $3.. = extra options
    local dev="$1" mnt="$2"; shift 2
    local fs
    fs=$(blkid -o value -s TYPE "$dev" 2>/dev/null || echo "")
    local opts="ro"
    case "$fs" in
        btrfs) opts="ro,norecovery,subvolid=5" ;;
        ext2|ext3|ext4) opts="ro,noload" ;;
        xfs) opts="ro,norecovery" ;;
    esac
    mount -o "$opts$( [[ $# -gt 0 ]] && printf ',%s' "$@" )" "$dev" "$mnt" 2>/dev/null \
        || mount -o "ro$( [[ $# -gt 0 ]] && printf ',%s' "$@" )" "$dev" "$mnt"
}
mount_ro "$SRC_FS" "$MNT_SRC"

# NOTE: no `-xdev`. Every btrfs subvolume has its own device id, so `-xdev`
# stops dead at `@`, `@home`, `@swap` and the walk returns four directories —
# which the comparison then reports as "0 differences". A verification that
# silently checks nothing is worse than none at all; hence also the floor in
# `require_plausible` below.
manifest() { # $1 = mountpoint, $2 = output file
    local root="$1" out="$2"
    ( cd "$root" && find . \( -type f -o -type l -o -type d \) -printf '%y\t%s\t%P\n' ) \
        | LC_ALL=C sort -t$'\t' -k3 > "$out.meta"
    if [[ $FAST -eq 0 ]]; then
        # `hash<TAB>path`, sorted by path: sha256sum's own "hash  path" splits
        # on blanks, and half the paths on a Linux root have spaces in them.
        ( cd "$root" && find . -type f -printf '%P\0' \
            | xargs -0 -r -P 4 -n 64 sha256sum 2>/dev/null ) \
            | sed 's/^\([0-9a-f]\{64\}\)  /\1\t/' \
            | LC_ALL=C sort -t$'\t' -k2 > "$out.sha" || true
    else
        : > "$out.sha"
    fi
}

# A Linux root has hundreds of thousands of entries. Anything near zero means
# the walk never happened, and the verdict below would be a lie.
require_plausible() { # $1 = count, $2 = what
    local floor=5000
    [[ -n $PART ]] && floor=3     # an explicit partition may be a small /boot
    if [[ $1 -lt $floor ]]; then
        echo
        echo "!! в описи $2 всего $1 записей — обход не состоялся, сравнивать нечего."
        echo "   (подтома не обошлись? не тот раздел? пустой образ?)"
        exit 3
    fi
}

echo "== снимаю опись источника (это долго: читается каждый файл)"
manifest "$MNT_SRC" "$WORK/src"
SRC_FILES=$(wc -l < "$WORK/src.meta")
require_plausible "$SRC_FILES" "источника"
umount "$MNT_SRC"
[[ -n $SRC_PART ]] && cryptsetup close obk_src
echo "   файлов/каталогов: $SRC_FILES"

# --- Phase 2: the copy ------------------------------------------------------
# The copy holds the LUKS payload DECRYPTED, so its btrfs is readable without
# a password — the partition simply starts where the partition table says.
echo "== разворачиваю копию: $COPY"
case "$COPY" in
    *.qcow2|*.vhdx) qemu-img convert -p -O raw -S 4096 "$COPY" "$RAW" ;;
    *) RAW="$COPY" ;;
esac

OFF=""
n=0
while read -r start size; do
    [[ -n $start ]] || continue
    n=$((n + 1))
    o=$((start * 512))
    if [[ -n $PART ]]; then
        # The caller named the partition: take it whatever filesystem it holds.
        if [[ $n -eq $PART ]]; then OFF=$o; LEN=$((size * 512)); fi
    elif [[ $(dd if="$RAW" bs=1 skip=$((o + 65600)) count=8 2>/dev/null) == "_BHRfS_M" ]]; then
        # btrfs superblock magic lives 64 KiB into the partition.
        OFF=$o
        LEN=$((size * 512))
    fi
done < <(sfdisk -d "$RAW" 2>/dev/null | sed -n 's/.*start=\s*\([0-9]*\).*size=\s*\([0-9]*\).*/\1 \2/p')
if [[ -z $OFF ]] && [[ $(dd if="$RAW" bs=1 skip=65600 count=8 2>/dev/null) == "_BHRfS_M" ]]; then
    # Not a whole disk but the partition itself, already carved out.
    OFF=0
    LEN=$(stat -c %s "$RAW")
fi
[[ -n $OFF ]] || { echo "в копии не нашёлся btrfs — нечего сравнивать"; exit 1; }
echo "   btrfs в копии: смещение $OFF, длина $LEN"

echo "== структурные проверки копии"
LOOP=$(losetup --find --show --read-only --offset "$OFF" --sizelimit "$LEN" "$RAW")
COPY_FS=$(blkid -o value -s TYPE "$LOOP" 2>/dev/null || echo "")
echo "   файловая система копии: ${COPY_FS:-неизвестна}"
case "$COPY_FS" in
    btrfs) btrfs check --readonly --check-data-csum "$LOOP" 2>&1 \
        | grep -aiE "no error found|ERROR|WARNING|corrupt" | head -5 || true ;;
    ext2|ext3|ext4) e2fsck -fn "$LOOP" 2>&1 | tail -3 || true ;;
    xfs) xfs_repair -n "$LOOP" 2>&1 | tail -3 || true ;;
esac
mount_ro "$LOOP" "$MNT_COPY"
echo "== снимаю опись копии"
manifest "$MNT_COPY" "$WORK/copy"
COPY_FILES=$(wc -l < "$WORK/copy.meta")
echo "   файлов/каталогов: $COPY_FILES"
require_plausible "$COPY_FILES" "копии"

# --- Phase 3: compare -------------------------------------------------------
echo
echo "=============== РЕЗУЛЬТАТ ==============="
LC_ALL=C join -t$'\t' -j 3 -v 1 -o 0 "$WORK/src.meta" "$WORK/copy.meta" > "$WORK/missing.txt" || true
# -v 1 with the copy FIRST: unpairable lines of the copy. (-v 2 with the copy
# first prints the source's, which made "есть только в копии" mirror the
# missing count exactly — 2043 both ways, and neither number meant that.)
LC_ALL=C join -t$'\t' -j 3 -v 1 -o 0 "$WORK/copy.meta" "$WORK/src.meta" > "$WORK/extra.txt" || true
echo "нет в копии (удалено):        $(wc -l < "$WORK/missing.txt")"
echo "есть только в копии:          $(wc -l < "$WORK/extra.txt")"

if [[ $FAST -eq 0 ]]; then
    LC_ALL=C join -t$'\t' -j 2 -o 0,1.1,2.1 "$WORK/src.sha" "$WORK/copy.sha" 2>/dev/null \
        | awk -F'\t' '$2 != $3 {print $1}' > "$WORK/differ.txt" || true
    DIFF_N=$(wc -l < "$WORK/differ.txt")
    echo "содержимое различается:       $DIFF_N"
    # A "ghost" is a file whose copy is all zeros: data excluded, entry kept.
    # An unreadable one is worse than either — that is the EIO an excluded
    # file used to give when its blocks never made it into the image.
    : > "$WORK/ghosts.txt"; : > "$WORK/broken.txt"; : > "$WORK/unreadable.txt"
    while IFS= read -r f; do
        [[ -n $f ]] || continue          # sha256sum escapes odd names; skip the debris
        [[ -f "$MNT_COPY/$f" ]] || continue
        if ! cat -- "$MNT_COPY/$f" > /dev/null 2>&1; then
            echo "$f" >> "$WORK/unreadable.txt"
        elif [[ -z $(LC_ALL=C tr -d '\000' < "$MNT_COPY/$f" | head -c1) ]]; then
            echo "$f" >> "$WORK/ghosts.txt"      # nothing but zeros
        else
            echo "$f" >> "$WORK/broken.txt"      # real mismatch
        fi
    done < "$WORK/differ.txt"
    echo "   из них нулевые «призраки»: $(wc -l < "$WORK/ghosts.txt")"
    echo "   из них НЕ ЧИТАЮТСЯ (EIO):  $(wc -l < "$WORK/unreadable.txt")"
    echo "   из них ИСПОРЧЕНО:          $(wc -l < "$WORK/broken.txt")"
    if [[ -s "$WORK/unreadable.txt" ]]; then
        echo
        echo "!! файлы, чтение которых даёт ошибку — копия непригодна:"
        head -20 "$WORK/unreadable.txt"
    fi
    if [[ -s "$WORK/broken.txt" ]]; then
        echo
        echo "!! файлы, которые не исключались и всё равно не совпали:"
        head -20 "$WORK/broken.txt"
        echo "   (полный список: $WORK/broken.txt)"
    fi
fi
echo
echo "описи и списки: $WORK"
echo "Замечание: файлы, изменённые в источнике ПОСЛЕ снятия копии, тоже попадут"
echo "в расхождения — сверяйтесь со временем копии, если система с тех пор работала."
