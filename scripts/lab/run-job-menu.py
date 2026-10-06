#!/usr/bin/env python3
"""The menu of a job, on a real Windows: the service's half and the window's.

    tmp/sync/build-win.sh               # the four exes, with the frontend built
    scripts/lab/run-job-menu.py         # about fifteen minutes
    scripts/lab/run-job-menu.py --resume    # the window's half again

Three boots of overlays on the laboratory's Windows and its data disk
(~/lab/resume-h02), which are never written to:

1. `job-menu-setup.ps1`, as SYSTEM: update the service and arrange console login.
2. `job-menu-stage.ps1`, as the active-console user: two jobs with real
   chains, then versions / verify / export / clean through the command line,
   every answer compared with the files. A saved VHDX is attached by Windows
   itself and the data in it hashed.
3. `job-menu-user.ps1`, in a desktop session: the same menu in the window,
   pressed through UI Automation and the mouse, each result looked for outside
   the window.

The guest asks for its photographs: a serial line `HOST shot=<name> keys=<k>`
is answered here with a screenshot into the evidence folder and those keys on
the virtual keyboard. The screenshots are for a person to look at.

The disks are saved after the CLI stage, and `--resume` starts from those
copies with the scripts and the exes as they are now. Everything is kept in
~/lab/active-job-menu; delete it before the next full run.
"""
import base64
import importlib.util
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB = Path.home() / "lab"
ACTIVE = LAB / "active-job-menu"
EVIDENCE = ACTIVE / "evidence"
BASE = LAB / "resume-h02"
RELEASE = REPO / "target" / "x86_64-pc-windows-msvc" / "release"
SCRIPTS = REPO / "scripts" / "lab"
MESSAGES = REPO / "crates" / "gui" / "frontend" / "messages" / "ru.json"
DISKS = ("windows", "data")

# What the guest calls a thing -> the message the window shows for it. A
# `:prefix` takes the sentence up to its first number.
LABELS = {
    "menu": "bsm_menu", "rename": "bsm_rename", "verify_latest": "bsm_verify_latest",
    "verify_all": "bsm_verify_all", "clean": "bsm_clean", "clone": "bsm_clone",
    "mount": "bsm_mount", "open": "bsm_open", "export": "bsm_export", "delete": "bsm_delete",
    "clean_title": "jvc_title", "delete_button": "bs_delete", "deleted_prefix": "jvc_deleted:prefix",
    "close": "common_close", "cancel": "common_cancel", "save": "common_save",
    "verify_latest_title": "jvr_title_latest", "verify_all_title": "jvr_title_all",
    "verify_ok_prefix": "jvr_ok:prefix", "export_title": "jex_title",
    "verify_bad_prefix": "jvr_bad:prefix",
    "export_done_prefix": "jex_done:prefix", "mount_title": "jmt_title",
    "mount_button": "tools_mount_btn", "tools_title": "tools_title", "nav_backup": "nav_backup",
    "rename_title": "bsm_rename_title", "other_disk": "wiz_src_other_disk",
    "delete_title": "bs_delete_title",
}


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def labels():
    messages = json.loads(MESSAGES.read_text(encoding="utf-8"))
    out = {}
    for name, key in LABELS.items():
        key, _, how = key.partition(":")
        text = messages[key]
        out[name] = text.split("{")[0] if how == "prefix" else text
    return out


def build_media():
    """The CD the image's boot task copies into C:\\Lab at every start."""
    shutil.rmtree(ACTIVE / "payload", ignore_errors=True)
    payload = ACTIVE / "payload" / "lab"
    payload.mkdir(parents=True)
    for source, name in [
        (RELEASE / "own-backup-cli.exe", "own-backup-cli.exe"),
        (RELEASE / "own-backup-gui.exe", "own-backup-gui.exe"),
        # Under a name the image does not have: its own service runs from
        # C:\Lab\own-backup-service.exe, so a file of that name is locked when
        # the media is copied over. The stage script updates the service from
        # this copy.
        (RELEASE / "own-backup-service.exe", "own-backup-service-menu.exe"),
        # The image's boot task runs whatever is called lab-stage2.ps1.
        (SCRIPTS / "job-menu-setup.ps1", "lab-stage2.ps1"),
        (SCRIPTS / "job-menu-stage.ps1", "job-menu-stage.ps1"),
        (SCRIPTS / "job-menu-user.ps1", "job-menu-user.ps1"),
        (SCRIPTS / "autounattend.xml", "autounattend.xml"),
        (LAB / "media" / "WebView2Runtime-x64.exe", "WebView2Runtime-x64.exe"),
    ]:
        shutil.copyfile(source, payload / name)
    (payload / "job-menu-labels.json").write_text(
        json.dumps(labels(), ensure_ascii=False, indent=1), encoding="utf-8"
    )
    subprocess.run(
        ["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB",
         "-o", str(ACTIVE / "answer.iso"), str(ACTIVE / "payload")],
        check=True, stderr=subprocess.DEVNULL,
    )


def main():
    resume = "--resume" in sys.argv[1:]
    if any(path != ACTIVE for path in LAB.glob("active-*")):
        raise SystemExit("another laboratory run is in ~/lab: preserve and clean it first")
    if subprocess.run(["pgrep", "-f", "^qemu-system-"], capture_output=True).returncode == 0:
        raise SystemExit("a laboratory VM is already running")
    needed = [RELEASE / f"own-backup-{part}.exe" for part in ("cli", "gui", "service")]
    for path in needed + [BASE / "windows.qcow2", BASE / "data.qcow2", LAB / "media" / "WebView2Runtime-x64.exe"]:
        if not path.exists():
            raise SystemExit(f"missing: {path}")
    if shutil.disk_usage(LAB).free < 30 * 2**30:
        raise SystemExit("less than 30 GiB free under ~/lab")

    if resume:
        if not (ACTIVE / "after-boot1-windows.qcow2").exists():
            raise SystemExit("nothing to resume: a full run saves the disks after the CLI stage")
        earlier = len(list(ACTIVE.glob("evidence*")))
        if EVIDENCE.exists():
            EVIDENCE.rename(ACTIVE / f"evidence-{earlier}")
        for name in DISKS:
            shutil.copyfile(ACTIVE / f"after-boot1-{name}.qcow2", ACTIVE / f"{name}.qcow2")
    else:
        if ACTIVE.exists():
            raise SystemExit(f"{ACTIVE} is from the previous run: look at it, then delete it")
        ACTIVE.mkdir()
        for name in DISKS:
            subprocess.run(
                ["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2",
                 "-b", str(BASE / f"{name}.qcow2"), str(ACTIVE / f"{name}.qcow2")],
                check=True,
            )
    EVIDENCE.mkdir()
    build_media()

    monitor_socket = ACTIVE / "monitor.sock"
    serial_log = EVIDENCE / "serial.log"
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096"]
    for index, name in enumerate(DISKS):
        command += ["-drive", f"file={ACTIVE / (name + '.qcow2')},format=qcow2,if=ide,index={index},media=disk"]
    command += [
        "-drive", f"file={ACTIVE / 'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
        # Keep the same isolated adapter as the iSCSI acceptance stands, so
        # Windows has its network stack configured even for a loopback target.
        "-netdev", "user,id=n0,restrict=on", "-device", "e1000,netdev=n0",
        "-chardev", f"file,id=serial,path={serial_log},append=on", "-serial", "chardev:serial",
        "-monitor", f"unix:{monitor_socket},server,nowait",
        "-display", "none", "-no-user-config", "-no-reboot", "-boot", "order=c",
    ]
    serial_log.write_text("")
    monitor = load("qemu-monitor")
    screenshot = SCRIPTS / "qemu-screenshot.py"

    problems = []
    lines_seen = 0
    phase = "user" if resume else "setup"
    with (EVIDENCE / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline = time.monotonic() + 60 * 60
        try:
            while True:
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
                    elif line.startswith("MENU-TEXT "):
                        # Text the guest could not put on a serial line as it is.
                        label, _, encoded = line[len("MENU-TEXT "):].partition(" ")
                        print(f"    [{label}] {base64.b64decode(encoded).decode('utf-16-le')!r}", flush=True)
                lines_seen = len(lines)
                if process.poll() is not None:
                    if phase == "setup" and "MENU-SETUP-READY REBOOT" in text and "FAIL" not in text:
                        process = subprocess.Popen(command, stdout=log, stderr=log)
                        phase = "stage"
                        continue
                    if phase == "stage" and "MENU-STAGE-READY REBOOT" in text and "FAIL" not in text:
                        for name in DISKS:
                            shutil.copyfile(ACTIVE / f"{name}.qcow2", ACTIVE / f"after-boot1-{name}.qcow2")
                        process = subprocess.Popen(command, stdout=log, stderr=log)
                        phase = "user"
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
    if "MENU-PASS" not in text:
        problems.append("the guest did not report MENU-PASS")
    problems += [line.strip() for line in text.split("\n") if "FAIL" in line]
    print()
    print(f"checks passed: {text.count('MENU-OK ')}; screenshots: {EVIDENCE}")
    if problems:
        raise SystemExit("JOB MENU LAB FAIL\n  " + "\n  ".join(problems))
    print("JOB MENU LAB PASS — now look at the screenshots")


if __name__ == "__main__":
    main()
