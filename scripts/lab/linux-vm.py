#!/usr/bin/env python3
"""Linux guest for the Linux stand: an Ubuntu 26.04 cloud image under QEMU/KVM.

    python3 -I scripts/lab/linux-vm.py prepare [--distro D]  # download + verify the base once
    python3 -I scripts/lab/linux-vm.py up [--distro D] [--disks N] [--size G] [--bios] [--tpm]
                                          [--stick G]
    python3 -I scripts/lab/linux-vm.py media (--iso PATH | --stick) [--firmware uefi-sb|uefi|bios]
                                             [--attach cdrom|usb]   # boot the boot medium
    python3 -I scripts/lab/linux-vm.py ssh -- <command...>   # exit code is the guest's
    python3 -I scripts/lab/linux-vm.py put <host-path> <guest-path>
    python3 -I scripts/lab/linux-vm.py get <guest-path> <host-path>
    python3 -I scripts/lab/linux-vm.py status       # exit 0 only while the VM runs
    python3 -I scripts/lab/linux-vm.py budget [--step NAME]  # record + check space
    python3 -I scripts/lab/linux-vm.py down         # ACPI power-off, then force
    python3 -I scripts/lab/linux-vm.py clean        # archive evidence, drop the run

Layout (`~/lab/linux/`):
- `base/` — the read-only (0444) cloud image and the SHA256SUMS it was checked
  against. Never written: every run boots a qcow2 overlay backed by it.
  `base-noble/`, `base-trixie/` — the same for `--distro noble` (Ubuntu 24.04)
  and `--distro trixie` (Debian 13, SHA512SUMS): clean systems the release
  `.deb` must install on. A run remembers its distro.
- `run/` — the one run: root overlay, data disks `data-N.qcow2` (virtio, serial
  `ob-data-N`, so `/dev/disk/by-id/virtio-ob-data-N` in the guest), a private
  OVMF VARS copy, the per-run ed25519 key and known_hosts, the cloud-init seed,
  `serial.log`, `qemu.log`, `qmp.sock`, `monitor.sock` (for qemu-monitor.py),
  `budgets.jsonl` and `evidence/` for scenarios; with `--tpm`, the state of the
  guest's TPM 2.0 (`tpm/`, an `swtpm` emulator the run starts beside QEMU).
- `evidence/<run-id>/` — what `clean` keeps of a run before deleting it.

The boot medium (`media`, plan §6 stage 6) boots an ISO — as a CD or as a USB
stick written with `dd` — or the run's own USB stick `stick.img` (`up --stick
G` gives the Ubuntu guest that stick, so the service's stick writer can be run
on it). Firmware: OVMF with Secure Boot and Microsoft's keys enrolled
(`uefi-sb`, what a PC ships with: Debian's shim is signed by Microsoft's UEFI
CA), OVMF without Secure Boot, or SeaBIOS. The medium's self-test is asked for
through QEMU's firmware config and reports on the serial port; `media` waits
for it and takes a screenshot (`evidence/media-<label>.png`). No SSH there.

The default guest is Ubuntu 26.04 on purpose: binaries built on this host need
its glibc (noble/trixie take packages built on the 24.04 baseline). User `lab` (passwordless sudo) gets the run key through a NoCloud seed
served over HTTP from this process to the guest at 10.0.2.2 (`-smbios
type=1,serial=ds=nocloud;s=...`); the seed is only needed while `up` waits for
the first boot. `ssh -- one-argument` hands that string to the guest shell as it
is; several arguments are quoted one by one.

QEMU comes from the system or from `tmp/tools/qemu` (extracted packages, see
`tmp/tools/env.sh`; a worktree uses the main checkout's `tmp/tools`). Stand
limits (AGENTS.md): one VM at a time, at least 30 GiB left free after the worst
case growth of the run, a run of at most 20 GiB, the lab at most 65 GiB.
Python standard library only.
"""
import argparse
import re
import hashlib
import http.server
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LAB_ROOT = Path.home() / 'lab'
LAB = LAB_ROOT / 'linux'
BASE_DIR = LAB / 'base'
RUN = LAB / 'run'
ARCHIVE = LAB / 'evidence'
# distro -> (base directory, image URL, image name, sums file)
DISTROS = {
    'resolute': ('base', 'https://cloud-images.ubuntu.com/resolute/current/',
                 'resolute-server-cloudimg-amd64.img', 'SHA256SUMS'),
    'noble': ('base-noble', 'https://cloud-images.ubuntu.com/noble/current/',
              'noble-server-cloudimg-amd64.img', 'SHA256SUMS'),
    'trixie': ('base-trixie', 'https://cloud.debian.org/images/cloud/trixie/latest/',
               'debian-13-genericcloud-amd64.qcow2', 'SHA512SUMS'),
}
DISTRO = 'resolute'
IMAGE_URL = DISTROS[DISTRO][1]
IMAGE_NAME = DISTROS[DISTRO][2]
SUMS = DISTROS[DISTRO][3]
BASE = BASE_DIR / IMAGE_NAME
GIB = 1 << 30
MIN_FREE = 30 * GIB
MAX_RUN = 20 * GIB
MAX_LAB = 65 * GIB
MAX_TARGET = 30 * GIB
PACKAGES = ['lvm2', 'thin-provisioning-tools', 'cryptsetup', 'btrfs-progs', 'xfsprogs',
            'dosfstools', 'ntfs-3g', 'qemu-utils']
USER = 'lab'


def use_distro(name: str) -> None:
    global DISTRO, BASE_DIR, IMAGE_URL, IMAGE_NAME, SUMS, BASE
    DISTRO = name
    folder, IMAGE_URL, IMAGE_NAME, SUMS = DISTROS[name]
    BASE_DIR = LAB / folder
    BASE = BASE_DIR / IMAGE_NAME


def say(*parts) -> None:
    print('[linux-vm]', *parts, flush=True)


def fail(message: str, code: int = 1):
    print(f'[linux-vm] ERROR: {message}', file=sys.stderr, flush=True)
    raise SystemExit(code)


# ---------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------

def tool_dirs():
    """`tmp/tools` of this checkout, then of the main checkout (a worktree has
    no ignored files of its own)."""
    dirs = [REPO / 'tmp' / 'tools']
    try:
        common = subprocess.run(['git', '-C', str(REPO), 'rev-parse', '--path-format=absolute',
                                 '--git-common-dir'], capture_output=True, text=True, check=True)
        dirs.append(Path(common.stdout.strip()).parent / 'tmp' / 'tools')
    except (OSError, subprocess.CalledProcessError):
        pass
    return dirs


class Qemu:
    """Where the QEMU binaries, their libraries and the firmware are."""

    def __init__(self):
        self.env = dict(os.environ)
        self.datadir = None
        for tools in tool_dirs():
            root = tools / 'qemu' / 'usr'
            if (root / 'bin' / 'qemu-system-x86_64').exists():
                self.system = str(root / 'bin' / 'qemu-system-x86_64')
                self.img = str(root / 'bin' / 'qemu-img')
                libs = str(root / 'lib' / 'x86_64-linux-gnu')
                old = self.env.get('LD_LIBRARY_PATH')
                self.env['LD_LIBRARY_PATH'] = f'{libs}:{old}' if old else libs
                self.datadir = root / 'share' / 'qemu'
                share = root / 'share'
                break
        else:
            self.system = shutil.which('qemu-system-x86_64')
            self.img = shutil.which('qemu-img')
            share = Path('/usr/share')
            if not self.system or not self.img:
                fail('no QEMU: neither tmp/tools/qemu nor qemu-system-x86_64 on PATH')
        self.ovmf_code = share / 'OVMF' / 'OVMF_CODE_4M.fd'
        self.ovmf_vars = share / 'OVMF' / 'OVMF_VARS_4M.fd'
        # Secure Boot: the SMM build, and VARS with Microsoft's keys enrolled.
        self.ovmf_code_sb = share / 'OVMF' / 'OVMF_CODE_4M.secboot.fd'
        self.ovmf_vars_ms = share / 'OVMF' / 'OVMF_VARS_4M.ms.fd'
        self.seabios = share / 'seabios' / 'bios-256k.bin'

    def run_img(self, *args, capture=False) -> str:
        out = subprocess.run([self.img, *args], env=self.env, check=True, text=True,
                             capture_output=capture)
        return out.stdout if capture else ''

    def info(self, path: Path) -> dict:
        return json.loads(self.run_img('info', '--output=json', '-U', str(path), capture=True))


# ---------------------------------------------------------------------------
# Space (AGENTS.md "Stand resource limits")
# ---------------------------------------------------------------------------

def du(path: Path) -> int:
    if not path.exists():
        return 0
    out = subprocess.run(['du', '-s', '-B1', str(path)], capture_output=True, text=True)
    try:
        return int(out.stdout.split()[0])
    except (IndexError, ValueError):
        return 0


def free_bytes() -> int:
    probe = LAB
    while not probe.exists():
        probe = probe.parent
    return shutil.disk_usage(probe).free


def measure(step: str, growth: int = 0) -> dict:
    """One budget row: free space of the filesystem holding ~/lab, the lab,
    the run, this checkout's target/, and the worst-case growth still to come."""
    return {'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'step': step,
            'free': free_bytes(), 'lab': du(LAB_ROOT), 'run': du(RUN),
            'target': du(REPO / 'target'), 'growth': growth}


def over_budget(row: dict, run_worst: int = 0) -> list:
    problems = []
    if row['free'] - row['growth'] < MIN_FREE:
        problems.append(f"free {gib(row['free'])} - growth {gib(row['growth'])} < 30 GiB")
    if row['lab'] + row['growth'] > MAX_LAB:
        problems.append(f"lab {gib(row['lab'])} + growth {gib(row['growth'])} > 65 GiB")
    if max(row['run'], run_worst) > MAX_RUN:
        problems.append(f"run worst case {gib(max(row['run'], run_worst))} > 20 GiB")
    if row['target'] > MAX_TARGET:
        problems.append(f"target/ {gib(row['target'])} > 30 GiB")
    return problems


def record(row: dict) -> None:
    folder = RUN if RUN.exists() else LAB
    folder.mkdir(parents=True, exist_ok=True)
    with (folder / 'budgets.jsonl').open('a') as out:
        out.write(json.dumps(row) + '\n')


def gib(n: int) -> str:
    return f'{n / GIB:.1f} GiB'


def show(row: dict) -> str:
    return (f"free {gib(row['free'])}, lab {gib(row['lab'])}, run {gib(row['run'])}, "
            f"target {gib(row['target'])}" + (f", growth {gib(row['growth'])}" if row['growth'] else ''))


# ---------------------------------------------------------------------------
# Run state
# ---------------------------------------------------------------------------

def config() -> dict:
    try:
        return json.loads((RUN / 'config.json').read_text())
    except (OSError, ValueError):
        return {}


def save_config(cfg: dict) -> None:
    tmp = RUN / 'config.json.tmp'
    tmp.write_text(json.dumps(cfg, indent=2) + '\n')
    tmp.replace(RUN / 'config.json')


def qemu_cmdlines():
    """(pid, argv) of every running qemu-system process of this user."""
    for proc in Path('/proc').iterdir():
        if not proc.name.isdigit():
            continue
        try:
            if proc.stat().st_uid != os.getuid():
                continue
            argv = (proc / 'cmdline').read_bytes().split(b'\0')
        except OSError:
            continue
        if argv and b'qemu-system' in Path(argv[0].decode(errors='replace')).name.encode():
            yield int(proc.name), [a.decode(errors='replace') for a in argv if a]


def our_pid():
    """PID of this run's QEMU, if it is alive (zombies have no cmdline)."""
    for pid, argv in qemu_cmdlines():
        if any(str(RUN) in a for a in argv):
            return pid
    return None


def other_vms():
    return [(pid, argv) for pid, argv in qemu_cmdlines() if not any(str(RUN) in a for a in argv)]


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


# ---------------------------------------------------------------------------
# SSH
# ---------------------------------------------------------------------------

def ssh_options(cfg: dict, connect_timeout: int = 10):
    return ['-i', str(RUN / 'id_ed25519'), '-o', 'IdentitiesOnly=yes', '-o', 'BatchMode=yes',
            '-o', 'StrictHostKeyChecking=no', '-o', f"UserKnownHostsFile={RUN / 'known_hosts'}",
            '-o', 'UpdateHostKeys=no', '-o', 'LogLevel=ERROR',
            '-o', f'ConnectTimeout={connect_timeout}', '-o', 'ServerAliveInterval=15',
            '-o', 'ServerAliveCountMax=4']


def ssh_argv(cfg: dict, remote=None, tty=False, connect_timeout: int = 10):
    argv = ['ssh', *ssh_options(cfg, connect_timeout), '-p', str(cfg['ssh_port'])]
    if tty:
        argv.append('-t')
    argv.append(f'{USER}@127.0.0.1')
    if remote is not None:
        argv += ['--', remote]
    return argv


def remote_command(words) -> str:
    # One argument is a shell snippet already; several are quoted as given.
    return words[0] if len(words) == 1 else shlex.join(words)


def need_running() -> dict:
    cfg = config()
    if not cfg or our_pid() is None:
        fail('the VM is not running (python3 -I scripts/lab/linux-vm.py up)')
    return cfg


# ---------------------------------------------------------------------------
# QMP
# ---------------------------------------------------------------------------

def qmp(command: str, arguments=None, timeout: float = 10.0) -> dict:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(str(RUN / 'qmp.sock'))
        stream = sock.makefile('rwb')

        def call(name, args=None):
            request = {'execute': name}
            if args:
                request['arguments'] = args
            stream.write((json.dumps(request) + '\n').encode())
            stream.flush()
            while True:
                line = stream.readline()
                if not line:
                    raise ConnectionError('QMP closed the connection')
                reply = json.loads(line)
                if 'return' in reply or 'error' in reply:
                    return reply

        json.loads(stream.readline())  # greeting
        call('qmp_capabilities')
        return call(command, arguments)


def wait_gone(pid: int, seconds: float) -> bool:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if our_pid() != pid:
            return True
        time.sleep(1)
    return our_pid() != pid


# ---------------------------------------------------------------------------
# Cloud-init seed over HTTP
# ---------------------------------------------------------------------------

class SeedHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.reply(True)

    def do_HEAD(self):
        self.reply(False)

    def reply(self, body: bool):
        name = self.path.split('?', 1)[0].strip('/')
        data = self.server.files.get(name)
        if data is None:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain; charset=utf-8')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        if body:
            self.wfile.write(data)

    def log_message(self, fmt, *args):
        with (RUN / 'seed-http.log').open('a') as log:
            log.write(f"{time.strftime('%H:%M:%S')} {self.address_string()} {fmt % args}\n")


def seed_files(cfg: dict) -> dict:
    key = (RUN / 'id_ed25519.pub').read_text().strip()
    user_data = {
        'hostname': 'obk-lab',
        'users': [{'name': USER, 'gecos': 'Own Backup lab', 'groups': ['adm', 'sudo'],
                   'sudo': 'ALL=(ALL) NOPASSWD:ALL', 'shell': '/bin/bash',
                   'lock_passwd': True, 'ssh_authorized_keys': [key]}],
        'ssh_pwauth': False,
        'disable_root': True,
        'package_update': True,
        'package_upgrade': False,
        'packages': PACKAGES,
        'final_message': 'obk-lab: cloud-init finished after $UPTIME s',
    }
    files = {
        # JSON is YAML: no quoting rules to get wrong for the key.
        'user-data': ('#cloud-config\n' + json.dumps(user_data, indent=2) + '\n').encode(),
        'meta-data': (f"instance-id: {cfg['instance_id']}\nlocal-hostname: obk-lab\n").encode(),
        'vendor-data': b'',
    }
    seed = RUN / 'seed'
    seed.mkdir(exist_ok=True)
    for name, data in files.items():
        (seed / name).write_bytes(data)
    return files


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------

def sum_file(path: Path) -> str:
    digest = hashlib.new(SUMS[:-4].lower())
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(8 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def expected_sum(sums: str):
    for line in sums.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1].lstrip('*') == IMAGE_NAME:
            return parts[0].lower()
    return None


def fetch(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def cmd_prepare(args) -> int:
    BASE_DIR.mkdir(parents=True, exist_ok=True)
    recorded = BASE_DIR / SUMS
    if BASE.exists():
        want = expected_sum(recorded.read_text()) if recorded.exists() else None
        have = sum_file(BASE)
        if want is None or have != want:
            fail(f'{BASE} does not match the {SUMS} it was downloaded with; '
                 'inspect it and remove it by hand to download again')
        os.chmod(BASE, 0o444)
        say(f'base present and verified: {BASE} {SUMS[:-4].lower()} {have}')
        try:
            upstream = expected_sum(fetch(IMAGE_URL + SUMS).decode())
            if upstream and upstream != have:
                say('note: the current upstream build differs; the verified base is kept')
        except OSError as error:
            say(f'note: upstream {SUMS} not reachable ({error}); local check only')
        return 0

    sums = fetch(IMAGE_URL + SUMS).decode()
    want = expected_sum(sums)
    if not want:
        fail(f'{IMAGE_NAME} is not listed in {IMAGE_URL}{SUMS}')
    with urllib.request.urlopen(IMAGE_URL + IMAGE_NAME, timeout=60) as response:
        total = int(response.headers.get('Content-Length') or 0)
        row = measure('prepare', total)
        record(row)
        problems = over_budget(row)
        if problems:
            fail('space budget: ' + '; '.join(problems))
        part = BASE_DIR / (IMAGE_NAME + '.part')
        digest = hashlib.new(SUMS[:-4].lower())
        done = 0
        last = 0.0
        with part.open('wb') as out:
            while True:
                chunk = response.read(4 << 20)
                if not chunk:
                    break
                out.write(chunk)
                digest.update(chunk)
                done += len(chunk)
                if time.monotonic() - last > 10:
                    last = time.monotonic()
                    say(f'downloading {done >> 20} / {total >> 20} MiB')
            out.flush()
            os.fsync(out.fileno())
    if digest.hexdigest() != want:
        part.unlink()
        fail(f'checksum mismatch: got {digest.hexdigest()}, {SUMS} says {want}')
    part.replace(BASE)
    os.chmod(BASE, 0o444)
    recorded.write_text(sums)
    os.chmod(recorded, 0o444)
    say(f'base downloaded and verified: {BASE} {SUMS[:-4].lower()} {want}')
    info = Qemu().info(BASE)
    say(f"format {info['format']}, virtual size {gib(info['virtual-size'])}")
    return 0


def create_run(args, q: Qemu) -> dict:
    """A new run directory: overlay, data disks, key, VARS, config."""
    RUN.mkdir(parents=True, mode=0o700)
    cfg = {'run_id': time.strftime('%Y%m%dT%H%M%SZ', time.gmtime()),
           'instance_id': 'obk-lab-' + uuid.uuid4().hex[:12],
           'disks': args.disks, 'size_gib': args.size, 'root_gib': args.root_size,
           'bios': args.bios, 'mem_mib': args.mem, 'cpus': args.cpus,
           'distro': DISTRO, 'base': str(BASE),
           'base_sum': f'{SUMS[:-4].lower()}:{expected_sum((BASE_DIR / SUMS).read_text())}'}
    save_config(cfg)
    subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-C', 'obk-lab-run',
                    '-f', str(RUN / 'id_ed25519')], check=True)
    q.run_img('create', '-q', '-f', 'qcow2', '-F', 'qcow2', '-b', str(BASE),
              str(RUN / 'root.qcow2'), f'{args.root_size}G')
    for n in range(1, args.disks + 1):
        q.run_img('create', '-q', '-f', 'qcow2', str(RUN / f'data-{n}.qcow2'), f'{args.size}G')
    make_stick(cfg, getattr(args, 'stick', 0))
    if not args.bios:
        shutil.copyfile(q.ovmf_vars, RUN / 'vars.fd')
        os.chmod(RUN / 'vars.fd', 0o600)
    (RUN / 'evidence').mkdir(exist_ok=True)
    return cfg


def make_stick(cfg: dict, gib_size: int) -> None:
    """The run's USB stick: an empty sparse raw image, `stick.img`. A raw
    file, not qcow2, so `media --stick` boots exactly the bytes the guest's
    stick writer put there."""
    if not gib_size or (RUN / 'stick.img').exists():
        return
    with (RUN / 'stick.img').open('wb') as f:
        f.truncate(gib_size * GIB)
    cfg['stick_gib'] = gib_size
    save_config(cfg)


def worst_growth(cfg: dict, q: Qemu) -> int:
    """How much the run's images can still grow: virtual minus allocated."""
    growth = 0
    names = [f"data-{n}.qcow2" for n in range(1, cfg.get('disks', 0) + 1)]
    if (RUN / 'root.qcow2').exists():
        names.insert(0, 'root.qcow2')
    for name in names:
        info = q.info(RUN / name)
        growth += max(0, info['virtual-size'] - info.get('actual-size', 0))
    stick = RUN / 'stick.img'
    if stick.exists():
        growth += max(0, stick.stat().st_size - stick.stat().st_blocks * 512)
    return growth


def stick_argv(bootindex=None) -> list:
    """The run's stick on a USB 3 controller, as the guest's /dev/sdX on the
    USB bus (what the stick writer offers)."""
    boot = f',bootindex={bootindex}' if bootindex is not None else ''
    return ['-device', 'qemu-xhci,id=xhci',
            '-drive', f"file={RUN / 'stick.img'},if=none,id=stick,format=raw,discard=unmap",
            '-device', f'usb-storage,bus=xhci.0,drive=stick,serial=ob-stick,removable=on{boot}']


def qemu_argv(cfg: dict, q: Qemu) -> list:
    argv = [q.system, '-name', 'obk-lab-linux', '-machine', 'q35,accel=kvm', '-cpu', 'host',
            '-smp', str(cfg['cpus']), '-m', str(cfg['mem_mib']),
            '-nodefaults', '-no-user-config', '-display', 'none',
            '-pidfile', str(RUN / 'qemu.pid')]
    if q.datadir:
        argv += ['-L', str(q.datadir)]
    if cfg['bios']:
        argv += ['-bios', str(q.seabios)]
    else:
        argv += ['-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={q.ovmf_code}',
                 '-drive', f"if=pflash,format=raw,unit=1,file={RUN / 'vars.fd'}"]
    if cfg.get('tpm'):
        argv += ['-chardev', f"socket,id=chrtpm,path={RUN / 'swtpm.sock'}",
                 '-tpmdev', 'emulator,id=tpm0,chardev=chrtpm',
                 '-device', 'tpm-tis,tpmdev=tpm0']
    argv += ['-drive', f"file={RUN / 'root.qcow2'},if=none,id=root,format=qcow2,discard=unmap",
             '-device', 'virtio-blk-pci,drive=root,bootindex=0']
    late = set(cfg.get('late', []))
    for n in range(1, cfg['disks'] + 1):
        if n in late:
            # Plugged in after boot (`plug_late`): a disk that is a clone of
            # the root (the restore scenario's source system) must not be
            # where the guest finds its root by UUID or label.
            argv += ['-device', f'pcie-root-port,id=hp{n},chassis={10 + n}']
            continue
        argv += ['-drive', f"file={RUN / f'data-{n}.qcow2'},if=none,id=data{n},format=qcow2,"
                           'discard=unmap',
                 '-device', f'virtio-blk-pci,drive=data{n},serial=ob-data-{n}']
    if (RUN / 'stick.img').exists():
        argv += stick_argv()
    argv += ['-netdev', f"user,id=net0,hostfwd=tcp:127.0.0.1:{cfg['ssh_port']}-:22",
             '-device', 'virtio-net-pci,netdev=net0,romfile=',
             '-smbios', f"type=1,serial=ds=nocloud;s=http://10.0.2.2:{cfg['http_port']}/",
             '-chardev', f"file,id=ser0,path={RUN / 'serial.log'},append=on",
             '-serial', 'chardev:ser0',
             '-qmp', f"unix:{RUN / 'qmp.sock'},server=on,wait=off",
             '-monitor', f"unix:{RUN / 'monitor.sock'},server=on,wait=off",
             '-device', 'virtio-rng-pci']
    return argv


def start_swtpm() -> None:
    """The guest's TPM 2.0: an `swtpm` emulator on a socket QEMU connects to.
    Its state lives in the run (`tpm/`), so a key sealed to it survives the
    run's reboots and goes with `clean`. It ends when QEMU disconnects."""
    swtpm = shutil.which('swtpm')
    if swtpm is None:
        fail('--tpm needs swtpm on this host (apt install swtpm swtpm-tools)')
    state = RUN / 'tpm'
    state.mkdir(mode=0o700, exist_ok=True)
    sock = RUN / 'swtpm.sock'
    if sock.exists():
        sock.unlink()
    subprocess.run([swtpm, 'socket', '--tpm2', '--tpmstate', f'dir={state}',
                    '--ctrl', f'type=unixio,path={sock}', '--terminate', '--daemon',
                    '--log', f"file={RUN / 'swtpm.log'}"], check=True)
    for _ in range(50):
        if sock.exists():
            return
        time.sleep(0.1)
    fail(f'swtpm did not open {sock} (see {RUN / "swtpm.log"})')


def tail(path: Path, lines: int = 25) -> str:
    try:
        return '\n'.join(path.read_text(errors='replace').splitlines()[-lines:])
    except OSError:
        return '(none)'


def plug_late(cfg: dict) -> None:
    """Attach the `--late` data disks to the running guest, as the same
    `virtio-ob-data-N` it would have had from the start."""
    for n in cfg.get('late', []):
        steps = [('blockdev-add', {'driver': 'qcow2', 'node-name': f'data{n}',
                                   'discard': 'unmap',
                                   'file': {'driver': 'file',
                                            'filename': str(RUN / f'data-{n}.qcow2')}}),
                 ('device_add', {'driver': 'virtio-blk-pci', 'id': f'data{n}dev',
                                 'drive': f'data{n}', 'bus': f'hp{n}',
                                 'serial': f'ob-data-{n}'})]
        for command, arguments in steps:
            reply = qmp(command, arguments)
            if 'error' in reply:
                fail(f'plugging data-{n} in: {command}: {reply["error"]}')
        deadline = time.monotonic() + 30
        while subprocess.run(ssh_argv(cfg, f'test -b /dev/disk/by-id/virtio-ob-data-{n}'),
                             stdin=subprocess.DEVNULL, capture_output=True).returncode != 0:
            if time.monotonic() > deadline:
                fail(f'data-{n} plugged in, but the guest shows no virtio-ob-data-{n}')
            time.sleep(1)
        say(f'data-{n}.qcow2 plugged in after boot')


def cmd_up(args) -> int:
    if not os.access('/dev/kvm', os.R_OK | os.W_OK):
        fail('/dev/kvm is not usable by this user')
    if our_pid() is not None:
        fail('this run is already up (status / down)')
    others = other_vms()
    if others:
        listing = '; '.join(f'pid {pid}: {" ".join(argv[:3])}…' for pid, argv in others)
        fail(f'another VM is running ({listing}); the stand runs one VM at a time')
    if not BASE.exists():
        fail('no base image: run `prepare` first')
    if args.disks < 0 or args.disks > 8:
        fail('--disks must be 0..8')
    if not 1 <= args.size <= 8:
        fail('--size must be 1..8 GiB per data disk')
    if not 4 <= args.root_size <= 16:
        fail('--root-size must be 4..16 GiB')
    if not 0 <= args.stick <= 8:
        fail('--stick must be 0..8 GiB')
    q = Qemu()
    firmware = q.seabios if args.bios else q.ovmf_code
    if not Path(firmware).exists():
        fail(f'firmware not found: {firmware}')

    reuse = (RUN / 'config.json').exists()
    if reuse:
        cfg = config()
        if cfg.get('kind') == 'media':
            fail('this run only boots the medium (`media`); `clean` it for an Ubuntu guest')
        if cfg.get('distro', 'resolute') != DISTRO:
            fail(f"the run is {cfg.get('distro', 'resolute')}, not {DISTRO}; `clean` it first")
        say(f"booting the existing run {cfg['run_id']} again (its disks and firmware kept; "
            'use `clean` for a fresh one)')
        growth = worst_growth(cfg, q)
        run_worst = du(RUN) + growth
    else:
        if RUN.exists():
            fail(f'{RUN} exists without config.json; inspect and remove it')
        growth = (args.root_size + args.disks * args.size + args.stick) * GIB
        run_worst = growth
    row = measure('up', growth)
    record(row)
    problems = over_budget(row, run_worst)
    if problems:
        fail('space budget (AGENTS.md): ' + '; '.join(problems) + ' — use fewer/smaller disks')
    say('space: ' + show(row))
    if not reuse:
        cfg = create_run(args, q)
    else:
        make_stick(cfg, args.stick)
    # A boot option, not a property of the run: only this boot plugs late.
    cfg['late'] = sorted(set(args.late or []))
    bad = [n for n in cfg['late'] if not 1 <= n <= cfg['disks']]
    if bad:
        fail(f'--late {bad}: the run has data disks 1..{cfg["disks"]}')
    if args.tpm and not cfg.get('tpm'):
        # A TPM added to a run later is a new one, and says so to the guest.
        cfg['tpm'] = True
    if cfg.get('tpm'):
        start_swtpm()

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), SeedHandler)
    server.files = seed_files(cfg)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    cfg['http_port'] = server.server_address[1]
    cfg['ssh_port'] = free_port()
    save_config(cfg)
    argv = qemu_argv(cfg, q)
    (RUN / 'qemu-argv.json').write_text(json.dumps(argv, indent=1) + '\n')
    with (RUN / 'qemu.log').open('a') as log:
        log.write(f"--- start {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n")
        log.flush()
        proc = subprocess.Popen(argv, env=q.env, stdin=subprocess.DEVNULL, stdout=log,
                                stderr=subprocess.STDOUT, start_new_session=True, cwd=RUN)
    say(f"QEMU pid {proc.pid}, {'SeaBIOS' if cfg['bios'] else 'UEFI (OVMF)'}, "
        f"{'TPM 2.0 (swtpm), ' if cfg.get('tpm') else ''}"
        f"{cfg['disks']} data disk(s) of {cfg['size_gib']} GiB, ssh port {cfg['ssh_port']}")

    try:
        deadline = time.monotonic() + args.timeout
        last_note = 0.0
        while True:
            if proc.poll() is not None:
                fail(f'QEMU exited with {proc.returncode}\n--- qemu.log\n{tail(RUN / "qemu.log")}'
                     f'\n--- serial.log\n{tail(RUN / "serial.log")}')
            probe = subprocess.run(ssh_argv(cfg, 'true', connect_timeout=5),
                                   stdin=subprocess.DEVNULL, capture_output=True)
            if probe.returncode == 0:
                break
            if time.monotonic() > deadline:
                fail(f'no SSH after {args.timeout} s (VM left running; see {RUN}/serial.log, '
                     'then `down`)')
            if time.monotonic() - last_note > 30:
                last_note = time.monotonic()
                say('waiting for SSH…')
            time.sleep(3)
        say('SSH answers; waiting for cloud-init to finish')
        try:
            wait = subprocess.run(ssh_argv(cfg, 'cloud-init status --wait --long'),
                                  stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                  timeout=max(60, deadline - time.monotonic() + 600))
        except subprocess.TimeoutExpired:
            fail('cloud-init is still running; VM left running (see serial.log, then `down`)')
        (RUN / 'cloud-init-status.txt').write_text(wait.stdout + wait.stderr)
        if wait.returncode != 0:
            print(wait.stdout + wait.stderr)
            fail(f'cloud-init did not finish cleanly (exit {wait.returncode}); VM left running')
        missing = subprocess.run(ssh_argv(cfg, 'dpkg-query -W -f=\'${Package} ${Status}\\n\' '
                                          + ' '.join(PACKAGES)),
                                 stdin=subprocess.DEVNULL, capture_output=True, text=True)
        absent = [line for line in (missing.stdout + missing.stderr).splitlines()
                  if 'install ok installed' not in line]
        if missing.returncode != 0 or absent:
            fail('packages missing in the guest: ' + '; '.join(absent))
    finally:
        server.shutdown()

    plug_late(cfg)
    record(measure('up-done'))
    say('VM is up.')
    print(f'  ssh:     python3 -I {Path(__file__).relative_to(REPO)} ssh -- <command>\n'
          f'  direct:  {shlex.join(ssh_argv(cfg))}\n'
          f'  disks:   /dev/disk/by-id/virtio-ob-data-1..{cfg["disks"]} in the guest\n'
          f'  serial:  {RUN / "serial.log"}\n'
          f'  QMP:     {RUN / "qmp.sock"}   HMP: {RUN / "monitor.sock"}\n'
          f'  stop:    python3 -I {Path(__file__).relative_to(REPO)} down', flush=True)
    return 0


def media_argv(cfg: dict, q: Qemu, args, label: str) -> list:
    sb = args.firmware == 'uefi-sb'
    machine = 'q35,accel=kvm' + (',smm=on' if sb else '')
    argv = [q.system, '-name', 'obk-lab-media', '-machine', machine, '-cpu', 'host',
            '-smp', str(args.cpus), '-m', str(args.mem),
            '-nodefaults', '-no-user-config', '-display', 'none',
            '-pidfile', str(RUN / 'qemu.pid')]
    if q.datadir:
        argv += ['-L', str(q.datadir)]
    if args.firmware == 'bios':
        argv += ['-bios', str(q.seabios)]
    else:
        code = q.ovmf_code_sb if sb else q.ovmf_code
        if sb:
            # The SMM build refuses to run with its flash writable from the guest.
            argv += ['-global', 'driver=cfi.pflash01,property=secure,value=on']
        argv += ['-drive', f'if=pflash,format=raw,unit=0,readonly=on,file={code}',
                 '-drive', f"if=pflash,format=raw,unit=1,file={RUN / f'vars-{label}.fd'}"]
    # A plain VGA (bochs-drm in the guest): the kiosk must come up on the
    # dullest display there is, and screendump reads it. The EDID offers
    # `--screen` as the preferred mode.
    xres, yres = args.screen
    # 16 MiB of video memory by default: enough up to 1920x1200 at 32 bits,
    # with room for the double buffer only below that.
    vgamem = max(16, -(-xres * yres * 4 * 2 // (1 << 20)))
    argv += ['-device', f'VGA,edid=on,xres={xres},yres={yres},vgamem_mb={vgamem}']
    if args.stick:
        argv += stick_argv(bootindex=0)
    elif args.attach == 'cdrom':
        argv += ['-drive', f'file={args.iso},if=none,id=cd,media=cdrom,readonly=on,format=raw',
                 '-device', 'ide-cd,drive=cd,bootindex=0']
    else:
        argv += ['-device', 'qemu-xhci,id=xhci',
                 '-drive', f'file={args.iso},if=none,id=stick,readonly=on,format=raw',
                 '-device', 'usb-storage,bus=xhci.0,drive=stick,serial=ob-iso,removable=on,'
                            'bootindex=0']
    # On a run made by `up`, its Ubuntu disk is the machine the medium
    # rescues: copied offline, restored to. Never the boot device here.
    if (RUN / 'root.qcow2').exists():
        argv += ['-drive', f"file={RUN / 'root.qcow2'},if=none,id=root,format=qcow2",
                 '-device', 'virtio-blk-pci,drive=root,serial=ob-root']
    for n in range(1, cfg.get('disks', 0) + 1):
        argv += ['-drive', f"file={RUN / f'data-{n}.qcow2'},if=none,id=data{n},format=qcow2",
                 '-device', f'virtio-blk-pci,drive=data{n},serial=ob-data-{n}']
    argv += ['-netdev', 'user,id=net0', '-device', 'virtio-net-pci,netdev=net0,romfile=',
             '-fw_cfg', 'name=opt/own-backup/selftest,string=1',
             '-chardev', f"file,id=ser0,path={RUN / 'serial.log'},append=on",
             '-serial', 'chardev:ser0',
             '-qmp', f"unix:{RUN / 'qmp.sock'},server=on,wait=off",
             '-monitor', f"unix:{RUN / 'monitor.sock'},server=on,wait=off",
             '-device', 'virtio-rng-pci']
    return argv


def cmd_media(args) -> int:
    """Boot the boot medium and wait for its self-test (`OBK-SELFTEST done`)."""
    if not os.access('/dev/kvm', os.R_OK | os.W_OK):
        fail('/dev/kvm is not usable by this user')
    if our_pid() is not None:
        fail('this run is already up (status / down)')
    others = other_vms()
    if others:
        fail('another VM is running; the stand runs one VM at a time')
    if args.iso and not Path(args.iso).is_file():
        fail(f'no such ISO: {args.iso}')
    args.iso = str(Path(args.iso).resolve()) if args.iso else None
    if args.stick and not (RUN / 'stick.img').exists():
        fail('the run has no stick.img (`up --stick G`, then the stick writer in the guest)')
    q = Qemu()
    needed = {'uefi-sb': [q.ovmf_code_sb, q.ovmf_vars_ms], 'uefi': [q.ovmf_code, q.ovmf_vars],
              'bios': [q.seabios]}[args.firmware]
    for f in needed:
        if not Path(f).exists():
            fail(f'firmware not found: {f}')

    if (RUN / 'config.json').exists():
        cfg = config()
        growth = worst_growth(cfg, q)
    else:
        if RUN.exists():
            fail(f'{RUN} exists without config.json; inspect and remove it')
        growth = args.disks * args.size * GIB
    row = measure('media', growth)
    record(row)
    problems = over_budget(row, du(RUN) + growth)
    if problems:
        fail('space budget (AGENTS.md): ' + '; '.join(problems))
    if not (RUN / 'config.json').exists():
        RUN.mkdir(parents=True, mode=0o700)
        cfg = {'run_id': time.strftime('%Y%m%dT%H%M%SZ', time.gmtime()), 'kind': 'media',
               'disks': args.disks, 'size_gib': args.size, 'bios': args.firmware == 'bios'}
        save_config(cfg)
        for n in range(1, args.disks + 1):
            q.run_img('create', '-q', '-f', 'qcow2', str(RUN / f'data-{n}.qcow2'),
                      f'{args.size}G')
    (RUN / 'evidence').mkdir(exist_ok=True)
    source = 'stick' if args.stick else args.attach
    label = args.label or f'{args.firmware}-{source}'
    if args.firmware != 'bios':
        # Fresh variables for every boot: no boot entry or key a previous boot
        # left decides this one.
        template = q.ovmf_vars_ms if args.firmware == 'uefi-sb' else q.ovmf_vars
        shutil.copyfile(template, RUN / f'vars-{label}.fd')
        os.chmod(RUN / f'vars-{label}.fd', 0o600)

    argv = media_argv(cfg, q, args, label)
    (RUN / f'qemu-argv-{label}.json').write_text(json.dumps(argv, indent=1) + '\n')
    serial = RUN / 'serial.log'
    with serial.open('a') as log:
        log.write(f'\n--- media boot {label} '
                  f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n")
    start = serial.stat().st_size
    with (RUN / 'qemu.log').open('a') as log:
        log.write(f"--- media {label} {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n")
        log.flush()
        proc = subprocess.Popen(argv, env=q.env, stdin=subprocess.DEVNULL, stdout=log,
                                stderr=subprocess.STDOUT, start_new_session=True, cwd=RUN)
    say(f'QEMU pid {proc.pid}: medium from {source}, firmware {args.firmware}')

    deadline = time.monotonic() + args.timeout
    began = time.monotonic()
    lines = []
    keys_sent = 0
    while True:
        if proc.poll() is not None:
            fail(f'QEMU exited with {proc.returncode}\n--- qemu.log\n{tail(RUN / "qemu.log")}')
        # The boot menus wait for a key when their timeout is off; Enter takes
        # the first entry. Only while a menu can be up — later the window has
        # the keyboard.
        if keys_sent < 4 and time.monotonic() - began > 12 + 8 * keys_sent:
            try:
                qmp('send-key', {'keys': [{'type': 'qcode', 'data': 'ret'}]})
            except (OSError, ValueError):
                pass
            keys_sent += 1
        with serial.open('rb') as f:
            f.seek(start)
            text = f.read().decode(errors='replace')
        lines = [ln.strip() for ln in text.splitlines() if ln.startswith('OBK-SELFTEST')]
        if 'OBK-SELFTEST done' in lines:
            break
        if time.monotonic() > deadline:
            lines.append(shot(label))
            (RUN / 'evidence' / f'media-{label}.txt').write_text('\n'.join(lines) + '\n')
            fail(f'no self-test after {args.timeout} s (VM left running; see {serial}, '
                 f"evidence/media-{label}.png, then `down`)")
        time.sleep(3)
    # The page is drawn by now (the self-test waited for it), but WebKit may
    # still show its white background for a while (stand 20.10: one boot of
    # four caught it): look again for up to 30 s before judging the screen.
    for _ in range(10):
        time.sleep(3)
        screen = shot(label)
        m = re.search(r'colours=(\d+) dominant=(\d+)', screen)
        if m and int(m.group(1)) >= 16 and int(m.group(2)) <= 90:
            break
    lines.append(screen)
    (RUN / 'evidence' / f'media-{label}.txt').write_text('\n'.join(lines) + '\n')
    for ln in lines:
        print(ln, flush=True)
    say(f'self-test done; evidence/media-{label}.txt and .png (VM left running: `down`)')
    return 0


def shot(label: str) -> str:
    """Screenshot into `evidence/media-<label>.png`, and a line saying whether
    anything is on the screen: how many colours, and how much of it is the
    most common one. A kiosk that never drew is one colour (black, or the
    compositor's grey) over the whole screen."""
    import importlib.util
    ppm = RUN / f'screen-{label}.ppm'
    out = RUN / 'evidence' / f'media-{label}.png'
    try:
        reply = qmp('screendump', {'filename': str(ppm)}, timeout=30)
    except (OSError, ValueError) as error:
        return f'SCREEN error={error}'
    if 'error' in reply or not ppm.exists():
        return f"SCREEN error={reply.get('error')}"
    spec = importlib.util.spec_from_file_location('qshot', Path(__file__).with_name(
        'qemu-screenshot.py'))
    qshot = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(qshot)
    width, height, rgb = qshot.read_ppm(str(ppm))
    qshot.write_png(str(out), width, height, rgb)
    ppm.unlink()
    counts = {}
    for i in range(0, len(rgb), 3 * 7):  # every 7th pixel is plenty
        px = rgb[i:i + 3]
        counts[px] = counts.get(px, 0) + 1
    total = sum(counts.values()) or 1
    dominant = max(counts.values(), default=0) * 100 // total
    return f'SCREEN {width}x{height} colours={len(counts)} dominant={dominant}'


def cmd_ssh(args) -> int:
    cfg = need_running()
    words = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not words:
        return subprocess.call(ssh_argv(cfg, tty=sys.stdin.isatty()))
    return subprocess.call(ssh_argv(cfg, remote_command(words)))


def scp(cfg: dict, source: str, dest: str) -> int:
    return subprocess.call(['scp', '-q', '-r', *ssh_options(cfg), '-P', str(cfg['ssh_port']),
                            source, dest])


def cmd_put(args) -> int:
    cfg = need_running()
    return scp(cfg, args.host_path, f'{USER}@127.0.0.1:{args.guest_path}')


def cmd_get(args) -> int:
    cfg = need_running()
    return scp(cfg, f'{USER}@127.0.0.1:{args.guest_path}', args.host_path)


def cmd_down(args) -> int:
    pid = our_pid()
    if pid is None:
        say('not running')
        return 0
    try:
        qmp('system_powerdown')
        say('ACPI power-down sent')
    except (OSError, ValueError) as error:
        say(f'QMP power-down failed: {error}')
    if wait_gone(pid, args.timeout):
        say('powered off')
        record(measure('down'))
        return 0
    say(f'still running after {args.timeout} s; quitting QEMU')
    try:
        qmp('quit')
    except (OSError, ValueError):
        pass
    for sig, seconds in ((None, 10), (signal.SIGTERM, 10), (signal.SIGKILL, 5)):
        if sig is not None:
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        if wait_gone(pid, seconds):
            say('stopped' + (f' with {sig.name}' if sig else ''))
            record(measure('down-forced'))
            return 0
    fail(f'QEMU pid {pid} would not stop')


def cmd_status(args) -> int:
    cfg = config()
    pid = our_pid()
    if not RUN.exists():
        say('no run')
    else:
        say(f"run {cfg.get('run_id', '?')}: {'running, pid ' + str(pid) if pid else 'stopped'}; "
            f"{'SeaBIOS' if cfg.get('bios') else 'UEFI'}, {cfg.get('disks', '?')} data disk(s) "
            f"of {cfg.get('size_gib', '?')} GiB, ssh port {cfg.get('ssh_port', '?')}")
        if pid:
            probe = subprocess.run(ssh_argv(cfg, 'true', connect_timeout=3),
                                   stdin=subprocess.DEVNULL, capture_output=True)
            say('ssh: ' + ('answers' if probe.returncode == 0 else 'does not answer'))
    for other, argv in other_vms():
        say(f'another VM: pid {other} {" ".join(argv[:3])}…')
    say('space: ' + show(measure('status')))
    return 0 if pid else 1


def cmd_budget(args) -> int:
    row = measure(args.step)
    record(row)
    problems = over_budget(row)
    say(f'{args.step}: ' + show(row))
    if problems:
        say('OVER BUDGET: ' + '; '.join(problems))
        return 1
    return 0


def cmd_clean(args) -> int:
    if our_pid() is not None:
        fail('the VM is running: `down` first')
    if not RUN.exists():
        say('nothing to clean')
        return 0
    if BASE.resolve().is_relative_to(RUN.resolve()):
        fail('the base is inside the run directory; refusing')
    # Nothing outside the run may be backed by an image inside it.
    q = Qemu()
    for image in LAB.rglob('*.qcow2'):
        if image.resolve().is_relative_to(RUN.resolve()):
            continue
        backing = q.info(image).get('full-backing-filename') or ''
        if backing and Path(backing).resolve().is_relative_to(RUN.resolve()):
            fail(f'{image} is backed by {backing} inside the run; refusing')
    cfg = config()
    keep = ARCHIVE / cfg.get('run_id', time.strftime('%Y%m%dT%H%M%SZ', time.gmtime()))
    keep.mkdir(parents=True, exist_ok=True)
    record(measure('clean'))
    for name in ('evidence', 'seed', 'serial.log', 'qemu.log', 'seed-http.log', 'budgets.jsonl',
                 'config.json', 'qemu-argv.json', 'cloud-init-status.txt'):
        source = RUN / name
        if source.is_dir():
            shutil.copytree(source, keep / name, dirs_exist_ok=True)
        elif source.exists():
            shutil.copy2(source, keep / name)
    before = du(RUN)
    shutil.rmtree(RUN)
    say(f'run removed ({gib(before)} freed); evidence kept in {keep}')
    say('space: ' + show(measure('cleaned')))
    return 0


def screen_size(text):
    """`WxH` for `media --screen`: what the VGA's EDID offers as preferred."""
    m = re.fullmatch(r'(\d{3,4})x(\d{3,4})', text)
    if not m:
        raise argparse.ArgumentTypeError(f'want WxH, e.g. 1024x768: {text!r}')
    return int(m.group(1)), int(m.group(2))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    sub = parser.add_subparsers(dest='cmd', required=True)
    prepare = sub.add_parser('prepare', help='download and verify the base image once')
    prepare.add_argument('--distro', choices=sorted(DISTROS), default='resolute')
    up = sub.add_parser('up', help='boot the run (create it first if needed)')
    up.add_argument('--distro', choices=sorted(DISTROS), default='resolute',
                    help='guest system (default resolute = Ubuntu 26.04)')
    up.add_argument('--disks', type=int, default=3, help='extra empty data disks (default 3)')
    up.add_argument('--size', type=int, default=4, help='GiB per data disk (default 4)')
    up.add_argument('--root-size', type=int, default=6, help='GiB of the root overlay (default 6)')
    up.add_argument('--bios', action='store_true', help='SeaBIOS instead of UEFI')
    up.add_argument('--late', type=int, action='append', metavar='N',
                    help='plug data disk N in only after the guest has booted (repeatable)')
    up.add_argument('--tpm', action='store_true',
                    help='a TPM 2.0 for the guest (swtpm on this host; kept with the run)')
    up.add_argument('--mem', type=int, default=2048, help='MiB of RAM (default 2048)')
    up.add_argument('--cpus', type=int, default=2, help='vCPUs (default 2)')
    up.add_argument('--timeout', type=int, default=900, help='seconds to wait for SSH')
    up.add_argument('--stick', type=int, default=0, metavar='G',
                    help='an empty USB stick of G GiB (stick.img, kept with the run)')
    media = sub.add_parser('media', help='boot the boot medium (ISO or the run\'s stick)')
    source = media.add_mutually_exclusive_group(required=True)
    source.add_argument('--iso', help='the medium\'s ISO (scripts/build-linux-media.sh)')
    source.add_argument('--stick', action='store_true',
                        help='the run\'s stick.img, as the guest\'s stick writer left it')
    media.add_argument('--firmware', choices=['uefi-sb', 'uefi', 'bios'], default='uefi-sb')
    media.add_argument('--attach', choices=['cdrom', 'usb'], default='cdrom',
                       help='--iso as a CD or as a stick written with dd (default cdrom)')
    media.add_argument('--disks', type=int, default=0,
                       help='data disks for a new media run (an existing run keeps its own)')
    media.add_argument('--size', type=int, default=4, help='GiB per new data disk')
    media.add_argument('--mem', type=int, default=3072, help='MiB of RAM (default 3072)')
    media.add_argument('--cpus', type=int, default=2, help='vCPUs (default 2)')
    media.add_argument('--timeout', type=int, default=600,
                       help='seconds to wait for the self-test (default 600)')
    media.add_argument('--label', default='', help='name of the evidence files')
    media.add_argument('--screen', type=screen_size, default=(1280, 800), metavar='WxH',
                       help="the display's preferred mode (default 1280x800)")
    ssh = sub.add_parser('ssh', help='run a command in the guest: ssh -- <command...>')
    ssh.add_argument('command', nargs=argparse.REMAINDER)
    put = sub.add_parser('put', help='copy host -> guest')
    put.add_argument('host_path')
    put.add_argument('guest_path')
    get = sub.add_parser('get', help='copy guest -> host')
    get.add_argument('guest_path')
    get.add_argument('host_path')
    down = sub.add_parser('down', help='ACPI power-off, forced after --timeout')
    down.add_argument('--timeout', type=int, default=90)
    sub.add_parser('status', help='state of the run and the space budget')
    budget = sub.add_parser('budget', help='record a space measurement; exit 1 over budget')
    budget.add_argument('--step', default='manual')
    sub.add_parser('clean', help='archive the evidence and remove the run (never the base)')
    args = parser.parse_args()
    if getattr(args, 'distro', None):
        use_distro(args.distro)
    elif (RUN / 'config.json').exists():
        use_distro(config().get('distro', 'resolute'))
    return {'prepare': cmd_prepare, 'up': cmd_up, 'media': cmd_media, 'ssh': cmd_ssh,
            'put': cmd_put,
            'get': cmd_get, 'down': cmd_down, 'status': cmd_status, 'budget': cmd_budget,
            'clean': cmd_clean}[args.cmd](args)


if __name__ == '__main__':
    sys.exit(main())
