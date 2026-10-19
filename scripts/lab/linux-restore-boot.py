#!/usr/bin/env python3
"""Host half of the Linux restore scenario (`linux-restore-scenario.sh`).

    python3 -I scripts/lab/linux-restore-boot.py source       # data-1 := the base image
    python3 -I scripts/lab/linux-restore-boot.py boot DISK [--bios] [--timeout S]

`source` turns the run's first data disk into a bootable source system: a
qcow2 overlay of the read-only cloud base (the Ubuntu 26.04 image boots under
both OVMF and SeaBIOS — GPT with an ESP and a BIOS boot partition). The stand
VM must be down; run `linux-vm.py up` first once so the run exists, then
`down`, `source`, `up` again — the guest then sees the source system as
`/dev/disk/by-id/virtio-ob-data-1`.

`boot DISK` boots DISK (`data-2.qcow2` — what the scenario restored onto —
or a path) ALONE, through a throw-away overlay so the evidence disk is not
changed, with OVMF (a fresh VARS copy: no boot entries, so the firmware must
find `\\EFI\\BOOT\\BOOTX64.EFI` by itself, which is what a restored disk on a
new machine gets) or SeaBIOS, and waits for the kernel and a login prompt on
the serial console. Exit 0 when both appeared. One VM at a time, as the stand
rules say: the stand VM must be down.

Python standard library only.
"""
import argparse
import importlib.util
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('linux_vm', HERE / 'linux-vm.py')
vm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vm)


def stand_down() -> None:
    if vm.our_pid() is not None or vm.other_vms():
        vm.fail('a VM is running; the stand runs one at a time (`linux-vm.py down` first)')


def cmd_source(args) -> int:
    stand_down()
    cfg = vm.config()
    if cfg['disks'] < 3:
        vm.fail('the scenario needs three data disks (`up --disks 3`)')
    q = vm.Qemu()
    data1 = vm.RUN / 'data-1.qcow2'
    size = f"{cfg['size_gib']}G"
    data1.unlink(missing_ok=True)
    q.run_img('create', '-q', '-f', 'qcow2', '-F', 'qcow2', '-b', str(vm.BASE), str(data1), size)
    vm.say(f'{data1.name} is now an overlay of the base image ({size}); boot the stand again')
    return 0


def cmd_boot(args) -> int:
    stand_down()
    q = vm.Qemu()
    disk = Path(args.disk)
    if not disk.is_absolute() and not disk.exists():
        disk = vm.RUN / args.disk
    if not disk.exists():
        vm.fail(f'no such disk: {disk}')
    with tempfile.TemporaryDirectory(dir=vm.RUN) as tmp:
        tmp = Path(tmp)
        overlay = tmp / 'boot.qcow2'
        q.run_img('create', '-q', '-f', 'qcow2', '-F', 'qcow2', '-b', str(disk.resolve()),
                  str(overlay))
        serial = vm.RUN / 'evidence' / f"boot-{disk.stem}-{'bios' if args.bios else 'uefi'}.log"
        serial.parent.mkdir(exist_ok=True)
        serial.unlink(missing_ok=True)
        argv = [q.system, '-name', 'obk-lab-linux-boot', '-machine', 'q35,accel=kvm',
                '-cpu', 'host', '-smp', '2', '-m', '2048', '-nodefaults', '-no-user-config',
                '-display', 'none']
        if q.datadir:
            argv += ['-L', str(q.datadir)]
        if args.bios:
            argv += ['-bios', str(q.seabios)]
        else:
            vars_fd = tmp / 'vars.fd'
            shutil.copyfile(q.ovmf_vars, vars_fd)
            argv += ['-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={q.ovmf_code}',
                     '-drive', f'if=pflash,format=raw,unit=1,file={vars_fd}']
        argv += ['-drive', f'file={overlay},if=none,id=d0,format=qcow2',
                 '-device', 'virtio-blk-pci,drive=d0,bootindex=0',
                 '-chardev', f'file,id=ser0,path={serial}', '-serial', 'chardev:ser0',
                 '-device', 'virtio-rng-pci']
        vm.say(f"booting {disk.name} alone ({'SeaBIOS' if args.bios else 'OVMF, empty NVRAM'})")
        proc = subprocess.Popen(argv, env=q.env, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
        kernel = login = False
        deadline = time.monotonic() + args.timeout
        try:
            while time.monotonic() < deadline and proc.poll() is None:
                text = serial.read_text(errors='replace') if serial.exists() else ''
                kernel = kernel or 'Linux version' in text
                login = 'login:' in text
                if kernel and login:
                    break
                time.sleep(2)
        finally:
            proc.terminate()
            try:
                proc.wait(15)
            except subprocess.TimeoutExpired:
                proc.kill()
    print(f'CHECK boot-kernel {"PASS" if kernel else "FAIL"} the kernel of the restored disk started')
    print(f'CHECK boot-login  {"PASS" if login else "FAIL"} the restored system reached a login prompt')
    vm.say(f'serial log: {serial}')
    return 0 if kernel and login else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    sub = parser.add_subparsers(dest='cmd', required=True)
    sub.add_parser('source', help='make data-1 a bootable source system (stand down)')
    boot = sub.add_parser('boot', help='boot a restored disk alone and wait for a login prompt')
    boot.add_argument('disk', help='data-2.qcow2 (in the run) or a path')
    boot.add_argument('--bios', action='store_true', help='SeaBIOS instead of OVMF')
    boot.add_argument('--timeout', type=int, default=420)
    args = parser.parse_args()
    return {'source': cmd_source, 'boot': cmd_boot}[args.cmd](args)


if __name__ == '__main__':
    sys.exit(main())
