<script lang="ts">
  // What a finished copy has to say about itself. The job's page shows it as a
  // card the user can close for good, so the same words stay in Activity.
  import { media } from '../lib/mediaStore.svelte';
  import { isWindows } from '../lib/platformStore.svelte';
  import { jobs } from '../lib/jobStore.svelte';
  import { formatBytes } from '../lib/format';
  import type { JobRun } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let { run }: { run: JobRun } = $props();

  const KIND_LABELS: Record<string, () => string> = {
    incremental: m.kind_incremental,
    differential: m.kind_differential,
  };
  const kind = $derived((KIND_LABELS[run.backup_kind] ?? m.kind_full)());

  // The job asks for exclusions and the copy did not apply them. Reporting
  // "Исключено файлов: 0" for that reads as "the rules matched nothing", which
  // is the opposite of what happened — the rules were never run. A job deleted
  // since has no rules left to ask about.
  const job = $derived(jobs.list.find((j) => j.id === run.job_id));
  const exclusionsIgnored = $derived(
    !!run.file_scan_skipped &&
      run.excluded_files === 0 &&
      !!job?.exclusions.enabled &&
      job.exclusions.patterns.length > 0,
  );
</script>

<!-- `vss_used` is "read from a snapshot" on both systems: VSS on
     Windows, an LVM or device-mapper snapshot on Linux. -->
{run.vss_used
  ? isWindows()
    ? m.dash_created_vss({ kind })
    : m.dash_created_snapshot({ kind })
  : media.active
    ? m.dash_created_cold({ kind })
    : m.dash_created({ kind })}
{run.smart_mode ? m.dash_used_only() : m.dash_sector_by_sector()}
{#if run.backup_kind === 'incremental' || run.backup_kind === 'differential'}
  {m.dash_changed_only({ size: formatBytes(run.bytes_copied) })}
{/if}
{#if run.consolidated_increments > 0}
  {m.dash_consolidated({ count: run.consolidated_increments })}
{/if}
{#if run.excluded_files > 0}
  {m.dash_excluded_files({ count: run.excluded_files, size: formatBytes(run.excluded_bytes) })}
{:else if exclusionsIgnored && job}
  <br />
  <b>{m.dash_excl_ignored()}</b>
  {m.dash_excl_ignored_rest({
    rules: m.dash_rules_count({ count: job.exclusions.patterns.length }),
    reason: run.file_scan_skipped ?? '',
  })}
{/if}
{#if run.excluded_entries_kept}
  <br />
  <b>{m.dash_entries_kept()}</b>
  {run.excluded_entries_kept}.
{/if}
{#if run.layout_changed}
  <br />
  {run.layout_changed}.
{/if}
{#each run.notes ?? [] as note}
  <br />
  {note}
{/each}
