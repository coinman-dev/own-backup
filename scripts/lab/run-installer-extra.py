#!/usr/bin/env python3
"""Install, update and remove Own Backup with its setup program in a throwaway Windows.

    scripts/build-installer.sh              # target/dist/own-backup-install.exe
    scripts/lab/run-installer.py            # about twelve minutes
    scripts/lab/run-installer.py --resume   # the desktop half again, three minutes

Two boots of an overlay on the laboratory's Windows (~/lab/resume-h02), which
is never written to:

1. `installer-stage.ps1`, as SYSTEM with nobody logged on: silent install over
   the image's own service, update, removal, a developer copy registered from
   its own folder, a folder with a Cyrillic name.
2. `installer-user.ps1`, in a desktop session: removal and installation through
   the wizard, then the window from its last page.

The wizard is elevated and the guest script is not, so the guest asks for its
key presses: a serial line `HOST shot=<name> keys=<k1,k2>` is answered here with
a screenshot into the evidence folder and those keys on the virtual keyboard.
The screenshots are the part a person has to look at — they are the only proof
that the wizard's Russian text is Russian. Window captions are checked here.

The disk is saved between the two boots, and `--resume` starts from that copy
with the scripts and the installer as they are now. Everything is kept in
~/lab/active-installer; delete it before the next full run.
"""
import os
import json
import base64
import importlib.util
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB = Path.home() / "lab"
WORK = REPO / "tmp" / "unattended-acceptance-2026-10-07"
ACTIVE = WORK / "installer-active"
EVIDENCE = ACTIVE / "evidence"
BASE = LAB / "resume-h02" / "windows.qcow2"
INSTALLER = WORK / "own-backup-install.exe"
RELEASE = REPO / "target" / "x86_64-pc-windows-msvc" / "release"
SCRIPTS = REPO / "scripts" / "lab"

CYRILLIC_DIR = "C:\\Программы\\Own Backup"
# Text the guest read from Windows itself, sent as base64 of UTF-16 because the
# serial port is not Unicode. A caption that arrives as anything else was
# compiled from a script read in the wrong code page.
EXPECTED = {
    "uninstall": "Удаление Own Backup",
    "welcome": "Установка Own Backup",
    "installed": "Установка Own Backup",
    "service-after-reboot": f'"{CYRILLIC_DIR}\\own-backup-service.exe"',
}


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def build_media():
    """The CD the image's boot task copies into C:\\Lab at every start."""
    version = next(
        line.split('"')[1]
        for line in (REPO / "Cargo.toml").read_text().splitlines()
        if line.startswith("version")
    )
    shutil.rmtree(ACTIVE / "payload", ignore_errors=True)
    payload = ACTIVE / "payload" / "lab"
    payload.mkdir(parents=True)
    for source, name in [
        (INSTALLER, "own-backup-install.exe"),
        # The developer copy of test 4, under a name the image does not have:
        # its own service runs from C:\Lab\own-backup-service.exe, so a file of
        # that name is locked when the media is copied over and the OLD build
        # stays — the first run here tested that one and failed for it.
        (RELEASE / "own-backup-service.exe", "own-backup-service-dev.exe"),
        # The image's boot task runs whatever is called lab-stage2.ps1.
        (SCRIPTS / "installer-extra-stage.ps1", "lab-stage2.ps1"),
        (SCRIPTS / "installer-extra-user.ps1", "installer-extra-user.ps1"),
        (SCRIPTS / "autounattend.xml", "autounattend.xml"),
        (LAB / "media" / "WebView2Runtime-x64.exe", "WebView2Runtime-x64.exe"),
    ]:
        shutil.copyfile(source, payload / name)
    (payload / "installer-version.txt").write_text(version + "\n")
    subprocess.run(
        ["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB",
         "-o", str(ACTIVE / "answer.iso"), str(ACTIVE / "payload")],
        check=True, stderr=subprocess.DEVNULL,
    )


def main():
    resume = "--resume" in sys.argv[1:]
    disk = ACTIVE / "windows.qcow2"
    between = ACTIVE / "after-boot1.qcow2"
    if any(path != ACTIVE for path in LAB.glob("active-*")):
        raise SystemExit("another laboratory run is in ~/lab: preserve and clean it first")
    if subprocess.run(["pgrep", "-f", "^qemu-system-"], capture_output=True).returncode == 0:
        raise SystemExit("a laboratory VM is already running")
    for needed in (INSTALLER, BASE, LAB / "media" / "WebView2Runtime-x64.exe"):
        if not needed.exists():
            raise SystemExit(f"missing: {needed}")
    if shutil.disk_usage(LAB).free < 30 * 2**30:
        raise SystemExit("less than 30 GiB free under ~/lab")

    if resume:
        if not between.exists():
            raise SystemExit(f"nothing to resume: {between} is made by a full run")
        earlier = len(list(ACTIVE.glob("evidence*")))
        if EVIDENCE.exists():
            EVIDENCE.rename(ACTIVE / f"evidence-{earlier}")
        shutil.copyfile(between, disk)
    else:
        if ACTIVE.exists():
            raise SystemExit(f"{ACTIVE} is from the previous run: look at it, then delete it")
        ACTIVE.mkdir()
        subprocess.run(
            ["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", str(BASE), str(disk)],
            check=True,
        )
    EVIDENCE.mkdir()
    scratch = WORK / "scratch"
    scratch.mkdir(exist_ok=True)
    os.environ["TMPDIR"] = str(scratch)
    bounds = load("run-h03-iso-build")
    def budget():
        bounds.host_budget(EVIDENCE)
        row = {"utc": time.time(), "free": shutil.disk_usage(REPO).free}
        for name, path in [("lab", LAB), ("work", WORK), ("active", ACTIVE), ("target", REPO / "target")]:
            row[name] = int(subprocess.check_output(["du", "-s", "-B1", str(path)]).split()[0])
        with (EVIDENCE / "budgets.jsonl").open("a") as log:
            log.write(json.dumps(row) + "\n")
        if row["free"] < 30 * 2**30 or row["lab"] + row["work"] > 65 * 2**30 or row["active"] > 20 * 2**30 or row["target"] > 30 * 2**30:
            raise RuntimeError("space budget exceeded")
    budget()
    build_media()

    monitor_socket = ACTIVE / "monitor.sock"
    serial_log = EVIDENCE / "serial.log"
    command = [
        "qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096",
        "-drive", f"file={disk},format=qcow2,if=ide,index=0,media=disk",
        "-drive", f"file={ACTIVE / 'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
        "-nic", "none",
        "-chardev", f"file,id=serial,path={serial_log},append=on", "-serial", "chardev:serial",
        "-monitor", f"unix:{monitor_socket},server,nowait",
        "-display", "none", "-no-user-config", "-no-reboot", "-boot", "order=c",
    ]
    serial_log.write_text("")
    monitor = load("qemu-monitor")
    screenshot = SCRIPTS / "qemu-screenshot.py"

    problems = []
    lines_seen = 0
    boots = 2 if resume else 1
    with (EVIDENCE / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline = time.monotonic() + 25 * 60
        next_budget = 0
        try:
            while True:
                if time.monotonic() >= next_budget:
                    budget()
                    next_budget = time.monotonic() + 25
                text = serial_log.read_text(errors="replace")
                lines = text.split("\n")[:-1]  # the last piece is a line still being written
                for line in (raw.strip() for raw in lines[lines_seen:]):
                    print(line, flush=True)
                    if line.startswith("HOST "):
                        fields = dict(part.split("=", 1) for part in line.split()[1:])
                        if fields.get("shot"):
                            subprocess.run(
                                [sys.executable, str(screenshot), str(monitor_socket),
                                 str(EVIDENCE / f"{fields['shot']}.png")],
                                check=True, stdout=subprocess.DEVNULL,
                            )
                        for key in filter(None, fields.get("keys", "").split(",")):
                            monitor.send(str(monitor_socket), f"sendkey {key}")
                            time.sleep(0.4)
                    elif line.startswith("INSTALLER-TEXT "):
                        label, _, encoded = line[len("INSTALLER-TEXT "):].partition(" ")
                        said = base64.b64decode(encoded).decode("utf-16-le")
                        expected = EXPECTED.get(label)
                        wrong = expected is not None and not said.startswith(expected)
                        print(f"    [{label}] {said!r}" + (f" — WRONG, expected {expected!r}" if wrong else ""),
                              flush=True)
                        if wrong:
                            problems.append(f"{label}: {said!r}, expected {expected!r}")
                lines_seen = len(lines)
                if process.poll() is not None:
                    if boots == 1 and "INSTALLER-EXTRA-REBOOT" in text and "FAIL" not in text:
                        process = subprocess.Popen(command, stdout=log, stderr=log)
                        boots += 1
                        continue
                    break
                if time.monotonic() > deadline:
                    problems.append("timeout: the guest did not power off")
                    break
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                subprocess.run(
                    [sys.executable, str(screenshot), str(monitor_socket), str(EVIDENCE / "timeout.png")],
                    stdout=subprocess.DEVNULL,
                )
                monitor.send(str(monitor_socket), "quit")
                process.wait(timeout=30)

    text = serial_log.read_text(errors="replace")
    if "INSTALLER-EXTRA-USER-PASS" not in text:
        problems.append("the guest did not report INSTALLER-PASS")
    problems += [line.strip() for line in text.split("\n") if "FAIL" in line]
    print()
    print(f"checks passed: {text.count('INSTALLER-OK ')}; screenshots: {EVIDENCE}")
    if problems:
        raise SystemExit("INSTALLER LAB FAIL\n  " + "\n  ".join(problems))
    print("INSTALLER LAB PASS — now look at the screenshots: the Russian text is for eyes")


if __name__ == "__main__":
    main()
