// How a disk is named on screen, the same on every screen that draws one.
//
// Windows numbers its disks (`\\.\PhysicalDrive3` → «Диск 3»), and that number
// is what Disk Management shows. Linux does not: `/dev/vda` has no number, and
// the digits of `nvme0n1` or of a `/dev/mapper/luks-16e1…` name are not one —
// read as a number they gave «Диск», «Диск 01» and a string of hex digits.
// There the kernel's name is the name people know.
//
// The owner wanted numbers on Linux too, like Windows (2026-11-27): the window
// numbers the real disks itself, 1, 2, 3… in the order of their kernel names
// (`nvme0n1`, `sda`, `sdb`), and shows both: «Диск 1 (nvme0n1)». No system
// tool knows that number. A whole-disk job keeps the one its disk had when it
// was saved, for its file names only — the disk itself is found by its by-id
// device, so a number that changed after disks were swapped breaks nothing.
//
// A device-mapper volume (an opened LUKS container, an LVM logical volume) is
// listed as a «disk» of its own by the inventory, with one volume covering it.
// It is not a disk and has no partition table: it is named for what it is.

import type { DiskInfo, DiskSourceSpec } from './types';
import { m } from './paraglide/messages';

/** `crypt` / `lvm` for a device-mapper volume listed as a disk, else ''. */
export function mapperKind(d: DiskInfo): string {
  return d.partitions.length === 1 ? (d.partitions[0].mapper_kind ?? '') : '';
}

/** The window's numbers of the Linux disks, by `device_id`; set by
 *  [`orderDisks`] each time the disks are listed. */
let linuxNumbers = new Map<string, number>();

/** The disks in the order the window shows them, numbering the Linux ones:
 *  Windows disks by number; Linux real disks by kernel name, the
 *  device-mapper volumes after them in the inventory's order. */
export function orderDisks(disks: DiskInfo[]): DiskInfo[] {
  const linux = (d: DiskInfo) => d.device_id.startsWith('/');
  const sorted = [...disks].sort((a, b) => {
    if (!linux(a) || !linux(b)) return diskNumber(a) - diskNumber(b);
    const ma = mapperKind(a) ? 1 : 0;
    const mb = mapperKind(b) ? 1 : 0;
    if (ma || mb) return ma - mb;
    return diskShortName(a).localeCompare(diskShortName(b), 'en', { numeric: true });
  });
  const numbers = new Map<string, number>();
  for (const d of sorted) {
    if (linux(d) && !mapperKind(d)) numbers.set(d.device_id, numbers.size + 1);
  }
  linuxNumbers = numbers;
  return sorted;
}

/** The window's number of a Linux disk (1, 2, 3…); undefined for a Windows
 *  disk, a device-mapper volume, or a disk not in the current list. */
export function linuxDiskNumber(d: DiskInfo): number | undefined {
  return linuxNumbers.get(d.device_id);
}

/** What goes after «Диск»: the Windows disk number, or the kernel's name of a
 *  Linux disk (`vda`, `nvme0n1`, `sda`). */
export function diskShortName(d: DiskInfo): string {
  const id = d.device_id.trim();
  const win = id.match(/PhysicalDrive(\d+)\s*$/i);
  if (win) return win[1];
  if (id.startsWith('/')) return id.slice(id.lastIndexOf('/') + 1);
  return '';
}

/** The caption under a disk's icon: «Диск 3», «Диск 1 (vda)», «Открытый LUKS»
 *  — «Диск vda» for a Linux disk the current list does not hold. */
export function diskCaption(d: DiskInfo): string {
  const kind = mapperKind(d);
  if (kind === 'crypt') return m.disk_mapper_crypt();
  if (kind === 'lvm') return m.disk_mapper_lvm();
  const n = linuxDiskNumber(d);
  if (n !== undefined) return m.dmap_disk_linux({ n: String(n), name: diskShortName(d) });
  return m.dmap_disk({ n: diskShortName(d) });
}

/** The second half of a disk's size line: its partition table, or what a
 *  device-mapper volume is instead of having one. */
export function diskTableText(d: DiskInfo): string {
  const kind = mapperKind(d);
  if (kind === 'crypt') return m.disk_mapper_crypt_meta();
  if (kind === 'lvm') return m.disk_mapper_lvm_meta();
  const t = d.partition_table;
  return t === 'gpt' ? 'GPT' : t === 'mbr' ? 'MBR' : t.toUpperCase();
}

/** Windows: the number of `\\.\PhysicalDriveN`. Elsewhere 0 — a Linux disk
 *  has no number, and the digits in its name (`nvme0n1`) are not one. */
export function diskNumber(d: DiskInfo): number {
  const win = d.device_id.trim().match(/PhysicalDrive(\d+)\s*$/i);
  return win ? parseInt(win[1], 10) : 0;
}

/** Is `d` the disk this whole-disk spec names? By its by-id device where
 *  the spec has one (Linux), else by number — and by model too when asked
 *  (`PhysicalDriveN` is a slot another disk can take). */
export function isSpecDisk(spec: DiskSourceSpec, d: DiskInfo, checkModel = false): boolean {
  if (spec.device) return !!d.stable_device && d.stable_device === spec.device;
  if (d.device_id.startsWith('/')) return false;
  if (diskNumber(d) !== spec.disk_number) return false;
  return !checkModel || (d.model || d.device_id) === spec.disk_model;
}

/** The disk's part of an image file name — `own_backup_core::DiskSourceSpec::file_tag`:
 *  `disk<N>` on Windows; on Linux `disk<N>` with the window's number the job
 *  keeps, plain `disk` in a job saved without one (its chain is found by the
 *  device the images record, not by the name). */
export function diskFileTag(spec: DiskSourceSpec): string {
  if (!spec.device) return `disk${spec.disk_number}`;
  return spec.file_number ? `disk${spec.file_number}` : 'disk';
}
