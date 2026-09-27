#!/usr/bin/env bash
# Boot the existing Windows 10 lab through a new qcow2 overlay. The base VM
# and its previous evidence remain untouched. The guest startup task runs
# audit-service-stage2.ps1 from the new answer ISO and powers off afterwards.
set -euo pipefail

repo="${REPO_DIR:-$HOME/Development/own-backup}"
base="$HOME/lab/win-lab.qcow2"
lab="${AUDIT_LAB_DIR:-$HOME/lab-audit-2026-09-26}"
service="$repo/target/x86_64-pc-windows-msvc/release/own-backup-service.exe"
cli="$repo/target/x86_64-pc-windows-msvc/release/own-backup-cli.exe"
expected_hash="${AUDIT_SERVICE_SHA256:-}"

case "${1:-}" in
    prepare)
        test -r "$base" || { echo "missing base VM: $base" >&2; exit 1; }
        test -r "$service" || { echo "missing release service: $service" >&2; exit 1; }
        test -r "$cli" || { echo "missing release CLI: $cli" >&2; exit 1; }
        actual_hash="$(sha256sum "$service" | cut -d ' ' -f 1)"
        test -z "$expected_hash" || test "$actual_hash" = "$expected_hash" || {
            echo "wrong service build: $actual_hash" >&2; exit 1;
        }
        test ! -e "$lab/win-lab.qcow2" || {
            echo "overlay already exists: $lab/win-lab.qcow2" >&2; exit 1;
        }
        mkdir -p "$lab/answer/lab"
        cp "$repo/scripts/lab/audit-service-stage2.ps1" "$lab/answer/lab/lab-stage2.ps1"
        cp "$service" "$cli" "$lab/answer/lab/"
        printf '%s\n' "$actual_hash" > "$lab/answer/lab/expected-service.sha256"
        xorriso -as mkisofs -quiet -J -R -V OBKAUDIT -o "$lab/answer.iso" "$lab/answer"
        qemu-img create -f qcow2 -F qcow2 -b "$base" "$lab/win-lab.qcow2"
        echo "prepared isolated VM: $lab"
        ;;
    run|wait|status|shot)
        test -r "$lab/win-lab.qcow2" || {
            echo "run prepare first: $lab/win-lab.qcow2" >&2; exit 1;
        }
        action="$1"
        if [ "$action" = run ]; then action=coldstart; fi
        LAB_DIR="$lab" REPO_DIR="$repo" bash "$repo/scripts/wsl-windows-lab.sh" "$action" "${2:-}"
        ;;
    *)
        echo "usage: $0 prepare|run|wait|status|shot" >&2
        exit 2
        ;;
esac
