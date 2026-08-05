#!/usr/bin/env python3
"""Чем считается h_checksum блока расширенных атрибутов ext4 (пункт P8 плана
исключений). Ответ снят с живого образа, а не из чужих исходников: скрипт
строит ext4, делает xattr-блок общим для двух инодов руками e2fsck (он же
пишет эталонную сумму) и сверяет кандидатов при refcount 1 и 2.

Итог, перенесённый в `xattr_block_csum` (crates/backup/src/ext_exclusions.rs):
crc32c(csum_seed, LE64(номер блока)), затем по всему блоку с обнулённым полем
h_checksum (@0x10). Номер инода не участвует.

Запуск: python3 scripts/ext-xattr-csum-probe.py (нужны mke2fs/debugfs/e2fsck).
"""
import os, subprocess, sys, struct, shutil, tempfile

POLY = 0x82F63B78
TAB = []
for i in range(256):
    c = i
    for _ in range(8):
        c = (c >> 1) ^ (POLY if c & 1 else 0)
    TAB.append(c)

def crc32c(seed, data):
    c = seed & 0xFFFFFFFF
    for b in data:
        c = (c >> 8) ^ TAB[(c ^ b) & 0xFF]
    return c & 0xFFFFFFFF

def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)

def dbg(img, cmd, write=False):
    args = ["debugfs"] + (["-w"] if write else []) + ["-R", cmd, img]
    r = run(args)
    return r.stdout + r.stderr

WORK = tempfile.mkdtemp(prefix="ext-xattr-probe-")
os.makedirs(WORK + "/src/cache")
open(WORK + "/src/cache/a.bin", "wb").write(b"\xAB" * 8192)
open(WORK + "/src/cache/b.bin", "wb").write(b"\xAB" * 8192)
open(WORK + "/src/keep.txt", "wb").write(b"\xCD" * 4096)
IMG = WORK + "/fs.img"
r = run(["mke2fs", "-q", "-F", "-t", "ext4", "-b", "4096", "-d", WORK + "/src", IMG, "16384"])
if r.returncode:
    sys.exit("mke2fs: " + r.stderr)

BS = 4096
big = "A" * 300
print(dbg(IMG, f"ea_set /cache/a.bin user.big {big}", write=True))

def stat_field(img, path, field):
    out = dbg(img, f"stat {path}")
    for line in out.splitlines():
        line = line.strip()
        if line.startswith(field):
            return line
    return out

def file_acl(img, path):
    out = dbg(img, f"stat {path}")
    for line in out.splitlines():
        if "File ACL:" in line:
            tail = line.split("File ACL:")[1].strip()
            return int(tail.split()[0])
    return 0

def inode_num(img, path):
    out = dbg(img, f"stat {path}")
    # "Inode: 13   Type: regular    Mode:  0644 ..."
    for line in out.splitlines():
        if line.startswith("Inode:"):
            return int(line.split()[1])
    return 0

acl = file_acl(IMG, "/cache/a.bin")
ino_a = inode_num(IMG, "/cache/a.bin")
ino_b = inode_num(IMG, "/cache/b.bin")
print(f"xattr block = {acl}, ino a = {ino_a}, ino b = {ino_b}")
if acl == 0:
    sys.exit("xattr остался в иноде")

sb = open(IMG, "rb").read(2048)[1024:]
incompat = struct.unpack_from("<I", sb, 0x60)[0]
uuid = sb[0x68:0x78]
if incompat & 0x2000:  # INCOMPAT_CSUM_SEED
    seed = struct.unpack_from("<I", sb, 0x270)[0]
else:
    seed = crc32c(0xFFFFFFFF, uuid)
print(f"csum_seed = {seed:#010x} (incompat={incompat:#x})")

def read_block(img, blk):
    with open(img, "rb") as f:
        f.seek(blk * BS)
        return f.read(BS)

def candidates(block, blk, ino, gen):
    z = bytearray(block)
    struct.pack_into("<I", z, 0x10, 0)  # h_checksum @0x10 = 0
    z = bytes(z)
    out = {}
    out["block_nr+block"] = crc32c(crc32c(seed, struct.pack("<Q", blk)), z)
    out["block only"] = crc32c(seed, z)
    out["ino+block"] = crc32c(crc32c(seed, struct.pack("<I", ino)), z)
    out["ino+gen+block"] = crc32c(crc32c(crc32c(seed, struct.pack("<I", ino)), struct.pack("<I", gen)), z)
    out["ino_le64+block"] = crc32c(crc32c(seed, struct.pack("<Q", ino)), z)
    return out

def report(tag, blk, ino, gen):
    b = read_block(IMG, blk)
    magic, refc, blocks, h_hash, csum = struct.unpack_from("<IIIII", b, 0)
    print(f"\n== {tag}: magic={magic:#x} refcount={refc} h_blocks={blocks} "
          f"h_hash={h_hash:#x} h_checksum={csum:#010x}")
    for name, v in candidates(b, blk, ino, gen).items():
        print(f"   {name:20s} {v:#010x} {'<== СОВПАЛО' if v == csum else ''}")
    return refc, csum

def generation(img, path):
    out = dbg(img, f"stat {path}")
    for line in out.splitlines():
        if "Generation:" in line:
            return int(line.split("Generation:")[1].split()[0])
    return 0

gen_a = generation(IMG, "/cache/a.bin")
report("refcount=1 (написал e2fsprogs)", acl, ino_a, gen_a)

# Сделать блок общим: второй инод указывает на тот же xattr-блок.
print(dbg(IMG, f"sif /cache/b.bin file_acl {acl}", write=True))
r = run(["e2fsck", "-fy", IMG])
print("e2fsck -fy:\n" + r.stdout[-3000:])
r = run(["e2fsck", "-fn", IMG])
print(f"e2fsck -fn rc={r.returncode}\n" + r.stdout[-2000:])

acl2 = file_acl(IMG, "/cache/a.bin")
gen_a = generation(IMG, "/cache/a.bin")
gen_b = generation(IMG, "/cache/b.bin")
print(f"after fix: acl(a)={acl2} acl(b)={file_acl(IMG, '/cache/b.bin')}")
refc, csum = report("refcount=2 (написал e2fsck)", acl2, ino_a, gen_a)
print("\n-- те же кандидаты, но с инодом B:")
b = read_block(IMG, acl2)
for name, v in candidates(b, acl2, ino_b, gen_b).items():
    print(f"   {name:20s} {v:#010x} {'<== СОВПАЛО' if v == csum else ''}")

shutil.copy(IMG, WORK + "/shared.img")
print(f"\nобраз с refcount={refc} сохранён: {WORK}/shared.img (блок {acl2}, иноды {ino_a}/{ino_b})")
