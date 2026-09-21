#!/usr/bin/env python3
"""Refresh the laboratory payload inside the VM's own filesystem.

The guest re-copies `C:\\Lab` from the answer CD at every boot, using a
bootstrap script that stage 1 wrote at INSTALL time. A fix to that bootstrap
therefore cannot reach a VM that is already installed — the only ways in are a
reinstall (40 minutes) or this: mount the powered-off disk and write the files.

Run it from WSL with the disk attached, e.g.

    sudo modprobe nbd max_part=8
    sudo qemu-nbd --connect=/dev/nbd0 ~/lab/win-lab.qcow2
    sudo mkdir -p /mnt/labwin && sudo ntfs-3g /dev/nbd0p1 /mnt/labwin
    sudo python3 scripts/lab/refresh-guest-lab.py /mnt/labwin <repo>

It writes only under `<mount>/Lab`.
"""

import pathlib
import re
import shutil
import sys

BOM = "\ufeff"


def bootstrap_from_stage1(stage1: pathlib.Path) -> str:
    """The here-string stage 1 installs as `lab-stage2-boot.ps1`."""
    text = stage1.read_text(encoding="utf-8")
    match = re.search(r"\$bootstrap = @'\r?\n(.*?)\r?\n'@", text, re.S)
    if not match:
        raise SystemExit("could not find the bootstrap here-string in lab-stage1.ps1")
    return match.group(1)


def write_guest_text(path: pathlib.Path, body: str) -> None:
    """As PowerShell 5.1 writes it: UTF-8 with a BOM and CRLF line endings."""
    crlf = body.replace("\r\n", "\n").replace("\n", "\r\n")
    path.write_bytes((BOM + crlf + "\r\n").encode("utf-8"))


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit("usage: refresh-guest-lab.py <ntfs mount> <repo root>")
    mount = pathlib.Path(sys.argv[1])
    repo = pathlib.Path(sys.argv[2])
    lab = mount / "Lab"
    if not lab.is_dir():
        raise SystemExit(f"{lab} is not there — is the guest disk mounted?")

    stage1 = repo / "scripts/lab/lab-stage1.ps1"
    write_guest_text(lab / "lab-stage2-boot.ps1", bootstrap_from_stage1(stage1))
    print(f"bootstrap  -> {lab / 'lab-stage2-boot.ps1'}")

    for name in ("lab-stage1.ps1", "lab-stage2.ps1", "lab-obk.ps1"):
        shutil.copyfile(repo / "scripts/lab" / name, lab / name)
        print(f"script     -> {lab / name}")

    probes = {
        "obk_live_probe.exe": "target/x86_64-pc-windows-msvc/debug/examples/obk_live_probe.exe",
        "seal_store_service.exe": "target/x86_64-pc-windows-msvc/debug/examples/seal_store_service.exe",
    }
    for name, built in probes.items():
        source = repo / built
        if source.is_file():
            shutil.copyfile(source, lab / name)
            print(f"probe      -> {lab / name} ({source.stat().st_size} bytes)")
        else:
            print(f"probe      -- not built, left alone: {source}")

    # Images from an earlier run would be read as somebody else's chain.
    work = lab / "work"
    if work.is_dir():
        shutil.rmtree(work)
        print(f"cleared    -> {work}")


if __name__ == "__main__":
    main()
