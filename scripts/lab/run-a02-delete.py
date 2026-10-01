#!/usr/bin/env python3
"""A02: inspect the complete DeleteJob reply on fresh Windows VM overlays."""
import argparse
import hashlib
import importlib.util
import json
import shutil
import subprocess
import time
from pathlib import Path


def used(path):
    return int(subprocess.check_output(["du", "-s", "-B1", str(path)], text=True).split()[0]) if path.exists() else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("rpc", "legacy", "sidecar", "gui_probe", "gui_action"), default="rpc")
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    lab = Path.home() / "lab"
    base = lab / "resume-h02"
    active = lab / "active-a02"
    if any(lab.glob("active-*")): raise SystemExit("preserve and clean the existing active run first")

    def budget():
        free = shutil.disk_usage(repo).free
        target, total, current = used(repo / "target"), used(lab), used(active)
        print(f"BUDGET free={free/2**30:.2f}GiB target={target/2**30:.2f}GiB lab={total/2**30:.2f}GiB active={current/2**30:.2f}GiB", flush=True)
        return free >= 30*2**30 and target <= 30*2**30 and total <= 65*2**30 and current <= 20*2**30

    if not budget() or shutil.disk_usage(repo).free < 36*2**30: raise SystemExit("space reserve insufficient")
    active.mkdir()
    media = base / "media/lab"
    outputs = repo / "target/x86_64-pc-windows-msvc/release"
    if args.mode in ("rpc", "legacy"):
        shutil.copyfile(repo / "scripts/lab/audit-a02-stage.ps1", media / "lab-stage2.ps1")
        shutil.copyfile(repo / "scripts/lab/audit-a02-user.ps1", media / "audit-a02-user.ps1")
        for source, dest in ((outputs / "own-backup-service.exe", "own-backup-service-a02.exe"),
                             (outputs / "own-backup-cli.exe", "own-backup-cli.exe"),
                             (outputs / "examples/audit_delete_report.exe", "audit_delete_report.exe")):
            shutil.copyfile(source, media / dest)
        (media / "a02-mode.txt").write_text(args.mode + "\n")
        if args.mode == "legacy":
            shutil.copyfile(outputs / "examples/audit_a02_legacy_sidecar.exe", media / "audit_a02_legacy_sidecar.exe")
    elif args.mode == "sidecar":
        shutil.copyfile(repo / "scripts/lab/audit-a02-sidecar-stage.ps1", media / "lab-stage2.ps1")
        tests = list((outputs / "deps").glob("own_backup_backup-*.exe"))
        if len(tests) != 1: raise SystemExit("one Windows backup unit-test binary expected")
        shutil.copyfile(tests[0], media / "a02_chains_windows.exe")
    else:
        shutil.copyfile(repo / "scripts/lab/audit-a02-gui-probe-stage.ps1", media / "lab-stage2.ps1")
        user_script = "audit-a02-gui-action-user.ps1" if args.mode == "gui_action" else "audit-a02-gui-probe-user.ps1"
        shutil.copyfile(repo / f"scripts/lab/{user_script}", media / "audit-a02-gui-probe-user.ps1")
        shutil.copyfile(outputs / "own-backup-service.exe", media / "own-backup-service-a02.exe")
        shutil.copyfile(outputs / "own-backup-cli.exe", media / "own-backup-cli.exe")
        shutil.copyfile(lab / "media/WebView2Runtime-x64.exe", media / "WebView2Runtime-x64.exe")
        if not (media / "own-backup-gui.exe").is_file(): raise SystemExit("GUI binary missing from media")
    (active / "payload-hashes.json").write_text(json.dumps({p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in media.iterdir() if p.suffix in (".exe", ".ps1")}, indent=2)+"\n")
    for name in ("windows", "data"):
        subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(base / f"{name}.qcow2"), str(active / f"{name}.qcow2")], check=True)
    subprocess.run(["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB", "-o", str(active / "answer.iso"), str(base / "media")], check=True)
    spec = importlib.util.spec_from_file_location("monitor", repo / "scripts/lab/qemu-monitor.py")
    monitor = importlib.util.module_from_spec(spec); spec.loader.exec_module(monitor)
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096"]
    for index, name in ((0, "windows"), (1, "data")):
        command += ["-drive", f"file={active/(name+'.qcow2')},format=qcow2,if=ide,index={index},media=disk"]
    command += ["-drive", f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on", "-netdev", "user,id=n0,restrict=on", "-device", "e1000,netdev=n0", "-serial", f"file:{active/'serial.log'}", "-monitor", f"unix:{active/'monitor.sock'},server,nowait", "-display", "none", "-no-user-config", "-boot", "order=c"]
    (active / "command.json").write_text(json.dumps(command, indent=2)+"\n")
    with (active / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline, next_budget, offset = time.monotonic()+args.timeout, 0, 0
        try:
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_budget:
                    if not budget(): raise RuntimeError("space budget exceeded")
                    next_budget = now+30
                p = active / "serial.log"; serial = p.read_text(errors="replace") if p.exists() else ""
                if len(serial)>offset: print(serial[offset:], end="", flush=True); offset=len(serial)
                if now > deadline: raise RuntimeError("VM timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/"monitor.sock"), "system_powerdown")
                try: process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active/"monitor.sock"), "quit"); process.wait(timeout=10)
            print(f"HOST-VM-EXIT code={process.returncode}", flush=True)
    for name in ("windows", "data"):
        result = subprocess.run(["qemu-img", "check", str(active/f"{name}.qcow2")], capture_output=True, text=True)
        (active/f"{name}-check.log").write_text(result.stdout+result.stderr)
        if result.returncode: raise SystemExit("qcow2 check failed")
    serial = (active/"serial.log").read_text(errors="replace")
    marker = {"rpc":"A02-PASS", "legacy":"A02-PASS", "sidecar":"A02-SIDECAR-PASS", "gui_probe":"A02-GUI-READY", "gui_action":"A02-GUI-ACTION-PASS"}[args.mode]
    if marker not in serial or "A02-FAIL" in serial or "A02-SIDECAR-FAIL" in serial or "A02-GUI-FAIL" in serial or "A02-GUI-ACTION-FAIL" in serial:
        raise SystemExit("scenario failed; save evidence and clean before retry")


if __name__ == "__main__": main()
