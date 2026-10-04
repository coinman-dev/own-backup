#!/usr/bin/env python3
"""Boot an unchanged product ISO with read-only fixtures and one blank test disk.

Run prepare after extracting product.iso from the build VM. Run then leaves
the normal desktop available: open its console and run the fixture's run.cmd.
The probe checks the product environment before using production engine APIs.
"""
import argparse
import hashlib
import importlib.util
import json
import shutil
import signal
import subprocess
import time
from pathlib import Path


def module(name, path):
    spec=importlib.util.spec_from_file_location(name,path)
    result=importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def main():
    def interrupted(signum, frame):
        raise KeyboardInterrupt('stop requested')
    signal.signal(signal.SIGTERM,interrupted)
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode',choices=['prepare','run'])
    parser.add_argument('--diagnose-webview',action='store_true')
    args=parser.parse_args()
    repo=Path(__file__).resolve().parents[2]
    lab=Path.home()/'lab';active=lab/'active-h03-iso';evidence=active/'evidence'
    if any(p!=active for p in lab.glob('active-*')):raise SystemExit('preserve and clean other run first')
    if subprocess.run(['pgrep','-f','^qemu-system-|^qemu-storage-daemon'],capture_output=True).returncode==0:
        raise SystemExit('stop existing VM/export first')
    build=module('build',repo/'scripts/lab/run-h03-iso-build.py')
    monitor=module('monitor',repo/'scripts/lab/qemu-monitor.py')
    def budget():
        build.host_budget(evidence)
        row={'utc':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'free':shutil.disk_usage(repo).free}
        for name,path in [('lab',lab),('active',active),('target',repo/'target')]:
            row[name]=int(subprocess.check_output(['du','-s','-B1',str(path)],text=True).split()[0])
        with (evidence/'product-budgets.jsonl').open('a') as log:log.write(json.dumps(row)+'\n')
        if row['free']<30*2**30 or row['lab']>65*2**30 or row['active']>20*2**30 or row['target']>30*2**30:
            raise RuntimeError('space budget exceeded')
    budget()
    if args.mode=='prepare':
        tree=active/'product-fixture';tree.mkdir()
        probe=repo/'target/x86_64-pc-windows-msvc/release/examples/audit_h03_product_iso.exe'
        shutil.copyfile(probe,tree/'probe.exe')
        (tree/'h03-product-fixture.marker').write_text('Disposable product ISO acceptance. One blank 1 GiB QEMU target.\n')
        release=repo/'target/x86_64-pc-windows-msvc/release'
        names=['own-backup-gui.exe','own-backup-service.exe','own-backup-cli.exe','own-backup-shell.exe']
        (tree/'program-hashes.json').write_text(json.dumps({n:hashlib.sha256((release/n).read_bytes()).hexdigest() for n in names},indent=2)+'\n')
        (tree/'run.cmd').write_bytes(b'@echo off\r\n"%~dp0probe.exe" > X:\\h03-product-console.log 2>&1\r\nfor %%D in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%D:\\h03-product-output.marker copy /y X:\\h03-product-console.log %%D:\\ >nul\r\nping -n 20 127.0.0.1 >nul\r\nwpeutil shutdown\r\n')
        subprocess.run(['xorriso','-as','mkisofs','-quiet','-J','-R','-V','H03FIXTURE','-o',str(active/'product-fixture.iso'),str(tree)],check=True)
        if (active/'product-target.qcow2').exists():raise SystemExit('preserve previous product target first')
        subprocess.run(['qemu-img','create','-f','qcow2',str(active/'product-target.qcow2'),'1G'],check=True)
        with (active/'product-output.img').open('wb') as file:file.truncate(32*2**20)
        subprocess.run([str(lab/'tools/usr/bin/mformat'),'-i',str(active/'product-output.img'),'-v','H03LOG','::'],check=True)
        marker=active/'h03-product-output.marker';marker.write_text('Evidence only. Not a restore target.\n')
        subprocess.run([str(lab/'tools/usr/bin/mcopy'),'-i',str(active/'product-output.img'),str(marker),'::'],check=True)
        if args.diagnose_webview:
            flags=active/'ownbackup-webview2-args.txt'
            flags.write_text('--allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu --disable-gpu-compositing --disable-direct-composition --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired --no-first-run --enable-logging=file --v=1 --log-file=C:\\webview2.log\n')
            subprocess.run([str(lab/'tools/usr/bin/mcopy'),'-i',str(active/'product-output.img'),str(flags),'::'],check=True)
            shutil.copyfile(flags,evidence/'diagnostic-browser-args.txt')
        shutil.copyfile('/usr/share/OVMF/OVMF_VARS_4M.ms.fd',active/'product-vars.fd')
        hashes={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in [active/'product.iso',active/'product-fixture.iso',lab/'media/h03-archive-restore.iso',probe,Path('/usr/share/OVMF/OVMF_CODE_4M.ms.fd'),active/'product-vars.fd']}
        (evidence/'product-payload-hashes.json').write_text(json.dumps(hashes,indent=2)+'\n')
        sources=evidence/'product-sources';sources.mkdir()
        for src in [Path(__file__),repo/'crates/backup/examples/audit_h03_product_iso.rs',tree/'run.cmd',tree/'program-hashes.json']:
            shutil.copyfile(src,sources/src.name)
        budget();return
    command=['qemu-system-x86_64','-machine','q35,accel=kvm,smm=on','-cpu','host','-smp','4','-m','4096',
             '-global','driver=cfi.pflash01,property=secure,value=on',
             '-drive','if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.ms.fd',
             '-drive',f'if=pflash,format=raw,unit=1,file={active/"product-vars.fd"}',
             '-drive',f'file={active/"product-target.qcow2"},format=qcow2,if=none,id=target',
             '-device','ide-hd,drive=target,bus=ide.0',
             '-netdev','user,id=n0,restrict=on','-device','e1000,netdev=n0',
             '-device','qemu-xhci,id=xhci','-device','usb-tablet,bus=xhci.0',
             '-drive',f'file={active/"product-output.img"},format=raw,if=none,id=logs',
             '-device','usb-storage,drive=logs,bus=xhci.0,removable=on',
             '-chardev',f'file,id=serial,path={evidence/"product-serial.log"}', '-serial','chardev:serial',
             '-monitor',f'unix:{active/"product-monitor.sock"},server,nowait',
             '-qmp',f'unix:{active/"product-qmp.sock"},server,nowait',
             '-display','none','-no-user-config','-no-reboot']
    for number,name,path in [(1,'product',active/'product.iso'),(2,'fixture',active/'product-fixture.iso'),(3,'archive',lab/'media/h03-archive-restore.iso')]:
        command+=['-drive',f'file={path},format=raw,if=none,id={name},readonly=on','-device',f'ide-cd,drive={name},bus=ide.{number}'+(',bootindex=1' if number==1 else '')]
    (evidence/'product-command.json').write_text(json.dumps(command,indent=2)+'\n')
    serial='';offset=0;started=time.monotonic();next_budget=next_key=next_log=0;photographed=False
    with (evidence/'product-qemu.log').open('w') as log:
        process=subprocess.Popen(command,stdout=log,stderr=log)
        try:
            while process.poll() is None:
                now=time.monotonic()
                if now>=next_budget:budget();next_budget=now+30
                if now>=next_key and now<started+15:
                    monitor.send(str(active/'product-monitor.sock'),'sendkey ret');next_key=now+2
                serial=(evidence/'product-serial.log').read_text(errors='replace')
                if now>=next_log:
                    saved=subprocess.run([str(lab/'tools/usr/lib/7zip/7z'),'e','-so',str(active/'product-output.img'),'h03-product.log'],capture_output=True,timeout=10)
                    candidate=saved.stdout.decode(errors='replace')
                    if saved.returncode==0 and candidate.startswith('PRODUCT-ISO-BEGIN') and '\x00' not in candidate and 'PRODUCT-ISO-DONE' in candidate:
                        serial=candidate
                        (evidence/'product-guest.log').write_text(candidate)
                    next_log=now+3
                if len(serial)>offset:print(serial[offset:],end='',flush=True);offset=len(serial)
                if 'PRODUCT-ISO-DONE' in serial and not photographed:
                    subprocess.run(['python3',str(repo/'scripts/lab/qemu-screenshot.py'),str(active/'product-monitor.sock'),str(evidence/'product-result-screen.png')],check=True)
                    photographed=True
                if now>started+900:raise RuntimeError('product boot/acceptance timeout')
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active/'product-monitor.sock'),'system_powerdown')
                try:process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active/'product-monitor.sock'),'quit');process.wait(timeout=15)
            serial=(evidence/'product-serial.log').read_text(errors='replace')
            if (evidence/'product-guest.log').exists():serial=(evidence/'product-guest.log').read_text()
            passed='PRODUCT-ISO-PASS' in serial and 'PRODUCT-ISO-FAIL' not in serial
            (evidence/'product-result.json').write_text(json.dumps({'pass':passed,'qemu_exit':process.returncode,'unchanged_product_iso':True,'dense_tb_acceptance':False},indent=2)+'\n')
            budget()
    with (evidence/'product-container-check.log').open('w') as log:
        subprocess.run(['qemu-img','check',str(active/'product-target.qcow2')],stdout=log,stderr=log,check=True)
    if not passed:raise SystemExit('product ISO FAIL; preserve evidence before retry')


if __name__=='__main__':main()
