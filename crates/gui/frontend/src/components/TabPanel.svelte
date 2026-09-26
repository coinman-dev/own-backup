<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Tab {
    id: string;
    label: string;
  }

  interface Props {
    tabs: Tab[];
    activeTab: string;
    onTabChange: (tabId: string) => void;
    children: Snippet;
  }

  let { tabs, activeTab, onTabChange, children }: Props = $props();
</script>

<div class="tab-panel">
  <div class="tab-headers">
    {#each tabs as tab}
      <button
        class="tab-header"
        class:active={activeTab === tab.id}
        onclick={() => onTabChange(tab.id)}
      >
        {tab.label}
      </button>
    {/each}
  </div>

  <div class="tab-content">
    {@render children()}
  </div>
</div>

<style>
  .tab-panel {
    width: 100%;
  }

  .tab-headers {
    display: flex;
    gap: 0;
    border-bottom: 2px solid var(--border);
    margin-bottom: 20px;
  }

  .tab-header {
    padding: 10px 20px;
    background: none;
    border: none;
    font-size: 14px;
    font-weight: 500;
    color: var(--text-muted);
    border-bottom: 2px solid transparent;
    margin-bottom: -2px;
    transition: color 0.12s ease, border-color 0.12s ease;
  }

  .tab-header:hover {
    color: var(--text-secondary);
  }

  .tab-header.active {
    color: var(--accent-text);
    border-bottom-color: var(--primary);
  }

  .tab-content {
    min-height: 0;
  }
</style>
