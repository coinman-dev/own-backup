#!/usr/bin/env python3
"""The whole service killed while it deletes excluded files inside a plain
VHDX (CHECKS 2), then the job goes on: full, increment, a new chain whose
retention deletes the old one, and the last copy mounted and compared.

    scripts/lab/run-kill-exclusions.py prepare   # answer CD, blank disks
    scripts/lab/run-kill-exclusions.py install   # Windows Setup + product
    OBK_FORMAT=qcow2 scripts/lab/run-kill-exclusions.py test   # vhdx (default), qcow2, obk, consol

The VM, the bounds and the evidence layout are those of run-system-obk.py;
only the guest script differs (killx-guest.ps1).
"""
import hashlib
import importlib.util
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB = Path.home() / 'lab'


def base():
    spec = importlib.util.spec_from_file_location('sysobk', REPO / 'scripts/lab/run-system-obk.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    module.WORK = REPO / 'tmp' / os.environ.get('OBK_RUN', 'kill-exclusions-2026-10-17')
    module.ACTIVE = module.WORK / 'active'
    module.EVIDENCE = module.WORK / 'evidence'
    return module


S = base()


def answer_iso() -> Path:
    payload = S.ACTIVE / 'payload'
    if payload.exists():
        shutil.rmtree(payload)
    lab = payload / 'lab'
    lab.mkdir(parents=True)
    # The accepted answer file, pointed at this suite's guest script.
    unattend = (REPO / 'scripts/lab/autounattend-uefi.xml').read_text(encoding='utf-8')
    assert unattend.count('sysrestore-guest.ps1') == 1
    unattend = unattend.replace('sysrestore-guest.ps1', 'killx-guest.ps1')
    (payload / 'autounattend.xml').write_text(unattend, encoding='utf-8')
    (lab / 'autounattend.xml').write_text(unattend, encoding='utf-8')
    shutil.copyfile(REPO / 'scripts/lab/killx-guest.ps1', lab / 'killx-guest.ps1')
    (lab / 'sys-marker.txt').write_text('Own Backup kill-during-exclusions acceptance media\n')
    # vhdx | qcow2 | obk: which container the guest's job writes this time.
    fmt = os.environ.get('OBK_FORMAT', 'vhdx')
    assert fmt in ('vhdx', 'qcow2', 'obk', 'consol', 'consol-vhdx', 'consol-qcow2', 'flush-vhdx', 'flush-obk', 'probe-io', 'stall-vhdx', 'stall-obk', 'history'), fmt
    # OBK_TAG names a fresh attempt of a format that has already passed.
    # OBK_KILL_PHASE: the phase run 1 dies in (default exclusions).
    (lab / 'killx-format.txt').write_text(fmt + '\n' + os.environ.get('OBK_TAG', '') + '\n'
                                          + os.environ.get('OBK_KILL_PHASE', '') + '\n')
    shutil.copyfile(REPO / 'target/dist/own-backup-install.exe', lab / 'own-backup-install.exe')
    shutil.copyfile(REPO / 'target/x86_64-pc-windows-msvc/release/examples/audit_autonomous.exe',
                    lab / 'audit_autonomous.exe')
    hashes = {p.relative_to(payload).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in sorted(payload.rglob('*')) if p.is_file()}
    (S.EVIDENCE / 'payload-hashes.json').write_text(json.dumps(hashes, indent=2) + '\n')
    iso = S.ACTIVE / 'answer.iso'
    subprocess.run(['xorriso', '-as', 'mkisofs', '-quiet', '-J', '-R', '-V', 'OBKKILLX', '-o', str(iso),
                    str(payload)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(payload)
    return iso


def step_prepare() -> None:
    S.ACTIVE.mkdir(parents=True, exist_ok=True)
    S.EVIDENCE.mkdir(parents=True, exist_ok=True)
    S.budget('prepare')
    answer_iso()
    S.qcow('windows.qcow2', '40G')
    if not (S.ACTIVE / 'vars.fd').exists():
        shutil.copyfile(S.VARS, S.ACTIVE / 'vars.fd')
    print('PREPARED', flush=True)


def step_install() -> None:
    S.refuse_if_done('install')
    cmd = S.base_cmd(S.ACTIVE / 'vars.fd', S.EVIDENCE / 'install-serial.log', S.ACTIVE / 'install.sock')
    cmd += S.disk(S.ACTIVE / 'windows.qcow2', 0) + S.cdrom(LAB / 'win10-ltsc.iso', 2) + S.cdrom(S.ACTIVE / 'answer.iso', 3)
    if S.run_vm('install', cmd, ['KILLX-INSTALL-DONE'], ['KILLX-FAIL'], 7200, press_keys_s=60) != 'done':
        raise SystemExit('install failed; evidence kept')


class Qmp:
    """Just enough QMP to insert and remove a fault node in a running VM."""

    def __init__(self, path: Path, log):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(15)
        self.sock.connect(str(path))
        self.stream = self.sock.makefile('rb')
        self.log = log
        self.events = []
        self.receive()
        self.call('qmp_capabilities')

    def receive(self):
        reply = json.loads(self.stream.readline())
        self.log.write(json.dumps({'utc': time.time(), 'receive': reply}) + '\n')
        self.log.flush()
        if 'event' in reply:
            self.events.append(reply)
        return reply

    def call(self, name, args=None):
        request = {'execute': name}
        if args is not None:
            request['arguments'] = args
        self.log.write(json.dumps({'utc': time.time(), 'send': request}) + '\n')
        self.log.flush()
        self.sock.sendall((json.dumps(request) + '\r\n').encode())
        while True:
            reply = self.receive()
            if 'error' in reply:
                raise RuntimeError(reply)
            if 'return' in reply:
                return reply['return']


# OBK_FAULT_IO: which requests fail. A flush error never reached a program
# (2026-10-16: 122 device errors, FlushFileBuffers succeeded in the guest),
# so 'write' is the one that tests the program.
IOTYPE = os.environ.get('OBK_FAULT_IO') or 'flush'


def fault_watch(serial: Path, qmp_path: Path, stop: threading.Event) -> None:
    """Every flush of the fault disk fails while the guest says it should.

    The guest prints KILLX-ARM-FLUSH and KILLX-DISARM-FLUSH; between them a
    blkdebug node sits under the disk and answers every flush with EIO —
    persistently, unlike the single error of 2026-10-08 that Windows retried
    away, so that this time the error reaches the program."""
    for _ in range(300):
        if qmp_path.exists() or stop.is_set():
            break
        time.sleep(0.2)
    with (S.EVIDENCE / 'test-qmp.jsonl').open('a') as log:
        q = Qmp(qmp_path, log)
        seen = 0
        armed = False
        while not stop.is_set():
            lines = serial.read_text(errors='replace').splitlines()
            for line in lines[seen:]:
                if 'KILLX-ARM-FLUSH' in line and not armed:
                    q.call('blockdev-add', {'driver': 'blkdebug', 'node-name': 'fault', 'image': 'fstore-file',
                                            'inject-error': [{'event': 'none', 'iotype': IOTYPE, 'errno': 5, 'once': False}]})
                    q.call('blockdev-reopen', {'options': [{'driver': 'raw', 'node-name': 'fstore', 'file': 'fault'}]})
                    armed = True
                    print(f'HOST armed: every {IOTYPE} of the fault disk fails', flush=True)
                if 'KILLX-DISARM-FLUSH' in line and armed:
                    q.call('blockdev-reopen', {'options': [{'driver': 'raw', 'node-name': 'fstore', 'file': 'fstore-file'}]})
                    q.call('blockdev-del', {'node-name': 'fault'})
                    armed = False
                    errors = [e for e in q.events if e.get('event') == 'BLOCK_IO_ERROR']
                    (S.EVIDENCE / 'test-fault.json').write_text(json.dumps(
                        {'block_io_errors': len(errors), 'first': errors[:3]}, indent=2) + '\n')
                    print(f'HOST disarmed after {len(errors)} device errors', flush=True)
            seen = len(lines)
            try:
                q.call('query-status')  # drains queued events
            except Exception:
                break
            time.sleep(0.5)


def step_test() -> None:
    S.refuse_if_done('test')
    answer_iso()  # the guest script as it is now
    S.qcow('data.qcow2', '12G')
    cmd = S.base_cmd(S.ACTIVE / 'vars.fd', S.EVIDENCE / 'test-serial.log', S.ACTIVE / 'test.sock')
    cmd += S.disk(S.ACTIVE / 'windows.qcow2', 0) + S.disk(S.ACTIVE / 'data.qcow2', 1) + S.cdrom(S.ACTIVE / 'answer.iso', 2)
    watch = None
    if os.environ.get('OBK_FORMAT', '').startswith(('flush', 'probe-io', 'stall')):
        # A disk of its own for the copies, so that only it fails: raw under
        # a node that a fault can be slid beneath while the VM runs.
        fault = S.ACTIVE / 'fault.raw'
        if not fault.exists():
            with fault.open('wb') as f:
                f.truncate(3 * 2 ** 30)
        qmp_path = S.ACTIVE / 'test-qmp.sock'
        qmp_path.unlink(missing_ok=True)
        cmd += ['-qmp', f'unix:{qmp_path},server=on,wait=off',
                '-blockdev', json.dumps({'driver': 'file', 'node-name': 'fstore-file', 'filename': str(fault),
                                         'cache': {'direct': False, 'no-flush': False}}),
                '-blockdev', json.dumps({'driver': 'raw', 'node-name': 'fstore', 'file': 'fstore-file'}),
                '-device', 'ide-hd,drive=fstore,bus=ide.3,write-cache=on,werror=report,rerror=report']
        stop = threading.Event()
        watch = threading.Thread(target=fault_watch, args=(S.EVIDENCE / 'test-serial.log', qmp_path, stop), daemon=True)
        watch.start()
    try:
        verdict = S.run_vm('test', cmd, ['KILLX-PASS', 'KILLX-ALREADY-DONE'], ['KILLX-FAIL'], 7200)
    finally:
        if watch:
            stop.set()
    if verdict != 'done':
        raise SystemExit('test failed; evidence kept')


def main() -> None:
    steps = {'prepare': step_prepare, 'install': step_install, 'test': step_test}
    if len(sys.argv) != 2 or sys.argv[1] not in steps:
        raise SystemExit(f'usage: {sys.argv[0]} {"|".join(steps)}')
    steps[sys.argv[1]]()


if __name__ == '__main__':
    main()
