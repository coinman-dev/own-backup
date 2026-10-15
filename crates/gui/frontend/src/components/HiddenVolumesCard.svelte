<script lang="ts">
  /* Letters for hidden system partitions: EFI, the recovery environment, a
     vendor's recovery or tools partition. Disk Management will not give them
     one; the service can. Which partitions qualify is decided there
     (`hidden_volumes` in common/disk.rs) — this card only shows its answer. */
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import { refreshVolumes } from '../lib/engineStore.svelte';
  import type { HiddenVolume } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let volumes = $state<HiddenVolume[]>([]);
  let loading = $state(true);
  let busy = $state<string | null>(null);
  /** Letters taken in THIS session — network drives and `subst` included,
   *  which the service (another session) cannot see. */
  let usedMask = $state(0);
  /** The letter picked for each letterless volume, by device. */
  let picked = $state<Record<string, string>>({});
  let okMsg: (() => string) | null = $state(null);
  let errMsg: string | null = $state(null);

  const FS: Record<string, string> = {
    ntfs: 'NTFS', fat32: 'FAT32', ex_fat: 'exFAT', re_fs: 'ReFS',
  };

  const freeLetters = $derived(
    'DEFGHIJKLMNOPQRSTUVWXYZ'
      .split('')
      .filter((l) => !(usedMask & (1 << (l.charCodeAt(0) - 65)))),
  );

  /** S for the system partition, R for recovery — what the guides use — when
   *  free; otherwise the last free letter, out of the way of new drives. */
  function suggest(v: HiddenVolume): string {
    const wish = v.kind === 'esp' ? 'S' : v.kind === 'winre' ? 'R' : '';
    if (wish && freeLetters.includes(wish)) return wish;
    return freeLetters[freeLetters.length - 1] ?? '';
  }

  function title(v: HiddenVolume): string {
    const kind =
      v.kind === 'esp'
        ? m.tools_letters_kind_esp()
        : v.kind === 'winre'
          ? m.tools_letters_kind_winre()
          : m.tools_letters_kind_other();
    return v.label ? `${kind} «${v.label}»` : kind;
  }

  function adopt(list: HiddenVolume[]) {
    volumes = list;
    const next: Record<string, string> = {};
    for (const v of list) {
      if (!v.letter) next[v.device] = picked[v.device] && freeLetters.includes(picked[v.device])
        ? picked[v.device]
        : suggest(v);
    }
    picked = next;
  }

  async function load() {
    loading = true;
    errMsg = null;
    try {
      usedMask = await api.driveSignature();
      adopt(await api.listHiddenVolumes());
    } catch (e) {
      errMsg = String(e);
    } finally {
      loading = false;
    }
  }

  async function change(v: HiddenVolume, letter: string | null) {
    busy = v.device;
    okMsg = null;
    errMsg = null;
    const shown = letter ?? v.letter ?? '';
    try {
      const list = await api.setVolumeLetter(v.device, letter);
      usedMask = await api.driveSignature();
      adopt(list);
      okMsg = letter
        ? () => m.tools_letters_assigned({ letter: shown })
        : () => m.tools_letters_removed({ letter: shown });
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
      <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M8 15V9l4 4 4-4v6" /></svg>
    </span>
    <div class="grow">
      <div class="tool-title">{m.tools_letters_title()}</div>
      <div class="tool-desc">{m.tools_letters_desc()}</div>
    </div>
    <button class="btn outline" onclick={load} disabled={loading || busy !== null}>
      {m.tools_letters_refresh()}
    </button>
  </div>

  {#if loading}
    <div class="muted">{m.tools_letters_loading()}</div>
  {:else if volumes.length === 0 && !errMsg}
    <div class="muted">{m.tools_letters_none()}</div>
  {:else}
    <div class="list">
      {#each volumes as v (v.device)}
        <div class="item">
          <div class="what">
            <div class="name">
              {#if v.letter}<span class="letter">{v.letter}:</span>{/if}
              {title(v)}
            </div>
            <div class="where">
              {m.tools_letters_where({
                disk: v.disk_number,
                model: v.disk_model.trim(),
                part: v.part_index,
                fs: FS[v.filesystem] ?? v.filesystem,
                size: formatBytes(v.size_bytes),
              })}
            </div>
          </div>
          {#if v.letter}
            <button class="btn outline" onclick={() => change(v, null)} disabled={busy !== null}>
              {m.tools_letters_remove()}
            </button>
          {:else if freeLetters.length === 0}
            <span class="muted">{m.tools_letters_no_free()}</span>
          {:else}
            <select
              bind:value={picked[v.device]}
              disabled={busy !== null}
              aria-label={m.tools_letters_letter_label()}
            >
              {#each freeLetters as l}<option value={l}>{l}:</option>{/each}
            </select>
            <button
              class="btn primary"
              onclick={() => change(v, picked[v.device])}
              disabled={busy !== null || !picked[v.device]}
            >
              {m.tools_letters_assign()}
            </button>
          {/if}
        </div>
      {/each}
    </div>
    <div class="note">{m.tools_letters_note()}</div>
  {/if}
  {#if okMsg}<div class="banner ok">{okMsg()}</div>{/if}
  {#if errMsg}<div class="banner err">{errMsg}</div>{/if}
</div>

<style>
  /* Svelte scopes styles per component: the card classes are repeated here,
     matching Mount and Verify in Tools (see BootMediaCard). */
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
  .letter { display: inline-block; min-width: 24px; margin-right: 6px; color: var(--accent-text); }
  .where { font-size: 12px; color: var(--text-secondary); margin-top: 2px; }
  .muted { font-size: 13px; color: var(--text-muted); }
  .note { font-size: 12px; color: var(--text-secondary); margin-top: 10px; line-height: 1.5; }

  select { padding: 8px 10px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 13px; font-family: var(--font); background: var(--input-bg); color: var(--text); }

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
