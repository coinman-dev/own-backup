<script lang="ts">
  // What a finished copy has to say about itself. Activity shows all of it.
  // The job's page shows it `brief`: a card of a few lines the user can close
  // for good — what was made, and how many remarks there are, with a way to
  // Activity for their text (owner, 16.11.2026: the full text grew to eleven
  // lines over the job's page).
  import { media } from '../lib/mediaStore.svelte';
  import { isWindows } from '../lib/platformStore.svelte';
  import { jobs } from '../lib/jobStore.svelte';
  import { formatBytes } from '../lib/format';
  import type { JobRun } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { goTo } from '../lib/navStore.svelte';

  let { run, brief = false }: { run: JobRun; brief?: boolean } = $props();

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
  /** Everything below the first line of the full summary, counted. */
  const remarks = $derived(
    (exclusionsIgnored ? 1 : 0) +
      (run.excluded_entries_kept ? 1 : 0) +
      (run.layout_changed ? 1 : 0) +
      (run.notes?.length ?? 0),
  );
</script>

{#if brief}
{m.dash_brief_created({ kind, size: formatBytes(run.bytes_copied) })}
{#if run.consolidated_increments > 0}
  {m.dash_brief_consolidated({ count: run.consolidated_increments })}
{/if}
{#if run.excluded_files > 0}
  {m.dash_excluded_files({ count: run.excluded_files, size: formatBytes(run.excluded_bytes) })}
{/if}
{#if remarks > 0}
  <br />
  {m.dash_brief_remarks({ count: remarks })}
  <button class="to-activity" onclick={() => goTo('activity')}>{m.dash_brief_details()}</button>
{/if}
{:else}

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
{/if}

<style>
  .to-activity {
    border: none;
    background: none;
    padding: 0;
    font: inherit;
    color: var(--accent-text);
    text-decoration: underline;
    cursor: pointer;
  }
</style>
