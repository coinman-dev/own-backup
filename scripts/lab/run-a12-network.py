#!/usr/bin/env python3
"""A12 disposable Windows VM, using its own SMB share and DATA partitions."""
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
    repo=Path(__file__).resolve().parents[2];lab=Path.home()/"lab";base=lab/"resume-h02";active=lab/"active-a12"
    if any(lab.glob("active-*")): raise SystemExit("preserve and clean the existing active run first")
    def budget():
        free=shutil.disk_usage(repo).free;target=used(repo/"target");total=used(lab);current=used(active)
        print(f"BUDGET free={free/2**30:.2f}GiB target={target/2**30:.2f}GiB lab={total/2**30:.2f}GiB active={current/2**30:.2f}GiB",flush=True)
        return free >= 30*2**30 and target <= 30*2**30 and total <= 65*2**30 and current <= 20*2**30
    if not budget() or shutil.disk_usage(repo).free < 35*2**30: raise SystemExit("space reserve insufficient for the estimated 5-GiB peak")
    active.mkdir()
    media=base/"media/lab"
    for part in ("stage","user","watch","threads"):
        name=f"audit-a12-{part}.ps1"
        shutil.copyfile(repo/"scripts/lab"/name,media/("lab-stage2.ps1" if part=="stage" else name))
    shutil.copyfile(repo/"target/x86_64-pc-windows-msvc/release/own-backup-service.exe",media/"own-backup-service-a12.exe")
    (active/"payload-hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in media.iterdir() if p.suffix in (".exe",".ps1")},indent=2)+"\n")
    for name in ("windows","data"):
        subprocess.run(["qemu-img","create","-f","qcow2","-F","qcow2","-b",str(base/f"{name}.qcow2"),str(active/f"{name}.qcow2")],check=True)
    subprocess.run(["xorriso","-as","mkisofs","-quiet","-J","-R","-V","OBKLAB","-o",str(active/"answer.iso"),str(base/"media")],check=True)
    spec=importlib.util.spec_from_file_location("monitor",repo/"scripts/lab/qemu-monitor.py")
    monitor=importlib.util.module_from_spec(spec);spec.loader.exec_module(monitor)
    command=["qemu-system-x86_64","-machine","pc,accel=kvm","-cpu","host","-smp","4","-m","4096"]
    for index,name in ((0,"windows"),(1,"data")):
        command += ["-drive",f"file={active/(name+'.qcow2')},format=qcow2,if=ide,index={index},media=disk"]
    command += ["-drive",f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
        "-netdev","user,id=n0,restrict=on","-device","e1000,netdev=n0","-serial",f"file:{active/'serial.log'}",
        "-monitor",f"unix:{active/'monitor.sock'},server,nowait","-display","none","-no-user-config","-boot","order=c"]
    (active/"command.json").write_text(json.dumps(command,indent=2)+"\n")
    with (active/"qemu.log").open("w") as log:
        proc=subprocess.Popen(command,stdout=log,stderr=log)
        deadline=time.monotonic()+1500;next_budget=0;offset=0
        try:
            while proc.poll() is None:
                now=time.monotonic()
                if now >= next_budget:
                    if not budget():raise RuntimeError("space budget exceeded")
                    next_budget=now+30
                serial_path=active/"serial.log";serial=serial_path.read_text(errors="replace") if serial_path.exists() else ""
                if len(serial)>offset:print(serial[offset:],end="",flush=True);offset=len(serial)
                if now>deadline:raise RuntimeError("VM timeout")
                time.sleep(0.5)
        finally:
            if proc.poll() is None:
                monitor.send(str(active/"monitor.sock"),"system_powerdown")
                try:proc.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active/"monitor.sock"),"quit");proc.wait(timeout=10)
            print(f"HOST-VM-EXIT code={proc.returncode}",flush=True)
    for name in ("windows","data"):
        checked=subprocess.run(["qemu-img","check",str(active/f"{name}.qcow2")],capture_output=True,text=True)
        (active/f"{name}-check.log").write_text(checked.stdout+checked.stderr)
        if checked.returncode:raise SystemExit("qcow2 check failed")
    if "A12-USER-PASS" not in (active/"serial.log").read_text(errors="replace"):
        raise SystemExit("scenario did not pass; save evidence before cleaning")


if __name__=="__main__":main()
