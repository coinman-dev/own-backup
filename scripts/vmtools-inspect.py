#!/usr/bin/env python3
"""Take VMware Tools' guest drivers apart, on any machine.

Why this exists: the boot medium needs VMware's display driver, and it is not on
`windows.iso` as files — only `pvscsi` is. Everything else lives inside
`setup.exe`, and the way it lives there is the whole problem:

  * `setup.exe` is an InstallShield self-extractor. Appended to it is an OLE
    compound file — the MSI database — and the driver cabinets are STREAMS
    inside that compound file.
  * A stream is stored in 4096-byte sectors that need not be next to each other.
    So a cabinet's header reads fine (it fits in the first sector) while its
    data is interleaved with other streams. Carving the byte range out of
    `setup.exe` yields a cabinet that `expand` opens and then rejects with
    0x8007000D — checked, and that is exactly what happened.

This script reassembles the streams properly (through the FAT), keeps the ones
that are cabinets, and verifies every CFDATA block against its own checksum, so
"the cabinet is intact" is a fact rather than a hope. It is the reference for
`crates/backup/src/media_guest_drivers_win.rs` and the way to check a new
VMware Tools release without a Windows machine.

Usage:
    scripts/vmtools-inspect.py <windows.iso | setup.exe> [-o OUTDIR]

With -o the cabinets are written out (they can then be unpacked with
`expand`/`cabextract`); without it the script only reports.
"""

import argparse
import struct
import sys
from pathlib import Path

OLE_MAGIC = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"
ENDOFCHAIN = 0xFFFFFFFE
FREESECT = 0xFFFFFFFF


# --- ISO9660 (Joliet) — just enough to pull setup.exe off the image ----------

def read_setup_from_iso(path: Path) -> bytes:
    try:
        import pycdlib
    except ImportError:
        sys.exit("need pycdlib to read an ISO (pip install pycdlib), or pass setup.exe directly")
    import io
    iso = pycdlib.PyCdlib()
    iso.open(str(path))
    try:
        buf = io.BytesIO()
        iso.get_file_from_iso_fp(buf, joliet_path="/setup.exe")
        return buf.getvalue()
    finally:
        iso.close()


# --- OLE compound file -------------------------------------------------------

class Ole:
    """The parts of a compound file needed to read one stream out of it."""

    def __init__(self, data: bytes, base: int):
        self.data = data
        self.base = base
        h = data[base:base + 512]
        (self.sector_shift, self.mini_shift) = struct.unpack_from("<HH", h, 30)
        (self.n_fat, self.dir_start, _sig, self.mini_cutoff,
         self.mini_fat_start, self.n_mini_fat,
         self.difat_start, self.n_difat) = struct.unpack_from("<IIIIIIII", h, 44)
        self.sector = 1 << self.sector_shift

        # DIFAT: 109 entries in the header, the rest in a chain of sectors.
        difat = list(struct.unpack_from("<109I", h, 76))
        nxt = self.difat_start
        guard = 0
        while nxt < 0xFFFFFFFA and guard < 100000:
            s = self._raw(nxt)
            difat.extend(struct.unpack_from(f"<{self.sector // 4 - 1}I", s))
            nxt = struct.unpack_from("<I", s, self.sector - 4)[0]
            guard += 1

        self.fat = []
        for fs in difat[:self.n_fat]:
            if fs >= 0xFFFFFFFA:
                continue
            self.fat.extend(struct.unpack_from(f"<{self.sector // 4}I", self._raw(fs)))

    def _raw(self, n: int) -> bytes:
        # Sector 0 starts one WHOLE sector in, not 512 bytes in: with 4096-byte
        # sectors the 512-byte header is padded out to a full sector. Getting
        # this wrong reads the file at a 3584-byte offset and every chain in it
        # turns to noise.
        p = self.base + (n + 1) * self.sector
        return self.data[p:p + self.sector]

    def chain(self, start: int, max_sectors: int) -> list:
        out, c = [], start
        while c < 0xFFFFFFFA and len(out) <= max_sectors:
            out.append(c)
            if c >= len(self.fat):
                break
            c = self.fat[c]
        return out

    def stream(self, start: int, size: int) -> bytes:
        need = (size + self.sector - 1) // self.sector
        return b"".join(self._raw(c) for c in self.chain(start, need))[:size]

    def entries(self):
        """(name, start_sector, size) for every stream in the directory."""
        out = []
        for ds in self.chain(self.dir_start, 1 << 20):
            sec = self._raw(ds)
            for k in range(self.sector // 128):
                e = sec[k * 128:(k + 1) * 128]
                if len(e) < 128:
                    break
                nlen = struct.unpack_from("<H", e, 64)[0]
                kind = e[66]
                if nlen < 2 or kind != 2:  # 2 = stream
                    continue
                name = e[:nlen - 2].decode("utf-16-le", "replace")
                start, size = struct.unpack_from("<IQ", e, 116)
                out.append((decode_msi_name(name), start, size))
        return out


def decode_msi_name(name: str) -> str:
    """MSI packs stream names into the 0x3800..0x4840 range, two chars each."""
    alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._"
    out = []
    for ch in name:
        c = ord(ch)
        if 0x3800 <= c < 0x4800:
            v = c - 0x3800
            out.append(alphabet[v & 0x3F])
            out.append(alphabet[(v >> 6) & 0x3F])
        elif 0x4800 <= c < 0x4840:
            out.append(alphabet[c - 0x4800])
        else:
            out.append(ch)
    return "".join(out)


# --- CAB --------------------------------------------------------------------

def cab_checksum(buf: bytes, seed: int = 0) -> int:
    ul, n, i = seed, len(buf), 0
    while n >= 4:
        ul ^= struct.unpack_from("<I", buf, i)[0]
        i += 4
        n -= 4
    tmp = 0
    if n == 3:
        tmp = (buf[i] << 16) | (buf[i + 1] << 8) | buf[i + 2]
    elif n == 2:
        tmp = (buf[i] << 8) | buf[i + 1]
    elif n == 1:
        tmp = buf[i]
    return (ul ^ tmp) & 0xFFFFFFFF


def cab_report(blob: bytes):
    """(files, ok, note) — walk the cabinet and verify every data block."""
    if blob[:4] != b"MSCF":
        return None
    size, coff_files = struct.unpack_from("<I", blob, 8)[0], struct.unpack_from("<I", blob, 16)[0]
    n_folders, n_files, flags = struct.unpack_from("<HHH", blob, 26)
    reserve_folder = reserve_data = 0
    p = 36
    if flags & 0x0004:
        cb_hdr, reserve_folder, reserve_data = struct.unpack_from("<HBB", blob, p)
        p += 4 + cb_hdr

    names, q = [], coff_files
    for _ in range(n_files):
        if q + 17 > len(blob):
            break
        end = blob.find(b"\0", q + 16)
        if end < 0:
            break
        try:
            names.append(blob[q + 16:end].decode("ascii"))
        except UnicodeDecodeError:
            break
        q = end + 1

    bad = 0
    checked = 0
    for f in range(n_folders):
        coff_data, n_blocks, comp = struct.unpack_from("<IHH", blob, p + f * (8 + reserve_folder))
        d = coff_data
        for _ in range(n_blocks):
            if d + 8 > len(blob):
                bad += 1
                break
            csum, cb_data, cb_uncomp = struct.unpack_from("<IHH", blob, d)
            if cb_data == 0 or cb_data > 33000 or d + 8 + reserve_data + cb_data > len(blob):
                bad += 1
                break
            body = blob[d + 8 + reserve_data: d + 8 + reserve_data + cb_data]
            seed = cab_checksum(blob[d + 4: d + 8])
            if cab_checksum(body, seed) != csum:
                bad += 1
            checked += 1
            d += 8 + reserve_data + cb_data
    return names, bad, checked


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path", type=Path, help="windows.iso or setup.exe")
    ap.add_argument("-o", "--out", type=Path, help="write the cabinets here")
    args = ap.parse_args()

    data = read_setup_from_iso(args.path) if args.path.suffix.lower() == ".iso" \
        else args.path.read_bytes()
    print(f"setup.exe: {len(data) / 1048576:.1f} MB")

    base = data.find(OLE_MAGIC)
    if base < 0:
        sys.exit("no compound file inside — this is not the installer we know")
    print(f"compound file (the MSI) at {base}")

    ole = Ole(data, base)
    print(f"  sector {ole.sector} B, FAT {ole.n_fat} sectors "
          f"({len(ole.fat)} entries ≈ {len(ole.fat) * ole.sector / 1048576:.0f} MB)")

    streams = ole.entries()
    print(f"  streams: {len(streams)}")
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)

    cabs = 0
    for name, start, size in sorted(streams, key=lambda x: -x[2]):
        if size < 4096:
            continue
        blob = ole.stream(start, size)
        if blob[:4] != b"MSCF":
            continue
        cabs += 1
        names, bad, checked = cab_report(blob)
        infs = [n for n in names if ".inf" in n.lower()]
        state = "INTACT" if bad == 0 else f"{bad} BAD BLOCKS"
        print(f"\n  {name}: {size / 1048576:.1f} MB, {len(names)} files, "
              f"{checked} data blocks — {state}")
        for i in sorted(infs)[:8]:
            print(f"      {i}")
        if args.out:
            out = args.out / f"{name}.cab"
            out.write_bytes(blob)
            print(f"      → {out}")
    print(f"\n{cabs} cabinets found inside the installer")


if __name__ == "__main__":
    main()
