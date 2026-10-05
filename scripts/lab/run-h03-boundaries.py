#!/usr/bin/env python3
"""H03 power cuts at test-only publication checkpoints on disposable NTFS.

build: compile the Windows test harness. run: three fresh overlays, each with
one planned power cut and a recovery boot. finish: verify/archive evidence and
remove disposable data. Kept bases are never opened for writing.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import tarfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB = Path.home() / 'lab'
ACTIVE = LAB / 'active-h03-boundaries'
EVIDENCE = ACTIVE / 'evidence'
POINTS = ('before-rename', 'after-rename', 'header-before-sync')
ARCHIVE_PREFIX = 'continuation/h03-boundaries/'
STAGE_SCRIPT = 'audit-h03-boundary-stage.ps1'
GUEST_RESULTS = 'Lab/h03-boundary/*.json'


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def sha256(path):
    with path.open('rb') as file:
        return hashlib.file_digest(file, 'sha256').hexdigest()


def used(path):
    result = subprocess.run(['du', '-s', '-B1', str(path)], capture_output=True, text=True)
    benign_race = result.returncode == 1 and result.stderr.splitlines() and all(
        'No such file or directory' in line for line in result.stderr.splitlines())
    if result.returncode and not benign_race:
        raise RuntimeError(result.stderr)
    return int(result.stdout.split()[0])


def budget():
    load_module('build', REPO / 'scripts/lab/run-h03-iso-build.py').host_budget(EVIDENCE)
    record = {'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'free': shutil.disk_usage(REPO).free, 'lab': used(LAB),
              'active': used(ACTIVE), 'target': used(REPO / 'target')}
    with (EVIDENCE / 'budgets.jsonl').open('a') as log:
        log.write(json.dumps(record) + '\n')
    if record['free'] < 30*2**30 or record['lab'] > 65*2**30 or record['active'] > 20*2**30 or record['target'] > 30*2**30:
        raise RuntimeError('space budget exceeded')


def run_command(command, name):
    budget()
    with (EVIDENCE / name).open('w') as log:
        process = subprocess.Popen(command, cwd=REPO, stdout=log, stderr=subprocess.STDOUT)
        try:
            while process.poll() is None:
                try:
                    process.wait(timeout=25)
                except subprocess.TimeoutExpired:
                    budget()
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=15)
    (EVIDENCE / (name + '.command.json')).write_text(json.dumps({'command': command, 'exit': process.returncode}, indent=2) + '\n')
    if process.returncode:
        raise RuntimeError(f'command failed: see {name}')


def pack():
    import gzip
    import io
    archive = REPO / 'docs/WINDOWS-TEST-EVIDENCE-2026-09-30.tar.gz'
    temporary = archive.with_name('.h03-boundary-evidence.tmp.tar.gz')
    prefix = ARCHIVE_PREFIX
    manifest = {}
    with tarfile.open(archive) as old:
        expected = json.load(old.extractfile('SHA256-MANIFEST.json'))
        seen = set()
        with temporary.open('wb') as raw, gzip.GzipFile(filename='', fileobj=raw, mode='wb', mtime=0) as zipped, tarfile.open(fileobj=zipped, mode='w') as new:
            for member in old:
                if not member.isfile() or member.name == 'SHA256-MANIFEST.json':
                    continue
                if member.name in seen:
                    raise RuntimeError('duplicate evidence name')
                seen.add(member.name)
                data = old.extractfile(member).read()
                digest = hashlib.sha256(data).hexdigest()
                if expected.get(member.name) != digest:
                    raise RuntimeError('existing evidence hash mismatch: ' + member.name)
                if not member.name.startswith(prefix):
                    new.addfile(member, io.BytesIO(data))
                    manifest[member.name] = digest
            if seen != set(expected):
                raise RuntimeError('existing manifest differs')
            for path in sorted(EVIDENCE.rglob('*')):
                if not path.is_file():
                    continue
                name = prefix + path.relative_to(EVIDENCE).as_posix()
                data = path.read_bytes()
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o644
                new.addfile(member, io.BytesIO(data))
                manifest[name] = hashlib.sha256(data).hexdigest()
            data = (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()
            member = tarfile.TarInfo('SHA256-MANIFEST.json')
            member.size = len(data)
            new.addfile(member, io.BytesIO(data))
    with tarfile.open(temporary) as packed:
        actual = {m.name: hashlib.sha256(packed.extractfile(m).read()).hexdigest()
                  for m in packed if m.isfile() and m.name != 'SHA256-MANIFEST.json'}
        if actual != manifest or json.load(packed.extractfile('SHA256-MANIFEST.json')) != manifest:
            raise RuntimeError('new archive verification failed')
    os.replace(temporary, archive)
    result = {'files': len(manifest), 'bytes': archive.stat().st_size,
              'sha256': sha256(archive), 'all_hashes_verified': True}
    (ACTIVE / 'archive-result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result), flush=True)


def extract(point):
    marker = ACTIVE / 'virtual-windows.raw'
    marker.touch()
    command = ['qemu-storage-daemon', '--blockdev',
               f'driver=qcow2,node-name=disk,read-only=on,file.driver=file,file.filename={ACTIVE / "windows.qcow2"}',
               '--export', f'type=fuse,id=read,node-name=disk,mountpoint={marker}']
    with (EVIDENCE / point / 'fuse.log').open('w') as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        try:
            for _ in range(100):
                if marker.stat().st_size > 0:
                    break
                if process.poll() is not None:
                    raise RuntimeError('read-only export failed')
                time.sleep(.1)
            else:
                raise RuntimeError('read-only export timeout')
            run_command([str(LAB / 'tools/usr/lib/7zip/7z'), 'x', '-y', '-r',
                         f'-o{EVIDENCE / point / "guest"}', str(marker),
                         GUEST_RESULTS, 'Lab/namespace-expected.json', 'Lab/boundary-*.log'], point + '-extract.log')
        finally:
            process.terminate()
            process.wait(timeout=15)
    marker.unlink()


def recover_case(point, suite):
    """Continue only the recovery boot of an already recorded owned power cut."""
    evidence = EVIDENCE / point
    if not (ACTIVE / 'windows.qcow2').exists() or not (evidence / 'cut.json').exists():
        raise SystemExit('recorded cut and saved overlay are required')
    if (ACTIVE / 'payload/lab/boundary-phase.txt').read_text().strip() != 'verify':
        raise SystemExit('recovery requires the saved verify-phase CD')
    if (evidence / 'result.json').exists() and json.loads((evidence / 'result.json').read_text())['pass']:
        raise SystemExit('case already passed; do not repeat it')
    command = json.loads((evidence / 'command.json').read_text())
    monitor = load_module('monitor', REPO / 'scripts/lab/qemu-monitor.py')
    recovery = {'reason': 'External WSL shutdown interrupted recovery boot after recorded cut',
                'point': point, 'extra_external_interruption': True, 'recovery_only': True}
    (evidence / 'external-interruption.json').write_text(json.dumps(recovery, indent=2)+'\n')
    with (evidence / 'qemu-recovery.log').open('a') as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        started = time.monotonic(); next_budget = 0; offset = 0; failure = None; restarts = 0
        try:
            while True:
                now = time.monotonic()
                if now >= next_budget:
                    budget(); next_budget = now + 25
                serial = (evidence / 'serial.log').read_text(errors='replace')
                if len(serial) > offset:
                    print(serial[offset:], end='', flush=True); offset = len(serial)
                if 'H03-BOUNDARY-FAIL' in serial:
                    raise RuntimeError('guest recovery failed')
                if process.poll() is not None:
                    if f'H03-BOUNDARY-CASE-PASS point={point}' not in serial and restarts < 2:
                        restarts += 1
                        process = subprocess.Popen(command, stdout=log, stderr=log)
                        continue
                    break
                if now > started + 600:
                    raise RuntimeError('recovery timeout')
                time.sleep(.2)
        except BaseException as error:
            failure = error
        finally:
            if process.poll() is None:
                monitor.send(str(ACTIVE/'monitor.sock'), 'system_powerdown')
                try:
                    process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(ACTIVE/'monitor.sock'), 'quit'); process.wait(timeout=15)
    serial = (evidence/'serial.log').read_text(errors='replace')
    complete = (f'H03-CONSOLIDATION-VALIDATE-PASS point={point}' if suite == 'consolidation'
                else f'H03-NAMESPACE-VALIDATE-PASS point={point}' if suite == 'namespace'
                else f'H03-BOUNDARY-RETRY-PASS point={point}')
    passed = complete in serial and f'H03-BOUNDARY-CASE-PASS point={point}' in serial and 'H03-BOUNDARY-FAIL' not in serial
    (evidence/'result.json').write_text(json.dumps({'pass':passed,'power_cut':True,
        'baseline_cold_boot':True,'extra_external_interruption':True,'recovery_only':True,
        'qemu_exit':process.returncode,'dense_tb_acceptance':False},indent=2)+'\n')
    if failure is not None:
        (evidence/'runner-error.txt').write_text(f'{type(failure).__name__}: {failure}\n')
    run_command(['qemu-img','check',str(ACTIVE/'windows.qcow2')],point+'-container-check.log')
    extract(point); pack()
    if passed:
        for path in (ACTIVE/'windows.qcow2',ACTIVE/'answer.iso',ACTIVE/'monitor.sock'):
            path.unlink(missing_ok=True)
        shutil.rmtree(ACTIVE/'payload')
    budget()
    if not passed:
        raise SystemExit('recovery failed; keep this one overlay and archived evidence for continuation')


def main():
    global ACTIVE, EVIDENCE, POINTS, ARCHIVE_PREFIX, STAGE_SCRIPT, GUEST_RESULTS
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=('build', 'run', 'recover', 'finish'))
    parser.add_argument('--point', choices=POINTS + ('namespace-existing-directory','namespace-new-directory','namespace-new-directory-compact','fold-before-marker','repoint-before-delete','victim-deleted-before-marker','marker-deleted-after-commit'))
    parser.add_argument('--suite', choices=('publication','namespace','consolidation'), default='publication')
    args = parser.parse_args()
    if args.suite == 'namespace':
        ACTIVE = LAB / 'active-h03-new-names'
        EVIDENCE = ACTIVE / 'evidence'
        POINTS = ('namespace-existing-directory','namespace-new-directory','namespace-new-directory-compact')
        ARCHIVE_PREFIX = 'continuation/h03-new-names/'
        STAGE_SCRIPT = 'audit-h03-namespace-stage.ps1'
        GUEST_RESULTS = 'Lab/h03-new-names/*.json'
    elif args.suite == 'consolidation':
        ACTIVE = LAB / 'active-h03-consolidation'
        EVIDENCE = ACTIVE / 'evidence'
        POINTS = ('fold-before-marker','repoint-before-delete','victim-deleted-before-marker','marker-deleted-after-commit')
        ARCHIVE_PREFIX = 'continuation/h03-consolidation/'
        STAGE_SCRIPT = 'audit-h03-consolidation-stage.ps1'
        GUEST_RESULTS = 'Lab/h03-consolidation/*.json'
    if args.point and args.point not in POINTS:
        raise SystemExit('point does not belong to the selected suite')
    def interrupted(signum, frame):
        raise KeyboardInterrupt('stop requested')
    signal.signal(signal.SIGTERM, interrupted)
    if any(p != ACTIVE for p in LAB.glob('active-*')):
        raise SystemExit('preserve and clean another active run first')
    if subprocess.run(['pgrep', '-f', '^qemu-system-|^qemu-storage-daemon'], capture_output=True).returncode == 0:
        raise SystemExit('stop existing VM/export first')
    EVIDENCE.mkdir(parents=True, exist_ok=True)
    budget()
    if args.mode == 'recover':
        if not args.point:
            raise SystemExit('recover requires --point')
        recover_case(args.point,args.suite)
        return
    if args.mode == 'build':
        run_command(['cargo', 'xwin', 'test', '--release', '--locked', '--target',
                     'x86_64-pc-windows-msvc', '-p', 'own-backup-backup', '--lib', '--no-run',
                     '--message-format=json', '-j', '2'], 'windows-build.log')
        artifacts = []
        for line in (EVIDENCE / 'windows-build.log').read_text().splitlines():
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if row.get('reason') == 'compiler-artifact' and row.get('executable') and row.get('profile', {}).get('test') and row.get('target', {}).get('name') == 'own_backup_backup':
                artifacts.append(row['executable'])
        if len(artifacts) != 1:
            raise RuntimeError('could not identify exactly one native test harness')
        (EVIDENCE / 'build.json').write_text(json.dumps({'executable': artifacts[0], 'sha256': sha256(Path(artifacts[0]))}, indent=2) + '\n')
        return
    if args.mode == 'finish':
        results = {point: json.loads((EVIDENCE / point / 'result.json').read_text())
                   for point in POINTS if (EVIDENCE / point / 'result.json').exists()}
        (EVIDENCE / 'all-results.json').write_text(json.dumps({
            'points': results, 'pass': len(results) == len(POINTS) and all(r['pass'] for r in results.values()),
            'dense_tb_acceptance': False}, indent=2) + '\n')
        pack()
        disposable = [ACTIVE / n for n in ('windows.qcow2', 'answer.iso', 'virtual-windows.raw', 'monitor.sock', 'payload')]
        for path in disposable:
            if path.is_dir():
                shutil.rmtree(path)
            else:
                path.unlink(missing_ok=True)
        return
    if (ACTIVE / 'windows.qcow2').exists():
        raise SystemExit('preserve and clean previous VM before running')
    built = json.loads((EVIDENCE / 'build.json').read_text())
    exe = Path(built['executable'])
    if sha256(exe) != built['sha256']:
        raise SystemExit('native test harness changed since build')
    sources = EVIDENCE / 'sources'
    sources.mkdir(exist_ok=True)
    for src in [Path(__file__), REPO / ('scripts/lab/'+STAGE_SCRIPT), REPO / 'crates/backup/src/engine_win.rs', REPO / 'crates/backup/src/engine_namespace_tests.rs', REPO / 'crates/backup/src/consolidate.rs', REPO / 'crates/backup/src/consolidate_fault_tests.rs'] + list((REPO / 'crates/backup/src/seal/obk').glob('*.rs')):
        shutil.copyfile(src, sources / src.name)
    monitor = load_module('monitor', REPO / 'scripts/lab/qemu-monitor.py')
    selected = (args.point,) if args.point else POINTS
    for point in selected:
        evidence = EVIDENCE / point
        evidence.mkdir()
        payload = ACTIVE / 'payload/lab'
        payload.mkdir(parents=True)
        shutil.copyfile(exe, payload / 'obk_boundary_tests.exe')
        shutil.copyfile(REPO / ('scripts/lab/'+STAGE_SCRIPT), payload / 'lab-stage2.ps1')
        (payload / 'boundary-point.txt').write_text(point)
        (payload / 'sharing-check.txt').write_text('yes' if point == POINTS[0] else 'no')
        (payload / 'boundary-phase.txt').write_text('prepare')
        run_command(['qemu-img', 'create', '-f', 'qcow2', '-F', 'qcow2', '-b',
                     str(LAB / 'resume-h02/windows.qcow2'), str(ACTIVE / 'windows.qcow2')], point + '-disk.log')
        run_command(['xorriso', '-as', 'mkisofs', '-quiet', '-J', '-R', '-V', 'OBKLAB',
                     '-o', str(ACTIVE / 'answer.iso'), str(ACTIVE / 'payload')], point + '-iso.log')
        command = ['qemu-system-x86_64', '-machine', 'pc,accel=kvm', '-cpu', 'host', '-smp', '4', '-m', '4096',
                   '-drive', f'file={ACTIVE/"windows.qcow2"},format=qcow2,if=ide,index=0,media=disk,cache=writeback',
                   '-drive', f'file={ACTIVE/"answer.iso"},format=raw,if=ide,index=2,media=cdrom,readonly=on',
                   '-nic', 'none', '-chardev', f'file,id=serial,path={evidence/"serial.log"},append=on',
                   '-serial', 'chardev:serial', '-monitor', f'unix:{ACTIVE/"monitor.sock"},server,nowait',
                   '-display', 'none', '-no-user-config', '-no-reboot', '-boot', 'order=c']
        (evidence / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
        (evidence / 'serial.log').write_text('')
        serial = ''; offset = 0; power_cut = False; prepared = False; boots = 1
        def set_phase(phase):
            (payload / 'boundary-phase.txt').write_text(phase)
            run_command(['xorriso', '-as', 'mkisofs', '-quiet', '-J', '-R', '-V', 'OBKLAB',
                         '-o', str(ACTIVE / 'answer.iso'), str(ACTIVE / 'payload')], point + '-' + phase + '-iso.log')
        with (evidence / 'qemu.log').open('w') as log:
            process = subprocess.Popen(command, stdout=log, stderr=log)
            started = time.monotonic(); next_budget = 0; failure = None
            try:
                while True:
                    now = time.monotonic()
                    if now >= next_budget:
                        budget(); next_budget = now + 25
                    serial = (evidence / 'serial.log').read_text(errors='replace')
                    if len(serial) > offset:
                        print(serial[offset:], end='', flush=True); offset = len(serial)
                    if 'H03-BOUNDARY-FAIL' in serial:
                        raise RuntimeError('guest failed; preserve logs before retry')
                    if not power_cut and 'H03-BOUNDARY-CUT ' in serial:
                        cuts = [line.split('H03-BOUNDARY-CUT ', 1)[1] for line in serial.splitlines() if 'H03-BOUNDARY-CUT ' in line]
                        cut = json.loads(cuts[-1])
                        if cut['point'] != point:
                            raise RuntimeError('unexpected cut point')
                        process.kill(); process.wait(timeout=15)
                        (evidence / 'cut.json').write_text(json.dumps({'point': point, 'signal': 'SIGKILL', 'checkpoint': cut}, indent=2) + '\n')
                        power_cut = True
                        if args.suite == 'namespace':
                            (payload / 'namespace-expected.json').write_text(json.dumps(cut,indent=2)+'\n')
                        set_phase('verify')
                        boots += 1
                        process = subprocess.Popen(command, stdout=log, stderr=log)
                    if process.poll() is not None:
                        if not prepared and f'H03-BOUNDARY-PREPARED point={point}' in serial:
                            prepared = True
                            set_phase('cut')
                            boots += 1
                            process = subprocess.Popen(command, stdout=log, stderr=log)
                            continue
                        break
                    if now > started + 900:
                        raise RuntimeError('VM timeout')
                    time.sleep(.2)
            except BaseException as error:
                failure = error
            finally:
                if process.poll() is None:
                    monitor.send(str(ACTIVE / 'monitor.sock'), 'system_powerdown')
                    try:
                        process.wait(timeout=45)
                    except subprocess.TimeoutExpired:
                        monitor.send(str(ACTIVE / 'monitor.sock'), 'quit'); process.wait(timeout=15)
                serial = (evidence / 'serial.log').read_text(errors='replace')
                complete = (f'H03-NAMESPACE-VALIDATE-PASS point={point}' if args.suite == 'namespace'
                            else f'H03-CONSOLIDATION-VALIDATE-PASS point={point}' if args.suite == 'consolidation'
                            else f'H03-BOUNDARY-RETRY-PASS point={point}')
                passed = prepared and power_cut and f'H03-BOUNDARY-BASELINE-PASS point={point}' in serial and f'H03-BOUNDARY-CASE-PASS point={point}' in serial and complete in serial and 'H03-BOUNDARY-FAIL' not in serial
                (evidence / 'result.json').write_text(json.dumps({'pass': passed, 'power_cut': power_cut, 'baseline_cold_boot': prepared, 'boots':boots, 'qemu_exit': process.returncode, 'dense_tb_acceptance': False}, indent=2) + '\n')
                if failure is not None:
                    (evidence / 'runner-error.txt').write_text(f'{type(failure).__name__}: {failure}\n')
        run_command(['qemu-img', 'check', str(ACTIVE / 'windows.qcow2')], point + '-container-check.log')
        extract(point)
        pack()
        for path in (ACTIVE / 'windows.qcow2', ACTIVE / 'answer.iso', ACTIVE / 'monitor.sock'):
            path.unlink(missing_ok=True)
        shutil.rmtree(ACTIVE / 'payload')
        budget()
        if not passed:
            raise SystemExit('case failed; evidence archived and temporary data removed')
    (EVIDENCE / 'all-results.json').write_text(json.dumps({'points': list(selected), 'pass': True, 'dense_tb_acceptance': False}, indent=2) + '\n')


if __name__ == '__main__':
    main()
