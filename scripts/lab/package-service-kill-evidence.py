#!/usr/bin/env python3
"""Package sanitized continued stand evidence and verify each accepted result."""
import hashlib
import io
import json
import re
import tarfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
OUTPUT = REPO / 'docs/WINDOWS-SERVICE-KILL-EVIDENCE-2026-10-08.tar.gz'
PREFIX = 'service-kill-2026-10-08/'
SUITES = ['service-kill', 'publication-check']


def main():
    payload = {}
    allowed = {'.json', '.jsonl', '.log', '.png', '.out', '.err'}
    for suite in SUITES:
        work = REPO / f'tmp/{suite}-2026-10-08'
        result = json.loads((work / 'evidence/result.json').read_text())
        if not result['pass']:
            raise SystemExit(f'final scenario is not accepted: {suite}')
        for evidence in work.glob('evidence*'):
            if evidence.is_dir():
                for file in evidence.rglob('*'):
                    if file.is_file() and file.suffix in allowed:
                        payload[suite + '/' + str(file.relative_to(work))] = file.read_bytes()
        for file in work.glob('*'):
            if file.is_file() and file.suffix in allowed:
                payload[suite + '/' + file.name] = file.read_bytes()
    sources = [
        'crates/backup/src/chains.rs',
        'crates/backup/src/engine_win.rs',
        'crates/backup/src/seal/context.rs',
        'crates/backup/src/seal/obk/new_image.rs',
        'crates/backup/src/seal/obk/staging.rs',
        'crates/backup/src/seal/obk/listing.rs',
        'crates/client/examples/audit_autonomous.rs',
        'scripts/lab/acceptance-common.ps1',
        'scripts/lab/run-obk-interruptions.py',
        'scripts/lab/acceptance-service-kill-stage.ps1',
        'scripts/lab/acceptance-service-kill-user.ps1',
        'scripts/lab/acceptance-publication-check-stage.ps1',
        'scripts/lab/acceptance-publication-check-user.ps1',
        'scripts/lab/package-service-kill-evidence.py',
        'docs/WINDOWS-SERVICE-KILL-2026-10-08.md',
    ]
    for name in sources:
        payload['sources/' + name] = (REPO / name).read_bytes()
    for name, data in payload.items():
        if not name.startswith('sources/') and re.search(rb'obkr1[a-z0-9]{40,}', data):
            raise SystemExit(f'portable secret detected in evidence: {name}')
    manifest = {name: {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data)}
                for name, data in sorted(payload.items())}
    payload['SHA256-MANIFEST.json'] = (json.dumps(manifest, indent=2) + '\n').encode()
    payload['README.txt'] = (
        'Accepted current runs are each suite/evidence. Earlier evidence-attempt* '
        'folders retain failures: initial fixtures and the confirmed pre-fix orphan.\n'
        'Consult the report for scope and limits. No legacy orphan is blindly deleted.\n'
        'No guest disks, ISO, DPAPI blobs, password/recovery text files or owner '
        'runtime data are included. Source passwords are synthetic fixtures.\n'
    ).encode()
    with tarfile.open(OUTPUT, 'w:gz') as archive:
        for name, data in sorted(payload.items()):
            item = tarfile.TarInfo(PREFIX + name)
            item.size = len(data)
            item.mode = 0o644
            archive.addfile(item, io.BytesIO(data))
    with tarfile.open(OUTPUT) as archive:
        for name, expected in manifest.items():
            data = archive.extractfile(PREFIX + name).read()
            assert len(data) == expected['bytes']
            assert hashlib.sha256(data).hexdigest() == expected['sha256']
    with OUTPUT.open('rb') as file:
        digest = hashlib.file_digest(file, 'sha256').hexdigest()
    OUTPUT.with_suffix(OUTPUT.suffix + '.sha256').write_text(f'{digest}  {OUTPUT.name}\n')
    print(f'SERVICE-KILL-EVIDENCE-PASS files={len(manifest)} sha256={digest}')


if __name__ == '__main__':
    main()
