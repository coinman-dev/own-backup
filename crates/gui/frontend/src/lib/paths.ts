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

/** The separator `base` already writes with — a drive letter or a UNC name
 *  means `\`, a leading `/` means `/` — and the system's own when it shows
 *  none. A path keeps the style it came in: a folder the service reported on
 *  the other system, or one typed by hand, is not rewritten halfway. */
export function pathSeparator(base: string): '\\' | '/' {
  const b = base.trim();
  if (/^[A-Za-z]:/.test(b) || b.startsWith('\\\\')) return '\\';
  if (b.startsWith('/')) return '/';
  if (b.includes('\\')) return '\\';
  if (b.includes('/')) return '/';
  return isWindows() ? '\\' : '/';
}

/** `name` inside the folder `base`, with exactly one separator between them.
 *  Joining with a fixed `\` gave `/mnt/data/Backup\name.vhdx` on Linux. */
export function joinPath(base: string, name: string): string {
  if (!base) return name;
  if (/[\\/]$/.test(base)) return base + name;
  return base + pathSeparator(base) + name;
}
