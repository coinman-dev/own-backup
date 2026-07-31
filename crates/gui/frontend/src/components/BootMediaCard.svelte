<script lang="ts">
  // "Create a boot medium" — a frame holding one clickable card per base system.
  //
  // The card itself asks nothing: what to include, where to put it and which
  // drivers to take are the build's questions, and they belong in the dialog
  // that opens on the choice, not spread across the Tools page.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import MediaBuildModal from './MediaBuildModal.svelte';
  import type { BaseImage } from '../lib/types';

  let bases: BaseImage[] = $state([]);
  let searching = $state(true);
  let searchError: string | null = $state(null);
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
</script>

<div class="tool-card">
  <div class="tool-head">
    <span class="glyph">🚀</span>
    <div>
      <div class="tool-title">Создать загрузочный носитель</div>
      <div class="tool-desc">
        Носитель, с которого можно снять копию выключенной системы и
        восстановить её обратно — включая тот диск, с которого система
        загружается. Выберите, на чём его собрать.
      </div>
    </div>
  </div>

  <!-- Windows -->
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div
    class="pick"
    class:disabled={!windowsBase}
    onclick={() => windowsBase && (open = 'windows')}
    role="button"
    tabindex="0"
  >
    <span class="pick-glyph">🪟</span>
    <div class="pick-body">
      <div class="pick-title">На основе Microsoft Windows Recovery Environment (x64)</div>
      <div class="pick-desc">
        Собирается из того, что уже есть на этой машине: среда восстановления
        Windows, программа с интерфейсом, рантайм WebView2 и, по желанию,
        драйверы этого компьютера. Родные NTFS и ReFS, VSS не нужен — система
        на копируемом диске выключена.
      </div>
      {#if searching}
        <div class="pick-meta">поиск образа восстановления…</div>
      {:else if searchError}
        <div class="pick-warn">{searchError}</div>
      {:else if windowsBase}
        <div class="pick-meta">
          {windowsBase.name} · сборка {windowsBase.build} · {windowsBase.language} ·
          {formatBytes(windowsBase.total_bytes)}
        </div>
        {#each windowsBase.warnings as w}<div class="pick-warn">{w}</div>{/each}
      {:else}
        <div class="pick-warn">
          Образ среды восстановления не найден. Включите её командой
          <code>reagentc /enable</code> или возьмите Winre.wim из установочного
          образа Windows.
        </div>
      {/if}
    </div>
    <span class="chev">›</span>
  </div>

  <!-- Linux -->
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="pick" onclick={() => (open = 'linux')} role="button" tabindex="0">
    <span class="pick-glyph">🐧</span>
    <div class="pick-body">
      <div class="pick-title">На основе Linux (Ubuntu / Debian) x64</div>
      <div class="pick-desc">
        Минимальная система с ядром, драйверами дисков и сети и той же
        программой. Не требует Windows для сборки и читает ext4, btrfs, XFS и
        LUKS напрямую — тем, кому надо снять копию или добраться до данных на
        машине без Windows.
      </div>
      <div class="pick-meta soon">в разработке — пока только описание</div>
    </div>
    <span class="chev">›</span>
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
  .pick { display: flex; gap: 14px; align-items: flex-start; padding: 13px 15px; margin-top: 12px; border: 1px solid var(--border); border-radius: 6px; cursor: pointer; background: var(--card-bg); }
  .pick:hover:not(.disabled) { border-color: var(--primary); }
  .pick.disabled { cursor: default; opacity: .75; }
  .pick-glyph { font-size: 22px; line-height: 1.2; flex-shrink: 0; }
  .pick-body { flex: 1; min-width: 0; }
  .pick-title { font-size: 14px; color: var(--head); margin-bottom: 4px; }
  .pick-desc { font-size: 12px; color: var(--text-muted); line-height: 1.55; }
  .pick-meta { font-size: 12px; color: var(--text-muted); margin-top: 6px; }
  .pick-meta.soon { color: var(--warning); }
  .pick-warn { font-size: 12px; color: var(--warning); margin-top: 6px; line-height: 1.45; }
  .pick-warn code { font-family: monospace; }
  .chev { color: var(--text-muted); font-size: 20px; align-self: center; flex-shrink: 0; }
</style>
