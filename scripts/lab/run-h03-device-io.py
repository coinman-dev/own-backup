#!/usr/bin/env python3
"""Isolated NTFS device faults; no fault rule touches the Windows system disk."""
import argparse,codecs,hashlib,importlib.util,json,os,select,shutil,signal,socket,subprocess,time
from pathlib import Path
REPO=Path(__file__).resolve().parents[2];LAB=Path.home()/'lab';ACTIVE=LAB/'active-h03-device-io';EVIDENCE=ACTIVE/'evidence'
CASES=('write-eio','flush-eio','inflight-cut')
spec=importlib.util.spec_from_file_location('bounds',REPO/'scripts/lab/run-h03-boundaries.py');bounds=importlib.util.module_from_spec(spec);spec.loader.exec_module(bounds)
bounds.ACTIVE=ACTIVE;bounds.EVIDENCE=EVIDENCE;bounds.ARCHIVE_PREFIX='continuation/h03-device-io/'
class Qmp:
    def __init__(self,path,log):
        self.sock=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);self.sock.settimeout(10);self.sock.connect(str(path));self.stream=self.sock.makefile('rb');self.log=log;self.events=[]
        self.receive();self.call('qmp_capabilities')
    def receive(self):
        r=json.loads(self.stream.readline());self.log.write(json.dumps({'receive':r})+'\n');self.log.flush()
        if 'event' in r:self.events.append(r)
        return r
    def call(self,name,args=None):
        r={'execute':name}
        if args is not None:r['arguments']=args
        self.log.write(json.dumps({'send':r})+'\n');self.log.flush();self.sock.sendall((json.dumps(r)+'\r\n').encode())
        while True:
            r=self.receive()
            if 'error' in r:raise RuntimeError(r)
            if 'return' in r:return r['return']
    def close(self):self.stream.close();self.sock.close()
def phase_iso(phase,name):
    (ACTIVE/'payload/lab/io-phase.txt').write_text(phase)
    bounds.run_command(['xorriso','-as','mkisofs','-quiet','-J','-R','-V','OBKLAB','-o',str(ACTIVE/'answer.iso'),str(ACTIVE/'payload')],name+'-'+phase+'-iso.log')
def export_results(name):
    marker=ACTIVE/'virtual-windows.raw';marker.touch()
    cmd=['qemu-storage-daemon','--blockdev',f'driver=qcow2,node-name=disk,read-only=on,file.driver=file,file.filename={ACTIVE/"windows.qcow2"}','--export',f'type=fuse,id=read,node-name=disk,mountpoint={marker}']
    with (EVIDENCE/name/'fuse.log').open('w') as log:
        p=subprocess.Popen(cmd,stdout=log,stderr=log)
        try:
            for _ in range(100):
                if marker.stat().st_size>0:break
                if p.poll() is not None:raise RuntimeError('readonly export failed')
                time.sleep(.1)
            else:raise RuntimeError('readonly export timeout')
            bounds.run_command([str(LAB/'tools/usr/lib/7zip/7z'),'x','-y','-r','-o'+str(EVIDENCE/name/'guest'),str(marker),'Lab/h03-device-io/*.json','Lab/io-*.log'],name+'-extract.log')
        finally:p.terminate();p.wait(timeout=15)
    marker.unlink()
def run_case(name):
    if (ACTIVE/'windows.qcow2').exists():raise SystemExit('preserve prior overlay before a new case')
    case=EVIDENCE/name;case.mkdir();payload=ACTIVE/'payload/lab';payload.mkdir(parents=True)
    built=json.loads((EVIDENCE/'build.json').read_text());exe=Path(built['executable']);assert bounds.sha256(exe)==built['sha256']
    shutil.copy2(exe,payload/'obk_io_tests.exe');shutil.copy2(REPO/'scripts/lab/audit-h03-device-io-stage.ps1',payload/'lab-stage2.ps1');(payload/'io-case.txt').write_text(name)
    bounds.run_command(['qemu-img','create','-f','qcow2','-F','qcow2','-b',str(LAB/'resume-h02/windows.qcow2'),str(ACTIVE/'windows.qcow2')],name+'-disk.log')
    with (ACTIVE/'data.raw').open('wb') as f:f.truncate(512*2**20)
    phase_iso('prepare',name)
    cmd=['qemu-system-x86_64','-machine','pc,accel=kvm','-cpu','host','-smp','4','-m','4096',
        '-drive',f'file={ACTIVE/"windows.qcow2"},format=qcow2,if=ide,index=0,media=disk',
        '-blockdev',json.dumps({'driver':'file','node-name':'data-file','filename':str(ACTIVE/'data.raw'),'cache':{'direct':False,'no-flush':False}}),
        '-blockdev',json.dumps({'driver':'raw','node-name':'target','file':'data-file'}),'-device','ide-hd,drive=target,bus=ide.0,unit=1,write-cache=on,werror=report,rerror=report',
        '-drive',f'file={ACTIVE/"answer.iso"},format=raw,if=ide,index=2,media=cdrom,readonly=on','-nic','none',
        '-chardev',f'socket,id=serial,path={ACTIVE/"serial.sock"},server=on,wait=off','-serial','chardev:serial',
        '-qmp',f'unix:{ACTIVE/"qmp.sock"},server=on,wait=off','-display','none','-no-user-config','-no-reboot','-boot','order=c']
    (case/'command.json').write_text(json.dumps(cmd,indent=2)+'\n');serial='';power_cut=False;armed=False;fault_observed=False;outcome=None;boots=0;failure=None
    def boot():
        nonlocal boots
        for path in [ACTIVE/'serial.sock',ACTIVE/'qmp.sock']:path.unlink(missing_ok=True)
        p=subprocess.Popen(cmd,stdout=qlog,stderr=qlog);boots+=1
        for _ in range(100):
            if (ACTIVE/'serial.sock').exists() and (ACTIVE/'qmp.sock').exists():break
            if p.poll() is not None:raise RuntimeError('qemu boot failed')
            time.sleep(.1)
        s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);s.connect(str(ACTIVE/'serial.sock'));s.setblocking(False)
        return p,s,Qmp(ACTIVE/'qmp.sock',qmp_log)
    with (case/'qemu.log').open('w') as qlog,(case/'serial.log').open('w') as slog,(case/'qmp.jsonl').open('a') as qmp_log:
        p,s,q=boot();decoder=codecs.getincrementaldecoder('utf-8')(errors='replace');phase='prepare';started=time.monotonic();next_budget=0;offset=0;arm_time=0
        try:
            while True:
                now=time.monotonic()
                if now>=next_budget:bounds.budget();next_budget=now+25
                if select.select([s],[],[],.1)[0]:
                    data=s.recv(65536)
                    if data:
                        text=decoder.decode(data);serial+=text;slog.write(text);slog.flush();print(text,end='',flush=True)
                complete=serial[:serial.rfind('\n')+1]
                if 'H03-IO-FAIL' in complete:raise RuntimeError('guest test failed')
                if phase=='io' and not armed and 'H03-IO-READY' in serial:
                    opts={'driver':'blkdebug','node-name':'fault','image':'data-file'}
                    if name!='inflight-cut':opts['inject-error']=[{'event':'none','iotype':'write' if name=='write-eio' else 'flush','errno':5,'once':name=='flush-eio'}]
                    q.call('blockdev-add',opts);q.call('blockdev-reopen',{'options':[{'driver':'raw','node-name':'target','file':'fault'}]})
                    if name=='inflight-cut':q.call('human-monitor-command',{'command-line':'qemu-io fault "break pwritev_done inflight"'})
                    q.call('query-block');armed=True;arm_time=now;s.sendall(b'G')
                    (case/'armed.json').write_text(json.dumps({'case':name,'fault_node':opts,'armed_after_baseline':True},indent=2)+'\n')
                if phase=='io' and armed:
                    # Poll QMP to drain queued BLOCK_IO_ERROR events.
                    q.call('query-status')
                    errors=[ev for ev in q.events if ev.get('event')=='BLOCK_IO_ERROR'];fault_observed=bool(errors)
                    if name=='inflight-cut' and "Suspended request 'inflight'" in (case/'qemu.log').read_text(errors='replace'):
                        assert 'H03-IO-START' in serial and 'H03-IO-OPERATION-DONE' not in serial
                        p.kill();p.wait(timeout=15);power_cut=True
                        (case/'cut.json').write_text(json.dumps({'signal':'SIGKILL','request_suspended':'pwritev_done','guest_operation_not_completed':True},indent=2)+'\n')
                        q.close();s.close();phase_iso('verify',name);phase='verify';p,s,q=boot();decoder=codecs.getincrementaldecoder('utf-8')(errors='replace');continue
                    if 'H03-IO-OPERATION-DONE ' in complete:
                        line=[l for l in complete.splitlines() if 'H03-IO-OPERATION-DONE ' in l][-1];outcome=json.loads(line.split('H03-IO-OPERATION-DONE ',1)[1])
                    if 'H03-IO-DISARM' in complete:
                        assert fault_observed,'no real device error event'
                        q.call('blockdev-reopen',{'options':[{'driver':'raw','node-name':'target','file':'data-file'}]});s.sendall(b'G');armed=False
                        (case/'io-outcome-host.json').write_text(json.dumps({'native':outcome,'errors':errors,'fault_disabled_after_operation':True},indent=2)+'\n');phase='io-shutdown'
                    elif name!='inflight-cut' and now>arm_time+180:
                        raise RuntimeError('guest I/O did not return within 180s; preserve overlay')
                if p.poll() is not None:
                    q.close();s.close()
                    if phase=='prepare' and 'H03-IO-PHASE-PASS phase=prepare' in serial:
                        phase_iso('io',name);phase='io';p,s,q=boot();decoder=codecs.getincrementaldecoder('utf-8')(errors='replace');continue
                    if phase=='io-shutdown' and 'H03-IO-PHASE-PASS phase=io' in serial:
                        phase_iso('verify',name);phase='verify';p,s,q=boot();decoder=codecs.getincrementaldecoder('utf-8')(errors='replace');continue
                    break
                if now>started+1000:raise RuntimeError('case timeout')
        except BaseException as err:failure=err
        finally:
            if p.poll() is None:
                try:q.call('system_powerdown');p.wait(timeout=30)
                except Exception:
                    p.terminate();p.wait(timeout=15)
            try:q.close();s.close()
            except Exception:pass
    passed='H03-IO-VERIFY-PASS' in serial and 'H03-IO-FAIL' not in serial and (power_cut if name=='inflight-cut' else fault_observed)
    (case/'result.json').write_text(json.dumps({'pass':passed,'boots':boots,'fault_observed':fault_observed,'power_cut':power_cut,'native_outcome':outcome,'failure':str(failure) if failure else None},indent=2)+'\n')
    bounds.run_command(['qemu-img','check',str(ACTIVE/'windows.qcow2')],name+'-windows-check.log');export_results(name);bounds.pack()
    if not passed:raise SystemExit('case failed; evidence archived, keep the single overlay for diagnosis/recovery')
    for n in ['windows.qcow2','data.raw','answer.iso','serial.sock','qmp.sock']:(ACTIVE/n).unlink(missing_ok=True)
    shutil.rmtree(ACTIVE/'payload');bounds.budget()
def main():
    def interrupted(signum,frame):raise KeyboardInterrupt('stop requested')
    signal.signal(signal.SIGTERM,interrupted)
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('mode',choices=['run','finish']);parser.add_argument('--case',choices=CASES);args=parser.parse_args()
    if any(p!=ACTIVE for p in LAB.glob('active-*')):raise SystemExit('another active run must be preserved first')
    if subprocess.run(['pgrep','-f','^qemu-system-|^qemu-storage-daemon'],capture_output=True).returncode==0:raise SystemExit('another VM/export is running')
    EVIDENCE.mkdir(parents=True,exist_ok=True);bounds.budget()
    if args.mode=='finish':
        results={c:json.loads((EVIDENCE/c/'result.json').read_text()) for c in CASES};assert all(r['pass'] for r in results.values());(EVIDENCE/'all-results.json').write_text(json.dumps({'cases':results,'pass':True,'physical_hardware_acceptance':False},indent=2)+'\n');bounds.pack();return
    for name in [args.case] if args.case else CASES:run_case(name)
if __name__=='__main__':main()
