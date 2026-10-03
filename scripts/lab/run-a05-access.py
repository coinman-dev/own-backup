#!/usr/bin/env python3
"""A05: native token creation, console/RunAs access, actual RDP and console transfer."""
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path


def main():
    repo = Path(__file__).resolve().parents[2]
    lab = Path.home() / "lab"
    active = lab / "active-a05"
    evidence = active / "evidence"
    if any(p != active for p in lab.glob("active-*")):
        raise SystemExit("preserve and clean the other active run first")
    if subprocess.run(["pgrep", "-f", "^qemu-system-"], capture_output=True).returncode == 0:
        raise SystemExit("stop the existing VM first")
    if (active / "windows.qcow2").exists():
        raise SystemExit("preserve and clean the previous A05 attempt first")
    evidence.mkdir(parents=True, exist_ok=True)

    def used(path):
        return int(subprocess.check_output(["du", "-s", "-B1", str(path)], text=True).split()[0])

    def budget():
        record = {"utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "free": shutil.disk_usage(repo).free, "target": used(repo / "target"),
            "lab": used(lab), "active": used(active)}
        with (evidence / "budgets.jsonl").open("a") as log:
            log.write(json.dumps(record)+"\n")
        if record["free"] < 30*2**30 or record["target"] > 30*2**30 or record["lab"] > 65*2**30 or record["active"] > 20*2**30:
            raise RuntimeError("space budget exceeded")

    budget()
    payload = active / "payload/lab"
    payload.mkdir(parents=True)
    output = repo / "target/x86_64-pc-windows-msvc/release"
    tests = list((output / "deps").glob("own_backup_service-*.exe"))
    if len(tests) != 1:
        raise SystemExit("exactly one Windows service test binary required")
    for source, name in [(output / "own-backup-service.exe", "own-backup-service-a05.exe"),
                         (output / "examples/audit_a05_client.exe", "audit_a05_client.exe"),
                         (tests[0], "a05_service_windows_tests.exe"),
                         (lab / "resume-h02/media/lab/autounattend.xml", "autounattend.xml")]:
        shutil.copyfile(source, payload / name)
    for name in ("stage", "user", "watch"):
        dest = "lab-stage2.ps1" if name == "stage" else f"audit-a05-{name}.ps1"
        shutil.copyfile(repo / f"scripts/lab/audit-a05-{name}.ps1", payload / dest)
    # Do not place the lab's autologon password in hashes/evidence or host logs.
    (evidence / "payload-hashes.json").write_text(json.dumps({p.name:hashlib.sha256(p.read_bytes()).hexdigest()
        for p in payload.iterdir() if p.suffix != ".xml"}, indent=2)+"\n")
    subprocess.run(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b",
        str(lab / "resume-h02/windows.qcow2"), str(active / "windows.qcow2")], check=True)
    with (evidence / "iso-build.log").open("w") as log:
        subprocess.run(["xorriso", "-as", "mkisofs", "-quiet", "-J", "-R", "-V", "OBKLAB",
            "-o", str(active / "answer.iso"), str(active / "payload")], stdout=log, stderr=log, check=True)
    spec = importlib.util.spec_from_file_location("monitor", repo / "scripts/lab/qemu-monitor.py")
    monitor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(monitor)
    command = ["qemu-system-x86_64", "-machine", "pc,accel=kvm", "-cpu", "host", "-smp", "4", "-m", "4096",
        "-drive", f"file={active/'windows.qcow2'},format=qcow2,if=ide,index=0,media=disk",
        "-drive", f"file={active/'answer.iso'},format=raw,if=ide,index=2,media=cdrom,readonly=on",
        "-netdev", "user,id=n0,restrict=on,hostfwd=tcp:127.0.0.1:13389-:3389", "-device", "e1000,netdev=n0",
        "-serial", f"file:{evidence/'serial.log'}", "-monitor", f"unix:{active/'monitor.sock'},server,nowait",
        "-display", "none", "-no-user-config", "-boot", "order=c"]
    (evidence / "command.json").write_text(json.dumps(command, indent=2)+"\n")
    env = os.environ.copy()
    env["LD_LIBRARY_PATH"] = str(lab / "tools/usr/lib/x86_64-linux-gnu")
    env["DISPLAY"] = ":90"
    env["PATH"] = str(lab / "tools/usr/bin")+os.pathsep+env.get("PATH", "")
    # The unpacked Xvfb package embeds /usr/bin for xkbcomp. Use a private,
    # equally long path in a disposable copy; no root installation is needed.
    while True:
        xkb_dir = Path("/tmp") / uuid.uuid4().hex[:3]
        try:
            xkb_dir.mkdir(mode=0o700)
            break
        except FileExistsError:
            continue
    (xkb_dir / "xkbcomp").symlink_to(lab / "tools/usr/bin/xkbcomp")
    original_xvfb = (lab / "tools/usr/bin/Xvfb").read_bytes()
    assert len(str(xkb_dir)) == len("/usr/bin") and b"/usr/bin" in original_xvfb
    portable_xvfb = active / "Xvfb.portable"
    portable_xvfb.write_bytes(original_xvfb.replace(b"/usr/bin", str(xkb_dir).encode()))
    portable_xvfb.chmod(0o755)
    (evidence / "xvfb-portable.json").write_text(json.dumps({"original_sha256":hashlib.sha256(original_xvfb).hexdigest(),
        "portable_sha256":hashlib.sha256(portable_xvfb.read_bytes()).hexdigest(), "xkbcomp_dir":str(xkb_dir)}, indent=2)+"\n")
    rdp = xvfb = None
    with (evidence / "qemu.log").open("w") as log, (evidence / "rdp.log").open("w") as rdp_log, (evidence / "xvfb.log").open("w") as xvfb_log:
        process = subprocess.Popen(command, stdout=log, stderr=log)
        deadline, next_budget, offset = time.monotonic()+900, 0, 0
        serial = ""
        try:
            while process.poll() is None:
                now = time.monotonic()
                if now >= next_budget:
                    budget()
                    next_budget = now+30
                path = evidence / "serial.log"
                serial = path.read_text(errors="replace") if path.exists() else ""
                if len(serial) > offset:
                    print(serial[offset:], end="", flush=True)
                    offset = len(serial)
                if "HOST-RDP-READY" in serial and rdp is None:
                    xvfb = subprocess.Popen([str(portable_xvfb), ":90", "-screen", "0", "1024x768x24", "-ac", "-nolisten", "tcp"], env=env, stdout=xvfb_log, stderr=xvfb_log)
                    time.sleep(3)
                    args = active / "rdp-args.txt"
                    args.write_text("/v:127.0.0.1:13389\n/u:AuditA05Rdp\n/d:.\n/p:A05-Disposable-20261003!\n/cert:ignore\n/size:1024x768\n/log-level:WARN\n-clipboard\n/audio-mode:2\n")
                    args.chmod(0o600)
                    rdp = subprocess.Popen([str(lab / "tools/usr/bin/xfreerdp"), f"/args-from:file:{args}"], env=env, stdout=rdp_log, stderr=rdp_log)
                    print("HOST actual RDP client started on loopback-only forwarded port", flush=True)
                if "A05-PASS" in serial and not (evidence / "screen.png").exists():
                    subprocess.run(["python3", str(repo / "scripts/lab/qemu-screenshot.py"),
                        str(active / "monitor.sock"), str(evidence / "screen.png")], check=True)
                if now > deadline:
                    raise RuntimeError("A05 VM timeout")
                time.sleep(0.5)
        finally:
            if process.poll() is None:
                monitor.send(str(active / "monitor.sock"), "system_powerdown")
                try:
                    process.wait(timeout=45)
                except subprocess.TimeoutExpired:
                    monitor.send(str(active / "monitor.sock"), "quit")
                    process.wait(timeout=15)
            for child in (rdp, xvfb):
                if child is not None and child.poll() is None:
                    child.terminate()
                    child.wait(timeout=15)
            shutil.rmtree(xkb_dir)
            (evidence / "result.json").write_text(json.dumps({"qemu_exit":process.returncode,
                "rdp_started":rdp is not None, "rdp_exit":rdp.poll() if rdp else None,
                "pass":"A05-PASS" in serial, "done":"A05-DONE" in serial}, indent=2)+"\n")
            budget()
    with (evidence / "container-check.log").open("w") as log:
        subprocess.run(["qemu-img", "check", str(active / "windows.qcow2")], stdout=log, stderr=log, check=True)
    if "A05-PASS" not in serial or any(x in serial for x in ("A05-STAGE-FAIL", "A05-USER-FAIL", "A05-WATCH-FAIL")):
        raise SystemExit("A05 FAIL; preserve evidence and clean before retry")


if __name__ == "__main__":
    main()
