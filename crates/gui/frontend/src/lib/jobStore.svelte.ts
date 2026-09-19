// App-lifetime store for the backup job model: jobs list, run queue, live
// progress and activity. Backups are enqueued and processed serially by the
// Rust worker; this store mirrors its events.

import { listen } from '@tauri-apps/api/event';
import { api } from './api';
import type { Job, JobRun, JobProgress, QueueSnapshot, DecryptResult, JobFilesDeleted } from './types';

/** What a running job's view needs but the events do not repeat: when the
 *  current phase started and a short window of recent throughput for the
 *  estimate. */
interface RunClock {
  phase: string;
  startMs: number;
  samples: { t: number; blocks: number }[];
}

export const jobs = $state({
  list: [] as Job[],
  loaded: false,
  queue: { current: null, pending: [] } as QueueSnapshot,
  progress: {} as Record<string, JobProgress>,
  activity: [] as JobRun[],
  error: null as string | null,
});

/** Encrypted volumes unlocked during this GUI session: device → inner volume
 *  info (label / used bytes / filesystem). Kept until the app closes, so
 *  reopening the backup wizard shows the disk already decrypted without asking
 *  for the password again (the backend also caches the password for the
 *  session). Keyed by the source device string. */
export const decryptedVolumes = $state<
  Record<string, { label: string; used_bytes: number; filesystem: string; linux_last_mounted: string }>
>({});

/** Timing of the runs going on right now, by job id. Here and not in the view
 *  because walking through the menu unmounts the view: the elapsed clock then
 *  started again from zero, and the estimate with it (2026-09-18). */
export const runClock = $state<Record<string, RunClock>>({});

/** Note the time of a progress message: a new phase starts the clock afresh,
 *  and the throughput window keeps the last twelve seconds. */
function keepTime(p: JobProgress): void {
  const now = Date.now();
  let c = runClock[p.job_id];
  if (!c || c.phase !== p.phase) {
    c = { phase: p.phase, startMs: now, samples: [] };
    runClock[p.job_id] = c;
  }
  const last = c.samples[c.samples.length - 1];
  if (last && last.blocks === p.blocks_done) return;
  c.samples.push({ t: now, blocks: p.blocks_done });
  while (c.samples.length > 2 && c.samples[0].t < now - 12000) c.samples.shift();
}

/** How long the current phase still has at the rate of the last few seconds,
 *  or `NaN` while the window is too short to say anything. */
export function etaMs(jobId: string): number {
  const p = jobs.progress[jobId];
  const c = runClock[jobId];
  if (!p || !c || c.samples.length < 2) return NaN;
  const first = c.samples[0];
  const last = c.samples[c.samples.length - 1];
  const dt = last.t - first.t;
  const db = last.blocks - first.blocks;
  if (dt < 1500 || db <= 0) return NaN;
  return ((p.blocks_total - p.blocks_done) * dt) / db;
}

let inited = false;

export async function initJobs(): Promise<void> {
  if (inited) return;
  inited = true;

  await listen<JobProgress>('job-progress', (e) => {
    const p = e.payload;
    // Разбивка по типам файлов приходит ОДИН раз, сразу после скана, и
    // следующие сообщения её не несут — здесь она переносится из предыдущего.
    // Пока она жила в представлении, достаточно было уйти в другой раздел и
    // вернуться, чтобы она исчезла до конца прогона (2026-09-18).
    const prev = jobs.progress[p.job_id];
    if (!p.categories?.length && prev?.categories?.length) {
      p.categories = prev.categories;
    }
    jobs.progress[p.job_id] = p;
    keepTime(p);
  });
  await listen<QueueSnapshot>('queue-changed', (e) => {
    jobs.queue = e.payload;
  });
  await listen<JobRun>('job-finished', (e) => {
    delete jobs.progress[e.payload.job_id];
    delete runClock[e.payload.job_id];
    void reloadJobs();
    void reloadActivity();
  });

  await reloadJobs();
  await reloadActivity();
  try {
    jobs.queue = await api.getRunQueue();
  } catch {
    /* ignore */
  }
}

export async function reloadJobs(): Promise<void> {
  try {
    jobs.list = await api.listBackupJobs();
    jobs.loaded = true;
  } catch (e) {
    jobs.error = String(e);
  }
}

export async function reloadActivity(): Promise<void> {
  try {
    jobs.activity = await api.listActivity();
  } catch {
    /* ignore */
  }
}

export async function saveJob(job: Job): Promise<Job> {
  const saved = await api.saveBackupJob(job);
  await reloadJobs();
  return saved;
}

/** Delete a job, optionally with its copies. Returns what actually went and
 *  what stayed — "delete the files" is not a promise that can always be kept,
 *  and saying nothing about the leftovers is how they go unnoticed. */
export async function deleteJob(
  id: string,
  deleteFiles = false,
): Promise<JobFilesDeleted> {
  const report = await api.deleteBackupJob(id, deleteFiles);
  await reloadJobs();
  return report;
}

/** Unlock an encrypted source and read its inner volume label / used bytes.
 *  On success the password is cached (and persisted when `remember`). */
export async function decryptVolume(
  volume: string,
  password: string,
  remember: boolean,
): Promise<DecryptResult> {
  return api.decryptVolume(volume, password, remember);
}

export async function runJob(id: string): Promise<void> {
  await api.runBackupJob(id);
  try {
    jobs.queue = await api.getRunQueue();
  } catch {
    /* the event will refresh it */
  }
}

export async function cancelQueued(id: string): Promise<void> {
  await api.cancelQueuedJob(id);
  try {
    jobs.queue = await api.getRunQueue();
  } catch {
    /* ignore */
  }
}

export async function cancelRunning(): Promise<void> {
  try {
    await api.cancelRunningBackup();
  } catch {
    /* ignore */
  }
}

/** Human summary of a job's source volume(s): one label, or "C: Windows, EFI"
 *  for many (prefers labels over raw device paths like `\\?\Volume{…}`). */
export function sourceSummary(job: Job): string {
  const devs = job.sources && job.sources.length ? job.sources : job.source ? [job.source] : [];
  if (devs.length === 0) return '—';
  const labels = job.source_labels && job.source_labels.length === devs.length ? job.source_labels : devs;
  if (devs.length === 1) return job.source_label || labels[0] || devs[0];
  return labels.join(', ');
}

/** Status of a job for the list: running / queued / result of last run. */
export function jobStatus(job: Job): 'running' | 'queued' | 'success' | 'error' | 'never' {
  if (jobs.queue.current === job.id) return 'running';
  if (jobs.queue.pending.includes(job.id)) return 'queued';
  if (job.last_run?.status === 'success') return 'success';
  if (job.last_run?.status === 'error') return 'error';
  return 'never';
}
