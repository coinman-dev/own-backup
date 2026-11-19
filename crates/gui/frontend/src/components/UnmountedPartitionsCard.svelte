<script lang="ts">
  /* Linux: the hidden system partitions nothing has mounted — EFI, Windows
     recovery, vendor partitions — mounted under /mnt by the service
     and unmounted again; the counterpart of HiddenVolumesCard on Windows.
     Which partitions qualify is decided there (`unmounted_volumes` in
     common/disk.rs); this card only shows its answer. */
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import { refreshVolumes } from '../lib/engineStore.svelte';
  import type { HiddenVolume } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let volumes = $state<HiddenVolume[]>([]);
  let loading = $state(true);
  let busy = $state<string | null>(null);
  let okMsg: (() => string) | null = $state(null);
  let errMsg: string | null = $state(null);

  const FS: Record<string, string> = {
    ntfs: 'NTFS', fat32: 'FAT32', ex_fat: 'exFAT',
  };

  function title(v: HiddenVolume): string {
    const kind =
      v.kind === 'esp'
        ? m.tools_letters_kind_esp()
        : v.kind === 'winre'
          ? m.tools_letters_kind_winre()
          : m.tools_mounts_kind_other();
    return v.label ? `${kind} «${v.label}»` : kind;
  }

  async function load() {
    loading = true;
    errMsg = null;
    try {
      volumes = await api.listHiddenVolumes();
    } catch (e) {
      errMsg = String(e);
    } finally {
      loading = false;
    }
  }

  async function change(v: HiddenVolume, mount: boolean) {
    busy = v.device;
    okMsg = null;
    errMsg = null;
    const before = v.mount_point ?? '';
    try {
      volumes = await api.setVolumeMount(v.device, mount);
      const now = volumes.find((x) => x.device === v.device);
      const dir = now?.mount_point ?? '';
      okMsg = mount
        ? now?.read_only
          ? () => m.tools_mounts_mounted_ro({ dir })
          : () => m.tools_mounts_mounted({ dir })
        : () => m.tools_mounts_unmounted({ dir: before });
      // The rest of the window (job wizard, restore) lists volumes too.
      void refreshVolumes();
    } catch (e) {
      errMsg = String(e);
    } finally {
      busy = null;
    }
  }

  onMount(load);
</script>

<div class="tool-card">
  <div class="tool-head">
    <span class="glyph">
      <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M7 12h10M12 7v10" /></svg>
    </span>
    <div class="grow">
      <div class="tool-title">{m.tools_mounts_title()}</div>
      <div class="tool-desc">{m.tools_mounts_desc()}</div>
    </div>
    <button class="btn outline" onclick={load} disabled={loading || busy !== null}>
      {m.tools_letters_refresh()}
    </button>
  </div>

  {#if loading}
    <div class="muted">{m.tools_letters_loading()}</div>
  {:else if volumes.length === 0 && !errMsg}
    <div class="muted">{m.tools_mounts_none()}</div>
  {:else}
    <div class="list">
      {#each volumes as v (v.device)}
        <div class="item">
          <div class="what">
            <div class="name">{title(v)}</div>
            <div class="where">
              {m.tools_mounts_where({
                dev: v.kernel_device || v.device,
                model: v.disk_model.trim(),
                fs: FS[v.filesystem] ?? v.filesystem,
                size: formatBytes(v.size_bytes),
              })}
            </div>
            {#if v.mount_point}
              <div class="at">
                {v.mount_point}{#if v.read_only}<span class="ro">{m.tools_mounts_ro()}</span>{/if}
              </div>
            {/if}
          </div>
          {#if v.mount_point}
            <button class="btn outline" onclick={() => api.openFolder(v.mount_point!).catch((e) => (errMsg = String(e)))}>
              {m.tools_mounts_open()}
            </button>
            <button class="btn outline" onclick={() => change(v, false)} disabled={busy !== null}>
              {m.tools_mounts_unmount()}
            </button>
          {:else}
            <button class="btn primary" onclick={() => change(v, true)} disabled={busy !== null}>
              {m.tools_mounts_mount()}
            </button>
          {/if}
        </div>
      {/each}
    </div>
    <div class="note">{m.tools_mounts_note()}</div>
  {/if}
  {#if okMsg}<div class="banner ok">{okMsg()}</div>{/if}
  {#if errMsg}<div class="banner err">{errMsg}</div>{/if}
</div>

<style>
  /* Svelte scopes styles per component: the card classes are repeated here,
     matching HiddenVolumesCard and the other cards of Tools. */
  .tool-card { border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); padding: 20px 22px; margin-bottom: 16px; }
  .tool-head { display: flex; gap: 14px; align-items: flex-start; margin-bottom: 14px; }
  .glyph { flex: none; width: 40px; height: 40px; border-radius: 8px; background: var(--accent-soft); color: var(--accent-text); display: flex; align-items: center; justify-content: center; }
  .grow { flex: 1; }
  .tool-title { font-size: 16px; font-weight: 600; color: var(--head); }
  .tool-desc { font-size: 12px; color: var(--text-secondary); margin-top: 3px; line-height: 1.5; }

  .list { display: flex; flex-direction: column; gap: 8px; }
  .item { display: flex; align-items: center; gap: 10px; padding: 10px 12px; border: 1px solid var(--border); border-radius: 8px; background: var(--panel2); }
  .what { flex: 1; min-width: 0; }
  .name { font-size: 13px; font-weight: 600; color: var(--head); }
  .where { font-size: 12px; color: var(--text-secondary); margin-top: 2px; }
  .at { font-size: 12px; color: var(--accent-text); margin-top: 2px; font-family: var(--font-mono, monospace); }
  .ro { margin-left: 8px; color: var(--text-secondary); font-family: var(--font); }
  .muted { font-size: 13px; color: var(--text-muted); }
  .note { font-size: 12px; color: var(--text-secondary); margin-top: 10px; line-height: 1.5; }

  .btn { border-radius: 6px; padding: 9px 20px; font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid transparent; white-space: nowrap; }
  .btn:disabled { opacity: 0.45; cursor: default; }
  .btn.primary { background: var(--primary); color: #fff; }
  .btn.primary:hover:not(:disabled) { background: var(--primary-hover); }
  .btn.outline { background: var(--card-bg); border-color: var(--border-strong); color: var(--text); }
  .btn.outline:hover:not(:disabled) { background: var(--panel2); }

  .banner { margin-top: 12px; border-radius: 8px; padding: 11px 14px; font-size: 13px; }
  .banner.ok { background: var(--success-soft); border: 1px solid var(--success-line); color: var(--success-text); }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
</style>
