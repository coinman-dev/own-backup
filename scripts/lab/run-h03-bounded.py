#!/usr/bin/env python3
"""H03 bounded index/growth and NTFS checks; never claims dense TB acceptance."""
import hashlib
import argparse
import importlib.util
import json
import shutil
import subprocess
import time
from pathlib import Path


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--filesystem-only",action="store_true",help="check NTFS without repeating accepted index measurements")
    args=parser.parse_args()
    repo=Path(__file__).resolve().parents[2]; lab=Path.home()/"lab"; active=lab/"active-h03"; evidence=active/"evidence"
    if any(p!=active for p in lab.glob("active-*")): raise SystemExit("preserve and clean other active run first")
    if subprocess.run(["pgrep","-f","^qemu-system-"],capture_output=True).returncode==0: raise SystemExit("stop existing VM first")
    if (active/"windows.qcow2").exists(): raise SystemExit("preserve and clean previous H03 attempt first")
    evidence.mkdir(parents=True,exist_ok=True)
    def used(p): return int(subprocess.check_output(["du","-s","-B1",str(p)],text=True).split()[0])
    def budget():
        row={"utc":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),"free":shutil.disk_usage(repo).free,"lab":used(lab),"active":used(active),"target":used(repo/"target")}
        with (evidence/"budgets.jsonl").open("a") as f:f.write(json.dumps(row)+"\n")
        if row["free"]<30*2**30 or row["lab"]>65*2**30 or row["active"]>20*2**30 or row["target"]>30*2**30:raise RuntimeError("space budget exceeded")
    budget()
    payload=active/"payload/lab";payload.mkdir(parents=True)
    release=repo/"target/x86_64-pc-windows-msvc/release/examples"
    for src,name in [(release/"audit_h03_index_growth.exe","audit_h03_index_growth.exe"),(release/"obk_live_probe.exe","obk_live_probe.exe"),(repo/"scripts/lab/audit-h03-bounded-stage.ps1","lab-stage2.ps1"),(evidence/"expected-files.json","expected-h03-files.json")]:shutil.copyfile(src,payload/name)
    (payload/"h03-bounded-mode.txt").write_text("filesystem-only" if args.filesystem_only else "full")
    (evidence/"payload-hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in payload.iterdir()},indent=2)+"\n")
    subprocess.run(["qemu-img","create","-f","qcow2","-F","qcow2","-b",str(lab/"resume-h02/windows.qcow2"),str(active/"windows.qcow2")],check=True)
    subprocess.run(["qemu-img","create","-f","qcow2",str(active/"target.qcow2"),"1G"],check=True)
    with (evidence/"iso-build.log").open("w") as log:subprocess.run(["xorriso","-as","mkisofs","-quiet","-J","-R","-V","OBKLAB","-o",str(active/"answer.iso"),str(active/"payload")],stdout=log,stderr=log,check=True)
    spec=importlib.util.spec_from_file_location("monitor",repo/"scripts/lab/qemu-monitor.py");monitor=importlib.util.module_from_spec(spec);spec.loader.exec_module(monitor)
    command=["qemu-system-x86_64","-machine","pc,accel=kvm","-cpu","host","-smp","4","-m","4096",
        "-drive",f"file={active/'windows.qcow2'},format=qcow2,if=ide,index=0,media=disk",
        "-drive",f"file={active/'target.qcow2'},format=qcow2,if=ide,index=1,media=disk",
        "-drive",f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
        "-drive",f"file={lab/'media/h03-archive-restore.iso'},format=raw,if=ide,index=3,media=cdrom,readonly=on",
        "-netdev","user,id=n0,restrict=on","-device","e1000,netdev=n0",
        "-chardev",f"file,id=h03serial,path={evidence/'serial.log'},append=on","-serial","chardev:h03serial",
        "-monitor",f"unix:{active/'monitor.sock'},server,nowait","-display","none","-no-user-config","-no-reboot","-boot","order=c"]
    (evidence/"command.json").write_text(json.dumps(command,indent=2)+"\n");(evidence/"serial.log").write_text("")
    serial=""
    with (evidence/"qemu.log").open("w") as log:
        process=subprocess.Popen(command,stdout=log,stderr=log);deadline=time.monotonic()+1200;next_budget=offset=0;boots=1
        try:
            while True:
                now=time.monotonic()
                if now>=next_budget:budget();next_budget=now+30
                serial=(evidence/"serial.log").read_text(errors="replace")
                if len(serial)>offset:print(serial[offset:],end="",flush=True);offset=len(serial)
                if process.poll() is not None:
                    if boots==1 and "H03-BOUNDED-REBOOT" in serial and "H03-BOUNDED-FAIL" not in serial:
                        process=subprocess.Popen(command,stdout=log,stderr=log);boots+=1;continue
                    break
                if now>deadline:raise RuntimeError("H03 timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/"monitor.sock"),"system_powerdown")
                try:process.wait(timeout=45)
                except subprocess.TimeoutExpired:monitor.send(str(active/"monitor.sock"),"quit");process.wait(timeout=15)
            budget();(evidence/"result.json").write_text(json.dumps({"qemu_exit":process.returncode,"boots":boots,"pass":"H03-BOUNDED-PASS" in serial,"filesystem_only":args.filesystem_only,"dense_tb_acceptance":False},indent=2)+"\n")
    with (evidence/"container-check.log").open("w") as log:
        for name in ["windows","target"]:subprocess.run(["qemu-img","check",str(active/(name+".qcow2"))],stdout=log,stderr=log,check=True)
    if "H03-BOUNDED-PASS" not in serial or "H03-BOUNDED-FAIL" in serial:raise SystemExit("H03 FAIL; preserve and clean before retry")


if __name__=="__main__":main()
