<script lang="ts">
  import QuickBackup from './QuickBackup.svelte';
  import Activity from './Activity.svelte';
  import Restore from './Restore.svelte';
  import type { BackupTab, ViewName } from '../lib/types';

  interface Props {
    onNavigate: (view: ViewName, jobId?: string) => void;
  }
  let { onNavigate }: Props = $props();

  let tab: BackupTab = $state('copy');

  const tabs: { id: BackupTab; label: string }[] = [
    { id: 'copy', label: 'Резервное копирование' },
    { id: 'activity', label: 'Активность' },
    { id: 'recovery', label: 'Восстановление' },
  ];
</script>

<div class="section">
  <div class="tab-bar">
    {#each tabs as t}
      <button class="tab" class:active={tab === t.id} onclick={() => (tab = t.id)}>
        {t.label}
      </button>
    {/each}
  </div>

  <div class="tab-body">
    {#if tab === 'copy'}
      <QuickBackup {onNavigate} />
    {:else if tab === 'activity'}
      <Activity />
    {:else}
      <Restore />
    {/if}
  </div>
</div>

<style>
  .section {
    height: 100%;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }

  .tab-bar {
    flex-shrink: 0;
    display: flex;
    padding: 22px 34px 0;
    border-bottom: 1px solid var(--border);
    background: var(--content-bg);
  }

  .tab {
    padding: 0 2px 14px;
    margin-right: 34px;
    background: none;
    border: none;
    border-bottom: 3px solid transparent;
    color: var(--text-muted);
    font-size: 15px;
    font-weight: 600;
    font-family: var(--font);
    cursor: pointer;
  }

  .tab.active {
    color: var(--head);
    border-bottom-color: var(--primary);
  }

  .tab-body {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
</style>
