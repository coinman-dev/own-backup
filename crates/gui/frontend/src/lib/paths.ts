// Paths as the system the window runs on writes them.

import { isWindows } from './platformStore.svelte';

/** Sample paths for empty fields. A `D:\…` hint on Linux shows a path that
 *  cannot exist there, so each one has its system's counterpart. Paths, not
 *  words: they stay the same in both languages. */
const EXAMPLES = {
  /** A destination or output folder. */
  folder: { windows: 'D:\\OwnBackup', linux: '/mnt/backup/OwnBackup' },
  /** One backup image. */
  image: {
    windows: 'D:\\OwnBackup\\disk_C_20260702.vhdx',
    linux: '/mnt/backup/OwnBackup/disk0_20260702.qcow2',
  },
  /** A removable drive, as the log mirror's target. */
  drive: { windows: 'E:\\', linux: '/media/user/USB' },
} as const;

export function examplePath(kind: keyof typeof EXAMPLES): string {
  return isWindows() ? EXAMPLES[kind].windows : EXAMPLES[kind].linux;
}
