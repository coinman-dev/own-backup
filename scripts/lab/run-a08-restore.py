#!/usr/bin/env python3
"""A08: fresh VM overlays and one disposable 2-GiB target, no RAW extraction."""
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
    parser.add_argument("--mode", choices=("baseline", "fixed", "letter", "mutations"), default="fixed")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    lab = Path.home() / "lab"
    base = lab / "resume-h02"
    active = lab / "active-a08"
    if any(lab.glob("active-*")): raise SystemExit("preserve and clean the existing active run first")
    def budget():
        free = shutil.disk_usage(repo).free
        target, total, current = used(repo / "target"), used(lab), used(active)
        print(f"BUDGET free={free/2**30:.2f}GiB target={target/2**30:.2f}GiB lab={total/2**30:.2f}GiB active={current/2**30:.2f}GiB", flush=True)
        return free >= 30*2**30 and target <= 30*2**30 and total <= 65*2**30 and current <= 20*2**30
    if not budget() or shutil.disk_usage(repo).free < 38*2**30: raise SystemExit("space reserve insufficient for the estimated 8-GiB peak")
    service = repo / "target/x86_64-pc-windows-msvc/release/own-backup-service.exe"
    if args.mode == "mutations" and b"A09 lab gate waiting before file verification" not in service.read_bytes():
        raise SystemExit("build own-backup-service --features a09-lab-gate before mutations")
    active.mkdir()
    media = base / "media/lab"
    for part in ("stage", "user", "restart") + (("a09-watch",) if args.mode == "mutations" else ()):
        source = repo / (f"scripts/lab/audit-{part}.ps1" if part == "a09-watch" else f"scripts/lab/audit-a08-{part}.ps1")
        shutil.copyfile(source, media / ("lab-stage2.ps1" if part == "stage" else source.name))
    shutil.copyfile(repo / "target/x86_64-pc-windows-msvc/release/examples/audit_a08_restore_cancel.exe", media / "audit_a08_restore_cancel.exe")
    if args.mode in ("fixed", "letter", "mutations"):
        shutil.copyfile(service, media / "own-backup-service-h02.exe")
        tests = list((repo / "target/x86_64-pc-windows-msvc/release/deps").glob("file_restore_windows-*.exe"))
        if len(tests) != 1: raise SystemExit("one Windows file-restore test binary expected")
        shutil.copyfile(tests[0], media / "file_restore_windows.exe")
    if args.mode in ("letter", "mutations"):
        tests = list((repo / "target/x86_64-pc-windows-msvc/release/deps").glob("letters_windows-*.exe"))
        if len(tests) != 1: raise SystemExit("one Windows drive-letter test binary expected")
        shutil.copyfile(tests[0], media / "letters_windows.exe")
    (media / "a08-mode.txt").write_text(args.mode + "\n")
    (active / "payload-hashes.json").write_text(json.dumps({p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in media.iterdir() if p.suffix in (".exe", ".ps1", ".txt")}, indent=2)+"\n")
    for name in ("windows", "data"):
        subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(base / f"{name}.qcow2"), str(active / f"{name}.qcow2")], check=True)
    subprocess.run(["qemu-img", "create", "-f", "qcow2", str(active / "target.qcow2"), "2G"], check=True)
    subprocess.run(["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB", "-o", str(active / "answer.iso"), str(base / "media")], check=True)
    spec = importlib.util.spec_from_file_location("monitor", repo / "scripts/lab/qemu-monitor.py")
    monitor = importlib.util.module_from_spec(spec); spec.loader.exec_module(monitor)
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096"]
    for index, name in ((0, "windows"), (1, "data"), (3, "target")):
        command += ["-drive", f"file={active/(name+'.qcow2')},format=qcow2,if=ide,index={index},media=disk"]
    command += ["-drive", f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on", "-netdev", "user,id=n0,restrict=on", "-device", "e1000,netdev=n0", "-serial", f"file:{active/'serial.log'}", "-monitor", f"unix:{active/'monitor.sock'},server,nowait", "-display", "none", "-no-user-config", "-boot", "order=c"]
    (active / "command.json").write_text(json.dumps(command, indent=2)+"\n")
    with (active / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline, next_budget, offset, throttled = time.monotonic()+args.timeout, 0, 0, False
        try:
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_budget:
                    if not budget(): raise RuntimeError("space budget exceeded")
                    next_budget = now+30
                p = active / "serial.log";serial = p.read_text(errors="replace") if p.exists() else ""
                if len(serial)>offset: print(serial[offset:], end="", flush=True);offset=len(serial)
                if "A08-STAGE-READY REBOOT" in serial and not throttled:
                    reply = monitor.send(str(active/"monitor.sock"), "block_set_io_throttle ide1-hd1 0 16777216 0 0 0 0")
                    (active/"throttle.log").write_text(reply);throttled=True
                    print("HOST-THROTTLE target_read=16777216B/s", flush=True)
                if now > deadline: raise RuntimeError("VM timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/"monitor.sock"), "system_powerdown")
                try: process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active/"monitor.sock"), "quit");process.wait(timeout=10)
            print(f"HOST-VM-EXIT code={process.returncode}", flush=True)
    for name in ("windows", "data", "target"):
        result = subprocess.run(["qemu-img", "check", str(active/f"{name}.qcow2")], capture_output=True, text=True)
        (active/f"{name}-check.log").write_text(result.stdout+result.stderr)
        if result.returncode: raise SystemExit("qcow2 check failed")
    serial = (active/"serial.log").read_text(errors="replace")
    if "A08-USER-PASS" not in serial: raise SystemExit("scenario failed; save evidence and clean before retry")


if __name__ == "__main__": main()
