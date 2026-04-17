<script lang="ts">
  import StatusBadge from './StatusBadge.svelte';
  import ProgressBar from './ProgressBar.svelte';
  import { formatBytes, formatDate } from '../lib/format';
  import type { BackupJob, BackupProgress } from '../lib/types';

  interface Props {
    job: BackupJob;
    lastBackupDate?: string | null;
    lastBackupStatus?: 'success' | 'warning' | 'error' | 'running' | 'idle';
    nextBackup?: string | null;
    progress?: BackupProgress | null;
    onClick?: () => void;
    onBackupNow?: () => void;
  }

  let {
    job,
    lastBackupDate = null,
    lastBackupStatus = 'idle',
    nextBackup = null,
    progress = null,
    onClick,
    onBackupNow,
  }: Props = $props();

  let isRunning = $derived(progress != null && progress.status === 'running');

  let progressPercent = $derived(
    progress && progress.blocks_total > 0
      ? (progress.blocks_done / progress.blocks_total) * 100
      : 0,
  );

  function sourceLabel(sourceType: string): string {
    switch (sourceType) {
      case 'full_disk': return 'Entire Disk';
      case 'partition': return 'Partition';
      case 'volume': return 'Volume';
      default: return sourceType;
    }
  }
</script>

<div
  class="disk-card"
  class:running={isRunning}
  role="button"
  tabindex="0"
  onclick={onClick}
  onkeydown={(e) => { if (e.key === 'Enter' || e.key === ' ') onClick?.(); }}
>
  <div class="card-header">
    <div class="disk-icon">
      <svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
        <rect x="3" y="4" width="18" height="16" rx="2" />
        <circle cx="12" cy="12" r="4" />
        <circle cx="12" cy="12" r="1" />
        <line x1="3" y1="18" x2="21" y2="18" />
        <circle cx="7" cy="20" r="0.5" fill="var(--primary)" />
        <circle cx="17" cy="20" r="0.5" fill="var(--primary)" />
      </svg>
    </div>
    <div class="card-title-area">
      <h3 class="job-name">{job.name}</h3>
      <span class="source-type">{sourceLabel(job.source_type)}</span>
    </div>
    <div class="card-status">
      <StatusBadge status={lastBackupStatus} />
    </div>
  </div>

  <div class="card-details">
    <div class="detail-row">
      <span class="detail-label">Source</span>
      <span class="detail-value">{job.source_device}</span>
    </div>
    <div class="detail-row">
      <span class="detail-label">Target</span>
      <span class="detail-value target-path">{job.target_path}</span>
    </div>
    {#if lastBackupDate}
      <div class="detail-row">
        <span class="detail-label">Last backup</span>
        <span class="detail-value">{formatDate(lastBackupDate)}</span>
      </div>
    {/if}
    {#if nextBackup}
      <div class="detail-row">
        <span class="detail-label">Next backup</span>
        <span class="detail-value">{nextBackup}</span>
      </div>
    {/if}
    {#if job.encryption_enabled}
      <div class="detail-row">
        <span class="detail-label">Encryption</span>
        <span class="detail-value">AES-256-GCM</span>
      </div>
    {/if}
  </div>

  {#if isRunning && progress}
    <div class="card-progress">
      <ProgressBar
        percent={progressPercent}
        speed="{progress.speed_mbps.toFixed(1)} MB/s"
        eta={progress.eta}
      />
    </div>
  {/if}

  <div class="card-actions">
    <button
      class="btn btn-primary btn-sm"
      onclick={(e: MouseEvent) => { e.stopPropagation(); onBackupNow?.(); }}
      disabled={isRunning}
    >
      {#if isRunning}
        Running...
      {:else}
        Back up now
      {/if}
    </button>
  </div>
</div>

<style>
  .disk-card {
    background: var(--card-bg);
    border-radius: var(--radius);
    box-shadow: var(--shadow);
    padding: 20px;
    cursor: pointer;
    transition: box-shadow 0.15s ease, transform 0.1s ease;
    border: 1px solid transparent;
  }

  .disk-card:hover {
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
    transform: translateY(-1px);
  }

  .disk-card:focus-visible {
    outline: 2px solid var(--primary);
    outline-offset: 2px;
  }

  .disk-card.running {
    border-color: rgba(45, 125, 210, 0.3);
  }

  .card-header {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    margin-bottom: 16px;
  }

  .disk-icon {
    flex-shrink: 0;
    width: 44px;
    height: 44px;
    background: rgba(45, 125, 210, 0.08);
    border-radius: 10px;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .card-title-area {
    flex: 1;
    min-width: 0;
  }

  .job-name {
    font-size: 16px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
    line-height: 1.3;
  }

  .source-type {
    font-size: 12px;
    color: var(--text-muted);
  }

  .card-status {
    flex-shrink: 0;
  }

  .card-details {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 16px;
  }

  .detail-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 12px;
  }

  .detail-label {
    font-size: 12px;
    color: var(--text-muted);
    white-space: nowrap;
  }

  .detail-value {
    font-size: 13px;
    color: var(--text-secondary);
    text-align: right;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .target-path {
    font-family: 'Consolas', 'Monaco', monospace;
    font-size: 12px;
  }

  .card-progress {
    margin-bottom: 14px;
  }

  .card-actions {
    display: flex;
    justify-content: flex-end;
  }

  .btn-sm {
    padding: 6px 14px;
    font-size: 13px;
  }
</style>
