#!/usr/bin/env python3
"""Package public acceptance evidence; never include guest disks or key files."""
import hashlib
import io
import json
import re
import subprocess
import tarfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
WORK = REPO / "tmp/unattended-acceptance-2026-10-07"
OUTPUT = REPO / "docs/WINDOWS-AUTONOMOUS-EVIDENCE-2026-10-08.tar.gz"
ROOT = "autonomous-acceptance-2026-10-08"


def main():
    files = {}
    allowed = {".json", ".jsonl", ".log", ".png", ".out", ".err"}
    for name in ["active", "failed-harness-01", "installer-active",
                 "installer-attempt1", "installer-attempt2",
                 "consolidation-active", "consolidation-attempt0",
                 "consolidation-attempt1", "consolidation-attempt2",
                 "consolidation-retry", "winpe-active"]:
        parent = WORK / name
        for evidence in parent.glob("evidence*"):
            if evidence.is_dir():
                for file in evidence.rglob("*"):
                    if file.is_file() and file.suffix in allowed:
                        files[str(file.relative_to(WORK))] = file
    for folder in [WORK / "shots", WORK / "winpe"]:
        for file in folder.glob("*"):
            if file.is_file() and file.suffix in allowed:
                files[str(file.relative_to(WORK))] = file
    for pattern in ["*controller*.log", "*build*.log", "*regression.log",
                    "*-parse-final.log"]:
        for file in WORK.glob(pattern):
            files[file.name] = file
    if (WORK / "final-artifacts.json").exists():
        files["final-artifacts.json"] = WORK / "final-artifacts.json"
    untracked = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard"], cwd=REPO,
        text=True).splitlines()
    for name in untracked:
        file = REPO / name
        if ((name.startswith("scripts/lab/") and file.suffix in {".py", ".ps1"})
                or name in {"crates/client/examples/audit_autonomous.rs",
                            "crates/backup/examples/audit_winpe_recovery.rs"}):
            files["sources/" + name] = file
    # These explicit entries also work after the fixtures have been committed.
    for file in (REPO / "scripts/lab").glob("*"):
        if file.name.startswith(("acceptance-", "installer-extra-", "run-autonomous-",
                                 "run-installer-extra", "run-encrypted-consolidation",
                                 "run-winpe-recovery", "package-autonomous-evidence")):
            if file.suffix in {".py", ".ps1"}:
                files["sources/" + str(file.relative_to(REPO))] = file
    for name in ["scripts/lab/qemu-monitor.py", "scripts/lab/qemu-screenshot.py",
                 "scripts/lab/run-h03-iso-build.py",
                 "crates/client/examples/audit_autonomous.rs",
                 "crates/backup/examples/audit_winpe_recovery.rs",
                 "docs/WINDOWS-AUTONOMOUS-TESTS-2026-10-08.md"]:
        files["sources/" + name] = REPO / name
    payload = {name: file.read_bytes() for name, file in sorted(files.items())}
    for name, data in payload.items():
        if not name.startswith("sources/") and re.search(rb"obkr1[a-z0-9]{40,}", data):
            raise SystemExit(f"portable key detected in evidence: {name}")
    manifest = {name: {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}
                for name, data in payload.items()}
    payload["SHA256-MANIFEST.json"] = (json.dumps(manifest, indent=2) + "\n").encode()
    payload["README.txt"] = (
        "Accepted final runs: active/evidence, installer-active/evidence, "
        "consolidation-retry/evidence, winpe-active/evidence.\n"
        "Earlier evidence-attempt* and other attempt folders include failures; "
        "they are retained for diagnosis, not counted as PASS.\n"
        "Main run resumes across attempts; consult the report for each assertion.\n"
        "No VM disks, ISO, protected key stores, password/key text files or "
        "owner runtime data are included. Source fixtures use synthetic secrets.\n"
    ).encode()
    with tarfile.open(OUTPUT, "w:gz") as archive:
        for name, data in payload.items():
            item = tarfile.TarInfo(ROOT + "/" + name)
            item.size = len(data)
            item.mode = 0o644
            archive.addfile(item, io.BytesIO(data))
    with OUTPUT.open("rb") as file:
        digest = hashlib.file_digest(file, "sha256").hexdigest()
    OUTPUT.with_suffix(OUTPUT.suffix + ".sha256").write_text(f"{digest}  {OUTPUT.name}\n")
    print(f"EVIDENCE-PACKAGED files={len(manifest)} bytes={OUTPUT.stat().st_size} sha256={digest}")


if __name__ == "__main__":
    main()
