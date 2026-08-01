<script lang="ts">
  // "Create a boot medium" — one card, one tab per base system.
  //
  // The card asks nothing: what goes in, which drivers and where the ISO lands
  // are the build's questions, and they belong in the dialog that opens when a
  // base is chosen, not spread across the Tools page.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import MediaBuildModal from './MediaBuildModal.svelte';
  import type { BaseImage } from '../lib/types';

  let bases: BaseImage[] = $state([]);
  let searching = $state(true);
  let searchError: string | null = $state(null);
  let tab: 'windows' | 'linux' = $state('windows');
  let open: 'windows' | 'linux' | null = $state(null);

  const windowsBase = $derived(bases[0] ?? null);

  onMount(search);

  async function search() {
    searching = true;
    searchError = null;
    try {
      bases = await api.findBaseImages();
    } catch (e) {
      searchError = String(e);
    } finally {
      searching = false;
    }
  }

  function openDialog() {
    if (tab === 'windows' && !windowsBase) return; // nothing to build on
    open = tab;
  }
</script>

<div class="tool-card">
  <div class="tool-head">
    <span class="glyph">💿</span>
    <div>
      <div class="tool-title">Загрузочный носитель</div>
      <div class="tool-desc">
        Носитель, с которого можно снять копию выключенной системы и
        восстановить её обратно — включая тот диск, с которого система
        загружается. Результат — файл ISO: записывается на флешку или
        подключается приводом в виртуальной машине.
      </div>
    </div>
  </div>

  <div class="tabs" role="tablist">
    <button class="tab" class:on={tab === 'windows'} role="tab"
            aria-selected={tab === 'windows'} onclick={() => (tab = 'windows')}>
      <svg class="ico" viewBox="0 0 24 24" aria-hidden="true">
        <path fill="currentColor" d="M3 5.4 10.4 4.4v7.1H3zM11.4 4.2 21 3v8.5h-9.6zM3 12.5h7.4v7.1L3 18.6zM11.4 12.5H21V21l-9.6-1.2z"/>
      </svg>
      <span>На основе Microsoft Windows Recovery Environment (x64)</span>
    </button>
    <button class="tab" class:on={tab === 'linux'} role="tab"
            aria-selected={tab === 'linux'} onclick={() => (tab = 'linux')}>
      <svg class="ico" viewBox="0 0 24 24" aria-hidden="true">
        <path fill="currentColor" d="M12 2c2.4 0 3.6 1.9 3.6 4.3 0 1.5-.3 2.3.4 3.4 1 1.5 2.4 3.2 2.9 5 .4 1.6-.1 2.7-.9 3.2.2.9.1 1.9-.6 2.4-.9.7-2.4.4-3.4-.4-.7.2-1.4.3-2 .3s-1.3-.1-2-.3c-1 .8-2.5 1.1-3.4.4-.7-.5-.8-1.5-.6-2.4-.8-.5-1.3-1.6-.9-3.2.5-1.8 1.9-3.5 2.9-5 .7-1.1.4-1.9.4-3.4C8.4 3.9 9.6 2 12 2m-1.6 3.5a.9 1.1 0 1 0 0 2.2.9 1.1 0 0 0 0-2.2m3.2 0a.9 1.1 0 1 0 0 2.2.9 1.1 0 0 0 0-2.2M12 8.4c-.7 0-1.7.5-1.7.9s1 .9 1.7.9 1.7-.5 1.7-.9-1-.9-1.7-.9"/>
      </svg>
      <span>На основе Linux (Ubuntu / Debian) x64</span>
    </button>
  </div>

  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="panel" class:disabled={tab === 'windows' && !windowsBase}
       onclick={openDialog} role="button" tabindex="0">
    {#if tab === 'windows'}
      <div class="p-desc">
        Собирается из того, что уже есть на этой машине: среда восстановления
        Windows, программа с интерфейсом, рантайм WebView2 и, по желанию,
        драйверы этого компьютера. Родные NTFS и ReFS, снимок VSS не нужен —
        система на копируемом диске выключена.
      </div>
      {#if searching}
        <div class="p-meta">поиск образа восстановления…</div>
      {:else if searchError}
        <div class="p-warn">{searchError}</div>
      {:else if windowsBase}
        <div class="p-meta">
          {windowsBase.name} · сборка {windowsBase.build} · {windowsBase.language} ·
          {formatBytes(windowsBase.total_bytes)}
        </div>
        {#each windowsBase.warnings as w}<div class="p-warn">{w}</div>{/each}
        <div class="p-go">Нажмите, чтобы настроить и собрать →</div>
      {:else}
        <div class="p-warn">
          Образ среды восстановления не найден. Включите её командой
          <code>reagentc /enable</code> или возьмите Winre.wim из установочного
          образа Windows.
        </div>
      {/if}
    {:else}
      <div class="p-desc">
        Минимальная система с ядром, драйверами дисков и сети и той же
        программой. Не требует Windows для сборки и читает ext4, btrfs, XFS и
        LUKS напрямую — для машин без Windows и для дисков, чью разметку удобнее
        разбирать из Linux.
      </div>
      <div class="p-meta soon">в разработке — пока только описание</div>
      <div class="p-go">Нажмите, чтобы посмотреть, что войдёт →</div>
    {/if}
  </div>
</div>

{#if open}
  <MediaBuildModal
    kind={open}
    base={open === 'windows' ? windowsBase : null}
    onClose={() => (open = null)}
  />
{/if}

<style>
  .tabs { display: flex; gap: 4px; margin-top: 4px; border-bottom: 1px solid var(--border); }
  .tab { display: flex; align-items: center; gap: 8px; padding: 9px 14px; border: 1px solid transparent; border-bottom: none; border-radius: 6px 6px 0 0; background: none; cursor: pointer; font-family: var(--font); font-size: 13px; color: var(--text-muted); margin-bottom: -1px; }
  .tab:hover { color: var(--head); }
  .tab.on { color: var(--head); border-color: var(--border); background: var(--card-bg); }
  .ico { width: 16px; height: 16px; flex-shrink: 0; }
  .panel { padding: 14px 16px; border: 1px solid var(--border); border-top: none; border-radius: 0 0 6px 6px; cursor: pointer; }
  .panel:hover:not(.disabled) { border-color: var(--primary); }
  .panel.disabled { cursor: default; }
  .p-desc { font-size: 12px; color: var(--text-muted); line-height: 1.6; }
  .p-meta { font-size: 12px; color: var(--text-muted); margin-top: 8px; }
  .p-meta.soon { color: var(--warning); }
  .p-warn { font-size: 12px; color: var(--warning); margin-top: 8px; line-height: 1.45; }
  .p-warn code { font-family: monospace; }
  .p-go { font-size: 12px; color: var(--primary); margin-top: 10px; }
</style>
