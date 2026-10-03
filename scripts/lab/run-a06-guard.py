#!/usr/bin/env python3
"""A06 isolated phases: BIOS conversion, UEFI PE setup, live guards and PE restore."""
import argparse
import hashlib
import importlib.util
import json
import re
import shutil
import subprocess
import time
from pathlib import Path


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode",choices=("convert","prepare-pe","setup","windows","pe-restore"))
    args=parser.parse_args()
    repo=Path(__file__).resolve().parents[2];lab=Path.home()/"lab";active=lab/"active-a06";evidence=active/"evidence"
    if any(p!=active for p in lab.glob("active-*")): raise SystemExit("preserve and clean other active run first")
    if subprocess.run(["pgrep","-f","^qemu-system-"],capture_output=True).returncode==0: raise SystemExit("stop the existing VM first")
    evidence.mkdir(parents=True,exist_ok=True)
    def used(p): return int(subprocess.check_output(["du","-s","-B1",str(p)],text=True).split()[0])
    def budget():
        row={"utc":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),"free":shutil.disk_usage(repo).free,"lab":used(lab),"active":used(active),"target":used(repo/"target")}
        with (evidence/"budgets.jsonl").open("a") as f:f.write(json.dumps(row)+"\n")
        if row["free"]<30*2**30 or row["lab"]>65*2**30 or row["active"]>20*2**30 or row["target"]>30*2**30:raise RuntimeError("space budget exceeded")
    def command(cmd,name,stdin=None):
        budget()
        with (evidence/name).open("w") as log:
            p=subprocess.Popen(cmd,stdin=stdin,stdout=log,stderr=log)
            try:
                while True:
                    try:code=p.wait(timeout=30);break
                    except subprocess.TimeoutExpired:budget()
            finally:
                if p.poll() is None:p.terminate();p.wait(timeout=15)
            if code:raise RuntimeError(f"command failed, see {name}")
    output=repo/"target/x86_64-pc-windows-msvc/release"
    if args.mode=="convert":
        if (active/"windows.qcow2").exists():raise SystemExit("conversion fixture already exists")
        payload=active/"payload/lab";payload.mkdir(parents=True)
        shutil.copyfile(repo/"scripts/lab/audit-a06-convert-stage.ps1",payload/"lab-stage2.ps1")
        command(["qemu-img","create","-f","qcow2","-F","qcow2","-b",str(lab/"resume-h02/windows.qcow2"),str(active/"windows.qcow2")],"create-windows.log")
        command(["qemu-img","create","-f","qcow2",str(active/"spare.qcow2"),"256M"],"create-spare.log")
        raw=active/"esp.img"
        with raw.open("wb") as f:f.truncate(512*2**20)
        command(["sgdisk","-o","-n","1:2048:1046527","-t","1:ef00","-c","1:A06ESP",str(raw)],"esp-gpt.log")
        mformat=shutil.which("mformat") or str(lab/"tools/usr/bin/mformat")
        command([mformat,"-i",str(raw)+"@@1048576","-F","-T","1044480","-v","A06ESP","::"],"esp-format.log")
        text=subprocess.check_output(["sgdisk","-p",str(raw)],text=True)
        esp_guid=re.search(r"Disk identifier \(GUID\): ([A-F0-9-]+)",text).group(1)
        (evidence/"esp-layout.txt").write_text(text)
        (active/"a06-auth.json").write_text(json.dumps({"esp_guid":esp_guid},indent=2)+"\n")
        command(["qemu-img","convert","-f","raw","-O","qcow2",str(raw),str(active/"esp.qcow2")],"esp-convert.log")
        raw.unlink()
        shutil.copyfile("/usr/share/OVMF/OVMF_VARS_4M.fd",active/"OVMF_VARS.fd")
        command(["xorriso","-as","mkisofs","-quiet","-J","-R","-V","OBKLAB","-o",str(active/"answer.iso"),str(active/"payload")],"answer-iso.log")
    elif args.mode=="prepare-pe":
        serial=(evidence/"convert-serial.log").read_text(errors="replace")
        match=re.search(r"A06-CONVERT-RESULT (\{[^\r\n]+\})",serial)
        if not match or "A06-CONVERT-PASS" not in serial:raise SystemExit("successful MBR2GPT result required")
        report=json.loads(match.group(1));auth=json.loads((active/"a06-auth.json").read_text());auth["windows_guid"]=report["windows_guid"].strip("{}").upper()
        (active/"a06-auth.json").write_text(json.dumps(auth,indent=2)+"\n")
        original=lab/"media/winpe-h03-restore.iso";tree=active/"pe-tree"
        if not tree.exists():command(["xorriso","-osirrox","on","-indev",str(original),"-extract","/",str(tree)],"pe-extract.log")
        wim=tree/"sources/boot.wim";ini=active/"winpeshl.ini"
        ini.write_bytes(b"[LaunchApps]\r\n%SYSTEMROOT%\\System32\\audit_a06_guard.exe, setup-pe\r\n%SYSTEMROOT%\\System32\\cmd.exe\r\n")
        commands=active/"wim-update.txt";commands.write_text(f'add "{output/"examples/audit_a06_guard.exe"}" /Windows/System32/audit_a06_guard.exe\nadd "{ini}" /Windows/System32/winpeshl.ini\n')
        with commands.open("rb") as f:command(["wimlib-imagex","update",str(wim),"2"],"wim-update.log",f)
        shutil.copyfile(active/"a06-auth.json",tree/"a06-auth.json")
        builder=shutil.which("genisoimage") or str(lab/"tools/usr/bin/genisoimage")
        (active/"winpe.iso").unlink(missing_ok=True)
        command([builder,"-quiet","-udf","-J","-joliet-long","-R","-V","OBKA06PE","-b","boot/etfsboot.com","-no-emul-boot","-boot-load-size","8","-eltorito-alt-boot","-e","efi/microsoft/boot/efisys.bin","-no-emul-boot","-o",str(active/"winpe.iso"),str(tree)],"pe-iso.log")
        print("prepared isolated UEFI PE",flush=True);return
    elif args.mode=="windows":
        payload=active/"payload/lab"
        for source,name in [(repo/"scripts/lab/audit-a06-stage.ps1","lab-stage2.ps1"),(repo/"scripts/lab/audit-a06-user.ps1","audit-a06-user.ps1"),(repo/"scripts/lab/audit-a06-watch.ps1","audit-a06-watch.ps1"),(output/"own-backup-cli.exe","own-backup-cli.exe"),(output/"own-backup-service.exe","own-backup-service-a06.exe"),(output/"examples/audit_a06_guard.exe","audit_a06_guard.exe"),(output/"examples/audit_a06_media_override.exe","audit_a06_media_override.exe"),(lab/"resume-h02/media/lab/autounattend.xml","autounattend.xml")]:shutil.copyfile(source,payload/name)
        if (active/"audit_a06_before.exe").exists():shutil.copyfile(active/"audit_a06_before.exe",payload/"audit_a06_before.exe")
        shutil.copyfile(active/"a06-auth.json",active/"payload/a06-auth.json")
        (evidence/"payload-hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in payload.iterdir() if p.suffix!=".xml"},indent=2)+"\n")
        (active/"answer.iso").unlink()
        command(["xorriso","-as","mkisofs","-quiet","-J","-R","-V","OBKLAB","-o",str(active/"answer.iso"),str(active/"payload")],"answer-iso.log")
    elif args.mode=="pe-restore":
        serial=(evidence/"windows-serial.log").read_text(errors="replace")
        if "A06-PASS" not in serial or any(word in serial for word in ["A06-STAGE-FAIL","A06-WATCH-FAIL","A06-USER-FAIL"]):
            raise SystemExit("successful live matrix and reboot proof required before destructive PE phase")
        # Preserve the tested Windows varstore; a fresh disposable one forces
        # the final PE CD instead of the persistent Windows Boot Manager entry.
        if not (evidence/"nvram-after-windows.fd").exists():shutil.copyfile(active/"OVMF_VARS.fd",evidence/"nvram-after-windows.fd")
        shutil.copyfile("/usr/share/OVMF/OVMF_VARS_4M.fd",active/"OVMF_VARS.fd")
        tree=active/"pe-tree";wim=tree/"sources/boot.wim";ini=active/"winpeshl.ini"
        ini.write_bytes(b"[LaunchApps]\r\n%SYSTEMROOT%\\System32\\audit_a06_guard.exe, pe-restore\r\n%SYSTEMROOT%\\System32\\cmd.exe\r\n")
        commands=active/"wim-update.txt";commands.write_text(f'add "{output/"examples/audit_a06_guard.exe"}" /Windows/System32/audit_a06_guard.exe\nadd "{ini}" /Windows/System32/winpeshl.ini\n')
        with commands.open("rb") as f:command(["wimlib-imagex","update",str(wim),"2"],"pe-final-wim-update.log",f)
        builder=shutil.which("genisoimage") or str(lab/"tools/usr/bin/genisoimage")
        (active/"winpe.iso").unlink()
        command([builder,"-quiet","-udf","-J","-joliet-long","-R","-V","OBKA06PE","-b","boot/etfsboot.com","-no-emul-boot","-boot-load-size","8","-eltorito-alt-boot","-e","efi/microsoft/boot/efisys.bin","-no-emul-boot","-o",str(active/"winpe.iso"),str(tree)],"pe-final-iso.log")
    spec=importlib.util.spec_from_file_location("monitor",repo/"scripts/lab/qemu-monitor.py");monitor=importlib.util.module_from_spec(spec);spec.loader.exec_module(monitor)
    mode=args.mode;command_line=["qemu-system-x86_64","-machine","pc,accel=kvm","-cpu","host","-smp","4","-m","4096"]
    if mode=="windows":command_line += ["-no-reboot"]
    if mode!="convert":command_line += ["-drive","if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd","-drive",f"if=pflash,format=raw,file={active/'OVMF_VARS.fd'}"]
    for index,name in [(0,"windows"),(1,"esp"),(3,"spare")]:command_line += ["-drive",f"file={active/(name+'.qcow2')},format=qcow2,if=ide,index={index},media=disk"]
    iso=active/("winpe.iso" if mode in ("setup","pe-restore") else "answer.iso")
    command_line += ["-drive",f"file={iso},format=raw,if=ide,index=2,media=cdrom,readonly=on","-netdev","user,id=n0,restrict=on","-device","e1000,netdev=n0","-chardev",f"file,id=a06serial,path={evidence/(mode+'-serial.log')},append=on","-serial","chardev:a06serial","-monitor",f"unix:{active/'monitor.sock'},server,nowait","-display","none","-no-user-config","-boot","order="+("d" if mode in ("setup","pe-restore") else "c")]
    (evidence/(mode+"-serial.log")).write_bytes(b"")
    (evidence/(mode+"-command.json")).write_text(json.dumps(command_line,indent=2)+"\n")
    budget()
    with (evidence/(mode+"-qemu.log")).open("w") as log:
        process=subprocess.Popen(command_line,stdout=log,stderr=log);started=time.monotonic();deadline=started+1200;next_budget=0;next_key=started+1;offset=0;serial="";boots=1;segment=0
        try:
            while True:
                now=time.monotonic()
                if now>=next_budget:budget();next_budget=now+30
                if mode in ("setup","pe-restore") and now>=next_key and now<started+12:monitor.send(str(active/"monitor.sock"),"sendkey ret");next_key=now+2
                path=evidence/(mode+"-serial.log");serial=path.read_text(errors="replace") if path.exists() else ""
                if len(serial)>offset:print(serial[offset:],end="",flush=True);offset=len(serial)
                if process.poll() is not None:
                    last=serial[segment:]
                    reboot=("A06-CLI-PASS" in last and "REBOOT" in last) or "A06-MATRIX-PASS REBOOT-PROOF-NEXT" in last or "A06-LOGIN-RESET REBOOT" in last
                    if mode=="windows" and reboot and "A06-PASS" not in last and boots<4:
                        segment=len(serial);boots+=1;budget()
                        print(f"HOST cold boot {boots} after guest-requested reboot",flush=True)
                        process=subprocess.Popen(command_line,stdout=log,stderr=log)
                        continue
                    break
                if now>deadline:raise RuntimeError(f"{mode} timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/"monitor.sock"),"system_powerdown")
                try:process.wait(timeout=45)
                except subprocess.TimeoutExpired:monitor.send(str(active/"monitor.sock"),"quit");process.wait(timeout=15)
            (evidence/(mode+"-result.json")).write_text(json.dumps({"exit":process.returncode,"serial_pass":("A06-CONVERT-PASS" if mode=="convert" else "A06-PASS") in serial},indent=2)+"\n")
            budget()
    for name in ["windows","esp","spare"]:command(["qemu-img","check",str(active/(name+".qcow2"))],mode+"-"+name+"-check.log")
    if mode=="convert" and "A06-CONVERT-PASS" not in serial:raise SystemExit("conversion FAIL; preserve evidence before retry")
    if mode=="windows" and ("A06-PASS" not in serial or any(w in serial for w in ["A06-STAGE-FAIL","A06-WATCH-FAIL","A06-USER-FAIL"])):raise SystemExit("live matrix FAIL; preserve evidence before retry")


if __name__=="__main__":main()
