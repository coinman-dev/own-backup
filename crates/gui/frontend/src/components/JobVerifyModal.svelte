<script lang="ts">
  // «Проверить последнюю версию» and «Проверить все версии» of a job.
  //
  // The latest version is read whole, through every copy under it — that
  // answers "can the newest state be restored". All versions are read the
  // same way, through their parents: the recorded checksums must still match
  // even if a damaged allocation table has lost a block's location.
  //
  // An encrypted job asks for its password first (§7.4): checking is reading,
  // and the key the service holds for scheduled copies is not lent to it.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import Modal from './Modal.svelte';
  import ArchiveUnlockModal from './ArchiveUnlockModal.svelte';
  import { api } from '../lib/api';
  import { cancelCurrent } from '../lib/engineStore.svelte';
  import { formatDuration } from '../lib/format';
  import type { ArchiveSecret, Job, JobVerifyProgress, JobVerifySummary } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    job: Job;
    allVersions: boolean;
    onClose: () => void;
  }
  let { job, allVersions, onClose }: Props = $props();

  let stage = $state<'loading' | 'empty' | 'unlock' | 'running' | 'done' | 'failed'>('loading');
  /** How many files will be read — said before the reading starts. */
  let planned = $state(0);
  let progress = $state<JobVerifyProgress | null>(null);
  let summary = $state<JobVerifySummary | null>(null);
  let failure = $state('');
  let unlockBusy = $state(false);
  let unlockError = $state('');
  let cancelling = $state(false);

  const title = $derived(allVersions ? m.jvr_title_all() : m.jvr_title_latest());
  const damaged = $derived((summary?.images ?? []).filter((i) => !i.valid));
  const blocksChecked = $derived((summary?.images ?? []).reduce((sum, i) => sum + i.blocks_checked, 0));

  onMount(() => {
    // The first progress message is the proof that the key opened the copy:
    // nothing is read before it does. That is when the password window goes.
    const unlisten = listen<JobVerifyProgress>('job-verify-progress', (e) => {
      progress = e.payload;
      if (stage === 'unlock') stage = 'running';
    });
    void prepare();
    return () => {
      void unlisten.then((stop) => stop());
    };
  });

  async function prepare() {
    try {
      const versions = await api.listJobVersions(job.id);
      planned = allVersions
        ? versions.chains.reduce((sum, c) => sum + c.images.length, 0)
        : new Set(versions.chains.map((c) => c.source)).size;
      if (planned === 0) {
        stage = 'empty';
      } else if (job.archive_encryption) {
        stage = 'unlock';
      } else {
        void run();
      }
    } catch (e) {
      failure = String(e);
      stage = 'failed';
    }
  }

  async function run(secret?: ArchiveSecret) {
    failure = '';
    unlockError = '';
    progress = null;
    cancelling = false;
    if (secret) unlockBusy = true;
    else stage = 'running';
    try {
      summary = await api.verifyJob(job.id, allVersions, secret);
      stage = 'done';
    } catch (e) {
      if (cancelling) {
        onClose();
      } else if (stage === 'unlock') {
        // A wrong password is a thing to retype, not a window to lose.
        unlockError = String(e);
      } else {
        failure = String(e);
        stage = 'failed';
      }
    } finally {
      unlockBusy = false;
    }
  }

  function close() {
    if (stage === 'running') {
      // Closing the window of a running check stops the check: nothing else
      // would show that it is still going.
      cancelling = true;
      void cancelCurrent();
      return;
    }
    onClose();
  }
</script>

{#if stage === 'unlock'}
  <ArchiveUnlockModal
    open
    imageName={job.name}
    busy={unlockBusy}
    error={unlockError}
    onUnlock={(secret) => run(secret)}
    onCancel={onClose}
  />
{:else}
  <Modal
    open
    {title}
    onClose={close}
    onConfirm={stage === 'running' ? undefined : onClose}
    confirmLabel={m.common_close()}
    cancelLabel={stage === 'running' ? (cancelling ? m.jv_cancelling() : m.common_cancel()) : ''}
  >
    {#if stage === 'loading'}
      <p class="plain">{m.jv_loading()}</p>
    {:else if stage === 'empty'}
      <p class="plain">{m.jv_none()}</p>
    {:else if stage === 'running'}
      <p class="plain">
        {#if progress}
          {m.jvr_reading({ n: progress.index + 1, count: progress.count })}
          <span class="file">{progress.file_name}</span>
        {:else}
          {m.jvr_starting({ versions: m.jvc_n_versions({ count: planned }) })}
        {/if}
      </p>
      <div class="bar"><div class="bar-fill" style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="percent">{progress?.percent ?? 0} %</div>
    {:else if stage === 'failed'}
      <div class="banner err">{failure}</div>
    {:else if summary}
      {#if summary.valid}
        <div class="banner ok">
          {m.jvr_ok({
            versions: m.jvc_n_versions({ count: summary.images.length }),
            blocks: m.tools_blocks({ count: blocksChecked }),
            time: formatDuration(summary.duration_secs),
          })}
        </div>
      {:else}
        <div class="banner err">
          {m.jvr_bad({ bad: damaged.length, count: summary.images.length })}
        </div>
      {/if}
      <div class="list">
        {#each summary.images as image (image.image_path)}
          <div class="item" class:bad={!image.valid}>
            <span class="mark">
              {#if image.valid}
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
              {:else}
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
              {/if}
            </span>
            <span class="name">{image.file_name}</span>
            <span class="what">
              {#if image.valid}
                {m.tools_blocks({ count: image.blocks_checked })}
              {:else if image.error}
                {image.error}
              {:else}
                {m.jvr_mismatch({ bad: image.mismatched_blocks, blocks: m.tools_blocks_of({ count: image.blocks_checked }) })}
              {/if}
            </span>
          </div>
        {/each}
      </div>
    {/if}
  </Modal>
{/if}

<style>
  .plain { margin: 0 0 12px; font-size: 13px; color: var(--text); line-height: 1.5; }
  .file { display: block; margin-top: 2px; color: var(--text-muted); overflow-wrap: anywhere; }
  .bar { height: 8px; border-radius: 4px; background: var(--track); overflow: hidden; }
  .bar-fill { height: 100%; border-radius: 4px; background: var(--success); transition: width 0.15s linear; }
  .percent { margin-top: 6px; font-size: 12px; color: var(--text-muted); text-align: right; }

  .banner { padding: 10px 12px; border-radius: 8px; font-size: 13px; line-height: 1.45; }
  .banner.ok { background: var(--success-soft); border: 1px solid var(--success-line); color: var(--success-text); }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }

  .list { margin-top: 12px; border: 1px solid var(--border); border-radius: 8px; overflow: hidden; }
  .item { display: grid; grid-template-columns: 18px minmax(0, 1fr); column-gap: 8px; padding: 8px 12px; font-size: 13px; border-top: 1px solid var(--border); }
  .item:first-child { border-top: none; }
  .mark { grid-row: span 2; display: flex; padding-top: 2px; color: var(--success); }
  .item.bad .mark { color: var(--error); }
  .name { color: var(--text); overflow-wrap: anywhere; }
  .what { font-size: 12px; color: var(--text-muted); }
  .item.bad .what { color: var(--error-text); }
</style>
