<script lang="ts">
  // «Сохранить как VHDX…» — the disk one version shows, written out as a single
  // file that stands on its own: no chain under it, no compression, no
  // encryption. Windows opens such a file by itself, and so does a virtual
  // machine.
  //
  // The file is written by the service, so the place is named in full; and an
  // encrypted job asks for its password first (§7.4) — the result is the data
  // in the clear, which is exactly what the password guards.
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import Modal from './Modal.svelte';
  import ArchiveUnlockModal from './ArchiveUnlockModal.svelte';
  import { api } from '../lib/api';
  import { cancelCurrent } from '../lib/engineStore.svelte';
  import { media } from '../lib/mediaStore.svelte';
  import { pickPath } from '../lib/pickPath.svelte';
  import { formatBytes, formatDuration } from '../lib/format';
  import { choiceLabel, newestFirst, severalSources, type VersionChoice } from '../lib/jobVersions';
  import type { ArchiveSecret, EngineProgress, ExportSummary, Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    job: Job;
    onClose: () => void;
  }
  let { job, onClose }: Props = $props();

  let stage = $state<'loading' | 'empty' | 'form' | 'unlock' | 'running' | 'done'>('loading');
  let choices = $state<VersionChoice[]>([]);
  let withSource = $state(false);
  let imagePath = $state('');
  // svelte-ignore state_referenced_locally
  let folder = $state(job.dest_folder);
  let fileName = $state('');
  let progress = $state<EngineProgress | null>(null);
  let summary = $state<ExportSummary | null>(null);
  let failure = $state('');
  let unlockBusy = $state(false);
  let unlockError = $state('');
  let cancelling = $state(false);

  const chosen = $derived(choices.find((c) => c.image.image_path === imagePath) ?? null);
  /** What Windows refuses in a file name. */
  const badName = $derived(/[\\/:*?"<>|]/.test(fileName.trim()));
  const target = $derived.by(() => {
    const dir = folder.trim();
    let name = fileName.trim();
    if (!dir || !name) return '';
    if (!/\.vhdx$/i.test(name)) name += '.vhdx';
    const sep = dir.includes('\\') || /^[A-Za-z]:/.test(dir) ? '\\' : '/';
    return dir.endsWith('\\') || dir.endsWith('/') ? dir + name : dir + sep + name;
  });
  const ready = $derived(!!chosen && !!target && !badName);

  /** A name that says whose disk this is and of which day. */
  function suggestName(c: VersionChoice): string {
    const day = c.image.created_at.slice(0, 10);
    const base = job.name.replace(/[\\/:*?"<>|]/g, '_').trim() || 'backup';
    return `${base} ${day}.vhdx`;
  }

  onMount(() => {
    const unlisten = listen<EngineProgress>('export-progress', (e) => {
      progress = e.payload;
      // The first block read is the proof that the key opened the copy.
      if (stage === 'unlock') stage = 'running';
    });
    void load();
    return () => {
      void unlisten.then((stop) => stop());
    };
  });

  async function load() {
    try {
      const versions = await api.listJobVersions(job.id);
      choices = newestFirst(versions);
      withSource = severalSources(versions);
      if (choices.length === 0) {
        stage = 'empty';
        return;
      }
      imagePath = choices[0].image.image_path;
      fileName = suggestName(choices[0]);
      stage = 'form';
    } catch (e) {
      failure = String(e);
      stage = 'form';
    }
  }

  function pickVersion(path: string) {
    const before = chosen;
    imagePath = path;
    // Follow the version with the name — unless the name was typed by hand.
    if (chosen && (!before || fileName === suggestName(before))) fileName = suggestName(chosen);
  }

  async function browse() {
    try {
      const picked = await pickPath({ directory: true, title: m.jex_pick_folder(), startPath: folder || undefined });
      if (picked) folder = picked;
    } catch {
      /* dialog cancelled */
    }
  }

  function start() {
    if (!ready) return;
    failure = '';
    if (job.archive_encryption) {
      unlockError = '';
      stage = 'unlock';
    } else {
      void run();
    }
  }

  async function run(secret?: ArchiveSecret) {
    progress = null;
    cancelling = false;
    unlockError = '';
    if (secret) unlockBusy = true;
    else stage = 'running';
    try {
      summary = await api.exportImage(imagePath, target, secret);
      stage = 'done';
    } catch (e) {
      if (stage === 'unlock') {
        unlockError = String(e);
      } else {
        // Stopped on request is not a failure to report: the form is back.
        failure = cancelling ? '' : String(e);
        stage = 'form';
      }
    } finally {
      unlockBusy = false;
    }
  }

  function close() {
    if (stage === 'running') {
      cancelling = true;
      void cancelCurrent();
      return;
    }
    onClose();
  }

  function openFolder() {
    api.openFolder(folder.trim()).catch((e) => (failure = String(e)));
  }
</script>

{#if stage === 'unlock'}
  <ArchiveUnlockModal
    open
    imageName={chosen?.image.file_name ?? job.name}
    busy={unlockBusy}
    error={unlockError}
    onUnlock={(secret) => run(secret)}
    onCancel={() => (stage = 'form')}
  />
{:else}
  <Modal
    open
    title={m.jex_title()}
    onClose={close}
    onConfirm={stage === 'form' ? start : stage === 'running' ? undefined : onClose}
    confirmDisabled={stage === 'form' && !ready}
    confirmLabel={stage === 'form' ? m.common_save() : m.common_close()}
    cancelLabel={stage === 'form'
      ? m.common_cancel()
      : stage === 'running'
        ? cancelling
          ? m.jv_cancelling()
          : m.common_cancel()
        : ''}
  >
    {#if stage === 'loading'}
      <p class="plain">{m.jv_loading()}</p>
    {:else if stage === 'empty'}
      <p class="plain">{m.jv_none()}</p>
    {:else if stage === 'form'}
      <p class="plain">{m.jex_intro()}</p>
      {#if choices.length > 0}
        <label class="fld">
          <span>{m.jv_version()}</span>
          <select class="form-select" value={imagePath} onchange={(e) => pickVersion(e.currentTarget.value)}>
            {#each choices as c (c.image.image_path)}
              <option value={c.image.image_path}>{choiceLabel(c, withSource)}</option>
            {/each}
          </select>
        </label>
        <div class="fld">
          <span>{m.jex_folder()}</span>
          <div class="pair">
            <input class="form-input" type="text" bind:value={folder} spellcheck="false" />
            <button class="btn btn-secondary" onclick={browse}>{m.common_browse()}</button>
          </div>
        </div>
        <label class="fld">
          <span>{m.jex_file_name()}</span>
          <input class="form-input" type="text" bind:value={fileName} spellcheck="false" />
        </label>
        {#if badName}
          <p class="hint bad">{m.jex_bad_name()}</p>
        {:else if target}
          <p class="hint">{target}</p>
        {/if}
        {#if job.archive_encryption}
          <div class="banner note">{m.jex_not_encrypted()}</div>
        {/if}
      {/if}
      {#if failure}<div class="banner err">{failure}</div>{/if}
    {:else if stage === 'running'}
      <p class="plain">
        {m.jex_writing()}
        <span class="file">{target}</span>
      </p>
      <div class="bar"><div class="bar-fill" style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="percent">
        {#if progress}
          {formatBytes(progress.bytes_done)} · {m.dash_speed({ speed: progress.speed_mbps.toFixed(1) })} · {progress.percent} %
        {:else}
          0 %
        {/if}
      </div>
    {:else if summary}
      <div class="banner ok">
        {m.jex_done({
          stored: formatBytes(summary.bytes_stored),
          size: formatBytes(summary.virtual_size),
          time: formatDuration(summary.duration_secs),
        })}
        <span class="file">{summary.target_path}</span>
      </div>
      {#if failure}<div class="banner err">{failure}</div>{/if}
      {#if !media.active}
        <button class="btn btn-secondary open" onclick={openFolder}>{m.jex_open_folder()}</button>
      {/if}
    {/if}
  </Modal>
{/if}

<style>
  .plain { margin: 0 0 14px; font-size: 13px; color: var(--text); line-height: 1.5; }
  .file { display: block; margin-top: 2px; overflow-wrap: anywhere; opacity: 0.85; }
  .fld { display: block; margin-bottom: 12px; font-size: 13px; color: var(--head); }
  .fld > span:first-child { display: block; margin-bottom: 5px; font-weight: 600; }
  .pair { display: flex; gap: 8px; }
  .pair .btn { flex-shrink: 0; }
  .hint { margin: -4px 0 12px; font-size: 12px; color: var(--text-muted); overflow-wrap: anywhere; }
  .hint.bad { color: var(--error-text); }
  .bar { height: 8px; border-radius: 4px; background: var(--track); overflow: hidden; }
  .bar-fill { height: 100%; border-radius: 4px; background: var(--success); transition: width 0.15s linear; }
  .percent { margin-top: 6px; font-size: 12px; color: var(--text-muted); text-align: right; }
  .banner { padding: 10px 12px; border-radius: 8px; font-size: 13px; line-height: 1.45; }
  .banner + .banner { margin-top: 12px; }
  .banner.ok { background: var(--success-soft); border: 1px solid var(--success-line); color: var(--success-text); }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
  .banner.note { background: var(--warning-soft); border: 1px solid var(--warning-line); color: var(--warning-text); }
  .open { margin-top: 14px; }
</style>
