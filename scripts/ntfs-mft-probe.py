#!/usr/bin/env python3
"""Проба этапа B0 плана .ai/exclusions-before-copy-plan_2026-08-19.md.

Читает $MFT прямо из образа (VHDX dynamic или сырой том), собирает пути по
родительским ссылкам, прогоняет тот же матчер исключений, что в
`backup/src/exclusions.rs`, и достаёт run list'ы совпавших файлов. Ни одного
открытия файла, ни одного обхода каталогов — ровно то, что план предлагает
делать в Rust вместо нынешнего дерева.

Смысл пробы (правило «Проверять на Python до Rust»): убедиться на РЕАЛЬНОМ
образе, что
  1. последовательное чтение MFT даёт тот же список исключаемых файлов, что и
     обход дерева (числа из лога прогона 2026-08-19: 17 978 совпадений,
     3 663 МиБ, 703 317 записей обхода);
  2. это занимает минуты, а не шестнадцать минут;
  3. видно, сколько файлов упирается в ловушки, названные в плане:
     ATTRIBUTE_LIST, резидентные данные, sparse/compressed.

Запуск:
    scripts/ntfs-mft-probe.py ОБРАЗ [--offset БАЙТ] [--limit N]
                              [--patterns ФАЙЛ] [--list ФАЙЛ] [--quiet]

`--offset` — смещение тома внутри образа (для копии целого диска);
по умолчанию 0, то есть образ и есть том.
"""

from __future__ import annotations

import argparse
import os
import struct
import sys
import time
import uuid

# --- те же 12 правил, что отдаёт core::default_exclusions() -----------------
DEFAULT_PATTERNS = [
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "$Recycle.Bin",
    "System Volume Information",
    "**\\Temp\\**",
    "**\\Tmp\\**",
    "*.tmp",
    "*.~",
    "*.vhdx",
    "*.avhdx",
    "*.obkm",
]


# ---------------------------------------------------------------------------
# Матчер — построчный перенос ExclusionPattern/match_path из exclusions.rs
# ---------------------------------------------------------------------------
def glob_match(pat: str, name: str) -> bool:
    """`*` — любой прогон символов, `?` — ровно один. Без классов."""
    pi = si = 0
    star_pi = -1
    star_si = 0
    while si < len(name):
        if pi < len(pat) and (pat[pi] == "?" or pat[pi] == name[si]):
            pi += 1
            si += 1
        elif pi < len(pat) and pat[pi] == "*":
            star_pi = pi
            star_si = si
            pi += 1
        elif star_pi >= 0:
            pi = star_pi + 1
            star_si += 1
            si = star_si
        else:
            return False
    while pi < len(pat) and pat[pi] == "*":
        pi += 1
    return pi == len(pat)


def match_path(pat: list[str], path: list[str]) -> bool:
    pi = si = 0
    star_pi = -1
    star_si = 0
    while si < len(path):
        if pi < len(pat) and pat[pi] != "**" and glob_match(pat[pi], path[si]):
            pi += 1
            si += 1
        elif pi < len(pat) and pat[pi] == "**":
            star_pi = pi
            star_si = si
            pi += 1
        elif star_pi >= 0:
            pi = star_pi + 1
            star_si += 1
            si = star_si
        else:
            return False
    while pi < len(pat) and pat[pi] == "**":
        pi += 1
    return pi == len(pat)


class Pattern:
    __slots__ = ("components", "anchored", "contents_only")

    def __init__(self, raw: str):
        trimmed = raw.strip().strip("\\/")
        comps = [c.lower() for c in trimmed.replace("/", "\\").split("\\") if c]
        if not comps or all(c == "**" for c in comps):
            raise ValueError(f"правило отвергнуто: {raw!r}")
        self.components = comps
        self.anchored = len(comps) > 1
        self.contents_only = len(comps) > 1 and comps[-1] == "**"

    def matches(self, name_lower: str, rel: list[str]) -> bool:
        if not self.anchored:
            return glob_match(self.components[0], name_lower)
        if self.contents_only:
            if not rel:
                return False
            return match_path(self.components, rel[:-1])
        return match_path(self.components, rel)


# ---------------------------------------------------------------------------
# VHDX (dynamic) — ровно та же арифметика, что в backup/src/vhdx_bat.rs
# ---------------------------------------------------------------------------
REGION1_OFF = 192 * 1024
BAT_GUID = "2dc27766-f623-4200-9d64-115e9bfd4a08"
META_GUID = "8b7ca206-4790-4b9a-b8fe-575f050f886e"
MI_FILE_PARAMS = "caa16737-fa36-4d43-b3b6-33f0aa44e76b"
MI_VDISK_SIZE = "2fa54224-cd1b-4876-b211-5dbed83bf4b8"
MI_LOG_SECTOR = "8141bf1d-a96f-4709-ba47-f233a8faab5f"


def guid_le(s: str) -> bytes:
    return uuid.UUID(s).bytes_le


class RawReader:
    """Сырой образ тома: виртуальное смещение = смещение в файле."""

    def __init__(self, path: str):
        self.f = open(path, "rb")
        self.virtual_size = os.path.getsize(path)
        self.kind = "raw"

    def read(self, off: int, length: int) -> bytes:
        self.f.seek(off)
        buf = self.f.read(length)
        return buf + b"\0" * (length - len(buf))


class VhdxReader:
    """Динамический VHDX только на чтение. Отсутствующий блок = нули."""

    def __init__(self, path: str):
        self.f = open(path, "rb")
        self.kind = "vhdx"
        if self.f.read(8) != b"vhdxfile":
            raise ValueError("не VHDX (нет сигнатуры)")

        self.f.seek(REGION1_OFF)
        region = self.f.read(64 * 1024)
        if region[0:4] != b"regi":
            raise ValueError("нет таблицы регионов")
        count = struct.unpack_from("<I", region, 8)[0]
        bat_off = bat_len = meta_off = meta_len = 0
        for slot in range(min(count, 2047)):
            base = 16 + slot * 32
            g = region[base : base + 16]
            off, ln = struct.unpack_from("<QI", region, base + 16)
            if g == guid_le(BAT_GUID):
                bat_off, bat_len = off, ln
            elif g == guid_le(META_GUID):
                meta_off, meta_len = off, ln
        if not bat_off or not meta_off:
            raise ValueError("в таблице регионов нет BAT или метаданных")

        self.f.seek(meta_off)
        meta = self.f.read(min(meta_len, 1 << 20))
        if meta[0:8] != b"metadata":
            raise ValueError("нет сигнатуры метаданных")
        mcount = struct.unpack_from("<H", meta, 10)[0]
        vsize = bsize = 0
        ssize = 512
        for slot in range(min(mcount, 2046)):
            e = 32 + slot * 32
            g = meta[e : e + 16]
            ioff = struct.unpack_from("<I", meta, e + 16)[0]
            if ioff + 8 > len(meta):
                continue
            if g == guid_le(MI_VDISK_SIZE):
                vsize = struct.unpack_from("<Q", meta, ioff)[0]
            elif g == guid_le(MI_FILE_PARAMS):
                bsize = struct.unpack_from("<I", meta, ioff)[0]
            elif g == guid_le(MI_LOG_SECTOR):
                ssize = struct.unpack_from("<I", meta, ioff)[0]
        if not (vsize and bsize and ssize):
            raise ValueError("в метаданных нет размеров")

        self.virtual_size = vsize
        self.block_size = bsize
        self.sector_size = ssize
        chunk_ratio = (1 << 23) * ssize // bsize
        data_blocks = (vsize + bsize - 1) // bsize
        self.f.seek(bat_off)
        raw = self.f.read(bat_len)
        # block index -> смещение в файле; отсутствующие блоки не храним
        self.bat: dict[int, int] = {}
        for i in range(data_blocks):
            idx = i + i // chunk_ratio
            if (idx + 1) * 8 > len(raw):
                break
            entry = struct.unpack_from("<Q", raw, idx * 8)[0]
            if entry & 7 != 0:  # 0 = PAYLOAD_BLOCK_NOT_PRESENT
                self.bat[i] = (entry >> 20) * (1 << 20)
        self.allocated = len(self.bat)

    def read(self, off: int, length: int) -> bytes:
        out = bytearray()
        while length > 0:
            blk, within = divmod(off, self.block_size)
            take = min(length, self.block_size - within)
            base = self.bat.get(blk)
            if base is None:
                out += b"\0" * take
            else:
                self.f.seek(base + within)
                buf = self.f.read(take)
                out += buf + b"\0" * (take - len(buf))
            off += take
            length -= take
        return bytes(out)


def open_image(path: str):
    with open(path, "rb") as f:
        head = f.read(8)
    return VhdxReader(path) if head == b"vhdxfile" else RawReader(path)


def find_volume_offset(reader) -> int:
    """Найти начало тома: наш образ одного тома обёрнут в GPT (том с 1 МиБ,
    следом раздел OwnBackupMeta), а образ целого диска — настоящая таблица.
    Если по нулевому смещению уже NTFS — так и вернуть 0."""
    if reader.read(0, 512)[3:11] == b"NTFS    ":
        return 0
    hdr = reader.read(512, 512)
    if hdr[0:8] != b"EFI PART":
        return 0
    entry_lba, count, size = struct.unpack_from("<QII", hdr, 72)
    table = reader.read(entry_lba * 512, count * size)
    for i in range(count):
        base = i * size
        if base + 128 > len(table):
            break
        type_guid = table[base : base + 16]
        if type_guid == b"\0" * 16:
            continue
        first_lba = struct.unpack_from("<Q", table, base + 32)[0]
        name = table[base + 56 : base + 128].decode("utf-16-le", "replace").rstrip("\0")
        if "OwnBackupMeta" in name:
            continue
        off = first_lba * 512
        if reader.read(off, 512)[3:11] == b"NTFS    ":
            print(f"  том найден: раздел «{name}» на {off / 2**20:.0f} МиБ")
            return off
    return 0


# ---------------------------------------------------------------------------
# NTFS
# ---------------------------------------------------------------------------
ATTR_ATTRIBUTE_LIST = 0x20
ATTR_FILE_NAME = 0x30
ATTR_DATA = 0x80
ROOT_REF = 5


class Window:
    """Том внутри образа: смещения считаются от начала тома."""

    def __init__(self, reader, offset: int):
        self.reader = reader
        self.offset = offset

    def read(self, off: int, length: int) -> bytes:
        return self.reader.read(self.offset + off, length)


def apply_fixups(rec: bytearray, sector_size: int) -> bool:
    """Update sequence array: последние 2 байта каждого сектора — подделка."""
    usa_off, usa_count = struct.unpack_from("<HH", rec, 4)
    if usa_count == 0 or usa_off + usa_count * 2 > len(rec):
        return False
    usn = rec[usa_off : usa_off + 2]
    for i in range(1, usa_count):
        end = i * sector_size - 2
        if end + 2 > len(rec):
            return False
        if rec[end : end + 2] != usn:
            return False  # запись битая или не дочитана
        src = usa_off + i * 2
        rec[end : end + 2] = rec[src : src + 2]
    return True


def decode_runlist(data: bytes, start_vcn: int = 0):
    """[(vcn, lcn|None, length)] — lcn None для sparse."""
    runs = []
    pos = 0
    lcn = 0
    vcn = start_vcn
    while pos < len(data):
        header = data[pos]
        if header == 0:
            break
        len_sz = header & 0x0F
        off_sz = (header >> 4) & 0x0F
        pos += 1
        if len_sz == 0 or pos + len_sz + off_sz > len(data):
            break
        run_len = int.from_bytes(data[pos : pos + len_sz], "little", signed=False)
        pos += len_sz
        if off_sz == 0:
            runs.append((vcn, None, run_len))  # sparse
        else:
            delta = int.from_bytes(data[pos : pos + off_sz], "little", signed=True)
            pos += off_sz
            lcn += delta
            runs.append((vcn, lcn, run_len))
        vcn += run_len
    return runs


class Mft:
    def __init__(self, win: Window):
        boot = win.read(0, 512)
        if boot[3:11] != b"NTFS    ":
            raise ValueError(f"не NTFS: OEM={boot[3:11]!r}")
        self.bytes_per_sector = struct.unpack_from("<H", boot, 0x0B)[0]
        spc = boot[0x0D]
        self.cluster_size = self.bytes_per_sector * spc
        self.total_sectors = struct.unpack_from("<Q", boot, 0x28)[0]
        mft_lcn = struct.unpack_from("<Q", boot, 0x30)[0]
        raw_cpr = struct.unpack_from("<b", boot, 0x40)[0]
        self.record_size = (
            raw_cpr * self.cluster_size if raw_cpr > 0 else 1 << (-raw_cpr)
        )
        self.win = win
        self.mft_offset = mft_lcn * self.cluster_size

        rec = bytearray(win.read(self.mft_offset, self.record_size))
        if rec[0:4] != b"FILE":
            raise ValueError("запись 0 MFT не начинается с FILE")
        apply_fixups(rec, self.bytes_per_sector)
        self.runs = []
        for attr in self.attributes(rec):
            if attr["type"] == ATTR_DATA and not attr["name"] and attr["non_resident"]:
                self.runs = attr["runs"]
                self.data_size = attr["real_size"]
                break
        if not self.runs:
            raise ValueError("у $MFT нет нерезидентного $DATA")
        self.record_count = self.data_size // self.record_size

    def attributes(self, rec: bytes):
        off = struct.unpack_from("<H", rec, 0x14)[0]
        while off + 8 <= len(rec):
            atype = struct.unpack_from("<I", rec, off)[0]
            if atype == 0xFFFFFFFF:
                break
            alen = struct.unpack_from("<I", rec, off + 4)[0]
            if alen == 0 or off + alen > len(rec):
                break
            non_res = rec[off + 8]
            name_len = rec[off + 9]
            name_off = struct.unpack_from("<H", rec, off + 10)[0]
            name = ""
            if name_len:
                name = rec[off + name_off : off + name_off + name_len * 2].decode(
                    "utf-16-le", "replace"
                )
            out = {"type": atype, "name": name, "non_resident": bool(non_res)}
            if non_res:
                run_off = struct.unpack_from("<H", rec, off + 0x20)[0]
                out["real_size"] = struct.unpack_from("<Q", rec, off + 0x30)[0]
                out["alloc_size"] = struct.unpack_from("<Q", rec, off + 0x28)[0]
                out["start_vcn"] = struct.unpack_from("<Q", rec, off + 0x10)[0]
                out["flags"] = struct.unpack_from("<H", rec, off + 0x0C)[0]
                out["runs"] = decode_runlist(
                    rec[off + run_off : off + alen], out["start_vcn"]
                )
            else:
                clen = struct.unpack_from("<I", rec, off + 0x10)[0]
                coff = struct.unpack_from("<H", rec, off + 0x14)[0]
                out["real_size"] = clen
                out["content"] = rec[off + coff : off + coff + clen]
            yield out
            off += alen

    def read_vcn(self, vcn: int, count: int) -> bytes:
        """Прочитать `count` кластеров MFT начиная с виртуального кластера."""
        out = bytearray()
        need = count
        for rvcn, lcn, rlen in self.runs:
            if need <= 0:
                break
            if vcn >= rvcn + rlen:
                continue
            if lcn is None:
                take = min(need, rvcn + rlen - vcn)
                out += b"\0" * (take * self.cluster_size)
                vcn += take
                need -= take
                continue
            skip = vcn - rvcn
            take = min(need, rlen - skip)
            out += self.win.read(
                (lcn + skip) * self.cluster_size, take * self.cluster_size
            )
            vcn += take
            need -= take
        return bytes(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("image")
    ap.add_argument("--offset", type=lambda s: int(s, 0), default=0)
    ap.add_argument("--limit", type=int, default=0, help="сколько записей MFT прочитать")
    ap.add_argument("--patterns", help="файл с правилами, по одному на строку")
    ap.add_argument("--list", help="выписать найденные пути сюда")
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()

    raws = DEFAULT_PATTERNS
    if args.patterns:
        with open(args.patterns, encoding="utf-8") as f:
            raws = [ln.strip() for ln in f if ln.strip()]
    pats = [Pattern(p) for p in raws]

    t0 = time.time()
    img = open_image(args.image)
    offset = args.offset if args.offset else find_volume_offset(img)
    win = Window(img, offset)
    mft = Mft(win)
    t_open = time.time() - t0

    print(f"образ:      {args.image}")
    print(f"  формат:   {img.kind}", end="")
    if img.kind == "vhdx":
        print(
            f", блок {img.block_size >> 20} МиБ, занято {img.allocated} блоков"
            f" ({img.allocated * img.block_size / 2**30:.1f} ГиБ)"
        )
    else:
        print()
    print(
        f"  NTFS:     кластер {mft.cluster_size} Б, том"
        f" {mft.total_sectors * mft.bytes_per_sector / 2**30:.1f} ГиБ"
    )
    print(
        f"  $MFT:     {mft.record_count} записей по {mft.record_size} Б"
        f" = {mft.data_size / 2**20:.0f} МиБ, {len(mft.runs)} экстент(ов)"
    )
    print(f"  открытие: {t_open:.2f} с")

    # --- проход 1: собрать имена и родителей -------------------------------
    total = args.limit if args.limit else mft.record_count
    per_cluster = max(1, mft.cluster_size // mft.record_size)
    batch_clusters = 1024  # ~4 МиБ за чтение
    names: dict[int, tuple[str, int, bool]] = {}
    link_count: dict[int, int] = {}
    stats = dict(
        read=0, filerec=0, inuse=0, dirs=0, files=0, bad_fixup=0, attrlist=0
    )
    matched: list[tuple[str, int, int, int, str]] = []  # путь, байты, экстенты, №, флаг
    pending_data: dict[int, dict] = {}

    t1 = time.time()
    vcn = 0
    recno = 0
    read_bytes = 0
    while recno < total:
        chunk = mft.read_vcn(vcn, batch_clusters)
        if not chunk:
            break
        read_bytes += len(chunk)
        vcn += batch_clusters
        for pos in range(0, len(chunk), mft.record_size):
            if recno >= total:
                break
            rec = bytearray(chunk[pos : pos + mft.record_size])
            n = recno
            recno += 1
            stats["read"] += 1
            if rec[0:4] != b"FILE":
                continue
            stats["filerec"] += 1
            if not apply_fixups(rec, mft.bytes_per_sector):
                stats["bad_fixup"] += 1
                continue
            flags = struct.unpack_from("<H", rec, 0x16)[0]
            if not flags & 1:
                continue  # удалённая запись
            stats["inuse"] += 1
            is_dir = bool(flags & 2)
            stats["dirs" if is_dir else "files"] += 1

            best_name = None
            fn_alloc = 0
            all_names = set()
            best_ns = 9
            parent = None
            data_runs = None
            data_size = 0
            resident_data = 0
            has_attrlist = False
            sparse = False
            for attr in mft.attributes(rec):
                t = attr["type"]
                if t == ATTR_ATTRIBUTE_LIST:
                    has_attrlist = True
                elif t == ATTR_FILE_NAME and not attr["non_resident"]:
                    c = attr["content"]
                    if len(c) < 66:
                        continue
                    pref = struct.unpack_from("<Q", c, 0)[0] & ((1 << 48) - 1)
                    nlen = c[64]
                    ns = c[65]
                    nm = c[66 : 66 + nlen * 2].decode("utf-16-le", "replace")
                    # Win32 (1) и Win32+DOS (3) предпочтительнее POSIX (0) и DOS (2)
                    if ns != 2:  # чистое DOS-имя ссылкой не считается
                        all_names.add((pref, nm.lower()))
                        fn_alloc = max(fn_alloc, struct.unpack_from("<Q", c, 0x28)[0])
                    rank = {1: 0, 3: 0, 0: 1, 2: 2}.get(ns, 3)
                    if rank < best_ns:
                        best_ns, best_name, parent = rank, nm, pref
                elif t == ATTR_DATA and not attr["name"]:
                    if attr["non_resident"]:
                        # ВАЖНО: длинный run list NTFS кладёт в НЕСКОЛЬКО
                        # экземпляров $DATA (у каждого свой StartVCN), и
                        # real_size заполнен только у первого (StartVCN=0).
                        # Брать один экземпляр — значит потерять большую часть
                        # экстентов крупного фрагментированного файла и не
                        # заметить этого: на реальном образе так пропало
                        # 1251 МиБ из 3663 (2026-08-19).
                        data_runs = (data_runs or []) + attr["runs"]
                        if attr.get("start_vcn", 0) == 0:
                            data_size = attr["real_size"]
                        if attr.get("flags", 0) & 0x8000:
                            sparse = True
                    else:
                        resident_data = attr["real_size"]
            if best_name is None:
                continue
            names[n] = (best_name, parent, is_dir)
            link_count[n] = len(all_names)
            if has_attrlist:
                stats["attrlist"] += 1
            pending_data[n] = {
                "fn_alloc": fn_alloc,
                "runs": data_runs,
                "size": data_size,
                "resident": resident_data,
                "attrlist": has_attrlist,
                "sparse": sparse,
            }
        if not args.quiet and recno % 200000 < mft.record_size and recno:
            el = time.time() - t1
            print(
                f"    … {recno} записей, {read_bytes / 2**20:.0f} МиБ,"
                f" {el:.0f} с ({read_bytes / 2**20 / max(el, 0.01):.0f} МиБ/с)"
            )
    t_scan = time.time() - t1

    # --- проход 2: пути и матчинг (в памяти, без единого чтения диска) -----
    t2 = time.time()
    path_cache: dict[int, list[str]] = {ROOT_REF: []}

    def path_of(n: int, depth: int = 0):
        if n in path_cache:
            return path_cache[n]
        if depth > 128 or n not in names:
            return None
        name, parent, _ = names[n]
        if parent is None or parent == n:
            return None
        up = path_of(parent, depth + 1)
        if up is None:
            return None
        out = up + [name]
        path_cache[n] = out
        return out

    excluded_dirs: set[int] = set()
    for n in names:
        p = path_of(n)
        if not p:  # корень тома пути не имеет — сравнивать нечего
            continue
        rel = [c.lower() for c in p]
        if any(pat.matches(rel[-1], rel) for pat in pats):
            if names[n][2]:
                excluded_dirs.add(n)

    def under_excluded(n: int, depth: int = 0) -> bool:
        if depth > 128 or n not in names:
            return False
        parent = names[n][1]
        if parent is None or parent == n or parent == ROOT_REF:
            return False
        if parent in excluded_dirs:
            return True
        return under_excluded(parent, depth + 1)

    bytes_excluded = 0
    alloc_bytes = 0
    extents = 0
    # Записи, для которых путь не собрался (родитель не разобран/не дочитан).
    # Их проба молча пропускает — а это ровно «исключили часть», поэтому они
    # считаются и предъявляются отдельно.
    unresolved = 0
    unresolved_bytes = 0
    hidden_bytes = 0
    n_resident = n_attrlist = n_sparse = n_nodata = 0
    for n, (name, parent, is_dir) in names.items():
        p = path_of(n)
        if p is None:
            unresolved += 1
            d0 = pending_data.get(n, {})
            unresolved_bytes += d0.get("size", 0) or d0.get("resident", 0)
            continue
        if not p:
            continue
        rel = [c.lower() for c in p]
        hit = any(pat.matches(rel[-1], rel) for pat in pats) or under_excluded(n)
        if not hit or is_dir:
            continue
        d = pending_data.get(n, {})
        runs = d.get("runs")
        note = ""
        if d.get("attrlist"):
            n_attrlist += 1
            note += "L"
        if d.get("sparse"):
            n_sparse += 1
            note += "S"
        if runs:
            real = sum(rl for _, lcn, rl in runs if lcn is not None)
            extents += len([1 for _, lcn, _ in runs if lcn is not None])
            # Две величины намеренно врозь: движок считает ВЫДЕЛЕННЫЕ кластеры
            # (столько он и не скопирует), а размер файла — это то, что видит
            # пользователь. Расхождение между ними и есть цена округления до
            # кластера плюс сжатие/разреженность.
            alloc_bytes += real * mft.cluster_size
            bytes_excluded += d.get("size", 0) or real * mft.cluster_size
        elif d.get("resident"):
            n_resident += 1
            bytes_excluded += d["resident"]
            note += "R"
        else:
            n_nodata += 1
            # Экстентов в базовой записи нет. Если файл при этом не пуст —
            # его run list за ATTRIBUTE_LIST, и вычитание молча потеряло бы
            # ВСЕ его кластеры.
            hidden_bytes += d.get("fn_alloc", 0)
        alloc_here = (
            sum(rl for _, lcn, rl in runs if lcn is not None) * mft.cluster_size
            if runs
            else 0
        )
        matched.append(("\\".join(p), alloc_here, len(runs or []), n, note))
    t_match = time.time() - t2

    print()
    print(f"прочитано:  {stats['read']} записей ({read_bytes / 2**20:.0f} МиБ)")
    print(f"  живых:    {stats['inuse']} (файлов {stats['files']}, каталогов {stats['dirs']})")
    print(f"  с ATTRIBUTE_LIST: {stats['attrlist']}, битых фиксапов: {stats['bad_fixup']}")
    print(f"чтение MFT: {t_scan:.1f} с ({read_bytes / 2**20 / max(t_scan, 0.01):.0f} МиБ/с)")
    print(f"матчинг:    {t_match:.1f} с (в памяти)")
    print()
    print(f"СОВПАЛО:    {len(matched)} файлов, {extents} экстентов")
    print(f"  по размеру файлов:      {bytes_excluded / 2**20:.0f} МиБ")
    print(f"  по выделенным кластерам: {alloc_bytes / 2**20:.0f} МиБ  (это и есть то, что не будет скопировано)")
    print(f"  резидентных (вычитанием не убрать): {n_resident}")
    print(f"  с ATTRIBUTE_LIST (run list за пределами записи): {n_attrlist}")
    print(f"  sparse: {n_sparse}, без $DATA вовсе: {n_nodata}")
    print(
        f"  СПРЯТАНО за ATTRIBUTE_LIST: {hidden_bytes / 2**20:.0f} МиБ"
        f" — эти кластеры вычитание бы не увидело"
    )
    print(f"  итого к вычитанию честно: {(alloc_bytes + hidden_bytes) / 2**20:.0f} МиБ")
    hard = sum(1 for _, _, _, n, _ in matched if link_count.get(n, 1) > 1)
    hard_bytes = sum(
        (link_count.get(n, 1) - 1) * sz for _, sz, _, n, _ in matched if link_count.get(n, 1) > 1
    )
    print(
        f"  из них с НЕСКОЛЬКИМИ именами (жёсткие ссылки): {hard}"
        f" — обход насчитал бы на {hard_bytes / 2**20:.0f} МиБ больше"
    )
    print(
        f"НЕ РАЗОБРАНО путей: {unresolved} записей"
        f" ({unresolved_bytes / 2**20:.0f} МиБ) — они не проверялись правилами"
    )
    print(f"ИТОГО:      {t_open + t_scan + t_match:.1f} с")

    if args.list:
        with open(args.list, "w", encoding="utf-8") as f:
            for path, size, nruns, n, note in sorted(matched):
                f.write(f"{size}\t{nruns}\t{note}\t{path}\n")
        print(f"список:     {args.list}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
