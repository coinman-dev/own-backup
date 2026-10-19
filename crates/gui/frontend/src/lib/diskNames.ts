// How a disk is named on screen, the same on every screen that draws one.
//
// Windows numbers its disks (`\\.\PhysicalDrive3` → «Диск 3»), and that number
// is what Disk Management shows. Linux does not: `/dev/vda` has no number, and
// the digits of `nvme0n1` or of a `/dev/mapper/luks-16e1…` name are not one —
// read as a number they gave «Диск», «Диск 01» and a string of hex digits.
// There the kernel's name is the name people know.
//
// A device-mapper volume (an opened LUKS container, an LVM logical volume) is
// listed as a «disk» of its own by the inventory, with one volume covering it.
// It is not a disk and has no partition table: it is named for what it is.

import type { DiskInfo } from './types';
import { m } from './paraglide/messages';

/** `crypt` / `lvm` for a device-mapper volume listed as a disk, else ''. */
export function mapperKind(d: DiskInfo): string {
  return d.partitions.length === 1 ? (d.partitions[0].mapper_kind ?? '') : '';
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

/** The caption under a disk's icon: «Диск 3», «Диск vda», «Открытый LUKS». */
export function diskCaption(d: DiskInfo): string {
  const kind = mapperKind(d);
  if (kind === 'crypt') return m.disk_mapper_crypt();
  if (kind === 'lvm') return m.disk_mapper_lvm();
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
