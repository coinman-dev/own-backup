// App-lifetime store for the backup job model: jobs list, run queue, live
// progress and activity. Backups are enqueued and processed serially by the
// Rust worker; this store mirrors its events.

import { listen } from '@tauri-apps/api/event';
import { api } from './api';
import type { Job, JobRun, JobProgress, QueueSnapshot } from './types';

export const jobs = $state({
  list: [] as Job[],
  loaded: false,
  queue: { current: null, pending: [] } as QueueSnapshot,
  progress: {} as Record<string, JobProgress>,
  activity: [] as JobRun[],
  error: null as string | null,
});

let inited = false;

export async function initJobs(): Promise<void> {
  if (inited) return;
  inited = true;

  await listen<JobProgress>('job-progress', (e) => {
    jobs.progress[e.payload.job_id] = e.payload;
  });
  await listen<QueueSnapshot>('queue-changed', (e) => {
    jobs.queue = e.payload;
  });
  await listen<JobRun>('job-finished', (e) => {
    delete jobs.progress[e.payload.job_id];
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

export async function deleteJob(id: string): Promise<void> {
  await api.deleteBackupJob(id);
  await reloadJobs();
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
    await api.cancelEngineOp();
  } catch {
    /* ignore */
  }
}

/** Status of a job for the list: running / queued / result of last run. */
export function jobStatus(job: Job): 'running' | 'queued' | 'success' | 'error' | 'never' {
  if (jobs.queue.current === job.id) return 'running';
  if (jobs.queue.pending.includes(job.id)) return 'queued';
  if (job.last_run?.status === 'success') return 'success';
  if (job.last_run?.status === 'error') return 'error';
  return 'never';
}
