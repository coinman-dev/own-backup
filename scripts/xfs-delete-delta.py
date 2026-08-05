#!/usr/bin/env python3
"""Эталонная дельта: что ядро XFS меняет при удалении файла — ПО СУЩЕСТВУ.

Строит образ, снимает копию, удаляет в копии штатным rm, затем разбирает оба
образа структурно и печатает разницу записями деревьев и счётчиками, а не
«блок такой-то изменился». Это эталон, с которым сверяется наш «сухой»
планировщик (этап D1, .ai/xfs-true-removal-plan_2026-08-05.md) — тот же приём,
что дал формулу h_checksum в P8: смотреть, что делает эталон.

Нужен root (loop mount) и xfsprogs:

    sudo python3 scripts/xfs-delete-delta.py
"""
import os, subprocess, sys, shutil, tempfile

# --- разбор структур XFS v5 -------------------------------------------------

BTREE_HDR_V5 = 56  # короткая форма (в пределах AG): xfs_btree_sblock v5

MAGIC_BNO = b"AB3B"
MAGIC_CNT = b"AB3C"
MAGIC_INO = b"IAB3"
MAGIC_FINO = b"FIB3"
MAGIC_RMAP = b"RMB3"

SPINODES = 0x2  # XFS_SB_FEAT_INCOMPAT_SPINODES (бит 0 — это FTYPE)


def be(b, o, n):
    return int.from_bytes(b[o:o + n], "big")


class Fs:
    def __init__(self, data):
        self.d = data
        sb = data[:512]
        assert sb[:4] == b"XFSB", "не XFS"
        self.blocksize = be(sb, 0x04, 4)
        self.agblocks = be(sb, 0x54, 4)
        self.agcount = be(sb, 0x58, 4)
        self.inodesize = be(sb, 0x68, 2)
        self.sectsize = be(sb, 0x66, 2)
        self.agblklog = sb[0x7C]
        self.icount = be(sb, 0x80, 8)
        self.ifree = be(sb, 0x88, 8)
        self.fdblocks = be(sb, 0x90, 8)
        self.incompat = be(sb, 0xD8, 4)
        self.sparse = bool(self.incompat & SPINODES)

    def ag_off(self, ag):
        return ag * self.agblocks * self.blocksize

    def block(self, ag, agbno):
        o = self.ag_off(ag) + agbno * self.blocksize
        return self.d[o:o + self.blocksize]

    def agf(self, ag):
        o = self.ag_off(ag) + self.sectsize
        b = self.d[o:o + self.sectsize]
        assert b[:4] == b"XAGF", f"AG {ag}: нет XAGF"
        return {
            "freeblks": be(b, 0x34, 4),
            "longest": be(b, 0x38, 4),
            "btreeblks": be(b, 0x3C, 4),
            "flcount": be(b, 0x30, 4),
            "bno_root": be(b, 0x10, 4), "bno_level": be(b, 0x1C, 4),
            "cnt_root": be(b, 0x14, 4), "cnt_level": be(b, 0x20, 4),
            "rmap_root": be(b, 0x18, 4), "rmap_level": be(b, 0x24, 4),
        }

    def agi(self, ag):
        o = self.ag_off(ag) + 2 * self.sectsize
        b = self.d[o:o + self.sectsize]
        assert b[:4] == b"XAGI", f"AG {ag}: нет XAGI"
        return {
            "count": be(b, 0x10, 4),
            "freecount": be(b, 0x1C, 4),
            "root": be(b, 0x14, 4), "level": be(b, 0x18, 4),
            "newino": be(b, 0x20, 4),
            "free_root": be(b, 0x148, 4), "free_level": be(b, 0x14C, 4),
        }

    # (длина записи, длина ключа, перекрывающееся ли дерево) по магии.
    # У перекрывающегося дерева (rmapbt) во внутреннем узле ДВА ключа на слот —
    # низкий и высокий, — иначе указатели считаются не с того смещения.
    SHAPE = {
        MAGIC_BNO: (8, 8, False),
        MAGIC_CNT: (8, 8, False),
        MAGIC_INO: (16, 4, False),
        MAGIC_FINO: (16, 4, False),
        MAGIC_RMAP: (24, 20, True),
    }

    def walk(self, ag, root, magic, parse):
        """Записи листьев B+дерева короткой формы (в пределах AG)."""
        reclen, keylen, overlapped = self.SHAPE[magic]
        blk = self.block(ag, root)
        if blk[:4] != magic:
            raise AssertionError(f"AG {ag} блок {root}: магия {blk[:4]!r} != {magic!r}")
        level = be(blk, 0x04, 2)
        nrecs = be(blk, 0x06, 2)
        if level == 0:
            return [parse(blk, BTREE_HDR_V5 + i * reclen) for i in range(nrecs)]
        slot = keylen * (2 if overlapped else 1)
        maxrecs = (self.blocksize - BTREE_HDR_V5) // (slot + 4)
        ptr_off = BTREE_HDR_V5 + maxrecs * slot
        out = []
        for i in range(nrecs):
            out += self.walk(ag, be(blk, ptr_off + i * 4, 4), magic, parse)
        return out

    def free_extents(self, ag, by_size=False):
        a = self.agf(ag)
        root = a["cnt_root"] if by_size else a["bno_root"]
        magic = MAGIC_CNT if by_size else MAGIC_BNO
        return set(self.walk(ag, root, magic,
                             lambda b, o: (be(b, o, 4), be(b, o + 4, 4))))

    def inode_records(self, ag, free_tree=False):
        a = self.agi(ag)
        root = a["free_root"] if free_tree else a["root"]
        magic = MAGIC_FINO if free_tree else MAGIC_INO
        if root == 0:
            return set()

        def parse(b, o):
            startino = be(b, o, 4)
            if self.sparse:
                holemask, count, freecount = be(b, o + 4, 2), b[o + 6], b[o + 7]
            else:
                holemask, count, freecount = 0, 64, be(b, o + 4, 4)
            return (startino, holemask, count, freecount, be(b, o + 8, 8))

        return set(self.walk(ag, root, magic, parse))

    def rmap_records(self, ag):
        a = self.agf(ag)
        if a["rmap_root"] == 0:
            return set()
        return set(self.walk(ag, a["rmap_root"], MAGIC_RMAP,
                             lambda b, o: (be(b, o, 4), be(b, o + 4, 4),
                                           be(b, o + 8, 8), be(b, o + 16, 8))))


# --- сравнение --------------------------------------------------------------

def fmt_ino_rec(r):
    startino, holemask, count, freecount, free = r
    return (f"startino={startino} count={count} freecount={freecount} "
            f"holemask={holemask:#06x} free={free:#018x}")


RMAP_OWNERS = {
    0xFFFFFFFFFFFFFFFF: "OWN_NULL", 0xFFFFFFFFFFFFFFFE: "OWN_UNKNOWN",
    0xFFFFFFFFFFFFFFFD: "OWN_FS", 0xFFFFFFFFFFFFFFFC: "OWN_LOG",
    0xFFFFFFFFFFFFFFFB: "OWN_AG", 0xFFFFFFFFFFFFFFFA: "OWN_INOBT",
    0xFFFFFFFFFFFFFFF9: "OWN_INODES", 0xFFFFFFFFFFFFFFF8: "OWN_REFC",
    0xFFFFFFFFFFFFFFF7: "OWN_COW",
}


def fmt_rmap(r):
    start, count, owner, offset = r
    own = RMAP_OWNERS.get(owner, f"инод {owner}")
    return f"agbno={start} len={count} владелец={own} offset={offset & ((1 << 54) - 1)}"


def compare(a: "Fs", b: "Fs"):
    print("  суперблок:")
    for name in ("icount", "ifree", "fdblocks"):
        x, y = getattr(a, name), getattr(b, name)
        if x != y:
            print(f"    sb_{name}: {x} → {y}  ({y - x:+d})")
    for ag in range(a.agcount):
        head = []
        fa, fb = a.agf(ag), b.agf(ag)
        for k in fa:
            if fa[k] != fb[k]:
                head.append(f"AGF.{k}: {fa[k]} → {fb[k]}")
        ia, ib = a.agi(ag), b.agi(ag)
        for k in ia:
            if ia[k] != ib[k]:
                head.append(f"AGI.{k}: {ia[k]} → {ib[k]}")
        bno_a, bno_b = a.free_extents(ag), b.free_extents(ag)
        cnt_a, cnt_b = a.free_extents(ag, True), b.free_extents(ag, True)
        ino_a, ino_b = a.inode_records(ag), b.inode_records(ag)
        fin_a, fin_b = a.inode_records(ag, True), b.inode_records(ag, True)
        rm_a, rm_b = a.rmap_records(ag), b.rmap_records(ag)
        if not (head or bno_a ^ bno_b or cnt_a ^ cnt_b or ino_a ^ ino_b
                or fin_a ^ fin_b or rm_a ^ rm_b):
            continue
        print(f"  AG {ag}:")
        for h in head:
            print(f"    {h}")
        for tag, sa, sb_ in (("BNOBT", bno_a, bno_b), ("CNTBT", cnt_a, cnt_b)):
            for r in sorted(sb_ - sa):
                print(f"    {tag} +  agbno={r[0]} len={r[1]}")
            for r in sorted(sa - sb_):
                print(f"    {tag} -  agbno={r[0]} len={r[1]}")
        for tag, sa, sb_ in (("INOBT", ino_a, ino_b), ("FINOBT", fin_a, fin_b)):
            for r in sorted(sb_ - sa):
                print(f"    {tag} +  {fmt_ino_rec(r)}")
            for r in sorted(sa - sb_):
                print(f"    {tag} -  {fmt_ino_rec(r)}")
        for r in sorted(rm_b - rm_a):
            print(f"    RMAPBT +  {fmt_rmap(r)}")
        for r in sorted(rm_a - rm_b):
            print(f"    RMAPBT -  {fmt_rmap(r)}")


# --- сценарии ---------------------------------------------------------------

def run(*cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"{' '.join(cmd)}: {r.stderr or r.stdout}")
    return r.stdout


def scenario(name, build, delete, mkfs_args=("-m", "reflink=1,crc=1"), size_mb=512):
    work = tempfile.mkdtemp(prefix="xfsd-")
    img, img2, mnt = f"{work}/a.img", f"{work}/b.img", f"{work}/mnt"
    os.makedirs(mnt)
    with open(img, "wb") as f:
        f.truncate(size_mb * 1024 * 1024)
    run("mkfs.xfs", "-q", *mkfs_args, img)
    run("mount", "-o", "loop", img, mnt)
    build(mnt)
    run("sync")
    run("umount", mnt)
    shutil.copyfile(img, img2)
    run("mount", "-o", "loop", img2, mnt)
    delete(mnt)
    run("sync")
    run("umount", mnt)
    print(f"\n=== {name}")
    compare(Fs(open(img, "rb").read()), Fs(open(img2, "rb").read()))
    shutil.rmtree(work, ignore_errors=True)


def w(p, n, byte=0xAB):
    with open(p, "wb") as f:
        f.write(bytes([byte]) * n)


if __name__ == "__main__":
    if os.geteuid() != 0:
        sys.exit("нужен root: sudo python3 scripts/xfs-delete-delta.py")

    scenario(
        "shortform-каталог: удалить файл 4 КиБ",
        lambda m: (os.makedirs(f"{m}/d"), [w(f"{m}/d/f{i}.bin", 4096) for i in range(3)]),
        lambda m: os.remove(f"{m}/d/f1.bin"),
    )

    def build_dense(m):
        os.makedirs(f"{m}/dense")
        for i in range(400):
            w(f"{m}/dense/name{i:03d}.tmp", 100)
        w(f"{m}/big.bin", 8 * 1024 * 1024)

    scenario(
        "плотный каталог (400 имён) + файл 8 МиБ",
        build_dense,
        lambda m: (os.remove(f"{m}/dense/name200.tmp"), os.remove(f"{m}/big.bin")),
    )

    def build_frag(m):
        with open(f"{m}/frag.bin", "wb") as f:
            for _ in range(200):
                f.write(bytes([0xAB]) * 65536)
                f.flush()
                os.fsync(f.fileno())
        for i in range(100):
            w(f"{m}/spacer{i}.bin", 65536)

    scenario(
        "фрагментированный файл ~13 МиБ",
        build_frag,
        lambda m: os.remove(f"{m}/frag.bin"),
    )

    scenario(
        "целый чанк инодов: 80 файлов, удалить все",
        lambda m: (os.makedirs(f"{m}/chunk"),
                   [w(f"{m}/chunk/c{i:03d}.bin", 100) for i in range(80)]),
        lambda m: [os.remove(f"{m}/chunk/c{i:03d}.bin") for i in range(80)],
    )

    scenario(
        "без rmapbt/reflink — насколько проще",
        build_dense,
        lambda m: (os.remove(f"{m}/dense/name200.tmp"), os.remove(f"{m}/big.bin")),
        mkfs_args=("-m", "reflink=0,rmapbt=0,crc=1"),
    )
