// What the service's backup engine carries out. The wizard offers only that:
// a job asking for more is refused at run time — after it was saved,
// scheduled and queued. On Linux that is, for now, VHDX chains (R2),
// encrypted archives (no key store yet) and a disk named by number.
//
// The service answers (`backup_capabilities`) from the same table its engine
// refuses by (`own_backup_core::capabilities`); the window does not decide
// from its own platform. Asked once a session — it cannot change while the
// service runs, and a new service comes with a new window in the same package.

import { api } from './api';
import { isWindows } from './platformStore.svelte';
import type { BackupCapabilities, Job } from './types';

const EVERYTHING: BackupCapabilities = {
  os: 'windows',
  methods: ['full', 'incremental', 'differential'],
  chain_formats: ['vhdx', 'qcow2', 'obk'],
  exclusions: true,
  whole_disk: true,
  disk_named_by: 'number',
  speed_limit: true,
  low_priority: true,
  after_backup: true,
  archive_encryption: true,
  schedule_events: ['dest_connected', 'logon', 'logoff', 'startup'],
  wake_to_run: true,
  keeps_awake: true,
};

/** Until the service answers: the cautious answer on Linux, so nothing is
 *  offered there that an older engine would refuse. The real answer comes
 *  from the service. */
const LINUX_GUESS: BackupCapabilities = {
  os: 'linux',
  methods: ['full'],
  chain_formats: [],
  exclusions: false,
  whole_disk: false,
  disk_named_by: 'device',
  speed_limit: false,
  low_priority: false,
  after_backup: false,
  archive_encryption: false,
  schedule_events: ['dest_connected', 'startup'],
  wake_to_run: false,
  keeps_awake: false,
};

export const caps = $state<{ value: BackupCapabilities; known: boolean }>({
  value: isWindows() ? EVERYTHING : LINUX_GUESS,
  known: false,
});

let asked: Promise<void> | null = null;

/** Ask once; a failed ask is asked again next time (the service was down). */
export function ensureCapabilities(): Promise<void> {
  asked ??= api.backupCapabilities().then(
    (c) => {
      // A service from before these fields (dev builds only) answered with
      // its methods meaning "any format".
      const win = c.os === 'windows';
      caps.value = {
        ...c,
        chain_formats: c.chain_formats ?? (win ? EVERYTHING.chain_formats : ['qcow2', 'obk']),
        disk_named_by: c.disk_named_by ?? (win ? 'number' : 'device'),
      };
      caps.known = true;
    },
    () => {
      asked = null;
      caps.value = isWindows() ? EVERYTHING : LINUX_GUESS;
    },
  );
  return asked;
}

/** The container a chain of this job is written in, by the names of
 *  `chain_formats`. `encrypted` is the wizard's switch before the key exists. */
export function chainFormat(job: Job, encrypted = !!job.archive_encryption): string {
  return encrypted ? 'obk' : job.image_format;
}

/** Can this engine write a chain in `format`? */
export function canChain(c: BackupCapabilities, format: string): boolean {
  return c.chain_formats.includes(format) && c.methods.some((x) => x !== 'full');
}

/** Does the window build whole-disk jobs for this engine? Where a disk is
 *  named by its device (Linux: `/dev/disk/by-id/…`), not yet: the disk list
 *  carries the kernel name, which is not stable across reboots, and the map
 *  tells disks apart by number. The engine itself copies such a disk
 *  (`own-backup-cli backup-disk --device`). */
export function offersWholeDisk(c: BackupCapabilities): boolean {
  return c.whole_disk && c.disk_named_by === 'number';
}

/** Take out of a job what this engine would refuse it for, and say what was
 *  taken. A job is otherwise refused at run time with every reason at once —
 *  after it was saved, scheduled and queued. Mirrors
 *  `own_backup_core::capabilities::refusals`.
 *
 *  `disk_source` goes too when it would be refused: without it the engine
 *  writes one image per volume, which is what the job then is. */
export function dropUnsupported(job: Job, c: BackupCapabilities): string[] {
  const dropped: string[] = [];
  const method = job.scheme.method;
  if (!c.methods.includes(method) || (method !== 'full' && !c.chain_formats.includes(chainFormat(job)))) {
    dropped.push('method');
    job.scheme.method = 'full';
    job.scheme.rolling_incrementals = null;
    job.scheme.only_incremental = false;
  }
  if (!c.exclusions && job.exclusions.enabled) {
    dropped.push('exclusions');
    job.exclusions.enabled = false;
  }
  if (
    job.disk_source != null &&
    (!c.whole_disk || (c.disk_named_by === 'device' && !job.disk_source.device))
  ) {
    dropped.push('whole_disk');
    job.disk_source = null;
  }
  if (!c.speed_limit && job.advanced.speed_limit_mbps > 0) {
    dropped.push('speed_limit');
    job.advanced.speed_limit_mbps = 0;
  }
  if (!c.low_priority && job.advanced.low_priority) {
    dropped.push('low_priority');
    job.advanced.low_priority = false;
  }
  if (!c.after_backup && !['', 'none'].includes(job.advanced.after_backup)) {
    dropped.push('after_backup');
    job.advanced.after_backup = 'none';
  }
  if (!c.wake_to_run) {
    if (job.schedule.wake_to_run) dropped.push('wake_to_run');
    job.schedule.wake_to_run = false;
    if (job.verification) job.verification.schedule.wake_to_run = false;
  }
  return dropped;
}
