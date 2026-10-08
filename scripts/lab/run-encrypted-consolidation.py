#!/usr/bin/env python3
"""Isolated encrypted-chain consolidation on overlays of the accepted stand."""
import base64,hashlib,importlib.util,json,os,shutil,subprocess,time
from pathlib import Path
REPO=Path(__file__).resolve().parents[2];WORK=REPO/'tmp/unattended-acceptance-2026-10-07';LAB=Path.home()/'lab';ACTIVE=WORK/'consolidation-active';EVIDENCE=ACTIVE/'evidence';SCRIPTS=REPO/'scripts/lab'
def module(name):
 spec=importlib.util.spec_from_file_location(name,SCRIPTS/f'{name}.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m
def main():
 if ACTIVE.exists():raise SystemExit('preserve the previous attempt before rerun')
 if subprocess.run(['pgrep','-f','^qemu-system-'],capture_output=True).returncode==0:raise SystemExit('another VM is running')
 EVIDENCE.mkdir(parents=True);(EVIDENCE/'reports').mkdir()
 bounds=module('run-h03-iso-build');monitor=module('qemu-monitor')
 def budget():
  bounds.host_budget(EVIDENCE)
  row={'utc':time.time(),'free':shutil.disk_usage(REPO).free}
  for name,path in [('lab',LAB),('work',WORK),('active',ACTIVE),('target',REPO/'target')]:row[name]=int(subprocess.check_output(['du','-s','-B1',str(path)]).split()[0])
  with (EVIDENCE/'budgets.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
  if row['free']<30*2**30 or row['lab']+row['work']>65*2**30 or row['active']>20*2**30 or row['target']>30*2**30:raise RuntimeError('space budget exceeded')
 budget()
 source=WORK/'active';media=ACTIVE/'payload/lab';media.mkdir(parents=True)
 for src,name in [(SCRIPTS/'acceptance-consolidation-stage.ps1','lab-stage2.ps1'),(SCRIPTS/'acceptance-consolidation-user.ps1','acceptance-consolidation-user.ps1'),(SCRIPTS/'acceptance-common.ps1','acceptance-common.ps1'),(SCRIPTS/'autounattend.xml','autounattend.xml'),(REPO/'target/x86_64-pc-windows-msvc/release/examples/audit_autonomous.exe','audit_autonomous.exe')]:shutil.copyfile(src,media/name)
 (EVIDENCE/'payload-hashes.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in media.iterdir()},indent=2)+'\n')
 for disk in ['windows','data']:
  subprocess.run(['qemu-img','create','-q','-f','qcow2','-F','qcow2','-b',str(source/f'{disk}.qcow2'),str(ACTIVE/f'{disk}.qcow2')],check=True)
 subprocess.run(['xorriso','-as','mkisofs','-quiet','-J','-R','-V','OBKLAB','-o',str(ACTIVE/'answer.iso'),str(ACTIVE/'payload')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 cmd=['qemu-system-x86_64','-machine','pc,accel=kvm','-cpu','host','-smp','4','-m','4096']
 for i,disk in enumerate(['windows','data']):cmd+=['-drive',f'file={ACTIVE/(disk+".qcow2")},format=qcow2,if=ide,index={i},media=disk']
 cmd+=['-drive',f'file={ACTIVE/"answer.iso"},format=raw,if=ide,index=2,media=cdrom,readonly=on','-netdev','user,id=n0,restrict=on','-device','e1000,netdev=n0','-chardev',f'file,id=serial,path={EVIDENCE/"serial.log"},append=on','-serial','chardev:serial','-monitor',f'unix:{ACTIVE/"m.sock"},server,nowait','-display','none','-no-user-config','-no-reboot','-boot','order=c']
 (EVIDENCE/'command.json').write_text(json.dumps(cmd,indent=2)+'\n');(EVIDENCE/'serial.log').touch()
 offset=0;boots=0;next_budget=0;deadline=time.monotonic()+1500;done=False;failed=None
 with (EVIDENCE/'qemu.log').open('w') as log:
  p=None
  try:
   while True:
    if p is None:(ACTIVE/'m.sock').unlink(missing_ok=True);p=subprocess.Popen(cmd,stdout=log,stderr=log);boots+=1;print('HOST-BOOT',boots,flush=True)
    if time.monotonic()>=next_budget:budget();next_budget=time.monotonic()+25
    lines=(EVIDENCE/'serial.log').read_text(errors='replace').split('\n')[:-1]
    for line in lines[offset:]:
     line=line.strip()
     if line.startswith('ACCEPT-REPORT '):
      _,name,encoded=line.split(' ',2)
      if Path(name).name!=name:raise RuntimeError('unsafe report name')
      (EVIDENCE/'reports'/f'{name}.json').write_bytes(base64.b64decode(encoded,validate=True));print('HOST-REPORT',name,flush=True)
     else:
      print(line,flush=True)
      if line.startswith('ACCEPT-FAIL'):raise RuntimeError(line)
      if line=='ACCEPT-SUITE-PASS':done=True
    offset=len(lines)
    if p.poll() is not None:
     if done:break
     if boots>=3:raise RuntimeError('unexpected reboot count')
     p=None;continue
    if time.monotonic()>deadline:raise RuntimeError('consolidation acceptance timeout')
    time.sleep(1)
  except BaseException as e:failed=e
  finally:
   if p is not None and p.poll() is None:
    try:monitor.send(str(ACTIVE/'m.sock'),'system_powerdown');p.wait(timeout=25)
    except Exception:p.kill();p.wait(timeout=15)
   budget()
 (EVIDENCE/'result.json').write_text(json.dumps({'pass':done and failed is None,'boots':boots,'error':str(failed) if failed else None},indent=2)+'\n')
 for disk in ['windows','data']:
  with (EVIDENCE/f'{disk}-check.log').open('w') as out:subprocess.run(['qemu-img','check',str(ACTIVE/f'{disk}.qcow2')],check=True,stdout=out,stderr=out)
 if failed:raise failed
 print('ENCRYPTED-CONSOLIDATION-PASS',flush=True)
if __name__=='__main__':main()
