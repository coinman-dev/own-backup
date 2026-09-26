<script lang="ts">
  // "Create a boot medium" — one card, one tab per base system.
  //
  // The card asks nothing: what goes in, which drivers and where the ISO lands
  // are the build's questions, and they belong in the dialog that opens when a
  // base is chosen, not spread across the Tools page.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import { media, ensureMediaMode } from '../lib/mediaStore.svelte';
  import { formatBytes } from '../lib/format';
  import MediaBuildModal from './MediaBuildModal.svelte';
  import type { BaseImage } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let bases: BaseImage[] = $state([]);
  let searching = $state(true);
  let searchError: string | null = $state(null);
  let tab: 'windows' | 'linux' = $state('windows');
  let open: 'windows' | 'linux' | null = $state(null);

  const windowsBase = $derived(bases[0] ?? null);

  onMount(async () => {
    await ensureMediaMode();
    // Nothing to look for when running from the medium: it cannot build one.
    if (!media.active) search();
    else searching = false;
  });

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
    if (media.active) return;
    if (tab === 'windows' && !windowsBase) return; // nothing to build on
    open = tab;
  }
</script>

{#snippet warnIcon()}
  <svg class="p-warn-icon" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--warning)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5l9 16H3z" /><path d="M12 10v4M12 17h.01" /></svg>
{/snippet}

<div class="tool-card">
  <div class="tool-head">
    <span class="glyph">
      <svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="8.5" /><circle cx="12" cy="12" r="2.5" /></svg>
    </span>
    <div>
      <div class="tool-title">{m.bmc_title()}</div>
      <div class="tool-desc">{m.bmc_desc()}</div>
    </div>
  </div>

  <div class="tabs" role="tablist">
    <button class="tab" class:on={tab === 'windows'} role="tab"
            aria-selected={tab === 'windows'} onclick={() => (tab = 'windows')}>
      <svg class="ico" viewBox="0 0 24 24" aria-hidden="true">
        <path fill="currentColor" d="M3 5.4 10.4 4.4v7.1H3zM11.4 4.2 21 3v8.5h-9.6zM3 12.5h7.4v7.1L3 18.6zM11.4 12.5H21V21l-9.6-1.2z"/>
      </svg>
      <span>{m.bmc_tab_windows()}</span>
    </button>
    <button class="tab" class:on={tab === 'linux'} role="tab"
            aria-selected={tab === 'linux'} onclick={() => (tab = 'linux')}>
      <svg class="ico" viewBox="0 0 24 24" aria-hidden="true">
        <path fill="currentColor" d="M12 2c2.4 0 3.6 1.9 3.6 4.3 0 1.5-.3 2.3.4 3.4 1 1.5 2.4 3.2 2.9 5 .4 1.6-.1 2.7-.9 3.2.2.9.1 1.9-.6 2.4-.9.7-2.4.4-3.4-.4-.7.2-1.4.3-2 .3s-1.3-.1-2-.3c-1 .8-2.5 1.1-3.4.4-.7-.5-.8-1.5-.6-2.4-.8-.5-1.3-1.6-.9-3.2.5-1.8 1.9-3.5 2.9-5 .7-1.1.4-1.9.4-3.4C8.4 3.9 9.6 2 12 2m-1.6 3.5a.9 1.1 0 1 0 0 2.2.9 1.1 0 0 0 0-2.2m3.2 0a.9 1.1 0 1 0 0 2.2.9 1.1 0 0 0 0-2.2M12 8.4c-.7 0-1.7.5-1.7.9s1 .9 1.7.9 1.7-.5 1.7-.9-1-.9-1.7-.9"/>
      </svg>
      <span>{m.bmc_tab_linux()}</span>
    </button>
  </div>

  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="panel" class:disabled={media.active || (tab === 'windows' && !windowsBase)}
       onclick={openDialog} role="button" tabindex="0">
    {#if media.active}
      <!-- Building from the medium failed at DISM halfway through, which
           explains nothing. Say so before, and say why. -->
      <div class="p-desc">
        {m.bmc_on_media()} <code>X:</code> {m.bmc_on_media_ramdisk()}
      </div>
      <div class="p-meta soon">{m.bmc_on_media_hint()}</div>
    {:else if tab === 'windows'}
      <div class="p-desc">{m.bmc_windows_desc()}</div>
      {#if searching}
        <div class="p-meta">{m.bmc_searching()}</div>
      {:else if searchError}
        <div class="p-warn">{@render warnIcon()}<span>{searchError}</span></div>
      {:else if windowsBase}
        <div class="p-meta">
          {windowsBase.name} · {m.bmc_build({ build: windowsBase.build })} · {windowsBase.language} ·
          {formatBytes(windowsBase.total_bytes)}
        </div>
        {#each windowsBase.warnings as w}<div class="p-warn">{@render warnIcon()}<span>{w}</span></div>{/each}
        <div class="p-go">{m.bmc_click_to_build()}</div>
      {:else}
        <div class="p-warn">
          {@render warnIcon()}
          <span>
            {m.bmc_no_winre()} <code>reagentc /enable</code> {m.bmc_no_winre_or()}
          </span>
        </div>
      {/if}
    {:else}
      <div class="p-desc">{m.bmc_linux_desc()}</div>
      <div class="p-meta soon">{m.bmc_linux_soon()}</div>
      <div class="p-go">{m.bmc_click_to_see()}</div>
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
  /* Svelte scopes styles per component, so the card classes defined in Tools
     never reached this one — the heading and the icon sat on the page with no
     frame around them. They live here now, matching Mount and Verify. */
  .tool-card { border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); padding: 20px 22px; margin-bottom: 16px; }
  .tool-head { display: flex; gap: 14px; align-items: flex-start; margin-bottom: 14px; }
  /* The tool's icon on a tinted square, as in the design. */
  .glyph { flex: none; width: 40px; height: 40px; border-radius: 8px; background: var(--accent-soft); color: var(--accent-text); display: flex; align-items: center; justify-content: center; }
  .tool-title { font-size: 16px; font-weight: 600; color: var(--head); }
  .tool-desc { font-size: 12px; color: var(--text-secondary); margin-top: 3px; line-height: 1.5; }

  /* The chosen tab and its panel are one raised surface (--panel2); the line
     under the strip stops where the chosen tab stands on it. */
  .tabs { display: flex; gap: 4px; margin-top: 4px; border-bottom: 1px solid var(--border); }
  .tab { display: flex; align-items: center; gap: 8px; padding: 9px 14px; border: 1px solid transparent; border-bottom: none; border-radius: 8px 8px 0 0; background: none; cursor: pointer; font-family: var(--font); font-size: 13px; color: var(--text-muted); margin-bottom: -1px; }
  .tab:hover { color: var(--head); }
  .tab.on { color: var(--head); border-color: var(--border); background: var(--panel2); }
  .ico { width: 16px; height: 16px; flex-shrink: 0; }
  .panel { padding: 14px 16px; border: 1px solid var(--border); border-top: none; border-radius: 0 0 8px 8px; background: var(--panel2); cursor: pointer; }
  .panel:hover:not(.disabled) { border-color: var(--primary); }
  .panel.disabled { cursor: default; }
  .p-desc { font-size: 12px; color: var(--text-secondary); line-height: 1.6; }
  .p-meta { font-size: 12px; font-weight: 600; color: var(--head); margin-top: 8px; }
  .p-meta.soon { font-weight: 400; color: var(--warning-text); }
  .p-warn { display: flex; align-items: flex-start; gap: 10px; font-size: 12px; margin-top: 8px; line-height: 1.45; padding: 10px 12px; border-radius: 8px; background: var(--warning-soft); border: 1px solid var(--warning-line); color: var(--warning-text); }
  .p-warn-icon { flex: none; margin-top: 1px; }
  .p-warn code, .p-desc code { font-family: monospace; }
  .p-go { font-size: 12px; font-weight: 600; color: var(--accent-text); margin-top: 10px; }
</style>
