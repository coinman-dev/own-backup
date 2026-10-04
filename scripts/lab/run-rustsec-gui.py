#!/usr/bin/env python3
"""Render the updated dependency stack in an isolated Windows desktop."""
import hashlib
import importlib.util
import json
import shutil
import subprocess
import time
from pathlib import Path

def main():
    repo=Path(__file__).resolve().parents[2];lab=Path.home()/"lab";active=lab/"active-rustsec";evidence=active/"evidence"
    if any(p!=active for p in lab.glob("active-*")):raise SystemExit("preserve and clean another lab run first")
    if (active/"windows.qcow2").exists():raise SystemExit("preserve and clean the previous GUI attempt first")
    if subprocess.run(["pgrep","-f","^qemu-system-"],capture_output=True).returncode==0:raise SystemExit("stop the other lab VM first")
    def used(p):
        result=subprocess.run(["du","-s","-B1",str(p)],text=True,capture_output=True)
        if result.returncode and not (result.returncode==1 and 'No such file or directory' in result.stderr):raise RuntimeError(result.stderr)
        return int(result.stdout.split()[0])
    def budget():
        row={"utc":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),"free":shutil.disk_usage(repo).free,"lab":used(lab),"active":used(active),"target":used(repo/"target")}
        with (evidence/"budgets.jsonl").open("a") as f:f.write(json.dumps(row)+"\n")
        if row['free']<30*2**30 or row['lab']>65*2**30 or row['active']>20*2**30 or row['target']>30*2**30:raise RuntimeError("space budget exceeded")
    budget();payload=active/"payload/lab";payload.mkdir(parents=True)
    release=repo/"target/x86_64-pc-windows-msvc/release"
    for src,name in [(release/"own-backup-gui.exe","own-backup-gui.exe"),(release/"own-backup-cli.exe","own-backup-cli.exe"),(release/"own-backup-service.exe","own-backup-service-rustsec.exe"),(repo/"scripts/lab/audit-rustsec-stage.ps1","lab-stage2.ps1"),(repo/"scripts/lab/audit-rustsec-user.ps1","audit-rustsec-user.ps1"),(repo/"scripts/lab/audit-rustsec-runtime.ps1","audit-rustsec-runtime.ps1"),(lab/"media/WebView2Runtime-x64.exe","WebView2Runtime-x64.exe"),(lab/"resume-h02/media/lab/autounattend.xml","autounattend.xml")]:shutil.copyfile(src,payload/name)
    (evidence/"gui-payload-hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in payload.iterdir() if p.suffix!='.xml'},indent=2)+"\n")
    subprocess.run(["qemu-img","create","-f","qcow2","-F","qcow2","-b",str(lab/"resume-h02/windows.qcow2"),str(active/"windows.qcow2")],check=True)
    with (evidence/"gui-iso-build.log").open("w") as log:subprocess.run(["xorriso","-as","mkisofs","-quiet","-J","-R","-V","OBKLAB","-o",str(active/"answer.iso"),str(active/"payload")],stdout=log,stderr=log,check=True)
    spec=importlib.util.spec_from_file_location("monitor",repo/"scripts/lab/qemu-monitor.py");monitor=importlib.util.module_from_spec(spec);spec.loader.exec_module(monitor)
    command=["qemu-system-x86_64","-machine","pc,accel=kvm","-cpu","host","-smp","4","-m","4096","-drive",f"file={active/'windows.qcow2'},format=qcow2,if=ide,index=0,media=disk","-drive",f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on","-nic","none","-chardev",f"file,id=serial,path={evidence/'gui-serial.log'},append=on","-serial","chardev:serial","-monitor",f"unix:{active/'monitor.sock'},server,nowait","-display","none","-no-user-config","-no-reboot","-boot","order=c"]
    (evidence/"gui-command.json").write_text(json.dumps(command,indent=2)+"\n");(evidence/"gui-serial.log").write_text("");serial=""
    with (evidence/"gui-qemu.log").open("w") as log:
        process=subprocess.Popen(command,stdout=log,stderr=log);deadline=time.monotonic()+900;next_budget=offset=0;boots=1
        try:
            while True:
                now=time.monotonic()
                if now>=next_budget:budget();next_budget=now+30
                serial=(evidence/"gui-serial.log").read_text(errors='replace')
                if len(serial)>offset:print(serial[offset:],end='',flush=True);offset=len(serial)
                if 'RUSTSEC-GUI-READY' in serial and not (evidence/'gui-screen.png').exists():subprocess.run(['python3',str(repo/'scripts/lab/qemu-screenshot.py'),str(active/'monitor.sock'),str(evidence/'gui-screen.png')],check=True)
                if 'RUSTSEC-MISSING-DIALOG-READY' in serial and not (evidence/'missing-screen.png').exists():subprocess.run(['python3',str(repo/'scripts/lab/qemu-screenshot.py'),str(active/'monitor.sock'),str(evidence/'missing-screen.png')],check=True)
                if process.poll() is not None:
                    if boots==1 and 'RUSTSEC-STAGE-READY REBOOT' in serial and 'FAIL' not in serial:process=subprocess.Popen(command,stdout=log,stderr=log);boots+=1;continue
                    break
                if now>deadline:raise RuntimeError('GUI timeout')
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/'monitor.sock'),'system_powerdown')
                try:process.wait(timeout=45)
                except subprocess.TimeoutExpired:monitor.send(str(active/'monitor.sock'),'quit');process.wait(timeout=15)
            budget();(evidence/'gui-result.json').write_text(json.dumps({'boots':boots,'qemu_exit':process.returncode,'pass':'RUSTSEC-GUI-PASS' in serial},indent=2)+'\n')
    with (evidence/'gui-container-check.log').open('w') as log:subprocess.run(['qemu-img','check',str(active/'windows.qcow2')],stdout=log,stderr=log,check=True)
    if 'RUSTSEC-GUI-PASS' not in serial or 'FAIL' in serial:raise SystemExit('GUI FAIL; preserve and clean before retry')

if __name__=='__main__':main()
