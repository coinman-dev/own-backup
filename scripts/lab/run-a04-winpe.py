#!/usr/bin/env python3
"""Run A04 against the saved Setup PE and portable H03 archive, with no hard disk.

Build audit_winpe_a04.exe for x86_64-pc-windows-msvc first. Evidence stays in
~/lab/active-a04-winpe/evidence; preserve it before removing the temporary ISO/WIM.
"""
import argparse
import hashlib
import importlib.util
import json
import shutil
import subprocess
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("prepare", "run"))
    parser.add_argument("--timeout", type=int, default=420)
    args = parser.parse_args()
    running = subprocess.run(["pgrep", "-f", "^qemu-system-"], capture_output=True, text=True)
    if running.returncode == 0:
        raise SystemExit("stop the existing QEMU VM before preparing or running A04")
    repo = Path(__file__).resolve().parents[2]
    lab = Path.home() / "lab"
    active = lab / "active-a04-winpe"
    evidence = active / "evidence"
    evidence.mkdir(parents=True, exist_ok=True)
    if any(p != active for p in lab.glob("active-*")):
        raise SystemExit("preserve and clean the other active run first")

    def used(path):
        return int(subprocess.check_output(["du", "-s", "-B1", str(path)], text=True).split()[0])

    def budget():
        record = {"utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  "wsl_free": shutil.disk_usage(repo).free, "target": used(repo / "target"),
                  "lab": used(lab), "active": used(active)}
        with (evidence / "budgets.jsonl").open("a") as log:
            log.write(json.dumps(record) + "\n")
        if (record["wsl_free"] < 30*2**30 or record["target"] > 30*2**30
                or record["lab"] > 65*2**30 or record["active"] > 20*2**30):
            raise RuntimeError("space budget exceeded")

    def run(command, name, stdin=None):
        budget()
        with (evidence / name).open("w") as log:
            process = subprocess.Popen(command, stdin=stdin, stdout=log, stderr=log)
            try:
                while True:
                    try:
                        code = process.wait(timeout=30)
                        break
                    except subprocess.TimeoutExpired:
                        budget()
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=15)
            if code:
                raise RuntimeError(f"{command[0]} failed: see {name}")

    if args.mode == "prepare":
        original = lab / "media/winpe-h03-restore.iso"
        exe = repo / "target/x86_64-pc-windows-msvc/release/examples/audit_winpe_a04.exe"
        if not exe.is_file():
            raise SystemExit("build audit_winpe_a04.exe first")
        wim = active / "boot.wim"
        if not wim.exists():
            run(["xorriso", "-osirrox", "on", "-indev", str(original), "-extract",
                 "/sources/boot.wim", str(wim)], "extract-wim.log")
        ini = active / "winpeshl.ini"
        ini.write_bytes(("[LaunchApps]\r\n%SYSTEMROOT%\\System32\\audit_winpe_a04.exe, --register-initiator\r\n"
                         "%SYSTEMROOT%\\System32\\cmd.exe, /k type X:\\audit-a04.log\r\n").encode())
        resources = active / "initiator-files"
        names = ("iscsidsc.dll", "iscsiexe.dll", "iscsium.dll", "iscsiwmi.dll", "iscsilog.dll", "iscsicli.exe")
        sevenzip = lab / "tools/usr/lib/7zip/7z"
        if not resources.exists():
            run([str(sevenzip), "x", "-y", f"-o{resources}", str(lab / "win-lab.qcow2")]
                + [f"Windows/System32/{name}" for name in names], "extract-initiator.log")
        commands = active / "wim-update.txt"
        commands.write_text(f'add "{exe}" /Windows/System32/audit_winpe_a04.exe\n'
                            f'add "{ini}" /Windows/System32/winpeshl.ini\n'
                            + "".join(f'add "{resources / "Windows/System32" / name}" /Windows/System32/{name}\n' for name in names))
        with commands.open("rb") as stdin:
            run(["wimlib-imagex", "update", str(wim), "2"], "wim-update.log", stdin)
        raw = active / "manual-probe.raw"
        with raw.open("wb") as file:
            file.write(b"OBK-MANUAL-CHAP-CONTROL")
            file.truncate(8*2**20)
        qcow = active / "manual-probe.qcow2"
        run(["qemu-img", "convert", "-f", "raw", "-O", "qcow2", str(raw), str(qcow)], "fixture-build.log")
        raw.unlink()
        marker = active / "a04-media.marker"
        marker.write_text("Disposable A04 WinPE media. No installed Windows or physical hard disk.\n")
        output = active / "output.img"
        with output.open("wb") as file:
            file.truncate(32*2**20)
        mformat = shutil.which("mformat") or str(lab / "tools/usr/bin/mformat")
        mcopy = shutil.which("mcopy") or str(lab / "tools/usr/bin/mcopy")
        run([mformat, "-i", str(output), "-v", "OBKA04LOG", "::"], "output-format.log")
        run([mcopy, "-i", str(output), str(marker), "::a04-output.marker"], "output-marker.log")
        tree = active / "iso-tree"
        if not tree.exists():
            run(["xorriso", "-osirrox", "on", "-indev", str(original), "-extract",
                 "/", str(tree)], "extract-tree.log")
        # Reuse the edited WIM's blocks rather than retaining a second copy.
        old_wim = tree / "sources/boot.wim"
        old_wim.unlink(missing_ok=True)
        old_wim.hardlink_to(wim)
        for source in (qcow, marker):
            shutil.copyfile(source, tree / source.name)
        builder = shutil.which("genisoimage") or str(lab / "tools/usr/bin/genisoimage")
        if not Path(builder).is_file():
            raise SystemExit("genisoimage required to preserve the Windows CD's UDF filesystem")
        iso = active / "winpe.iso"
        if iso.exists():
            iso.unlink()
        run([builder, "-quiet", "-udf", "-J", "-joliet-long", "-R", "-V", "OBKA04PE",
             "-b", "boot/etfsboot.com", "-no-emul-boot", "-boot-load-size", "8",
             "-eltorito-alt-boot", "-e", "efi/microsoft/boot/efisys.bin", "-no-emul-boot",
             "-o", str(iso), str(tree)], "iso-build.log")
        (evidence / "payload-hashes.json").write_text(json.dumps({str(p): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in (exe, ini, qcow, iso, original, lab / "media/h03-archive-restore.iso")}, indent=2)+"\n")
        budget()
        print(f"prepared {iso}")
        return

    spec = importlib.util.spec_from_file_location("monitor", repo / "scripts/lab/qemu-monitor.py")
    monitor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(monitor)
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096"]
    for index, iso in ((2, active / "winpe.iso"), (3, lab / "media/h03-archive-restore.iso")):
        command += ["-drive", f"file={iso},format=raw,if=ide,index={index},media=cdrom,readonly=on"]
    command += ["-netdev", "user,id=n0,restrict=on", "-device", "e1000,netdev=n0", "-serial",
                f"file:{evidence/'serial.log'}", "-monitor", f"unix:{active/'monitor.sock'},server,nowait",
                "-display", "none", "-no-user-config", "-boot", "order=d"]
    command += ["-usb", "-drive", f"file={active/'output.img'},format=raw,if=none,id=a04log",
                "-device", "usb-storage,drive=a04log,removable=on"]
    (evidence / "command.json").write_text(json.dumps(command, indent=2)+"\n")
    budget()
    with (evidence / "qemu.log").open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        started = time.monotonic()
        deadline, next_budget, next_key, offset = started+args.timeout, 0, started+1, 0
        serial = ""
        try:
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_key and now < started+12:
                    monitor.send(str(active / "monitor.sock"), "sendkey ret")
                    next_key = now+2
                if now >= next_budget:
                    budget()
                    next_budget = now+30
                path = evidence / "serial.log"
                serial = path.read_text(errors="replace") if path.exists() else ""
                extracted = subprocess.run([str(lab / "tools/usr/lib/7zip/7z"), "x", "-so",
                    str(active / "output.img"), "audit-a04.log"], capture_output=True)
                if extracted.returncode == 0 and extracted.stdout:
                    candidate = extracted.stdout.decode(errors="replace")
                    # During the guest's copy, FAT may expose a directory entry
                    # before all its data sectors are flushed. Wait for the complete log.
                    if candidate.startswith("WINPE-A04-BEGIN") and "\x00" not in candidate and "WINPE-A04-DONE" in candidate:
                        serial = candidate
                        (evidence / "guest.log").write_text(serial)
                if len(serial) > offset:
                    print(serial[offset:], end="", flush=True)
                    offset = len(serial)
                if "WINPE-A04-DONE" in serial:
                    run(["python3", str(repo / "scripts/lab/qemu-screenshot.py"),
                         str(active / "monitor.sock"), str(evidence / "screen.png")], "screenshot.log")
                    break
                if now > deadline:
                    raise RuntimeError("VM timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                if "WINPE-A04-DONE" in serial:
                    # The probe requests an orderly PE shutdown after its screenshot window.
                    try:
                        process.wait(timeout=20)
                    except subprocess.TimeoutExpired:
                        pass
                # The only writable drive is the disposable USB evidence volume.
                if process.poll() is None:
                    monitor.send(str(active / "monitor.sock"), "quit")
                    process.wait(timeout=15)
            run([str(lab / "tools/usr/lib/7zip/7z"), "x", "-y", f"-o{evidence/'guest-files'}",
                 str(active / "output.img")], "extract-output.log")
            guest_log = evidence / "guest-files/audit-a04.log"
            if guest_log.exists():
                serial = guest_log.read_text(errors="replace")
                (evidence / "guest.log").write_text(serial)
            (evidence / "result.json").write_text(json.dumps({"exit_code": process.returncode,
                "pass": "WINPE-A04-PASS" in serial, "done": "WINPE-A04-DONE" in serial}, indent=2)+"\n")
            budget()
    if "WINPE-A04-PASS" not in serial:
        raise SystemExit("A04 FAIL; preserve evidence before retrying")


if __name__ == "__main__":
    main()
