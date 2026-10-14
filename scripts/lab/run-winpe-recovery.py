#!/usr/bin/env python3
"""Portable OBK recovery with the current CLI in an instrumented WinPE fixture.

The original accepted ISO is preserved. Requires the accepted autonomous guest
disks containing the synthetic encrypted archive and its portable recovery key.
"""
import argparse,hashlib,importlib.util,json,os,shutil,stat,subprocess,time
from pathlib import Path
REPO=Path(__file__).resolve().parents[2];WORK=REPO/'tmp/unattended-acceptance-2026-10-07';LAB=Path.home()/'lab';ACTIVE=WORK/'winpe-active';EVIDENCE=ACTIVE/'evidence'
def mod(name):
 p=REPO/'scripts/lab'/f'{name}.py';s=importlib.util.spec_from_file_location(name,p);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
def main():
 parser=argparse.ArgumentParser();parser.add_argument('mode',choices=['prepare','run']);args=parser.parse_args()
 if args.mode=='prepare':
  if ACTIVE.exists():raise SystemExit('previous WinPE attempt must be preserved first')
  EVIDENCE.mkdir(parents=True)
 else:
  if not (ACTIVE/'fixture.iso').is_file():raise SystemExit('prepare the fixture before running')
  EVIDENCE.mkdir(exist_ok=True)
  if (EVIDENCE/'result.json').exists():raise SystemExit('preserve completed evidence before retrying')
  if not (EVIDENCE/'payload-hashes.json').exists():raise SystemExit('fixture payload hashes missing')
 bounds=mod('run-h03-iso-build');monitor=mod('qemu-monitor')
 def budget():
  bounds.host_budget(EVIDENCE);row={'utc':time.time(),'free':shutil.disk_usage(REPO).free}
  for n,p in [('lab',LAB),('work',WORK),('active',ACTIVE),('target',REPO/'target')]:row[n]=int(subprocess.check_output(['du','-s','-B1',str(p)]).split()[0])
  with (EVIDENCE/'budgets.jsonl').open('a') as out:out.write(json.dumps(row)+'\n')
  if row['free']<30*2**30 or row['lab']+row['work']>65*2**30 or row['active']>20*2**30 or row['target']>30*2**30:raise RuntimeError('space budget exceeded')
 if args.mode=='prepare':
  budget();root=ACTIVE/'iso-root';root.mkdir();scratch=WORK/'scratch';scratch.mkdir(exist_ok=True);os.environ['TMPDIR']=str(scratch)
  original=LAB/'media/own-backup-h03-product.iso';original_hash=hashlib.sha256(original.read_bytes()).hexdigest()
  subprocess.run(['xorriso','-osirrox','on','-indev',str(original),'-extract','/',str(root)],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  wim=root/'sources/boot.wim'
  wim.chmod(wim.stat().st_mode | stat.S_IWUSR)
  shell=ACTIVE/'winpeshl.ini';shell.write_bytes(b'[LaunchApps]\r\nX:\\Windows\\System32\\cmd.exe, /c X:\\Windows\\System32\\acceptance-winpe.cmd\r\n')
  command=ACTIVE/'acceptance-winpe.cmd';command.write_bytes(b'@echo off\r\nwpeinit\r\nX:\\Windows\\System32\\audit_winpe_recovery.exe\r\n')
  release=REPO/'target/x86_64-pc-windows-msvc/release'
  stage=release/'examples/audit_winpe_recovery.exe';cli=release/'own-backup-cli.exe'
  updates=''.join(f'add "{file}" "{dest}"\n' for file,dest in [(shell,'Windows/System32/winpeshl.ini'),(command,'Windows/System32/acceptance-winpe.cmd'),(stage,'Windows/System32/audit_winpe_recovery.exe'),(cli,'Program Files/OwnBackup/own-backup-cli.exe')])
  subprocess.run(['wimlib-imagex','update',str(wim),'1'],input=updates,text=True,check=True,stdout=subprocess.DEVNULL)
  subprocess.run(['xorriso','-indev',str(original),'-outdev',str(ACTIVE/'fixture.iso'),'-boot_image','any','replay','-map',str(wim),'/sources/boot.wim','-commit'],check=True,stdout=subprocess.DEVNULL)
  subprocess.run(['qemu-img','create','-q','-f','qcow2',str(ACTIVE/'blank.qcow2'),'2G'],check=True)
  shutil.copyfile('/usr/share/OVMF/OVMF_VARS_4M.ms.fd',ACTIVE/'vars.fd')
  hashes={'original_product_iso':original_hash,'fixture_iso':hashlib.sha256((ACTIVE/'fixture.iso').read_bytes()).hexdigest(),'boot_wim_fixture':hashlib.sha256(wim.read_bytes()).hexdigest(),'instrumentation':hashlib.sha256(stage.read_bytes()).hexdigest(),'current_cli':hashlib.sha256(cli.read_bytes()).hexdigest()}
  (EVIDENCE/'payload-hashes.json').write_text(json.dumps(hashes,indent=2)+'\n')
  print('WINPE-PREPARED',flush=True);return
 if subprocess.run(['pgrep','-f','^qemu-system-'],capture_output=True).returncode==0:raise SystemExit('another VM is running')
 original=LAB/'media/own-backup-h03-product.iso'
 hashes=json.loads((EVIDENCE/'payload-hashes.json').read_text());original_hash=hashes['original_product_iso']
 if hashlib.sha256((ACTIVE/'fixture.iso').read_bytes()).hexdigest()!=hashes['fixture_iso']:raise SystemExit('fixture ISO differs from recorded payload')
 output=ACTIVE/'output.img';tools=LAB/'tools/usr/bin'
 if not output.exists():
  with output.open('wb') as out:out.truncate(32*2**20)
  subprocess.run([str(tools/'mformat'),'-i',str(output),'-v','PELOG','::'],check=True)
  marker=ACTIVE/'pe-output.marker';marker.write_text('Disposable WinPE evidence volume.\n')
  subprocess.run([str(tools/'mcopy'),'-i',str(output),str(marker),'::'],check=True)
 for name in ['windows','data']:
  child=ACTIVE/f'{name}.qcow2'
  if not child.exists():subprocess.run(['qemu-img','create','-q','-f','qcow2','-F','qcow2','-b',str(WORK/'active'/f'{name}.qcow2'),str(child)],check=True)
 cmd=['qemu-system-x86_64','-machine','q35,accel=kvm,smm=on','-cpu','host','-smp','4','-m','4096','-global','driver=cfi.pflash01,property=secure,value=on','-drive','if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.ms.fd','-drive',f'if=pflash,format=raw,unit=1,file={ACTIVE/"vars.fd"}']
 for name,index in [('windows',0),('data',1)]:cmd+=['-drive',f'file={ACTIVE/(name+".qcow2")},format=qcow2,if=ide,index={index},media=disk']
 cmd+=['-drive',f'file={ACTIVE/"blank.qcow2"},format=qcow2,if=ide,index=3,media=disk','-drive',f'file={ACTIVE/"fixture.iso"},format=raw,if=ide,index=2,media=cdrom,readonly=on','-device','qemu-xhci,id=xhci','-drive',f'file={output},format=raw,if=none,id=logs','-device','usb-storage,drive=logs,bus=xhci.0,removable=on','-netdev','user,id=n0,restrict=on','-device','e1000,netdev=n0','-chardev',f'file,id=serial,path={EVIDENCE/"serial.log"}', '-serial','chardev:serial','-monitor',f'unix:{ACTIVE/"p.sock"},server,nowait','-display','none','-no-user-config','-no-reboot','-boot','order=d']
 (EVIDENCE/'command.json').write_text(json.dumps(cmd,indent=2)+'\n');(EVIDENCE/'serial.log').touch();budget()
 deadline=time.monotonic()+900;next_budget=0;offset=0;failure=None
 with (EVIDENCE/'qemu.log').open('w') as log:
  p=subprocess.Popen(cmd,stdout=log,stderr=log)
  try:
   while p.poll() is None:
    if time.monotonic()>=next_budget:budget();next_budget=time.monotonic()+25
    content=(EVIDENCE/'serial.log').read_text(errors='replace');lines=content.split('\n')[:-1]
    for line in lines[offset:]:
     if line.startswith(('PE-START','PE-OK','PE-CLI','PE-RESULT')):print(line,flush=True)
    offset=len(lines)
    if time.monotonic()>deadline:raise RuntimeError('WinPE fixture timed out')
    time.sleep(1)
  except BaseException as e:failure=e
  finally:
   if p.poll() is None:
    try:monitor.send(str(ACTIVE/'p.sock'),'system_powerdown');p.wait(timeout=25)
    except Exception:p.kill();p.wait(timeout=15)
   budget()
 guest=EVIDENCE/'guest';guest.mkdir(exist_ok=True)
 extraction=subprocess.run([str(tools/'mcopy'),'-i',str(output),'::pe-*',str(guest)+'/'],capture_output=True)
 (EVIDENCE/'extract.log').write_bytes(extraction.stdout+extraction.stderr)
 content=(guest/'pe-recovery.log').read_text(errors='replace') if (guest/'pe-recovery.log').exists() else (EVIDENCE/'serial.log').read_text(errors='replace')
 unchanged=hashlib.sha256(original.read_bytes()).hexdigest()==original_hash
 print(content,flush=True);passed='PE-RESULT PASS' in content and 'PE-RESULT FAIL' not in content and failure is None and unchanged and p.returncode==0
 (EVIDENCE/'result.json').write_text(json.dumps({'pass':passed,'error':str(failure) if failure else None,'original_iso_unchanged':unchanged,'qemu_exit':p.returncode},indent=2)+'\n')
 with (EVIDENCE/'blank-check.log').open('w') as out:subprocess.run(['qemu-img','check',str(ACTIVE/'blank.qcow2')],check=True,stdout=out,stderr=out)
 if not passed:raise SystemExit('WinPE recovery acceptance failed; preserve evidence')
 print('WINPE-RECOVERY-PASS',flush=True)
if __name__=='__main__':main()
