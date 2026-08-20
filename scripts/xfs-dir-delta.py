#!/usr/bin/env python3
"""Эталон для D2: что ядро XFS делает с КАТАЛОГОМ, когда удаляет из него имя.

D0 (`xfs-delete-delta.py`) отвечает на вопрос про деревья и счётчики AG. Он
намеренно не смотрит внутрь каталога, а именно туда пишет шаг D2. Этот скрипт
закрывает пробел: строит образ, где каталоги нарочно разных форм (shortform,
block, leaf, node), удаляет из каждого по одному имени ШТАТНЫМ rm и печатает
разницу — расшифрованными полями, а не «блок такой-то изменился».

Тот же приём, что дал формулу h_checksum и план D1: сначала посмотреть на
эталон, потом писать код. Правило проекта — с чужим форматом сначала проба на
настоящих данных здесь, и только потом перенос в Rust.

    sudo python3 scripts/xfs-dir-delta.py [--keep]

Нужен root (loop mount) и xfsprogs.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile

# --- формы каталога ---------------------------------------------------------

FMT_LOCAL, FMT_EXTENTS, FMT_BTREE = 1, 2, 3

DIR3_BLOCK_MAGIC = 0x58444233  # "XDB3"
DIR3_DATA_MAGIC = 0x58444433   # "XDD3"
DIR3_FREE_MAGIC = 0x58444633   # "XDF3"
DIR3_LEAF1_MAGIC = 0x3DF1
DIR3_LEAFN_MAGIC = 0x3DFF

DIR3_DATA_HDR = 64   # xfs_dir3_data_hdr (включая 3 записи bestfree)
DA3_BLKINFO = 56     # xfs_da3_blkinfo

FTYPE_NAME = {0: "?", 1: "reg", 2: "dir", 3: "chr", 4: "blk", 5: "fifo", 6: "sock", 7: "link"}


def be(b, o, n):
    return int.from_bytes(b[o:o + n], "big")


class Fs:
    def __init__(self, data):
        self.d = data
        sb = data[:512]
        assert sb[:4] == b"XFSB", "не XFS"
        self.blocksize = be(sb, 0x04, 4)
        self.rootino = be(sb, 0x38, 8)
        self.agblocks = be(sb, 0x54, 4)
        self.agcount = be(sb, 0x58, 4)
        self.sectsize = be(sb, 0x66, 2)
        self.inodesize = be(sb, 0x68, 2)
        self.inopblog = sb[0x7B]
        self.agblklog = sb[0x7C]
        self.incompat = be(sb, 0xD8, 4)
        self.ftype = bool(self.incompat & 0x1)   # XFS_SB_FEAT_INCOMPAT_FTYPE

    def inode_off(self, ino):
        """Физическое смещение инода по его номеру."""
        shift = self.agblklog + self.inopblog
        agno = ino >> shift
        agino = ino & ((1 << shift) - 1)
        agbno = agino >> self.inopblog
        slot = agino & ((1 << self.inopblog) - 1)
        return (agno * self.agblocks + agbno) * self.blocksize + slot * self.inodesize

    def inode(self, ino):
        o = self.inode_off(ino)
        return self.d[o:o + self.inodesize]

    def fsb_off(self, fsb):
        agno = fsb >> self.agblklog
        agbno = fsb & ((1 << self.agblklog) - 1)
        return (agno * self.agblocks + agbno) * self.blocksize

    def block(self, fsb):
        o = self.fsb_off(fsb)
        return self.d[o:o + self.blocksize]


def data_fork(fs, inode):
    """(начало, конец) фока данных внутри инода."""
    forkoff = inode[0x52]
    start = 176  # v3 inode core
    end = start + forkoff * 8 if forkoff else fs.inodesize
    return start, end


def fork_extents(fs, inode):
    """Экстенты фока данных для формы EXTENTS: [(startoff, startblock, count)]."""
    s, e = data_fork(fs, inode)
    n = be(inode, 0x4C, 4)
    out = []
    for i in range(n):
        o = s + i * 16
        if o + 16 > e:
            break
        l0, l1 = be(inode, o, 8), be(inode, o + 8, 8)
        startoff = (l0 >> 9) & ((1 << 54) - 1)
        startblock = ((l0 & 0x1FF) << 43) | (l1 >> 21)
        count = l1 & ((1 << 21) - 1)
        out.append((startoff, startblock, count))
    return out


# --- разбор содержимого -----------------------------------------------------


def parse_shortform(fs, inode):
    s, e = data_fork(fs, inode)
    sf = inode[s:e]
    count, i8count = sf[0], sf[1]
    ino_sz = 8 if i8count else 4
    p = 2 + ino_sz
    ents = []
    for _ in range(count):
        namelen = sf[p]
        off = be(sf, p + 1, 2)
        name = sf[p + 3:p + 3 + namelen].decode("utf-8", "replace")
        q = p + 3 + namelen
        ftype = sf[q] if fs.ftype else 0
        if fs.ftype:
            q += 1
        ino = be(sf, q, ino_sz)
        ents.append(dict(name=name, ino=ino, ftype=ftype, off=off, at=p, len=q + ino_sz - p))
        p = q + ino_sz
    return dict(count=count, i8count=i8count, used=p, ents=ents)


def parse_data_block(fs, blk):
    """Записи и bestfree одного data/block-блока."""
    magic = be(blk, 0, 4)
    hdr = DIR3_DATA_HDR
    bestfree = [(be(blk, 0x30 + i * 4, 2), be(blk, 0x30 + i * 4 + 2, 2)) for i in range(3)]
    ents, holes = [], []
    p = hdr
    limit = len(blk)
    if magic == DIR3_BLOCK_MAGIC:
        # У block-формы в хвосте лежат хеш-массив и tail — записи кончаются
        # раньше конца блока.
        count = be(blk, len(blk) - 8, 4)
        stale = be(blk, len(blk) - 4, 4)
        limit = len(blk) - 8 - count * 8
    else:
        count = stale = None
    while p + 8 <= limit:
        if be(blk, p, 2) == 0xFFFF:
            ln = be(blk, p + 2, 2)
            if ln == 0:
                break
            holes.append((p, ln))
            p += ln
            continue
        ino = be(blk, p, 8)
        namelen = blk[p + 8]
        if namelen == 0 or p + 9 + namelen > limit:
            break
        name = blk[p + 9:p + 9 + namelen].decode("utf-8", "replace")
        ftype = blk[p + 9 + namelen] if fs.ftype else 0
        raw = 8 + 1 + namelen + (1 if fs.ftype else 0) + 2
        rec = (raw + 7) & ~7
        ents.append(dict(name=name, ino=ino, ftype=ftype, at=p, len=rec))
        p += rec
    return dict(magic=magic, bestfree=bestfree, ents=ents, holes=holes,
                count=count, stale=stale)


def parse_leaf(fs, blk):
    """Хеш-массив листа (leaf1 или leafn)."""
    magic = be(blk, 8, 2)
    hdr = DA3_BLKINFO
    count = be(blk, hdr, 2)
    stale = be(blk, hdr + 2, 2)
    ents = [(be(blk, hdr + 4 + i * 8, 4), be(blk, hdr + 4 + i * 8 + 4, 4)) for i in range(count)]
    return dict(magic=magic, count=count, stale=stale, ents=ents)


def dir_shape(fs, inode):
    """Форма каталога словами: shortform / block / leaf / node."""
    fmt = inode[5]
    if fmt == FMT_LOCAL:
        return "shortform", []
    if fmt == FMT_BTREE:
        return "btree", []
    exts = fork_extents(fs, inode)
    magics = []
    for (startoff, startblock, count) in exts:
        for i in range(count):
            blk = fs.block(startblock + i)
            m32 = be(blk, 0, 4)
            m16 = be(blk, 8, 2)
            if m32 == DIR3_BLOCK_MAGIC:
                magics.append("block")
            elif m32 == DIR3_DATA_MAGIC:
                magics.append("data")
            elif m32 == DIR3_FREE_MAGIC:
                magics.append("free")
            elif m16 == DIR3_LEAF1_MAGIC:
                magics.append("leaf1")
            elif m16 == DIR3_LEAFN_MAGIC:
                magics.append("leafn")
            else:
                magics.append("?%08x" % m32)
    if "block" in magics:
        return "block", exts
    if "leafn" in magics or "free" in magics:
        return "node", exts
    if "leaf1" in magics:
        return "leaf", exts
    return "data-only", exts


# --- сравнение --------------------------------------------------------------


def diff_ranges(a, b, base=0):
    """Изменившиеся диапазоны байт: [(off, len)]."""
    out = []
    n = min(len(a), len(b))
    i = 0
    while i < n:
        if a[i] != b[i]:
            j = i
            while j < n and a[j] != b[j]:
                j += 1
            out.append((base + i, j - i))
            i = j
        else:
            i += 1
    return out


def show_dir(tag, fs, ino, victim):
    inode = fs.inode(ino)
    shape, exts = dir_shape(fs, inode)
    print(f"  [{tag}] форма={shape} размер={be(inode, 0x38, 8)} "
          f"nextents={be(inode, 0x4C, 4)} nblocks={be(inode, 0x40, 8)}")
    if shape == "shortform":
        sf = parse_shortform(fs, inode)
        names = [e["name"] for e in sf["ents"]]
        print(f"        count={sf['count']} i8count={sf['i8count']} "
              f"занято={sf['used']} байт, имена={names}")
        return sf
    out = {}
    for (startoff, startblock, count) in exts:
        for i in range(count):
            blk = fs.block(startblock + i)
            m32, m16 = be(blk, 0, 4), be(blk, 8, 2)
            if m32 in (DIR3_BLOCK_MAGIC, DIR3_DATA_MAGIC):
                d = parse_data_block(fs, blk)
                mark = "*" if any(e["name"] == victim for e in d["ents"]) else " "
                print(f"       {mark}fsb={startblock + i} data: записей={len(d['ents'])} "
                      f"дыр={len(d['holes'])} bestfree={d['bestfree']}"
                      + (f" count={d['count']} stale={d['stale']}" if d["count"] is not None else ""))
                out[startblock + i] = d
            elif m16 in (DIR3_LEAF1_MAGIC, DIR3_LEAFN_MAGIC):
                lf = parse_leaf(fs, blk)
                kind = "leaf1" if m16 == DIR3_LEAF1_MAGIC else "leafn"
                print(f"        fsb={startblock + i} {kind}: count={lf['count']} stale={lf['stale']}")
                out[startblock + i] = lf
            elif m32 == DIR3_FREE_MAGIC:
                print(f"        fsb={startblock + i} free (индекс свободного места)")
    return out


def compare(before, after, ino, victim, tag):
    """Побайтовая разница инода и блоков каталога, с расшифровкой."""
    print(f"\n  ── что изменилось в [{tag}] ──")
    ib, ia = before.inode(ino), after.inode(ino)
    ioff = before.inode_off(ino)
    dr = diff_ranges(ib, ia)
    if dr:
        print(f"    инод @{ioff}: диапазоны {dr}")
        for (o, ln) in dr:
            named = {0x38: "di_size", 0x40: "di_nblocks", 0x4C: "di_nextents",
                     0x64: "di_crc", 0x68: "di_changecount", 0x70: "di_lsn",
                     0x10: "di_nlink", 0x28: "di_mtime", 0x30: "di_ctime"}
            hit = [v for k, v in named.items() if o <= k < o + ln]
            print(f"      +{o} ({ln} б) {' '.join(hit)}  "
                  f"{ib[o:o+ln].hex()} → {ia[o:o+ln].hex()}")
    shape_b, exts_b = dir_shape(before, ib)
    shape_a, exts_a = dir_shape(after, ia)
    if shape_b != shape_a:
        print(f"    ФОРМА СМЕНИЛАСЬ: {shape_b} → {shape_a}")
    if shape_b == "shortform" and shape_a == "shortform":
        sb_, sa = parse_shortform(before, ib), parse_shortform(after, ia)
        print(f"    shortform: count {sb_['count']}→{sa['count']}, "
              f"i8count {sb_['i8count']}→{sa['i8count']}, "
              f"занято {sb_['used']}→{sa['used']} байт")
        gone = {e["name"] for e in sb_["ents"]} - {e["name"] for e in sa["ents"]}
        print(f"    исчезло имя: {gone}")
        print("    остальные записи сдвинулись:",
              [(e["name"], e["at"]) for e in sb_["ents"]], "→",
              [(e["name"], e["at"]) for e in sa["ents"]])
        return
    # блочные формы: сравнить те же fsb
    fsbs_b = {sb_ + i for (_, sb_, c) in exts_b for i in range(c)}
    fsbs_a = {sb_ + i for (_, sb_, c) in exts_a for i in range(c)}
    for fsb in sorted(fsbs_b & fsbs_a):
        bb, ba = before.block(fsb), after.block(fsb)
        if bb == ba:
            continue
        d = diff_ranges(bb, ba)
        m32, m16 = be(bb, 0, 4), be(bb, 8, 2)
        kind = ("block" if m32 == DIR3_BLOCK_MAGIC else
                "data" if m32 == DIR3_DATA_MAGIC else
                "free" if m32 == DIR3_FREE_MAGIC else
                "leaf1" if m16 == DIR3_LEAF1_MAGIC else
                "leafn" if m16 == DIR3_LEAFN_MAGIC else "?")
        total = sum(l for _, l in d)
        print(f"    fsb={fsb} ({kind}) @{before.fsb_off(fsb)}: {len(d)} диапазонов, {total} байт")
        for (o, ln) in d[:12]:
            note = ""
            if o < 0x30:
                note = "заголовок (crc @0x0C)" if 0x0C <= o < 0x10 else "заголовок"
            elif 0x30 <= o < 0x48:
                note = "bestfree"
            print(f"      +{o} ({ln} б) {note}  {bb[o:o+ln].hex()[:48]} → {ba[o:o+ln].hex()[:48]}")
        if len(d) > 12:
            print(f"      … ещё {len(d) - 12} диапазонов")
        if kind in ("block", "data"):
            db, da = parse_data_block(before, bb), parse_data_block(after, ba)
            gone = {e["name"] for e in db["ents"]} - {e["name"] for e in da["ents"]}
            if gone:
                print(f"      исчезло имя: {gone}")
            print(f"      bestfree {db['bestfree']} → {da['bestfree']}")
            print(f"      дыры {db['holes']} → {da['holes']}")
            if db["count"] is not None:
                print(f"      tail count {db['count']}→{da['count']} "
                      f"stale {db['stale']}→{da['stale']}")
        elif kind in ("leaf1", "leafn"):
            lb, la = parse_leaf(before, bb), parse_leaf(after, ba)
            print(f"      leaf count {lb['count']}→{la['count']} "
                  f"stale {lb['stale']}→{la['stale']}")
            if lb["ents"] != la["ents"]:
                bset, aset = set(lb["ents"]), set(la["ents"])
                print(f"      ушло из хеш-массива: {sorted(bset - aset)[:4]}")
                print(f"      появилось:           {sorted(aset - bset)[:4]}")


# --- построение образа ------------------------------------------------------


def run(*cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw)


def populate(mnt):
    """Каталоги четырёх форм; в каждом жертва и соседи вокруг неё."""
    plan = {"sf": 3, "blk": 24, "leaf": 400, "node": 6000}
    inos = {}
    for name, n in plan.items():
        d = os.path.join(mnt, name)
        os.mkdir(d)
        for i in range(n):
            with open(os.path.join(d, f"file-{i:05d}.dat"), "wb") as f:
                f.write(b"x" * 32)
        # Жертва — в середине, чтобы удаление не оказалось краевым случаем
        # («последняя запись» умеет схлопываться иначе, чем запись посередине).
        with open(os.path.join(d, "VICTIM-name-to-remove.dat"), "wb") as f:
            f.write(b"v" * 4096)
        for i in range(n, n + 2):
            with open(os.path.join(d, f"file-{i:05d}.dat"), "wb") as f:
                f.write(b"x" * 32)
        inos[name] = os.stat(d).st_ino
    return inos


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="не удалять образы")
    ap.add_argument("--size", default=768, type=int, help="размер образа, МиБ")
    args = ap.parse_args()

    if os.geteuid() != 0:
        sys.exit("нужен root: sudo python3 scripts/xfs-dir-delta.py")
    for t in ("mkfs.xfs", "xfs_repair"):
        if not shutil.which(t):
            sys.exit(f"нет {t} (поставьте xfsprogs)")

    tmp = tempfile.mkdtemp(prefix="xfsdir-")
    img = os.path.join(tmp, "before.img")
    img2 = os.path.join(tmp, "after.img")
    mnt = os.path.join(tmp, "mnt")
    os.mkdir(mnt)

    with open(img, "wb") as f:
        f.truncate(args.size * 1024 * 1024)
    run("mkfs.xfs", "-q", "-m", "reflink=1,crc=1", img)

    run("mount", "-o", "loop", img, mnt)
    try:
        inos = populate(mnt)
    finally:
        run("umount", mnt)

    shutil.copyfile(img, img2)
    run("mount", "-o", "loop", img2, mnt)
    try:
        for name in inos:
            os.unlink(os.path.join(mnt, name, "VICTIM-name-to-remove.dat"))
    finally:
        run("umount", mnt)

    before = Fs(open(img, "rb").read())
    after = Fs(open(img2, "rb").read())

    print(f"blocksize={before.blocksize} inodesize={before.inodesize} "
          f"ftype={before.ftype} agcount={before.agcount}")
    for name, ino in inos.items():
        print(f"\n═══════ каталог /{name} (инод {ino}) ═══════")
        print("  ДО:")
        show_dir("до", before, ino, "VICTIM-name-to-remove.dat")
        print("  ПОСЛЕ:")
        show_dir("после", after, ino, "VICTIM-name-to-remove.dat")
        compare(before, after, ino, "VICTIM-name-to-remove.dat", name)

    if args.keep:
        print(f"\nобразы оставлены: {img}  {img2}")
    else:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
