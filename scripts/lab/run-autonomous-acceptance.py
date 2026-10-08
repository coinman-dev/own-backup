#!/usr/bin/env python3
"""Remaining unattended acceptance. Only fresh guest disks are writable.

All budgets are sampled every 25 seconds. Accepted base disks, the owner's
installation, backup target and host service are never modified.
"""
import argparse,base64,hashlib,importlib.util,json,os,shutil,signal,subprocess,time
from pathlib import Path
REPO=Path(__file__).resolve().parents[2];LAB=Path.home()/'lab';WORK=REPO/'tmp/unattended-acceptance-2026-10-07';ACTIVE=WORK/'active';EVIDENCE=ACTIVE/'evidence'
def load(name):
 spec=importlib.util.spec_from_file_location(name,REPO/'scripts/lab'/f'{name}.py');m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);return m

def main():
 def interrupted(signum,frame):raise KeyboardInterrupt('stop requested')
 signal.signal(signal.SIGTERM,interrupted)
 if any(p!=ACTIVE for p in LAB.glob('active-*')):raise SystemExit('another active run must be preserved first')
 if subprocess.run(['pgrep','-f','^qemu-system-|^qemu-storage-daemon'],capture_output=True).returncode==0:raise SystemExit('another VM is running')
 parser=argparse.ArgumentParser();parser.add_argument('--resume',action='store_true');parser.add_argument('--resume-point',default='corruption-retry-1');args=parser.parse_args()
 if ACTIVE.exists() and not args.resume:raise SystemExit('previous attempt must be inspected and preserved before starting a fresh run')
 if args.resume:
  if not (EVIDENCE/'result.json').exists():raise SystemExit('resume requires a completed failed attempt')
  EVIDENCE.rename(ACTIVE/f'evidence-attempt-{len(list(ACTIVE.glob("evidence*")))}')
 scratch=WORK/'scratch';scratch.mkdir(exist_ok=True);os.environ['TMPDIR']=str(scratch)
 EVIDENCE.mkdir(parents=True);(EVIDENCE/'reports').mkdir();shots=REPO/'tmp/unattended-acceptance-2026-10-07/shots';shots.mkdir(parents=True,exist_ok=True)
 bounds=load('run-h03-iso-build');monitor=load('qemu-monitor')
 def budget():
  bounds.host_budget(EVIDENCE)
  row={'utc':time.time(),'free':shutil.disk_usage(REPO).free}
  for label,path in [('lab',LAB),('active',ACTIVE),('target',REPO/'target')]:row[label]=int(subprocess.check_output(['du','-s','-B1',str(path)]).split()[0])
  row['lab']+=int(subprocess.check_output(['du','-s','-B1',str(WORK)]).split()[0])
  with (EVIDENCE/'budgets.jsonl').open('a') as f:f.write(json.dumps(row)+'\n')
  if row['free']<30*2**30 or row['lab']>65*2**30 or row['active']>20*2**30 or row['target']>30*2**30:raise RuntimeError('space budget exceeded')
 budget();release=REPO/'target/x86_64-pc-windows-msvc/release';payload=ACTIVE/'payload/lab';payload.mkdir(parents=True,exist_ok=True)
 files=[(release/'own-backup-cli.exe','own-backup-cli.exe'),(release/'own-backup-service.exe','own-backup-service-acceptance.exe'),(release/'own-backup-gui.exe','own-backup-gui.exe'),(release/'examples/audit_autonomous.exe','audit_autonomous.exe'),(REPO/'scripts/lab/acceptance-stage.ps1','lab-stage2.ps1'),(REPO/'scripts/lab/acceptance-common.ps1','acceptance-common.ps1'),(REPO/'scripts/lab/acceptance-user.ps1','acceptance-user.ps1'),(REPO/'scripts/lab/acceptance-remaining-user.ps1','acceptance-remaining-user.ps1'),(REPO/'scripts/lab/acceptance-clusters.ps1','acceptance-clusters.ps1'),(REPO/'scripts/lab/acceptance-gui.ps1','acceptance-gui.ps1'),(REPO/'scripts/lab/acceptance-gui-only-user.ps1','acceptance-gui-only-user.ps1'),(LAB/'media/WebView2Runtime-x64.exe','WebView2Runtime-x64.exe'),(REPO/'scripts/lab/acceptance-post.ps1','acceptance-post.ps1'),(REPO/'scripts/lab/acceptance-bitlocker-user.ps1','acceptance-bitlocker-user.ps1'),(REPO/'scripts/lab/autounattend.xml','autounattend.xml')]
 for source,name in files:shutil.copyfile(source,payload/name)
 if args.resume:(payload/'acceptance-resume.txt').write_text(args.resume_point+'\n')
 keys={'activity':'nav_activity','activity_title':'act_title','verify_done':'act_verify_done','verify_cancelled':'act_verify_cancelled','verify_failed':'act_verify_failed','backup':'nav_backup','options':'dash_options','advanced':'wiz_tab_advanced','verify_schedule':'wiz_verify_schedule','verify_all':'wiz_verify_all_versions','save':'common_save'}
 labels={}
 for lang in ['ru','en']:
  messages=json.loads((REPO/f'crates/gui/frontend/messages/{lang}.json').read_text());labels[lang]={k:messages[v] for k,v in keys.items()};labels[lang]['language']=lang
 (payload/'acceptance-ui-labels.json').write_text(json.dumps(labels,ensure_ascii=False),encoding='utf-8')

 (EVIDENCE/'payload-hashes.json').write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in payload.iterdir()},indent=2)+'\n')
 if not args.resume:
  subprocess.run(['qemu-img','create','-q','-f','qcow2','-F','qcow2','-b',str(LAB/'resume-h02/windows.qcow2'),str(ACTIVE/'windows.qcow2')],check=True)
  subprocess.run(['qemu-img','create','-q','-f','qcow2',str(ACTIVE/'data.qcow2'),'10G'],check=True)
 subprocess.run(['xorriso','-as','mkisofs','-quiet','-J','-R','-V','OBKLAB','-o',str(ACTIVE/'answer.iso'),str(ACTIVE/'payload')],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 cmd=['qemu-system-x86_64','-machine','pc,accel=kvm','-cpu','host','-smp','4','-m','4096','-drive',f'file={ACTIVE/"windows.qcow2"},format=qcow2,if=ide,index=0,media=disk','-drive',f'file={ACTIVE/"data.qcow2"},format=qcow2,if=ide,index=1,media=disk','-drive',f'file={ACTIVE/"answer.iso"},format=raw,if=ide,index=2,media=cdrom,readonly=on','-netdev','user,id=n0,restrict=on','-device','e1000,netdev=n0','-chardev',f'file,id=serial,path={EVIDENCE/"serial.log"},append=on','-serial','chardev:serial','-monitor',f'unix:{ACTIVE/"monitor.sock"},server,nowait','-display','none','-no-user-config','-no-reboot','-boot','order=c']
 (EVIDENCE/'command.json').write_text(json.dumps(cmd,indent=2)+'\n');(EVIDENCE/'serial.log').touch()
 boots=0;seen=0;failed=None;deadline=time.monotonic()+5400;next_budget=0;last_shot=None
 with (EVIDENCE/'qemu.log').open('w') as log:
  p=None
  try:
   while True:
    if p is None:
     (ACTIVE/'monitor.sock').unlink(missing_ok=True);p=subprocess.Popen(cmd,stdout=log,stderr=log);boots+=1;print(f'HOST-BOOT {boots}',flush=True)
    now=time.monotonic()
    if now>=next_budget:budget();next_budget=now+25
    text=(EVIDENCE/'serial.log').read_text(errors='replace');lines=text.split('\n')[:-1]
    for raw in lines[seen:]:
     line=raw.strip()
     if line.startswith('ACCEPT-REPORT '):
      _,name,encoded=line.split(' ',2)
      if Path(name).name!=name:raise RuntimeError('invalid report name')
      (EVIDENCE/'reports'/f'{name}.json').write_bytes(base64.b64decode(encoded,validate=True));print('HOST-REPORT '+name,flush=True)
     elif line.startswith('HOST '):
      fields=dict(part.split('=',1) for part in line[5:].split() if '=' in part)
      shot=fields.get('shot','')
      if shot:
       if Path(shot).name!=shot:raise RuntimeError('invalid screenshot name')
       subprocess.run(['python3',str(REPO/'scripts/lab/qemu-screenshot.py'),str(ACTIVE/'monitor.sock'),str(shots/(shot+'.png'))],check=True,stdout=subprocess.DEVNULL)
      for key in filter(None,fields.get('keys','').split(',')):monitor.send(str(ACTIVE/'monitor.sock'),'sendkey '+key)
      print('HOST-GUI '+shot,flush=True)
     else:
      print(line,flush=True)
      if line.startswith('ACCEPT-FAIL'):raise RuntimeError(line)
      if line.startswith('ACCEPT-REBOOT'):last_shot=f'boot-{boots}-before-reboot'
    seen=len(lines)
    if last_shot and (ACTIVE/'monitor.sock').exists():
     subprocess.run(['python3',str(REPO/'scripts/lab/qemu-screenshot.py'),str(ACTIVE/'monitor.sock'),str(shots/(last_shot+'.png'))],check=True,stdout=subprocess.DEVNULL);last_shot=None
    if p.poll() is not None:
     if 'ACCEPT-SUITE-PASS' in text:break
     if boots>=9:raise RuntimeError('too many reboots')
     p=None;continue
    if now>deadline:raise RuntimeError('suite timeout')
    time.sleep(1)
  except BaseException as error:failed=error
  finally:
   if p is not None and p.poll() is None:
    try:monitor.send(str(ACTIVE/'monitor.sock'),'system_powerdown');p.wait(timeout=20)
    except Exception:p.kill();p.wait(timeout=10)
   budget()
 (EVIDENCE/'result.json').write_text(json.dumps({'pass':failed is None,'boots':boots,'error':str(failed) if failed else None},indent=2)+'\n')
 for disk in ['windows','data']:
  with (EVIDENCE/f'{disk}-container-check.log').open('w') as log:subprocess.run(['qemu-img','check',str(ACTIVE/f'{disk}.qcow2')],check=True,stdout=log,stderr=log)
 if failed:raise failed
 print('AUTONOMOUS-ACCEPTANCE-PASS',flush=True)
if __name__=='__main__':main()
