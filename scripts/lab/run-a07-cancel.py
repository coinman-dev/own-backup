#!/usr/bin/env python3
"""A07: real service cancellation of volume/disk preparation and readback."""
import hashlib
import importlib.util
import json
import shutil
import subprocess
import time
from pathlib import Path


def main():
    repo = Path(__file__).resolve().parents[2]
    lab = Path.home() / "lab"
    active = lab / "active-a07"
    evidence = active / "evidence"
    if any(p != active for p in lab.glob("active-*")):
        raise SystemExit("preserve and clean other active attempts first")
    if subprocess.run(["pgrep", "-f", "^qemu-system-"], capture_output=True).returncode == 0:
        raise SystemExit("stop existing VM first")
    if (active / "windows.qcow2").exists():
        raise SystemExit("preserve and clean previous A07 attempt first")
    evidence.mkdir(parents=True, exist_ok=True)

    def used(path):
        return int(subprocess.check_output(["du", "-s", "-B1", str(path)], text=True).split()[0])

    def budget():
        row = {"utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "free": shutil.disk_usage(repo).free, "lab": used(lab),
               "active": used(active), "target": used(repo / "target")}
        with (evidence / "budgets.jsonl").open("a") as log:
            log.write(json.dumps(row) + "\n")
        if row["free"] < 30*2**30 or row["lab"] > 65*2**30 or row["active"] > 20*2**30 or row["target"] > 30*2**30:
            raise RuntimeError("space budget exceeded")

    budget()
    payload = active / "payload/lab"
    payload.mkdir(parents=True)
    output = repo / "target/x86_64-pc-windows-msvc/release"
    for source, name in [(output / "own-backup-service.exe", "own-backup-service-a07.exe"),
                         (output / "examples/audit_a07_cancel.exe", "audit_a07_cancel.exe"),
                         (lab / "resume-h02/media/lab/autounattend.xml", "autounattend.xml")]:
        shutil.copyfile(source, payload / name)
    for name in ("stage", "user", "watch"):
        dest = "lab-stage2.ps1" if name == "stage" else f"audit-a07-{name}.ps1"
        shutil.copyfile(repo / f"scripts/lab/audit-a07-{name}.ps1", payload / dest)
    # Never include the lab autologon credential in evidence.
    (evidence / "payload-hashes.json").write_text(json.dumps({p.name: hashlib.sha256(p.read_bytes()).hexdigest()
        for p in payload.iterdir() if p.suffix != ".xml"}, indent=2) + "\n")
    subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b",
        str(lab / "resume-h02/windows.qcow2"), str(active / "windows.qcow2")], check=True)
    subprocess.run(["qemu-img", "create", "-f", "qcow2", str(active / "source.qcow2"), "2G"], check=True)
    with (evidence / "iso-build.log").open("w") as log:
        subprocess.run(["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB",
            "-o", str(active / "answer.iso"), str(active / "payload")], stdout=log, stderr=log, check=True)
    spec = importlib.util.spec_from_file_location("monitor", repo / "scripts/lab/qemu-monitor.py")
    monitor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(monitor)
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096",
        "-drive", f"file={active/'windows.qcow2'},format=qcow2,if=ide,index=0,media=disk",
        "-drive", f"file={active/'source.qcow2'},format=qcow2,if=ide,index=1,media=disk",
        "-drive", f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
        "-netdev", "user,id=n0,restrict=on", "-device", "e1000,netdev=n0",
        "-chardev", f"file,id=a07serial,path={evidence/'serial.log'},append=on", "-serial", "chardev:a07serial",
        "-monitor", f"unix:{active/'monitor.sock'},server,nowait", "-display", "none", "-no-user-config",
        "-no-reboot", "-boot", "order=c"]
    (evidence / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    (evidence / "serial.log").write_text("")
    serial = ""
    with (evidence / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline, next_budget, offset, boots = time.monotonic()+1200, 0, 0, 1
        try:
            while True:
                now = time.monotonic()
                if now >= next_budget:
                    budget(); next_budget = now+30
                serial = (evidence / "serial.log").read_text(errors="replace")
                if len(serial) > offset:
                    print(serial[offset:], end="", flush=True); offset = len(serial)
                if process.poll() is not None:
                    if boots == 1 and "A07-STAGE-READY REBOOT" in serial and "FAIL" not in serial:
                        process = subprocess.Popen(command, stdout=log, stderr=log)
                        boots += 1; continue
                    break
                if now > deadline:
                    raise RuntimeError("A07 timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active / "monitor.sock"), "system_powerdown")
                try:
                    process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active / "monitor.sock"), "quit"); process.wait(timeout=15)
            (evidence / "result.json").write_text(json.dumps({"qemu_exit": process.returncode,
                "boots": boots, "pass": "A07-PASS" in serial, "done": "A07-DONE" in serial}, indent=2)+"\n")
            budget()
    with (evidence / "container-check.log").open("w") as log:
        for name in ("windows", "source"):
            subprocess.run(["qemu-img", "check", str(active / f"{name}.qcow2")], stdout=log, stderr=log, check=True)
    if "A07-PASS" not in serial or any(x in serial for x in ("A07-STAGE-FAIL", "A07-USER-FAIL", "A07-WATCH-FAIL")):
        raise SystemExit("A07 FAIL; preserve evidence and clean before retry")


if __name__ == "__main__":
    main()
