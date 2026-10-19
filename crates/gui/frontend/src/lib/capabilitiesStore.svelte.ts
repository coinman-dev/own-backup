// What the service's backup engine carries out. The wizard offers only that:
// on Linux the engine refuses, for now, chains, exclusions, whole-disk copies,
// pacing and after-backup actions, and a job built with any of them never ran.
//
// The service answers (`backup_capabilities`); the window does not decide from
// its own platform, because the engine is the service's. Asked once a session —
// it cannot change while the service runs, and a new service comes with a new
// window in the same package.

import { api } from './api';
import { isWindows } from './platformStore.svelte';
import type { BackupCapabilities, Job } from './types';

const EVERYTHING: BackupCapabilities = {
  os: 'windows',
  methods: ['full', 'incremental', 'differential'],
  exclusions: true,
  whole_disk: true,
  speed_limit: true,
  low_priority: true,
  after_backup: true,
  archive_encryption: true,
  schedule_events: ['dest_connected', 'logon', 'logoff', 'startup'],
  wake_to_run: true,
  keeps_awake: true,
};

/** Until the service answers: what its engine on this system did when the
 *  window was built — the cautious answer on Linux, so nothing is offered
 *  there that a run would refuse. */
const LINUX_GUESS: BackupCapabilities = {
  os: 'linux',
  methods: ['full'],
  exclusions: false,
  whole_disk: false,
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
      caps.value = c;
      caps.known = true;
    },
    () => {
      asked = null;
      caps.value = isWindows() ? EVERYTHING : LINUX_GUESS;
    },
  );
  return asked;
}

/** Take out of a job what this engine would refuse it for, and say what was
 *  taken. A job is otherwise refused at run time with every reason at once —
 *  after it was saved, scheduled and queued.
 *
 *  `disk_source` goes too: without whole-disk copies the engine writes one
 *  image per volume, which is what the job then is. */
export function dropUnsupported(job: Job, c: BackupCapabilities): string[] {
  const dropped: string[] = [];
  if (!c.methods.includes(job.scheme.method)) {
    dropped.push('method');
    job.scheme.method = 'full';
    job.scheme.rolling_incrementals = null;
    job.scheme.only_incremental = false;
  }
  if (!c.exclusions && job.exclusions.enabled) {
    dropped.push('exclusions');
    job.exclusions.enabled = false;
  }
  if (!c.whole_disk && job.disk_source != null) {
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
