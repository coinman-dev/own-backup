// What the windows of a job's menu share: how a stored copy is named on screen
// and which of them is "the latest".

import { m } from './paraglide/messages';
import { intlLocale } from './i18n.svelte';
import { formatBytes } from './format';
import type { Job, JobRun, JobVersion, JobVersionChain, JobVersions } from './types';

export function roleLabel(role: string): string {
  return role === 'full'
    ? m.jv_role_full()
    : role === 'differential'
      ? m.jv_role_differential()
      : m.jv_role_incremental();
}

/** «06.10.2026» — a chain is named by the days it spans. */
export function shortDate(iso: string): string {
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toLocaleDateString(intlLocale());
}

/** «06.10.2026, 14:30» — the moment one version was made. */
export function shortDateTime(iso: string): string {
  const d = new Date(iso);
  return isNaN(d.getTime())
    ? iso
    : d.toLocaleString(intlLocale(), { dateStyle: 'short', timeStyle: 'short' });
}

/** «06.10.2026 – 12.10.2026», or one date when the chain is one day old. */
export function chainSpan(chain: JobVersionChain): string {
  const first = shortDate(chain.images[0]?.created_at ?? '');
  const last = shortDate(chain.images[chain.images.length - 1]?.created_at ?? '');
  return first === last ? first : `${first} – ${last}`;
}

export function chainSize(chain: JobVersionChain): number {
  return chain.images.reduce((sum, i) => sum + i.size_bytes, 0);
}

export interface VersionChoice {
  image: JobVersion;
  chain: JobVersionChain;
}

/** Every version, newest first — the order a person picks one in. */
export function newestFirst(versions: JobVersions): VersionChoice[] {
  return versions.chains
    .flatMap((chain) => chain.images.map((image) => ({ image, chain })))
    .sort((a, b) => b.image.created_at.localeCompare(a.image.created_at));
}

/** A job that copies several volumes has chains of several sources, and then
 *  a version has to say which one it is of. */
export function severalSources(versions: JobVersions): boolean {
  return new Set(versions.chains.map((c) => c.source)).size > 1;
}

/** One line for a list of versions: when, what kind, how big. */
export function choiceLabel(c: VersionChoice, withSource: boolean): string {
  const parts = [shortDateTime(c.image.created_at), roleLabel(c.image.role), formatBytes(c.image.size_bytes)];
  if (withSource && c.chain.source) parts.push(c.chain.source);
  return parts.join(' · ');
}

/** A list of jobs in the chosen order. The history records processed source
 *  bytes, not the compressed size of files still present after retention; a copy
 *  added by hand has no history and is measured by what its newest file covers. */
export function sortJobs(list: Job[], key: 'name' | 'time' | 'size', activity: JobRun[]): Job[] {
  const out = [...list];
  if (key === 'time') {
    out.sort((a, b) => (b.last_run?.finished_at ?? '').localeCompare(a.last_run?.finished_at ?? ''));
  } else if (key === 'size') {
    const totals = new Map<string, number>();
    for (const run of activity) {
      totals.set(run.job_id, (totals.get(run.job_id) ?? 0) + (run.bytes_copied || 0));
    }
    const size = (j: Job) => totals.get(j.id) ?? j.last_run?.source_bytes ?? 0;
    out.sort((a, b) => size(b) - size(a));
  } else {
    out.sort((a, b) => a.name.localeCompare(b.name, intlLocale()));
  }
  return out;
}
