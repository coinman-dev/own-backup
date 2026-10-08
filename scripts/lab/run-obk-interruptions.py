#!/usr/bin/env python3
"""Real worker kills and full NTFS on a bounded disposable Windows overlay."""
import hashlib
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import time
import base64
import argparse
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB = Path.home() / 'lab'
WORK = REPO / 'tmp/obk-interruptions-2026-10-08'
ACTIVE = WORK / 'active'
EVIDENCE = WORK / 'evidence'


def module(name):
    spec = importlib.util.spec_from_file_location(name, REPO / f'scripts/lab/{name}.py')
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def main():
    global WORK, ACTIVE, EVIDENCE
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--suite', choices=['interruptions', 'installer-live', 'installer-cross-account', 'service-kill', 'publication-check'], default='interruptions')
    parser.add_argument('--fixed-service', action='store_true', help='Deploy the current service only inside the service-kill VM')
    args = parser.parse_args()
    live = args.suite == 'installer-live'
    cross = args.suite == 'installer-cross-account'
    kill = args.suite == 'service-kill'
    publication = args.suite == 'publication-check'
    if args.fixed_service and not kill:
        raise SystemExit('--fixed-service belongs only to the service-kill fixture')
    if live or cross or kill or publication:
        WORK = REPO / f'tmp/{args.suite}-2026-10-08'
        ACTIVE = WORK / 'active'
        EVIDENCE = WORK / 'evidence'
    pass_token = 'PUBLICATION-CHECK-PASS' if publication else 'SERVICE-KILL-PASS' if kill else 'INSTALLER-CROSS-ACCOUNT-PASS' if cross else 'INSTALLER-LIVE-PASS' if live else 'OBK-FAULT-PASS'
    fail_token = 'PUBLICATION-CHECK-FAIL' if publication else 'SERVICE-KILL-FAIL' if kill else 'INSTALLER-CROSS-ACCOUNT-FAIL' if cross else 'INSTALLER-LIVE-FAIL' if live else 'OBK-FAULT-FAIL'
    if ACTIVE.exists():
        raise SystemExit('preserve the previous interruption attempt before rerun')
    if subprocess.run(['pgrep', '-f', '^qemu-system-|^qemu-storage-daemon'], capture_output=True).returncode == 0:
        raise SystemExit('another VM or export is running')
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    ACTIVE.mkdir()
    os.environ['TMPDIR'] = str(WORK)
    bounds = module('run-h03-iso-build')
    monitor = module('qemu-monitor')

    def budget():
        bounds.host_budget(EVIDENCE)
        row = {'utc': time.time(), 'free': shutil.disk_usage(REPO).free}
        for name, path in [('lab', LAB), ('previous', REPO / 'tmp/unattended-acceptance-2026-10-07'),
                           ('work', WORK), ('active', ACTIVE), ('target', REPO / 'target')]:
            row[name] = int(subprocess.check_output(['du', '-s', '-B1', str(path)]).split()[0])
        row['other_work'] = 0
        for name in ['obk-interruptions', 'installer-live', 'installer-cross-account', 'service-kill', 'publication-check']:
            other = REPO / f'tmp/{name}-2026-10-08'
            if other != WORK and other.exists():
                row['other_work'] += int(subprocess.check_output(['du', '-s', '-B1', str(other)]).split()[0])
        with (EVIDENCE / 'budgets.jsonl').open('a') as log:
            log.write(json.dumps(row) + '\n')
        if (row['free'] < 30*2**30 or row['lab'] + row['previous'] + row['work'] + row['other_work'] > 65*2**30
                or row['active'] > 20*2**30 or row['target'] > 30*2**30):
            raise RuntimeError('space budget exceeded')

    budget()
    payload = ACTIVE / 'payload/lab'
    payload.mkdir(parents=True)
    files = [
        (REPO / 'target/x86_64-pc-windows-msvc/release/examples/audit_obk_interruptions.exe', 'audit_obk_interruptions.exe'),
        (REPO / 'scripts/lab/acceptance-obk-interruptions-stage.ps1', 'lab-stage2.ps1')]
    if live:
        files = [
            (REPO / 'scripts/lab/acceptance-installer-live-stage.ps1', 'lab-stage2.ps1'),
            (REPO / 'scripts/lab/acceptance-installer-live-user.ps1', 'acceptance-installer-live-user.ps1'),
            (REPO / 'scripts/lab/autounattend.xml', 'autounattend.xml'),
            (REPO / 'scripts/lab/acceptance-common.ps1', 'acceptance-common.ps1'),
            (REPO / 'tmp/unattended-acceptance-2026-10-07/own-backup-install.exe', 'installer-before.exe'),
            (REPO / 'win/own-backup-install.exe', 'installer-current.exe'),
            (REPO / 'win/own-backup-cli.exe', 'own-backup-cli-current.exe'),
            (REPO / 'target/x86_64-pc-windows-msvc/release/examples/audit_autonomous.exe', 'audit_autonomous.exe')]
    if cross:
        files = [
            (REPO / 'scripts/lab/acceptance-installer-cross-account-stage.ps1', 'lab-stage2.ps1'),
            (REPO / 'scripts/lab/acceptance-installer-cross-account-user.ps1', 'acceptance-installer-cross-account-user.ps1'),
            (REPO / 'scripts/lab/acceptance-common.ps1', 'acceptance-common.ps1')]
    if kill:
        files = [
            (REPO / 'scripts/lab/acceptance-service-kill-stage.ps1', 'lab-stage2.ps1'),
            (REPO / 'scripts/lab/acceptance-service-kill-user.ps1', 'acceptance-service-kill-user.ps1'),
            (REPO / 'scripts/lab/acceptance-common.ps1', 'acceptance-common.ps1'),
            (REPO / 'scripts/lab/autounattend.xml', 'autounattend.xml'),
            (REPO / 'target/x86_64-pc-windows-msvc/release/examples/audit_autonomous.exe', 'audit_autonomous.exe')]
        if args.fixed_service:
            files.append((REPO / 'target/x86_64-pc-windows-msvc/release/own-backup-service.exe', 'own-backup-service-fixed.exe'))
    if publication:
        files = [
            (REPO / 'scripts/lab/acceptance-publication-check-stage.ps1', 'lab-stage2.ps1'),
            (REPO / 'scripts/lab/acceptance-publication-check-user.ps1', 'acceptance-publication-check-user.ps1'),
            (REPO / 'scripts/lab/acceptance-common.ps1', 'acceptance-common.ps1'),
            (REPO / 'scripts/lab/autounattend.xml', 'autounattend.xml'),
            (REPO / 'target/x86_64-pc-windows-msvc/release/examples/audit_autonomous.exe', 'audit_autonomous.exe')]
    for source, name in files:
        shutil.copyfile(source, payload / name)
    (EVIDENCE / 'payload-hashes.json').write_text(json.dumps({
        file.name: hashlib.sha256(file.read_bytes()).hexdigest() for file in payload.iterdir()}, indent=2) + '\n')
    source = REPO / ('tmp/installer-live-2026-10-08/active' if cross or kill else 'tmp/unattended-acceptance-2026-10-07/active')
    if publication:
        source = REPO / 'tmp/service-kill-2026-10-08/active'
    if (cross or kill or publication) and not json.loads((source.parent / 'evidence/result.json').read_text())['pass']:
        raise SystemExit('cross-account fixture requires an accepted live upgrade')
    subprocess.run(['qemu-img', 'create', '-q', '-f', 'qcow2', '-F', 'qcow2', '-b',
                    str(source / 'windows.qcow2'), str(ACTIVE / 'windows.qcow2')], check=True)
    if cross or kill or publication:
        subprocess.run(['qemu-img', 'create', '-q', '-f', 'qcow2', '-F', 'qcow2', '-b',
                        str(source / 'small.qcow2'), str(ACTIVE / 'small.qcow2')], check=True)
    else:
        subprocess.run(['qemu-img', 'create', '-q', '-f', 'qcow2', str(ACTIVE / 'small.qcow2'), '1G' if live else '96M'], check=True)
    subprocess.run(['xorriso', '-as', 'mkisofs', '-quiet', '-J', '-R', '-V', 'OBKLAB', '-o',
                    str(ACTIVE / 'answer.iso'), str(ACTIVE / 'payload')], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    cmd = ['qemu-system-x86_64', '-machine', 'pc,accel=kvm', '-cpu', 'host', '-smp', '4', '-m', '2560',
           '-drive', f'file={ACTIVE / "windows.qcow2"},format=qcow2,if=ide,index=0,media=disk',
           '-drive', f'file={ACTIVE / "small.qcow2"},format=qcow2,if=ide,index=1,media=disk',
           '-drive', f'file={ACTIVE / "answer.iso"},format=raw,if=ide,index=2,media=cdrom,readonly=on',
           '-netdev', 'user,id=n0,restrict=on', '-device', 'e1000,netdev=n0',
           '-chardev', f'file,id=serial,path={EVIDENCE / "serial.log"},append=on', '-serial', 'chardev:serial',
           '-monitor', f'unix:{ACTIVE / "m.sock"},server,nowait',
           '-display', 'none', '-no-user-config', '-no-reboot', '-boot', 'order=c']
    (EVIDENCE / 'command.json').write_text(json.dumps(cmd, indent=2) + '\n')
    (EVIDENCE / 'serial.log').touch()
    deadline = time.monotonic() + 900
    next_budget = 0
    offset = 0
    failure = None
    rebooted = False
    with (EVIDENCE / 'qemu.log').open('w') as log:
        process = subprocess.Popen(cmd, stdout=log, stderr=log)
        try:
            while True:
                if time.monotonic() >= next_budget:
                    budget()
                    next_budget = time.monotonic() + 25
                lines = (EVIDENCE / 'serial.log').read_text(errors='replace').split('\n')[:-1]
                for line in lines[offset:]:
                    if line.startswith('ACCEPT-REPORT '):
                        _, name, encoded = line.split(' ', 2)
                        if Path(name).name != name:
                            raise RuntimeError('unsafe report name')
                        reports = EVIDENCE / 'reports'
                        reports.mkdir(exist_ok=True)
                        (reports / f'{name}.json').write_bytes(base64.b64decode(encoded, validate=True))
                        print('REPORT-SAVED', name, flush=True)
                    elif line.startswith('OBK-FAULT-REPORT '):
                        data = base64.b64decode(line.split(' ', 1)[1], validate=True)
                        (EVIDENCE / 'native-result.json').write_bytes(data)
                        print('NATIVE-REPORT-SAVED', flush=True)
                    else:
                        print(line, flush=True)
                    if line.startswith(fail_token):
                        raise RuntimeError(line)
                offset = len(lines)
                if process.poll() is not None:
                    reboot_token = 'PUBLICATION-CHECK-REBOOT' if publication else 'SERVICE-KILL-REBOOT' if kill else 'INSTALLER-LIVE-REBOOT'
                    if (live or kill or publication) and not rebooted and reboot_token in (EVIDENCE / 'serial.log').read_text(errors='replace'):
                        rebooted = True
                        process = subprocess.Popen(cmd, stdout=log, stderr=log)
                        continue
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError('interruption fixture timed out')
                time.sleep(.5)
        except BaseException as error:
            failure = error
        finally:
            if process.poll() is None:
                try:
                    monitor.send(str(ACTIVE / 'm.sock'), 'system_powerdown')
                    process.wait(timeout=25)
                except Exception:
                    process.kill()
                    process.wait(timeout=15)
            budget()
    content = (EVIDENCE / 'serial.log').read_text(errors='replace')
    native_file = EVIDENCE / ('reports/publication-check-result.json' if publication else 'reports/service-kill-result.json' if kill else 'reports/installer-cross-account-result.json' if cross else 'reports/installer-live-result.json' if live else 'native-result.json')
    native = json.loads(native_file.read_text()) if native_file.exists() else {}
    passed = (pass_token in content and fail_token not in content and native.get('pass')
              and (live or cross or kill or publication or len(native.get('cases', [])) == 6) and failure is None and process.returncode == 0)
    (EVIDENCE / 'result.json').write_text(json.dumps({'pass': bool(passed), 'error': str(failure) if failure else None,
                                                     'qemu_exit': process.returncode}, indent=2) + '\n')
    for name in ['windows', 'small']:
        with (EVIDENCE / f'{name}-check.log').open('w') as log:
            subprocess.run(['qemu-img', 'check', str(ACTIVE / f'{name}.qcow2')], check=True, stdout=log, stderr=log)
    if not passed:
        raise SystemExit('interruption acceptance failed; preserve evidence')
    print('PUBLICATION-CHECK-STAND-PASS' if publication else 'SERVICE-KILL-STAND-PASS' if kill else 'INSTALLER-CROSS-ACCOUNT-STAND-PASS' if cross else 'INSTALLER-LIVE-STAND-PASS' if live else 'OBK-INTERRUPTIONS-STAND-PASS', flush=True)


if __name__ == '__main__':
    def interrupted(signum, frame):
        raise KeyboardInterrupt('stop requested')
    signal.signal(signal.SIGTERM, interrupted)
    main()
