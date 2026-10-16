#!/usr/bin/env python3
"""System-OBK restore acceptance (CHECKS 1): encrypted bootable copy of a
UEFI/GPT Windows -> the product's WinPE medium -> a blank disk of a new
machine -> that disk boots by itself and the data, ACLs, streams and logon are
intact.

    scripts/lab/run-system-obk.py prepare   # answer CD, blank system disk
    scripts/lab/run-system-obk.py install   # Windows Setup + product + data
    scripts/lab/run-system-obk.py backup    # encrypted whole-disk job runs
    scripts/lab/run-system-obk.py pe        # fixture medium from the product ISO
    scripts/lab/run-system-obk.py restore   # WinPE restores onto a blank disk
    scripts/lab/run-system-obk.py boot      # restored disk alone, fresh machine

Every step is idempotent about its evidence: a step that already produced its
result refuses to run again until that evidence is moved away. The source
system disk is deleted after `backup` — the scenario is a replaced disk, and it
keeps the run inside its 20 GiB budget.
"""
import base64
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB = Path.home() / 'lab'
# One folder per run; OBK_RUN names it (the accepted CLI-driven run kept its own).
WORK = REPO / 'tmp' / os.environ.get('OBK_RUN', 'system-obk-2026-10-16')
ACTIVE = WORK / 'active'
EVIDENCE = WORK / 'evidence'
GIB = 2 ** 30
CODE = '/usr/share/OVMF/OVMF_CODE_4M.ms.fd'
VARS = '/usr/share/OVMF/OVMF_VARS_4M.ms.fd'
PRODUCT_ISO = WORK / 'product.iso'  # the product's own medium, copied in by hand


def monitor():
    spec = importlib.util.spec_from_file_location('qm', REPO / 'scripts/lab/qemu-monitor.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def du(path: Path) -> int:
    if not path.exists():
        return 0
    return int(subprocess.check_output(['du', '-s', '-B1', str(path)]).split()[0])


def budget(step: str) -> None:
    row = {'utc': time.time(), 'step': step, 'free': shutil.disk_usage(REPO).free,
           'lab': du(LAB), 'work': du(WORK), 'active': du(ACTIVE), 'target': du(REPO / 'target')}
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    with (EVIDENCE / 'budgets.jsonl').open('a') as out:
        out.write(json.dumps(row) + '\n')
    if (row['free'] < 30 * GIB or row['lab'] + row['work'] > 65 * GIB
            or row['active'] > 20 * GIB or row['target'] > 30 * GIB):
        raise RuntimeError(f'space budget exceeded: {row}')


def qcow(name: str, size: str) -> Path:
    path = ACTIVE / name
    if not path.exists():
        subprocess.run(['qemu-img', 'create', '-q', '-f', 'qcow2', str(path), size], check=True)
    return path


def base_cmd(vars_fd: Path, serial: Path, sock: Path):
    return ['qemu-system-x86_64', '-machine', 'q35,accel=kvm,smm=on', '-cpu', 'host',
            '-smp', '4', '-m', '6144',
            '-global', 'driver=cfi.pflash01,property=secure,value=on',
            '-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={CODE}',
            '-drive', f'if=pflash,format=raw,unit=1,file={vars_fd}',
            '-netdev', 'user,id=n0,restrict=on', '-device', 'e1000,netdev=n0',
            '-chardev', f'file,id=serial,path={serial},append=on', '-serial', 'chardev:serial',
            '-monitor', f'unix:{sock},server,nowait', '-display', 'none', '-no-user-config']


def disk(path: Path, index: int):
    return ['-drive', f'file={path},format=qcow2,if=ide,index={index},media=disk']


def cdrom(path: Path, index: int):
    return ['-drive', f'file={path},format=raw,if=ide,index={index},media=cdrom,readonly=on']


def decode_reports(serial: Path, step: str) -> None:
    out = EVIDENCE / step
    out.mkdir(parents=True, exist_ok=True)
    for line in serial.read_text(errors='replace').splitlines():
        if line.startswith('SYS-REPORT '):
            _, name, data = line.split(' ', 2)
            try:
                (out / f'{name}.json').write_bytes(base64.b64decode(data.strip()))
            except Exception as error:  # a truncated line is evidence too
                (out / f'{name}.broken').write_text(f'{error}\n{line}\n')


def run_vm(step: str, cmd, done, fail, timeout_s: int, press_keys_s: int = 0) -> str:
    """Run until the serial log shows `done` or `fail` and the VM powers off."""
    serial = EVIDENCE / f'{step}-serial.log'
    sock = ACTIVE / f'{step}.sock'
    serial.touch()
    (EVIDENCE / f'{step}-command.json').write_text(json.dumps(cmd, indent=2) + '\n')
    budget(step)
    qm = monitor()
    started = time.monotonic()
    next_budget = 0.0
    seen = 0
    verdict = None
    failed_at = None
    with (EVIDENCE / f'{step}-qemu.log').open('w') as log:
        vm = subprocess.Popen(cmd, stdout=log, stderr=log)
        try:
            while vm.poll() is None:
                now = time.monotonic()
                if now - started < press_keys_s and sock.exists():
                    try:
                        qm.send(str(sock), 'sendkey ret', timeout=2)
                    except Exception:
                        pass
                if now >= next_budget:
                    budget(step)
                    next_budget = now + 25
                lines = serial.read_text(errors='replace').splitlines()
                for line in lines[seen:]:
                    if not line.startswith('SYS-REPORT'):
                        print(line[:300], flush=True)
                    if any(m in line for m in done):
                        verdict = verdict or 'done'
                    if any(m in line for m in fail):
                        verdict = 'fail'
                        failed_at = failed_at or now
                seen = len(lines)
                if now - started > timeout_s:
                    verdict = 'timeout'
                    break
                # Five minutes for the guest to say why and power off by itself.
                if failed_at and now - failed_at > 300:
                    break
                time.sleep(1)
        finally:
            if vm.poll() is None:
                try:
                    qm.send(str(sock), 'system_powerdown')
                    vm.wait(timeout=60)
                except Exception:
                    vm.kill()
                    vm.wait(timeout=30)
            budget(step)
    decode_reports(serial, step)
    if verdict is None:
        # Powered off without a word: read the log one last time.
        text = serial.read_text(errors='replace')
        verdict = 'done' if any(m in text for m in done) else 'fail'
        if any(m in text for m in fail):
            verdict = 'fail'
    (EVIDENCE / f'{step}-result.json').write_text(json.dumps(
        {'verdict': verdict, 'qemu_exit': vm.returncode, 'seconds': round(time.monotonic() - started)}) + '\n')
    print(f'STEP {step}: {verdict} after {round(time.monotonic() - started)} s', flush=True)
    return verdict


def refuse_if_done(step: str) -> None:
    result = EVIDENCE / f'{step}-result.json'
    if result.exists() and json.loads(result.read_text()).get('verdict') == 'done':
        raise SystemExit(f'{step} already passed; move {result} away to repeat it')
    if subprocess.run(['pgrep', '-f', '^qemu-system-'], capture_output=True).returncode == 0:
        raise SystemExit('another VM is running')


def answer_iso() -> Path:
    payload = ACTIVE / 'payload'
    if payload.exists():
        shutil.rmtree(payload)
    lab = payload / 'lab'
    lab.mkdir(parents=True)
    unattend = REPO / 'scripts/lab/autounattend-uefi.xml'
    shutil.copyfile(unattend, payload / 'autounattend.xml')
    shutil.copyfile(unattend, lab / 'autounattend.xml')
    shutil.copyfile(REPO / 'scripts/lab/sysrestore-guest.ps1', lab / 'sysrestore-guest.ps1')
    (lab / 'sys-marker.txt').write_text('Own Backup system restore acceptance media\n')
    # What the restored disk must look like, from the source's own report.
    for name in ('snapshot-summary.json', 'snapshot-source.json'):
        summary = EVIDENCE / 'install' / name
        if summary.exists():
            parts = json.loads(summary.read_text(encoding='utf-8-sig'))['partitions']
            (lab / 'expected-partitions.json').write_text(json.dumps(parts, indent=2) + '\n')
            break
    shutil.copyfile(REPO / 'target/dist/own-backup-install.exe', lab / 'own-backup-install.exe')
    shutil.copyfile(REPO / 'target/x86_64-pc-windows-msvc/release/examples/audit_autonomous.exe',
                    lab / 'audit_autonomous.exe')
    hashes = {p.relative_to(payload).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
              for p in sorted(payload.rglob('*')) if p.is_file()}
    (EVIDENCE / 'payload-hashes.json').write_text(json.dumps(hashes, indent=2) + '\n')
    iso = ACTIVE / 'answer.iso'
    subprocess.run(['xorriso', '-as', 'mkisofs', '-quiet', '-J', '-R', '-V', 'OBKSYS', '-o', str(iso),
                    str(payload)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(payload)
    return iso


def step_prepare() -> None:
    ACTIVE.mkdir(parents=True, exist_ok=True)
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    budget('prepare')
    answer_iso()
    qcow('windows.qcow2', '40G')
    if not (ACTIVE / 'vars-source.fd').exists():
        shutil.copyfile(VARS, ACTIVE / 'vars-source.fd')
    print('PREPARED', flush=True)


def step_install() -> None:
    refuse_if_done('install')
    sock = ACTIVE / 'install.sock'
    cmd = base_cmd(ACTIVE / 'vars-source.fd', EVIDENCE / 'install-serial.log', sock)
    cmd += disk(ACTIVE / 'windows.qcow2', 0) + cdrom(LAB / 'win10-ltsc.iso', 2) + cdrom(ACTIVE / 'answer.iso', 3)
    verdict = run_vm('install', cmd, ['SYS-INSTALL-DONE'], ['SYS-FAIL'], 7200, press_keys_s=60)
    if verdict != 'done':
        raise SystemExit('install failed; evidence kept')


def step_backup() -> None:
    refuse_if_done('backup')
    answer_iso()  # the scripts as they are now
    qcow('store.qcow2', '40G')
    sock = ACTIVE / 'backup.sock'
    cmd = base_cmd(ACTIVE / 'vars-source.fd', EVIDENCE / 'backup-serial.log', sock)
    cmd += disk(ACTIVE / 'windows.qcow2', 0) + disk(ACTIVE / 'store.qcow2', 1) + cdrom(ACTIVE / 'answer.iso', 2)
    verdict = run_vm('backup', cmd, ['SYS-BACKUP-DONE'], ['SYS-FAIL'], 7200)
    if verdict != 'done':
        raise SystemExit('backup failed; evidence kept')
    # The disk this copy was taken from is "replaced": the restore never sees it.
    (ACTIVE / 'windows.qcow2').unlink()
    print('source system disk removed (replaced-disk scenario)', flush=True)


def step_pe() -> None:
    if not PRODUCT_ISO.is_file():
        raise SystemExit(f'copy the product ISO to {PRODUCT_ISO} first')
    budget('pe')
    root = ACTIVE / 'pe'
    if root.exists():
        shutil.rmtree(root)
    root.mkdir()
    wim = root / 'boot.wim'
    subprocess.run(['xorriso', '-osirrox', 'on', '-indev', str(PRODUCT_ISO), '-extract', '/sources/boot.wim',
                    str(wim)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wim.chmod(0o644)
    listing = subprocess.run(['wimlib-imagex', 'dir', str(wim), '1', '--path', '/Program Files/OwnBackup'],
                             capture_output=True, text=True, check=True).stdout
    (EVIDENCE / 'pe-app-listing.txt').write_text(listing)
    if 'own-backup-service.exe' not in listing:
        raise SystemExit('the product image has no engine where expected')
    shell = root / 'winpeshl.ini'
    shell.write_bytes(b'[LaunchApps]\r\nX:\\Windows\\System32\\cmd.exe, /c X:\\Windows\\System32\\sysrestore-pe.cmd\r\n')
    script = root / 'sysrestore-pe.cmd'
    script.write_bytes(b'@echo off\r\n'
                       b'wpeinit\r\n'
                       b'for %%i in (%SystemRoot%\\INF\\oem*.inf) do drvload "%%i" >nul 2>&1\r\n'
                       b'wpeutil disablefirewall >nul 2>&1\r\n'
                       b'start "engine" /min "X:\\Program Files\\OwnBackup\\own-backup-service.exe" --run --media\r\n'
                       b'X:\\Windows\\System32\\audit_system_restore.exe\r\n'
                       b'wpeutil shutdown\r\n')
    release = REPO / 'target/x86_64-pc-windows-msvc/release'
    driver = release / 'examples/audit_system_restore.exe'
    updates = [(shell, 'Windows/System32/winpeshl.ini'), (script, 'Windows/System32/sysrestore-pe.cmd'),
               (driver, 'Windows/System32/audit_system_restore.exe')]
    # The engine and CLI of THIS build; the rest of the medium is the product's.
    for name in ('own-backup-service.exe', 'own-backup-cli.exe'):
        updates.append((release / name, f'Program Files/OwnBackup/{name}'))
    commands = ''.join(f'add "{src}" "/{dst}"\n' for src, dst in updates)
    subprocess.run(['wimlib-imagex', 'update', str(wim), '1'], input=commands, text=True, check=True,
                   stdout=subprocess.DEVNULL)
    fixture = ACTIVE / 'fixture.iso'
    if fixture.exists():
        fixture.unlink()
    subprocess.run(['xorriso', '-indev', str(PRODUCT_ISO), '-outdev', str(fixture), '-boot_image', 'any', 'replay',
                    '-map', str(wim), '/sources/boot.wim', '-commit'], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    shutil.rmtree(root)
    hashes = {'product_iso': hashlib.sha256(PRODUCT_ISO.read_bytes()).hexdigest(),
              'fixture_iso': hashlib.sha256(fixture.read_bytes()).hexdigest(),
              **{dst: hashlib.sha256(src.read_bytes()).hexdigest() for src, dst in updates if src.exists()}}
    (EVIDENCE / 'pe-hashes.json').write_text(json.dumps(hashes, indent=2) + '\n')
    budget('pe')
    print('PE-FIXTURE-READY', flush=True)


def log_disk() -> Path:
    """A small FAT disk for the WinPE log: WinPE has no serial driver."""
    image = ACTIVE / 'pelog.img'
    if image.exists():
        image.unlink()
    with image.open('wb') as out:
        out.truncate(64 * 2 ** 20)
    tools = LAB / 'tools/usr/bin'
    subprocess.run([str(tools / 'mformat'), '-i', str(image), '-v', 'PELOG', '::'], check=True)
    marker = ACTIVE / 'pe-log.marker'
    marker.write_text('Disposable WinPE log volume.\n')
    subprocess.run([str(tools / 'mcopy'), '-i', str(image), str(marker), '::'], check=True)
    marker.unlink()
    return image


def step_restore() -> None:
    refuse_if_done('restore')
    # Blank, always: the WinPE program refuses a target that already holds data.
    if (ACTIVE / 'target.qcow2').exists():
        (ACTIVE / 'target.qcow2').unlink()
    qcow('target.qcow2', '40G')
    pelog = log_disk()
    # A different machine: the NVRAM of a fresh one.
    if not (ACTIVE / 'vars-target.fd').exists():
        shutil.copyfile(VARS, ACTIVE / 'vars-target.fd')
    sock = ACTIVE / 'restore.sock'
    cmd = base_cmd(ACTIVE / 'vars-target.fd', EVIDENCE / 'restore-serial.log', sock)
    cmd += disk(ACTIVE / 'store.qcow2', 0) + disk(ACTIVE / 'target.qcow2', 1) + cdrom(ACTIVE / 'fixture.iso', 2)
    cmd += ['-device', 'qemu-xhci,id=xhci', '-drive', f'file={pelog},format=raw,if=none,id=pelog',
            '-device', 'usb-storage,drive=pelog,bus=xhci.0,removable=on']
    cmd += ['-boot', 'order=d', '-no-reboot']
    run_vm('restore', cmd, ['PE-RESULT PASS'], ['PE-RESULT FAIL'], 7200, press_keys_s=40)
    out = EVIDENCE / 'restore'
    out.mkdir(parents=True, exist_ok=True)
    subprocess.run([str(LAB / 'tools/usr/bin/mcopy'), '-o', '-i', str(pelog), '::pe-sysrestore.log', str(out) + '/'],
                   capture_output=True)
    log = out / 'pe-sysrestore.log'
    text = log.read_text(errors='replace') if log.exists() else ''
    print(text[-8000:], flush=True)
    verdict = 'done' if 'PE-RESULT PASS' in text and 'PE-RESULT FAIL' not in text else 'fail'
    (EVIDENCE / 'restore-result.json').write_text(json.dumps({'verdict': verdict, 'from': 'pe log'}) + '\n')
    if verdict != 'done':
        raise SystemExit('restore failed; evidence kept')


CDP_PORT = 19222  # host side; the relay listens on 9223 in the guest


def step_pe_gui() -> None:
    """The product medium as it boots for a person, plus the DevTools relay."""
    if not PRODUCT_ISO.is_file():
        raise SystemExit(f'copy the product ISO to {PRODUCT_ISO} first')
    budget('pe-gui')
    root = ACTIVE / 'pe'
    if root.exists():
        shutil.rmtree(root)
    root.mkdir()
    wim = root / 'boot.wim'
    subprocess.run(['xorriso', '-osirrox', 'on', '-indev', str(PRODUCT_ISO), '-extract', '/sources/boot.wim',
                    str(wim)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wim.chmod(0o644)
    startnet = root / 'startnet.cmd'
    subprocess.run(['wimlib-imagex', 'extract', str(wim), '1', '/Windows/System32/startnet.cmd',
                    f'--dest-dir={root}', '--no-acls'], check=True, stdout=subprocess.DEVNULL)
    original = startnet.read_bytes()
    (EVIDENCE / 'pe-gui-startnet-original.cmd').write_bytes(original)
    # Started before anything of ours, so the port is there when the window is.
    relay_line = b'start "" X:\\Windows\\System32\\cdp_relay.exe\r\n'
    lines = original.split(b'\r\n')
    patched = b'\r\n'.join(lines[:2]) + b'\r\n' + relay_line + b'\r\n'.join(lines[2:])
    startnet.write_bytes(patched)
    release = REPO / 'target/x86_64-pc-windows-msvc/release'
    updates = [(startnet, 'Windows/System32/startnet.cmd'),
               (release / 'examples/cdp_relay.exe', 'Windows/System32/cdp_relay.exe')]
    # The window and desktop stay the medium's own unless OBK_GUI_CURRENT=1:
    # a replaced own-backup-gui.exe failed to create its webview with
    # 0x800736B1 (side-by-side) on 2026-10-16, which is a question by itself.
    names = ['own-backup-service.exe', 'own-backup-cli.exe']
    if os.environ.get('OBK_GUI_CURRENT') == '1':
        names += ['own-backup-gui.exe', 'own-backup-shell.exe']
    for name in names:
        updates.append((release / name, f'Program Files/OwnBackup/{name}'))
    commands = ''.join(f'add "{src}" "/{dst}"\n' for src, dst in updates)
    subprocess.run(['wimlib-imagex', 'update', str(wim), '1'], input=commands, text=True, check=True,
                   stdout=subprocess.DEVNULL)
    fixture = ACTIVE / 'fixture-gui.iso'
    if fixture.exists():
        fixture.unlink()
    subprocess.run(['xorriso', '-indev', str(PRODUCT_ISO), '-outdev', str(fixture), '-boot_image', 'any', 'replay',
                    '-map', str(wim), '/sources/boot.wim', '-commit'], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    hashes = {'product_iso': hashlib.sha256(PRODUCT_ISO.read_bytes()).hexdigest(),
              'fixture_iso': hashlib.sha256(fixture.read_bytes()).hexdigest(),
              **{dst: hashlib.sha256(src.read_bytes()).hexdigest() for src, dst in updates}}
    (EVIDENCE / 'pe-gui-hashes.json').write_text(json.dumps(hashes, indent=2) + '\n')
    shutil.rmtree(root)
    budget('pe-gui')
    print('PE-GUI-FIXTURE-READY', flush=True)


def gui_log_disk() -> Path:
    """FAT disk with the medium's own WebView2 argument override on it."""
    image = log_disk()
    sys.path.insert(0, str(REPO / 'scripts/lab'))
    args = ('--allow-run-as-system --no-sandbox --in-process-gpu --disable-gpu --disable-gpu-compositing '
            '--disable-direct-composition --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,'
            'RendererCodeIntegrity,RemoveRedirectionBitmap,ForceSWDCompWhenDCompFallbackRequired --no-first-run '
            '--remote-debugging-port=9222 --remote-allow-origins=*')
    if os.environ.get('OBK_NO_DEVTOOLS') == '1':
        return image  # the medium's own arguments, to compare against
    override = ACTIVE / 'ownbackup-webview2-args.txt'
    override.write_bytes(args.encode() + b'\r\n')
    subprocess.run([str(LAB / 'tools/usr/bin/mcopy'), '-i', str(image), str(override), '::'], check=True)
    override.unlink()
    return image


def step_restore_gui() -> None:
    refuse_if_done('restore-gui')
    if (ACTIVE / 'target.qcow2').exists():
        (ACTIVE / 'target.qcow2').unlink()
    qcow('target.qcow2', '40G')
    if (ACTIVE / 'vars-target.fd').exists():
        (ACTIVE / 'vars-target.fd').unlink()
    shutil.copyfile(VARS, ACTIVE / 'vars-target.fd')  # a new machine
    pelog = gui_log_disk()
    sock = ACTIVE / 'restore-gui.sock'
    serial = EVIDENCE / 'restore-gui-serial.log'
    cmd = base_cmd(ACTIVE / 'vars-target.fd', serial, sock)
    i = cmd.index('user,id=n0,restrict=on')
    cmd[i] = f'user,id=n0,restrict=on,hostfwd=tcp:127.0.0.1:{CDP_PORT}-:9223'
    cmd += disk(ACTIVE / 'store.qcow2', 0) + disk(ACTIVE / 'target.qcow2', 1) + cdrom(ACTIVE / 'fixture-gui.iso', 2)
    cmd += ['-device', 'qemu-xhci,id=xhci', '-drive', f'file={pelog},format=raw,if=none,id=pelog',
            '-device', 'usb-storage,drive=pelog,bus=xhci.0,removable=on', '-boot', 'order=d', '-no-reboot']
    serial.touch()
    (EVIDENCE / 'restore-gui-command.json').write_text(json.dumps(cmd, indent=2) + '\n')
    shots = EVIDENCE / 'restore-gui'
    shots.mkdir(parents=True, exist_ok=True)
    budget('restore-gui')
    qm = monitor()
    sys.path.insert(0, str(REPO / 'scripts/lab'))
    scenario = importlib.import_module('gui_restore_scenario')
    verdict = 'fail'
    with (EVIDENCE / 'restore-gui-qemu.log').open('w') as log:
        vm = subprocess.Popen(cmd, stdout=log, stderr=log)
        try:
            started = time.monotonic()
            while time.monotonic() - started < 40:  # "Press any key to boot from CD"
                if sock.exists():
                    try:
                        qm.send(str(sock), 'sendkey ret', timeout=2)
                    except Exception:
                        pass
                time.sleep(1)
            verdict = scenario.run(f'127.0.0.1:{CDP_PORT}', shots, lambda: budget('restore-gui'),
                                   lambda name: qm.send(str(sock), f'screendump {shots / name}.ppm'))
        except Exception as error:
            (shots / 'error.txt').write_text(f'{type(error).__name__}: {error}\n')
            print(f'SCENARIO-ERROR {error}', flush=True)
        finally:
            try:
                qm.send(str(sock), f'screendump {shots / "final"}.ppm')
            except Exception:
                pass
            if vm.poll() is None:
                try:
                    qm.send(str(sock), 'system_powerdown')
                    vm.wait(timeout=90)
                except Exception:
                    vm.kill()
                    vm.wait(timeout=30)
            budget('restore-gui')
    subprocess.run([str(LAB / 'tools/usr/bin/mcopy'), '-o', '-i', str(pelog), '::*.log', str(shots) + '/'],
                   capture_output=True)
    (EVIDENCE / 'restore-gui-result.json').write_text(json.dumps({'verdict': verdict}) + '\n')
    print(f'STEP restore-gui: {verdict}', flush=True)
    if verdict != 'done':
        raise SystemExit('the GUI restore did not pass; evidence kept')


def step_boot() -> None:
    refuse_if_done('boot')
    sock = ACTIVE / 'boot.sock'
    # The restored disk alone, in the machine the restore ran on. The answer CD
    # only refreshes the checking script (the boot task copies it at startup);
    # it is not bootable and holds nothing of the system.
    answer_iso()
    cmd = base_cmd(ACTIVE / 'vars-target.fd', EVIDENCE / 'boot-serial.log', sock)
    cmd += disk(ACTIVE / 'target.qcow2', 0) + cdrom(ACTIVE / 'answer.iso', 2)
    verdict = run_vm('boot', cmd, ['SYS-PASS'], ['SYS-FAIL'], 3600)
    if verdict != 'done':
        raise SystemExit('the restored system did not pass; evidence kept')


def main() -> None:
    steps = {'prepare': step_prepare, 'install': step_install, 'backup': step_backup,
             'pe': step_pe, 'restore': step_restore, 'boot': step_boot,
             'pe-gui': step_pe_gui, 'restore-gui': step_restore_gui}
    if len(sys.argv) != 2 or sys.argv[1] not in steps:
        raise SystemExit(__doc__)
    steps[sys.argv[1]]()


if __name__ == '__main__':
    main()
