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
import subprocess
import sys
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
    assert fmt in ('vhdx', 'qcow2', 'obk', 'consol'), fmt
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


def step_test() -> None:
    S.refuse_if_done('test')
    answer_iso()  # the guest script as it is now
    S.qcow('data.qcow2', '12G')
    cmd = S.base_cmd(S.ACTIVE / 'vars.fd', S.EVIDENCE / 'test-serial.log', S.ACTIVE / 'test.sock')
    cmd += S.disk(S.ACTIVE / 'windows.qcow2', 0) + S.disk(S.ACTIVE / 'data.qcow2', 1) + S.cdrom(S.ACTIVE / 'answer.iso', 2)
    if S.run_vm('test', cmd, ['KILLX-PASS', 'KILLX-ALREADY-DONE'], ['KILLX-FAIL'], 7200) != 'done':
        raise SystemExit('test failed; evidence kept')


def main() -> None:
    steps = {'prepare': step_prepare, 'install': step_install, 'test': step_test}
    if len(sys.argv) != 2 or sys.argv[1] not in steps:
        raise SystemExit(f'usage: {sys.argv[0]} {"|".join(steps)}')
    steps[sys.argv[1]]()


if __name__ == '__main__':
    main()
