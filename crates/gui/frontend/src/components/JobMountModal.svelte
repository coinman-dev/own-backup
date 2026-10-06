<script lang="ts">
  // «Подключить как диск…» — pick which version, then hand it to
  // «Инструменты», where mounting lives: the same page shows the letters the
  // volumes got and is where the disk is detached again.
  import { onMount } from 'svelte';
  import Modal from './Modal.svelte';
  import { api } from '../lib/api';
  import { toolsState } from '../lib/engineStore.svelte';
  import { goTo } from '../lib/navStore.svelte';
  import { choiceLabel, newestFirst, severalSources, type VersionChoice } from '../lib/jobVersions';
  import type { Job } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  interface Props {
    job: Job;
    onClose: () => void;
  }
  let { job, onClose }: Props = $props();

  let choices = $state<VersionChoice[] | null>(null);
  let withSource = $state(false);
  let imagePath = $state('');
  let failure = $state('');

  onMount(async () => {
    try {
      const versions = await api.listJobVersions(job.id);
      withSource = severalSources(versions);
      choices = newestFirst(versions);
      imagePath = choices[0]?.image.image_path ?? '';
    } catch (e) {
      failure = String(e);
      choices = [];
    }
  });

  function mount() {
    if (!imagePath) return;
    toolsState.mountPath = imagePath;
    // Read-only: a copy opened to look inside must not be changed by looking.
    toolsState.rwMode = false;
    toolsState.autoMount = true;
    goTo('tools');
    onClose();
  }
</script>

<Modal
  open
  title={m.jmt_title()}
  {onClose}
  onConfirm={mount}
  confirmDisabled={!imagePath}
  confirmLabel={m.tools_mount_btn()}
  cancelLabel={m.common_cancel()}
>
  {#if failure}
    <div class="banner err">{failure}</div>
  {:else if choices === null}
    <p class="plain">{m.jv_loading()}</p>
  {:else if choices.length === 0}
    <p class="plain">{m.jv_none()}</p>
  {:else}
    <p class="plain">{m.jmt_intro()}</p>
    <label class="fld">
      <span>{m.jv_version()}</span>
      <select class="form-select" bind:value={imagePath}>
        {#each choices as c (c.image.image_path)}
          <option value={c.image.image_path}>{choiceLabel(c, withSource)}</option>
        {/each}
      </select>
    </label>
  {/if}
</Modal>

<style>
  .plain { margin: 0 0 14px; font-size: 13px; color: var(--text); line-height: 1.5; }
  .fld { display: block; font-size: 13px; color: var(--head); }
  .fld > span:first-child { display: block; margin-bottom: 5px; font-weight: 600; }
  .banner { padding: 10px 12px; border-radius: 8px; font-size: 13px; line-height: 1.45; }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
</style>
