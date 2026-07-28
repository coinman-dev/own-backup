/** One description of what a disk looks like, for every screen that draws one.
 *
 *  There were three: the backup wizard hid any gap under 1% of the disk (8.9 GiB
 *  on a 894 GiB drive — real free space, invisible), the restore map hid
 *  anything under 1 MiB, and the restore designer knew about the areas GPT
 *  reserves at both ends while the other two did not. Same disks, three
 *  pictures, and the user right to ask which one to believe.
 */
import type { DiskInfo, PartitionInfo } from './types';

export const MIB = 1024 * 1024;

/** Gaps smaller than this are not drawn.
 *
 *  Absolute, never a percentage of the disk: a percentage hides more free space
 *  the bigger the disk, which is exactly backwards. The value is the size below
 *  which a gap is alignment padding rather than space anyone could use — the
 *  smallest partition Windows itself creates (a Microsoft Reserved on a small
 *  disk) is 16 MiB. A 1 MiB sliver left between two aligned partitions is not
 *  free space in any useful sense, and labelling it «Не занято» only invites the
 *  question of why it cannot be used.
 */
export const MIN_FREE_GAP = 16 * MIB;

/** The stretch of a disk a partition may actually occupy.
 *
 *  Not the whole disk: a GPT keeps its header and entry array in the first 34
 *  sectors and a full mirror in the last 33. Treating the disk as [0, size)
 *  once put a restored partition at LBA 0 and let «расширить раздел» grow the
 *  last one over the backup GPT.
 *
 *  The head is rounded up to 1 MiB — where every partitioning tool since Vista
 *  starts the first partition.
 */
export function usableRange(d: DiskInfo, gpt = isGpt(d)): { start: number; end: number } {
  const sector = d.sector_size_logical || 512;
  const sectors = Math.floor(d.size_bytes / sector);
  return gpt
    ? {
        start: Math.ceil((34 * sector) / MIB) * MIB,
        // Last usable LBA is `sectors - 34`; the exclusive byte end is one
        // sector past it.
        end: Math.max(0, (sectors - 33) * sector),
      }
    : { start: Math.ceil(sector / MIB) * MIB, end: sectors * sector };
}

export function isGpt(d: DiskInfo): boolean {
  return ((d.partition_table as string) ?? 'gpt') !== 'mbr';
}

export interface DiskSeg {
  kind: 'part' | 'free';
  /** Bytes. For a partition, its size. */
  size: number;
  /** Byte offset on the disk. */
  offset: number;
  p?: PartitionInfo;
}

/** Partitions in on-disk order, plus the unallocated regions worth drawing. */
export function diskSegments(d: DiskInfo): DiskSeg[] {
  const parts = [...(d.partitions ?? [])].sort((a, b) => a.offset_bytes - b.offset_bytes);
  const usable = usableRange(d);
  const segs: DiskSeg[] = [];
  let cursor = usable.start;
  for (const p of parts) {
    const gap = p.offset_bytes - cursor;
    if (gap >= MIN_FREE_GAP) segs.push({ kind: 'free', size: gap, offset: cursor });
    segs.push({ kind: 'part', size: p.size_bytes, offset: p.offset_bytes, p });
    cursor = Math.max(cursor, p.offset_bytes + p.size_bytes);
  }
  const tail = usable.end - cursor;
  if (tail >= MIN_FREE_GAP) segs.push({ kind: 'free', size: tail, offset: cursor });
  return segs;
}
