#!/usr/bin/env python3
"""Эталон для D4: что ядро XFS делает с ДЕРЕВЬЯМИ СВОБОДНОГО МЕСТА.

D0 (`xfs-delete-delta.py`) показывает, какие ЗАПИСИ появились и исчезли. Для D4
этого мало: писателю нужно знать, обходится ли ядро правкой внутри существующих
листьев или выделяет новые блоки дерева, трогает ли AGFL и как меняет
`agf_longest` и `agf_btreeblks`. Разница принципиальная — вставка со сплитом
узла это отдельный класс работы и отдельный класс способов испортить ФС.

    sudo python3 scripts/xfs-free-delta.py [--files N] [--size МиБ]
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile

MAGIC = {b"AB3B": "BNOBT", b"AB3C": "CNTBT", b"RMB3": "RMAPBT",
         b"IAB3": "INOBT", b"FIB3": "FINOBT"}
BTREE_HDR = 56


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
        self.sectsize = be(sb, 0x66, 2)
        self.inodesize = be(sb, 0x68, 2)
        self.inopblog = sb[0x7B]
        self.agblklog = sb[0x7C]
        self.icount = be(sb, 0x80, 8)
        self.ifree = be(sb, 0x88, 8)
        self.fdblocks = be(sb, 0x90, 8)

    def ag_off(self, ag):
        return ag * self.agblocks * self.blocksize

    def sector(self, ag, n):
        o = self.ag_off(ag) + n * self.sectsize
        return self.d[o:o + self.sectsize]

    def block(self, ag, agbno):
        o = self.ag_off(ag) + agbno * self.blocksize
        return self.d[o:o + self.blocksize]

    def agf(self, ag):
        b = self.sector(ag, 1)
        assert b[:4] == b"XAGF"
        return dict(
            bno_root=be(b, 0x10, 4), cnt_root=be(b, 0x14, 4), rmap_root=be(b, 0x18, 4),
            bno_level=be(b, 0x1C, 4), cnt_level=be(b, 0x20, 4), rmap_level=be(b, 0x24, 4),
            flfirst=be(b, 0x28, 4), fllast=be(b, 0x2C, 4), flcount=be(b, 0x30, 4),
            freeblks=be(b, 0x34, 4), longest=be(b, 0x38, 4), btreeblks=be(b, 0x3C, 4),
        )

    def agi(self, ag):
        b = self.sector(ag, 2)
        assert b[:4] == b"XAGI"
        return dict(count=be(b, 0x10, 4), freecount=be(b, 0x1C, 4),
                    iblocks=be(b, 0x150, 4), fblocks=be(b, 0x154, 4))

    def tree_blocks(self, ag, root, magic, out=None, depth=0):
        """Все блоки дерева: (agbno, level, numrecs)."""
        if out is None:
            out = []
        if depth > 16 or root == 0:
            return out
        blk = self.block(ag, root)
        if blk[:4] != magic:
            return out
        level = be(blk, 4, 2)
        nrecs = be(blk, 6, 2)
        out.append((root, level, nrecs))
        if level:
            keylen = {b"AB3B": 8, b"AB3C": 8, b"RMB3": 40, b"IAB3": 4, b"FIB3": 4}[magic]
            maxrecs = (self.blocksize - BTREE_HDR) // (keylen + 4)
            ptr = BTREE_HDR + maxrecs * keylen
            for i in range(nrecs):
                self.tree_blocks(ag, be(blk, ptr + i * 4, 4), magic, out, depth + 1)
        return out


def run(*c):
    return subprocess.run(c, check=True, capture_output=True, text=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--files", type=int, default=12)
    ap.add_argument("--size", type=int, default=768)
    args = ap.parse_args()
    if os.geteuid() != 0:
        sys.exit("нужен root: sudo python3 scripts/xfs-free-delta.py")

    tmp = tempfile.mkdtemp(prefix="xfsfree-")
    img, img2, mnt = tmp + "/a.img", tmp + "/b.img", tmp + "/m"
    os.mkdir(mnt)
    open(img, "wb").truncate(args.size * 1024 * 1024)
    run("mkfs.xfs", "-q", "-m", "reflink=1,crc=1", img)

    run("mount", "-o", "loop", img, mnt)
    try:
        # Удаляемые вперемешку с сохраняемыми — так живёт настоящий каталог,
        # и возврат экстента почти никогда не примыкает к чужому свободному.
        for i in range(args.files):
            open(f"{mnt}/keep-{i:03}.bin", "wb").write(b"k" * (256 * 1024))
            open(f"{mnt}/drop-{i:03}.bin", "wb").write(b"d" * (512 * 1024))
    finally:
        run("umount", mnt)
    shutil.copyfile(img, img2)
    run("mount", "-o", "loop", img2, mnt)
    try:
        for i in range(args.files):
            os.unlink(f"{mnt}/drop-{i:03}.bin")
    finally:
        run("umount", mnt)

    A, B = Fs(open(img, "rb").read()), Fs(open(img2, "rb").read())
    print(f"blocksize={A.blocksize} agcount={A.agcount} удалено файлов={args.files}")
    print(f"sb_fdblocks: {A.fdblocks} → {B.fdblocks}  ({B.fdblocks - A.fdblocks:+})")
    print(f"sb_ifree:    {A.ifree} → {B.ifree}  ({B.ifree - A.ifree:+})")

    for ag in range(A.agcount):
        fa, fb = A.agf(ag), B.agf(ag)
        diff = {k: (fa[k], fb[k]) for k in fa if fa[k] != fb[k]}
        ia, ib = A.agi(ag), B.agi(ag)
        idiff = {k: (ia[k], ib[k]) for k in ia if ia[k] != ib[k]}
        if not diff and not idiff:
            continue
        print(f"\n═══ AG {ag} ═══")
        for k, (x, y) in diff.items():
            print(f"  AGF.{k:10s} {x} → {y}  ({y - x:+})")
        for k, (x, y) in idiff.items():
            print(f"  AGI.{k:10s} {x} → {y}  ({y - x:+})")
        for magic, root_key, lvl_key in (
            (b"AB3B", "bno_root", "bno_level"),
            (b"AB3C", "cnt_root", "cnt_level"),
            (b"RMB3", "rmap_root", "rmap_level"),
        ):
            ta = {b: (l, n) for b, l, n in A.tree_blocks(ag, fa[root_key], magic)}
            tb = {b: (l, n) for b, l, n in B.tree_blocks(ag, fb[root_key], magic)}
            name = MAGIC[magic]
            if ta == tb and fa[root_key] == fb[root_key]:
                continue
            print(f"  {name}: корень {fa[root_key]}→{fb[root_key]}, "
                  f"уровень {fa[lvl_key]}→{fb[lvl_key]}, блоков {len(ta)}→{len(tb)}")
            new = sorted(set(tb) - set(ta))
            gone = sorted(set(ta) - set(tb))
            if new:
                print(f"    НОВЫЕ блоки дерева: {new}")
            if gone:
                print(f"    исчезли блоки: {gone}")
            for b in sorted(set(ta) & set(tb)):
                if ta[b] != tb[b]:
                    print(f"    блок {b}: уровень {ta[b][0]}, "
                          f"записей {ta[b][1]} → {tb[b][1]} ({tb[b][1] - ta[b][1]:+})")
                elif A.block(ag, b) != B.block(ag, b):
                    print(f"    блок {b}: уровень {ta[b][0]}, записей {ta[b][1]} "
                          f"(без изменения числа) — правка на месте")

    print("\n── выводы для D4 ──")
    print("Если «НОВЫЕ блоки дерева» пусты и flcount не менялся — хватает")
    print("правки внутри существующих листьев, и AGFL трогать не нужно.")
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
