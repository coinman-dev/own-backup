#!/usr/bin/env python3
"""Run the bounded H02 consolidation lab on fresh, disposable overlays.

The storage preparation authorized by the owner is a prerequisite. This
runner measures WSL/lab/target usage; it cannot measure an unmounted host.
After completion, archive the small logs before removing the active directory.
"""

import argparse
import hashlib
import importlib.util
import json
import shutil
import subprocess
import time
from pathlib import Path


def used(path):
    if not path.exists():
        return 0
    return int(subprocess.check_output(["du", "-s", "-B1", str(path)], text=True).split()[0])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--active", type=Path, default=Path.home() / "lab/active-h02-consolidation")
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument("--action", choices=("stop", "update"), default="stop")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    lab = Path.home() / "lab"
    base = lab / "resume-h02"
    active = args.active.resolve()
    if active.exists():
        raise SystemExit("active directory already exists; preserve and clean the previous run first")
    if any(lab.glob("active-*")):
        raise SystemExit("another active lab directory exists")

    def budget():
        free = shutil.disk_usage(repo).free
        target = used(repo / "target")
        total = used(lab)
        current = used(active)
        print(f"BUDGET free={free / 2**30:.2f}GiB target={target / 2**30:.2f}GiB "
              f"lab={total / 2**30:.2f}GiB active={current / 2**30:.2f}GiB", flush=True)
        return free >= 30 * 2**30 and target <= 30 * 2**30 and total <= 65 * 2**30 and current <= 20 * 2**30

    if not budget() or shutil.disk_usage(repo).free < 36 * 2**30:
        raise SystemExit("insufficient space for the estimated 6-GiB growth plus the reserve")
    active.mkdir()
    media = base / "media/lab"
    (media / "h02-consolidation.action").write_text(args.action + "\n")
    for part in ("stage", "user", "watch"):
        source = repo / f"scripts/lab/audit-h02-consolidation-{part}.ps1"
        name = "lab-stage2.ps1" if part == "stage" else source.name
        shutil.copyfile(source, media / name)
    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
              for p in sorted(media.iterdir()) if p.suffix in (".exe", ".ps1", ".action")}
    (active / "payload-hashes.json").write_text(json.dumps(hashes, indent=2) + "\n")
    for name in ("windows", "data"):
        subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b",
                        str(base / f"{name}.qcow2"), str(active / f"{name}.qcow2")], check=True)
    subprocess.run(["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB",
                    "-o", str(active / "answer.iso"), str(base / "media")], check=True)
    spec = importlib.util.spec_from_file_location("monitor", repo / "scripts/lab/qemu-monitor.py")
    monitor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(monitor)
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host",
               "-smp", "4", "-m", "4096"]
    for index, name in enumerate(("windows", "data")):
        command += ["-drive", f"file={active / (name + '.qcow2')},format=qcow2,if=ide,index={index},media=disk"]
    command += ["-drive", f"file={active / 'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
                "-netdev", "user,id=n0,restrict=on", "-device", "e1000,netdev=n0",
                "-serial", f"file:{active / 'serial.log'}",
                "-monitor", f"unix:{active / 'monitor.sock'},server,nowait",
                "-display", "none", "-no-user-config", "-boot", "order=c"]
    (active / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    with (active / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline = time.monotonic() + args.timeout
        next_budget = 0
        offset = 0
        applied = set()
        failed = False
        try:
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_budget:
                    if not budget():
                        failed = True
                        break
                    next_budget = now + 30
                serial_path = active / "serial.log"
                serial = serial_path.read_text(errors="replace") if serial_path.exists() else ""
                if len(serial) > offset:
                    print(serial[offset:], end="", flush=True)
                    offset = len(serial)
                for phase in ("stop", "kill"):
                    for event, throttle in ((f"H02-CONSOL-ARM phase={phase}", "0 67108864 4194304 0 0 0"),
                                            (f"H02-CONSOL-STOPPED phase={phase}", "0 0 0 0 0 0")):
                        if event in serial and event not in applied:
                            reply = monitor.send(str(active / "monitor.sock"),
                                                 "block_set_io_throttle ide0-hd0 " + throttle)
                            with (active / "throttle.log").open("a") as f:
                                f.write(event + "\n" + reply + "\n")
                            print(f"HOST-THROTTLE event={event} values={throttle}", flush=True)
                            applied.add(event)
                if now > deadline:
                    print("HOST-TIMEOUT", flush=True)
                    failed = True
                    break
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active / "monitor.sock"), "system_powerdown")
                try:
                    process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active / "monitor.sock"), "quit")
                    process.wait(timeout=10)
            print(f"HOST-VM-EXIT code={process.returncode}", flush=True)
        if failed or process.returncode:
            raise SystemExit(1)
    serial = (active / "serial.log").read_text(errors="replace")
    if "H02-CONSOL-USER-PASS" not in serial:
        raise SystemExit("guest scenario did not pass; preserve logs before cleanup")
    for name in ("windows", "data"):
        checked = subprocess.run(["qemu-img", "check", str(active / f"{name}.qcow2")],
                                 capture_output=True, text=True)
        (active / f"{name}-check.log").write_text(checked.stdout + checked.stderr)
        if checked.returncode:
            raise SystemExit("container check failed")
    budget()


if __name__ == "__main__":
    main()
