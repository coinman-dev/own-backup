#!/usr/bin/env bash
# Linux acceptance of task K (keys, encryption, TPM2): the service key store
# with its machine key in systemd-creds, encrypted chains (OBK) through the
# CLI, the key directory left out of every copy, and LUKS — an opened mapping
# copied through a device-mapper snapshot, the rules applied inside a
# container the engine decrypts itself. Loop devices only, as root; it runs
# the same way inside a stand guest (`scripts/lab/linux-vm.py`, with `up
# --tpm` for a TPM 2.0) and on any root shell with /dev/loop*.
#
#   sudo scripts/lab/linux-keys-scenario.sh [CLI] [STAGE...]
#
# CLI defaults to target/debug/own-backup-cli (build it first, as yourself).
# STAGE is any of k1 k2 k3 k4 (default: all):
#   k1  key store: machine key through systemd-creds (tpm2 when the machine
#       has a TPM 2.0, host otherwise — said which), files 0600 in a 0700
#       root directory, an authorization reopened by ANOTHER process, a copy
#       of the directory useless elsewhere is not tested here (needs a second
#       machine), revoked at the end.
#   k2  encrypted chain: full + incremental + differential `.obk` of an idle
#       ext4, nothing of the files in the clear in the images, verify, restore
#       of the newest with the password — fsck clean, every file the same.
#   k3  key directory: an ext4 mounted (read-only, so it holds still without a
#       snapshot) as the data directory with a key store in it, copied with NO
#       rules — the keys are not in the image (byte search and restore), the
#       rest is; sector by sector is refused.
#   k4  LUKS: an opened LUKS2 mapping, mounted, copied through a dm snapshot
#       with the password from --password-stdin (and refused without it); the
#       raw container copied with `--exclude '*.tmp'` — the rule applied
#       inside it, the restore holds report.txt and not report.tmp.
# Each check prints `CHECK <id> PASS|FAIL|SKIP <text>`; the exit code is the
# number of FAILs. Evidence stays in $WORK/evidence until the next run.
set -uo pipefail

cd "$(dirname "$0")/../.."
CLI=$(readlink -f "${1:-target/debug/own-backup-cli}")
shift || true
STAGES=("$@")
[[ ${#STAGES[@]} -eq 0 ]] && STAGES=(k1 k2 k3 k4)

WORK=${WORK:-$PWD/tmp/lab-keys}
EV=$WORK/evidence
PW='lab archive password 2026'
LUKS_PW='lab luks password'
FAILS=0
CHECKS=0
LOOPS=()
MOUNTS=()
MAPPINGS=()

if [[ $(id -u) -ne 0 ]]; then
    echo "run as root (loop devices, mount, cryptsetup, systemd-creds)" >&2
    exit 2
fi
[[ -x $CLI ]] || { echo "no CLI at $CLI — cargo build -p own-backup-cli" >&2; exit 2; }

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    CHECKS=$((CHECKS + 1))
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-34s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}

verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }

run() { # NAME CMD...: output to $EV/NAME.out, exit code returned
    local name=$1
    shift
    echo "### $*" >"$EV/$name.out"
    "$@" >>"$EV/$name.out" 2>&1
    local rc=$?
    echo "### exit $rc" >>"$EV/$name.out"
    return "$rc"
}

loopdev() { # VAR name MiB
    truncate -s "${3}M" "$WORK/$2.img" || return 1
    local d
    d=$(losetup -f --show "$WORK/$2.img") || return 1
    LOOPS+=("$d")
    printf -v "$1" '%s' "$d"
}

mnt() { # device dir [opts]
    mkdir -p "$2"
    mount ${3:+-o "$3"} "$1" "$2" && MOUNTS+=("$2")
}

umnt() { mountpoint -q "$1" && umount "$1"; }

populate() { # dir
    local d=$1/data i
    mkdir -p "$d/nested" "$d/many" "$d/keep" || return 1
    head -c 8M /dev/urandom >"$d/big.bin" || return 1
    seq 1 100000 >"$d/nested/seq.txt" || return 1
    printf 'OBK-LAB-PLAINTEXT-MARKER %s\n' "$(date +%s)" >"$d/hello.txt" || return 1
    for i in $(seq 1 30); do head -c $((1024 + i * 37)) /dev/urandom >"$d/many/f$i"; done
    head -c 2M /dev/urandom >"$d/keep/report.tmp" || return 1
    head -c 1M /dev/urandom >"$d/keep/report.txt" || return 1
    sync
}

hashes() { (cd "$1" && find data -type f -print0 | sort -z | xargs -0 sha256sum) >"$2"; }
compare() { (cd "$1" && sha256sum -c --quiet "$2") >/dev/null 2>&1; }

cleanup() {
    local m d
    for m in "${MOUNTS[@]}"; do umnt "$m" 2>/dev/null; done
    for d in "${MAPPINGS[@]}"; do cryptsetup close "$d" 2>/dev/null; done
    for d in "${LOOPS[@]}"; do losetup -d "$d" 2>/dev/null; done
    "$CLI" cleanup-snapshots >/dev/null 2>&1 || true
}
trap cleanup EXIT

# encrypted_full NAME SOURCE OUTPUT: `backup --encrypt`, answering its
# questions as a person would — the password twice, then the recovery key it
# shows, typed back. The key goes to $EV/NAME.recovery (evidence only).
encrypted_full() {
    local name=$1 src=$2 out=$3 line key=""
    echo "### backup --encrypt $src -> $out" >"$EV/$name.out"
    coproc CP { "$CLI" backup "$src" -o "$out" --format qcow2 --encrypt 2>&1; }
    # Bash forgets both once the job ends: keep them.
    local pid=$CP_PID in=${CP[1]} outfd=${CP[0]}
    printf '%s\n%s\n' "$PW" "$PW" >&"$in"
    while IFS= read -r line <&"$outfd"; do
        echo "$line" >>"$EV/$name.out"
        if [[ -z $key && $line =~ ^\ +(obkr[a-z0-9\ ]+)$ ]]; then
            key=${BASH_REMATCH[1]}
            echo "$key" >"$EV/$name.recovery"
            printf '%s\n' "$key" >&"$in"
        fi
    done
    wait "$pid"
    local rc=$?
    echo "### exit $rc" >>"$EV/$name.out"
    [[ -n $key ]] || return 99
    return "$rc"
}

stage_k1() {
    local state=$WORK/state rc mode expect
    rm -rf "$state" && mkdir -m 0750 "$state"
    if ! command -v systemd-creds >/dev/null; then
        check k1-systemd-creds SKIP "no systemd-creds on this machine"
        return
    fi
    if systemd-creds has-tpm2 --quiet 2>/dev/null; then expect=tpm2; else expect=host; fi
    run k1-check "$CLI" key-store check --data-dir "$state"
    rc=$?
    check k1-authorize "$(verdict $rc)" "a job key authorized in a fresh store"
    mode=$(sed -n 's/^protection: //p' "$EV/k1-check.out" | head -1)
    [[ $mode == "$expect" ]]
    check k1-protection "$(verdict $?)" "machine key protected by '$mode' (expected '$expect')"
    local dir=$state/.own-backup-service-keys
    [[ $(stat -c '%a %U' "$dir") == "700 root" ]]
    check k1-dir-mode "$(verdict $?)" "key directory root 0700: $(stat -c '%a %U' "$dir")"
    local bad=0 f
    for f in "$dir"/*; do
        [[ $(stat -c '%a %U %h' "$f") == "600 root 1" ]] || bad=1
    done
    check k1-file-mode "$(verdict $bad)" "every key file root 0600 with one name"
    ls -la "$dir" >"$EV/k1-listing.out"
    ! grep -q '\.tmp$' "$EV/k1-listing.out"
    check k1-no-temporaries "$(verdict $?)" "no temporary file left in the directory"
    run k1-reopen "$CLI" key-store reopen --data-dir "$state"
    check k1-reopen "$(verdict $?)" "another process reopens the authorization with no password"
    run k1-revoke "$CLI" key-store revoke --data-dir "$state"
    check k1-revoke "$(verdict $?)" "the authorization is revoked"
    run k1-gone "$CLI" key-store reopen --data-dir "$state"
    [[ $? -ne 0 ]]
    check k1-gone "$(verdict $?)" "a revoked authorization no longer opens"
}

stage_k2() {
    local src out=$WORK/out-k2 tgt
    rm -rf "$out" && mkdir -p "$out"
    loopdev src k2-src 256 && loopdev tgt k2-tgt 256 || { check k2-setup FAIL "loop devices"; return; }
    mkfs.ext4 -q -F "$src" && mnt "$src" "$WORK/k2" && populate "$WORK/k2" \
        && hashes "$WORK/k2" "$EV/k2-full.sha" && umnt "$WORK/k2"
    check k2-setup "$(verdict $?)" "ext4 with files, unmounted (idle)"
    encrypted_full k2-full "$src" "$out/vol_full.obk"
    check k2-full "$(verdict $?)" "full encrypted copy, recovery key confirmed"
    mnt "$src" "$WORK/k2" && head -c 3M /dev/urandom >"$WORK/k2/data/added.bin" \
        && hashes "$WORK/k2" "$EV/k2-inc.sha" && umnt "$WORK/k2"
    sleep 1.1
    printf '%s\n' "$PW" | run k2-inc "$CLI" backup "$src" -o "$out/vol_inc1.obk" \
        --kind incremental --parent-auto --encrypt
    check k2-inc "$(verdict $?)" "incremental over the encrypted full (parent's key)"
    sleep 1.1
    printf '%s\n' "$PW" | run k2-diff "$CLI" backup "$src" -o "$out/vol_diff1.obk" \
        --kind differential --parent "$out/vol_full.obk"
    check k2-diff "$(verdict $?)" "differential continues the parent's key without --encrypt"
    local leaked=0 img
    for img in "$out"/*.obk; do
        grep -qa 'OBK-LAB-PLAINTEXT-MARKER' "$img" && leaked=1
    done
    check k2-no-plaintext "$(verdict $leaked)" "no file content in the clear in any image"
    run k2-plain-verify "$CLI" verify "$out/vol_inc1.obk" </dev/null
    [[ $? -ne 0 ]]
    check k2-needs-key "$(verdict $?)" "verify without the password is refused"
    printf '%s\n' "$PW" | run k2-verify "$CLI" verify "$out/vol_inc1.obk"
    check k2-verify "$(verdict $?)" "verify with the password"
    printf '%s\n' "$PW" | run k2-restore "$CLI" restore "$out/vol_inc1.obk" -t "$tgt" --yes
    check k2-restore "$(verdict $?)" "restore the newest increment with the password"
    run k2-fsck e2fsck -fn "$tgt"
    check k2-fsck "$(verdict $?)" "e2fsck clean after restore"
    mnt "$tgt" "$WORK/k2-rst" ro && compare "$WORK/k2-rst" "$EV/k2-inc.sha"
    check k2-files "$(verdict $?)" "every file of the increment reads back the same"
    umnt "$WORK/k2-rst"
    local rk
    rk=$(cat "$EV/k2-full.recovery" 2>/dev/null)
    printf '\n%s\n' "$rk" | run k2-recovery "$CLI" verify "$out/vol_full.obk"
    check k2-recovery "$(verdict $?)" "the recovery key alone opens the full"
}

stage_k3() {
    local src out=$WORK/out-k3 tgt data=$WORK/k3-data
    rm -rf "$out" && mkdir -p "$out"
    loopdev src k3-src 256 && loopdev tgt k3-tgt 256 || { check k3-setup FAIL "loop devices"; return; }
    mkfs.ext4 -q -F "$src" && mnt "$src" "$data" && populate "$data"
    local keys=$data/.own-backup-service-keys
    mkdir -m 0700 "$keys" && printf 'OBK-LAB-KEY-MARKER-%s' "$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')" \
        >"$keys/machine.cred" && chmod 0600 "$keys/machine.cred" && sync
    local marker
    marker=$(cat "$keys/machine.cred")
    mount -o remount,ro "$data"
    check k3-setup "$(verdict $?)" "ext4 mounted read-only as the data directory, a key store in it"
    # The engine finds the key directory through the configured data directory.
    OWN_BACKUP_DATA_DIR=$data run k3-copy "$CLI" backup "$src" -o "$out/data_full.qcow2" --format qcow2
    check k3-copy "$(verdict $?)" "copied with no exclusion rules at all (the volume is in use)"
    grep -q 'Excluded:' "$EV/k3-copy.out" && ! grep -q 'Exclusions not applied' "$EV/k3-copy.out"
    check k3-said "$(verdict $?)" "the copy says what it left out"
    ! grep -qa "$marker" "$out/data_full.qcow2"
    check k3-no-key-bytes "$(verdict $?)" "the key file's bytes are not in the image"
    run k3-restore "$CLI" restore "$out/data_full.qcow2" -t "$tgt" --yes
    check k3-restore "$(verdict $?)" "restore"
    mnt "$tgt" "$WORK/k3-rst" ro
    [[ ! -e $WORK/k3-rst/.own-backup-service-keys/machine.cred && -f $WORK/k3-rst/data/hello.txt ]]
    check k3-restored "$(verdict $?)" "the restored volume has the data and no key file"
    umnt "$WORK/k3-rst"
    OWN_BACKUP_DATA_DIR=$data run k3-sectors "$CLI" backup "$src" -o "$out/data_sec.qcow2" \
        --format qcow2 --all-sectors
    [[ $? -ne 0 && ! -e $out/data_sec.qcow2 ]]
    check k3-sector-refused "$(verdict $?)" "a sector-by-sector copy of it is refused, nothing written"
    umnt "$data"
}

stage_k4() {
    local raw out=$WORK/out-k4 tgt map=obk-lab-luks
    rm -rf "$out" && mkdir -p "$out"
    if ! command -v cryptsetup >/dev/null; then
        check k4-cryptsetup SKIP "no cryptsetup"
        return
    fi
    loopdev raw k4-raw 320 && loopdev tgt k4-tgt 320 || { check k4-setup FAIL "loop devices"; return; }
    printf '%s' "$LUKS_PW" | cryptsetup luksFormat -q --type luks2 --pbkdf pbkdf2 \
        --pbkdf-force-iterations 1000 "$raw" - \
        && printf '%s' "$LUKS_PW" | cryptsetup open "$raw" "$map" --key-file - \
        && MAPPINGS+=("$map") && mkfs.ext4 -q -F "/dev/mapper/$map" \
        && mnt "/dev/mapper/$map" "$WORK/k4" && populate "$WORK/k4" \
        && hashes "$WORK/k4" "$EV/k4.sha"
    check k4-setup "$(verdict $?)" "LUKS2 container opened, ext4 mounted inside"
    run k4-no-password "$CLI" backup "/dev/mapper/$map" -o "$out/map_nopw.qcow2" --format qcow2 </dev/null
    [[ $? -ne 0 && ! -e $out/map_nopw.qcow2 ]]
    check k4-needs-password "$(verdict $?)" "the mounted mapping is refused without its password"
    printf '%s\n' "$LUKS_PW" | run k4-mapping "$CLI" backup "/dev/mapper/$map" \
        -o "$out/map_full.qcow2" --format qcow2 --password-stdin
    check k4-mapping "$(verdict $?)" "the mounted mapping is copied through a dm snapshot (--password-stdin)"
    ! pgrep -af -- "--password" | grep -q "$LUKS_PW"
    check k4-no-argv-secret "$(verdict $?)" "the password never appears in a command line"
    umnt "$WORK/k4" && cryptsetup close "$map"
    printf '%s\n' "$LUKS_PW" | run k4-raw "$CLI" backup "$raw" -o "$out/raw_full.qcow2" \
        --format qcow2 --password-stdin --exclude '*.tmp'
    check k4-raw "$(verdict $?)" "the raw container, decrypted by the engine, with a rule"
    grep -q 'Excluded:' "$EV/k4-raw.out" && ! grep -q 'Exclusions not applied' "$EV/k4-raw.out"
    check k4-rule-inside "$(verdict $?)" "the rule ran inside the container"
    run k4-restore "$CLI" restore "$out/raw_full.qcow2" -t "$tgt" --yes
    check k4-restore "$(verdict $?)" "restore (the decrypted filesystem)"
    run k4-fsck e2fsck -fn "$tgt"
    check k4-fsck "$(verdict $?)" "e2fsck clean"
    mnt "$tgt" "$WORK/k4-rst" ro
    [[ -f $WORK/k4-rst/data/keep/report.txt && ! -e $WORK/k4-rst/data/keep/report.tmp ]]
    check k4-excluded "$(verdict $?)" "report.txt restored, report.tmp gone"
    umnt "$WORK/k4-rst"
}

mkdir -p "$EV"
: >"$EV/summary.txt"
for s in "${STAGES[@]}"; do
    case $s in
        k1 | k2 | k3 | k4) "stage_$s" ;;
        *) echo "unknown stage $s" >&2; exit 2 ;;
    esac
done
echo "checks: $CHECKS, failed: $FAILS (evidence: $EV)"
exit "$FAILS"
