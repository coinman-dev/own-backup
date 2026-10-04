#!/usr/bin/env python3
"""Build a product ISO through the production service and ordinary client."""
import hashlib
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

def host_budget(evidence):
    powershell=Path('/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe')
    distribution=os.environ.get('WSL_DISTRO_NAME','Ubuntu').replace("'","''")
    script=r'''$ErrorActionPreference="Stop"; [Console]::OutputEncoding=[Text.UTF8Encoding]::new(); Get-ChildItem HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss | ForEach-Object { $p=Get-ItemProperty $_.PSPath; if ($p.DistributionName -eq '__DISTRO__') { $base=[Environment]::ExpandEnvironmentVariables($p.BasePath); $root=[IO.Path]::GetPathRoot($base).Replace("\\?\",""); $d=[IO.DriveInfo]::new($root); [pscustomobject]@{distribution=$p.DistributionName;base_path=$base;volume=$root;free_bytes=$d.AvailableFreeSpace;total_bytes=$d.TotalSize} | ConvertTo-Json -Compress } }'''.replace('__DISTRO__',distribution)
    result=subprocess.run([str(powershell),'-NoProfile','-Command',script],capture_output=True,text=True,timeout=15,check=True)
    row=json.loads(result.stdout)
    row['utc']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime())
    with (evidence/'host-budget.jsonl').open('a') as log:log.write(json.dumps(row)+'\n')
    if row['free_bytes']<30*2**30:raise RuntimeError('physical Windows volume space budget exceeded')
    return row

def main():
    def interrupted(signum, frame):
        raise KeyboardInterrupt('stop requested')
    signal.signal(signal.SIGTERM,interrupted)
    repo=Path(__file__).resolve().parents[2];lab=Path.home()/"lab";active=lab/"active-h03-iso";evidence=active/"evidence"
    if any(p!=active for p in lab.glob("active-*")):raise SystemExit("preserve and clean another lab run first")
    if (active/"windows.qcow2").exists():raise SystemExit("preserve and clean the previous ISO build attempt first")
    if subprocess.run(["pgrep","-f","^qemu-system-"],capture_output=True).returncode==0:raise SystemExit("stop the other lab VM first")
    def used(p):
        result=subprocess.run(["du","-s","-B1",str(p)],text=True,capture_output=True)
        if result.returncode and not (result.returncode==1 and 'No such file or directory' in result.stderr):raise RuntimeError(result.stderr)
        return int(result.stdout.split()[0])
    def budget():
        host_budget(evidence)
        row={"utc":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),"free":shutil.disk_usage(repo).free,"lab":used(lab),"active":used(active),"target":used(repo/"target")}
        with (evidence/"budgets.jsonl").open("a") as f:f.write(json.dumps(row)+"\n")
        if row['free']<30*2**30 or row['lab']>65*2**30 or row['active']>20*2**30 or row['target']>30*2**30:raise RuntimeError("space budget exceeded")
    evidence.mkdir(parents=True,exist_ok=True)
    budget();payload=active/"payload/lab";payload.mkdir(parents=True)
    release=repo/"target/x86_64-pc-windows-msvc/release"
    for src,name in [(release/"own-backup-gui.exe","own-backup-gui.exe"),(release/"own-backup-cli.exe","own-backup-cli.exe"),(release/"own-backup-shell.exe","own-backup-shell.exe"),(release/"own-backup-service.exe","own-backup-service-iso.exe"),(repo/"scripts/lab/audit-h03-iso-stage.ps1","lab-stage2.ps1"),(repo/"scripts/lab/audit-h03-iso-user.ps1","audit-h03-iso-user.ps1"),(repo/"scripts/lab/audit-h03-iso-watch.ps1","audit-h03-iso-watch.ps1"),(lab/"media/WebView2Runtime-x64.exe","WebView2Runtime-x64.exe"),(lab/"resume-h02/media/lab/autounattend.xml","autounattend.xml")]:shutil.copyfile(src,payload/name)
    (evidence/"iso-build-payload-hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in payload.iterdir() if p.suffix!='.xml'},indent=2)+"\n")
    subprocess.run(["qemu-img","create","-f","qcow2","-F","qcow2","-b",str(lab/"resume-h02/windows.qcow2"),str(active/"windows.qcow2")],check=True)
    with (evidence/"iso-build-iso-build.log").open("w") as log:subprocess.run(["xorriso","-as","mkisofs","-quiet","-J","-R","-V","OBKLAB","-o",str(active/"answer.iso"),str(active/"payload")],stdout=log,stderr=log,check=True)
    spec=importlib.util.spec_from_file_location("monitor",repo/"scripts/lab/qemu-monitor.py");monitor=importlib.util.module_from_spec(spec);spec.loader.exec_module(monitor)
    command=["qemu-system-x86_64","-machine","pc,accel=kvm","-cpu","host","-smp","4","-m","4096","-drive",f"file={active/'windows.qcow2'},format=qcow2,if=ide,index=0,media=disk","-drive",f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on","-nic","none","-chardev",f"file,id=serial,path={evidence/'iso-build-serial.log'},append=on","-serial","chardev:serial","-monitor",f"unix:{active/'monitor.sock'},server,nowait","-display","none","-no-user-config","-no-reboot","-boot","order=c"]
    (evidence/"iso-build-command.json").write_text(json.dumps(command,indent=2)+"\n");(evidence/"iso-build-serial.log").write_text("");serial=""
    with (evidence/"iso-build-qemu.log").open("w") as log:
        process=subprocess.Popen(command,stdout=log,stderr=log);deadline=time.monotonic()+1800;next_budget=offset=0;boots=1
        try:
            while True:
                now=time.monotonic()
                if now>=next_budget:budget();next_budget=now+30
                serial=(evidence/"iso-build-serial.log").read_text(errors='replace')
                if len(serial)>offset:print(serial[offset:],end='',flush=True);offset=len(serial)
                if process.poll() is not None:
                    if boots==1 and 'H03-ISO-PREPARED REBOOT' in serial and 'FAIL' not in serial:process=subprocess.Popen(command,stdout=log,stderr=log);boots+=1;continue
                    break
                if now>deadline:raise RuntimeError('ISO build timeout')
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/'monitor.sock'),'system_powerdown')
                try:process.wait(timeout=45)
                except subprocess.TimeoutExpired:monitor.send(str(active/'monitor.sock'),'quit');process.wait(timeout=15)
            serial=(evidence/"iso-build-serial.log").read_text(errors='replace')
            passed='H03-ISO-BUILD-PASS' in serial and 'H03-ISO-BASE-PRESERVED' in serial and 'FAIL' not in serial
            (evidence/'iso-build-result.json').write_text(json.dumps({'boots':boots,'qemu_exit':process.returncode,'pass':passed},indent=2)+'\n')
            budget()
    with (evidence/'iso-build-container-check.log').open('w') as log:subprocess.run(['qemu-img','check',str(active/'windows.qcow2')],stdout=log,stderr=log,check=True)
    if not passed:raise SystemExit('ISO build FAIL; preserve and clean before retry')

if __name__=='__main__':main()
