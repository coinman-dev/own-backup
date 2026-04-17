<script lang="ts">
  import StatusBadge from '../components/StatusBadge.svelte';
  import Modal from '../components/Modal.svelte';
  import { api } from '../lib/api';
  import { formatBytes, formatDate } from '../lib/format';
  import type { BackupJob, BackupSet, DiskInfo } from '../lib/types';

  let jobs: BackupJob[] = $state([]);
  let disks: DiskInfo[] = $state([]);
  let selectedJobId: string = $state('');
  let sets: BackupSet[] = $state([]);
  let selectedSetId: string = $state('');
  let isLoading = $state(true);
  let isSetsLoading = $state(false);
  let error: string | null = $state(null);

  // Restore modal
  let showRestoreModal = $state(false);
  let restoreTarget = $state('');
  let restorePassword = $state('');
  let isRestoring = $state(false);

  async function loadData() {
    isLoading = true;
    error = null;
    try {
      const [jobsResult, disksResult] = await Promise.allSettled([
        api.listJobs(),
        api.listDisks(),
      ]);
      jobs = jobsResult.status === 'fulfilled' ? jobsResult.value : [];
      disks = disksResult.status === 'fulfilled' ? disksResult.value : [];
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    loadData();
  });

  $effect(() => {
    if (selectedJobId) {
      loadSets(selectedJobId);
    } else {
      sets = [];
      selectedSetId = '';
    }
  });

  async function loadSets(jobId: string) {
    isSetsLoading = true;
    try {
      const result = await api.listSets(jobId);
      sets = result
        .filter((s) => s.status === 'completed')
        .sort((a, b) => {
          const aDate = a.completed_at ?? '';
          const bDate = b.completed_at ?? '';
          return bDate > aDate ? 1 : bDate < aDate ? -1 : 0;
        });
      selectedSetId = sets.length > 0 ? sets[0].id : '';
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
      sets = [];
    } finally {
      isSetsLoading = false;
    }
  }

  let selectedSet = $derived(sets.find((s) => s.id === selectedSetId) ?? null);

  let selectedJob = $derived(jobs.find((j) => j.id === selectedJobId) ?? null);

  function openRestoreModal() {
    if (!selectedSet) return;
    restoreTarget = '';
    restorePassword = '';
    showRestoreModal = true;
  }

  async function handleRestore() {
    if (!selectedSetId || !restoreTarget) return;
    isRestoring = true;
    try {
      await api.startRestore(selectedSetId, restoreTarget, restorePassword);
      showRestoreModal = false;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isRestoring = false;
    }
  }
</script>

<div class="restore-view">
  <div class="page-header">
    <h1 class="page-title">Restore</h1>
  </div>

  {#if error}
    <div class="error-banner">
      <span>{error}</span>
      <button class="dismiss-btn" onclick={() => (error = null)}>Dismiss</button>
    </div>
  {/if}

  {#if isLoading}
    <div class="loading-state">
      <div class="spinner"></div>
      <p>Loading restore options...</p>
    </div>
  {:else}
    <div class="restore-layout">
      <!-- Left: Selection -->
      <div class="card selection-panel">
        <h2 class="panel-title">Select Backup</h2>

        <div class="form-group">
          <label for="restore-job">Backup Job</label>
          <select id="restore-job" class="form-select" bind:value={selectedJobId}>
            <option value="">Choose a job...</option>
            {#each jobs as job}
              <option value={job.id}>{job.name}</option>
            {/each}
          </select>
        </div>

        {#if selectedJobId}
          <div class="form-group">
            <label for="restore-version">Version</label>
            {#if isSetsLoading}
              <p class="text-muted">Loading versions...</p>
            {:else if sets.length === 0}
              <p class="text-muted">No completed backups found.</p>
            {:else}
              <div class="version-list">
                {#each sets as set (set.id)}
                  <button
                    class="version-item"
                    class:selected={selectedSetId === set.id}
                    onclick={() => (selectedSetId = set.id)}
                  >
                    <div class="version-info">
                      <span class="version-date">
                        {set.completed_at ? formatDate(set.completed_at) : '---'}
                      </span>
                      <span class="version-meta">
                        {set.backup_type} | {set.file_size != null ? formatBytes(set.file_size) : '---'}
                      </span>
                    </div>
                    <StatusBadge status="success" />
                  </button>
                {/each}
              </div>
            {/if}
          </div>
        {/if}
      </div>

      <!-- Right: Details + Action -->
      <div class="details-panel">
        {#if selectedSet && selectedJob}
          <div class="card">
            <h2 class="panel-title">Backup Details</h2>
            <div class="detail-grid">
              <div class="detail-item">
                <span class="detail-label">Job</span>
                <span class="detail-value">{selectedJob.name}</span>
              </div>
              <div class="detail-item">
                <span class="detail-label">Type</span>
                <span class="detail-value">{selectedSet.backup_type}</span>
              </div>
              <div class="detail-item">
                <span class="detail-label">Source</span>
                <span class="detail-value">{selectedJob.source_device}</span>
              </div>
              <div class="detail-item">
                <span class="detail-label">Source Size</span>
                <span class="detail-value">{formatBytes(selectedSet.source_size)}</span>
              </div>
              <div class="detail-item">
                <span class="detail-label">Backup Size</span>
                <span class="detail-value">
                  {selectedSet.file_size != null ? formatBytes(selectedSet.file_size) : '---'}
                </span>
              </div>
              <div class="detail-item">
                <span class="detail-label">Blocks</span>
                <span class="detail-value">
                  {selectedSet.block_count.toLocaleString()}
                  {#if selectedSet.changed_block_count != null}
                    ({selectedSet.changed_block_count.toLocaleString()} changed)
                  {/if}
                </span>
              </div>
              <div class="detail-item">
                <span class="detail-label">File</span>
                <span class="detail-value file-path">{selectedSet.file_path}</span>
              </div>
              {#if selectedSet.checksum}
                <div class="detail-item">
                  <span class="detail-label">Checksum</span>
                  <span class="detail-value file-path">{selectedSet.checksum}</span>
                </div>
              {/if}
              <div class="detail-item">
                <span class="detail-label">Encrypted</span>
                <span class="detail-value">{selectedJob.encryption_enabled ? 'Yes (AES-256-GCM)' : 'No'}</span>
              </div>
            </div>

            <div class="action-row">
              <button class="btn btn-primary" onclick={openRestoreModal}>
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
                  <path d="M4 4v5h5M20 20v-5h-5M4 9a8 8 0 0 1 14-4M20 15a8 8 0 0 1-14 4" />
                </svg>
                Restore This Backup
              </button>
            </div>
          </div>
        {:else}
          <div class="card empty-details">
            <div class="empty-icon">
              <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1" stroke-linecap="round">
                <path d="M4 4v5h5M20 20v-5h-5M4 9a8 8 0 0 1 14-4M20 15a8 8 0 0 1-14 4" />
              </svg>
            </div>
            <p class="text-muted">Select a backup job and version to view details.</p>
          </div>
        {/if}
      </div>
    </div>
  {/if}
</div>

<Modal
  open={showRestoreModal}
  title="Restore Backup"
  onClose={() => (showRestoreModal = false)}
  onConfirm={handleRestore}
  confirmLabel={isRestoring ? 'Restoring...' : 'Start Restore'}
  danger
>
  <p style="margin-bottom: 16px; color: var(--text-secondary);">
    This will overwrite the target device with the backup data.
    Make sure you have selected the correct target.
  </p>

  <div class="form-group">
    <label for="restore-target">Target Device</label>
    <select id="restore-target" class="form-select" bind:value={restoreTarget}>
      <option value="">Select target disk...</option>
      {#each disks as disk}
        <option value={disk.device_id}>
          {disk.model || disk.device_id} ({formatBytes(disk.size_bytes)})
        </option>
      {/each}
    </select>
  </div>

  {#if selectedJob?.encryption_enabled}
    <div class="form-group">
      <label for="restore-password">Encryption Password</label>
      <input
        id="restore-password"
        class="form-input"
        type="password"
        placeholder="Enter backup password"
        bind:value={restorePassword}
      />
    </div>
  {/if}
</Modal>

<style>
  .restore-view {
    padding: 28px 32px;
    height: 100%;
    overflow-y: auto;
  }

  .page-header {
    margin-bottom: 24px;
  }

  .page-title {
    font-size: 24px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
  }

  .error-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    background: rgba(234, 67, 53, 0.08);
    border: 1px solid rgba(234, 67, 53, 0.2);
    border-radius: var(--radius);
    color: var(--error);
    margin-bottom: 20px;
    font-size: 13px;
  }

  .dismiss-btn {
    margin-left: auto;
    background: none;
    border: none;
    color: var(--error);
    font-size: 12px;
    font-weight: 500;
    text-decoration: underline;
    cursor: pointer;
  }

  .loading-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 80px 20px;
    color: var(--text-muted);
  }

  .spinner {
    width: 36px;
    height: 36px;
    border: 3px solid var(--border);
    border-top-color: var(--primary);
    border-radius: 50%;
    animation: spin 0.8s linear infinite;
    margin-bottom: 16px;
  }

  .restore-layout {
    display: grid;
    grid-template-columns: 380px 1fr;
    gap: 24px;
    align-items: start;
  }

  .panel-title {
    font-size: 16px;
    font-weight: 600;
    color: var(--text);
    margin: 0 0 16px;
  }

  .version-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-height: 360px;
    overflow-y: auto;
  }

  .version-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 12px;
    background: var(--content-bg);
    border: 2px solid transparent;
    border-radius: 6px;
    cursor: pointer;
    text-align: left;
    transition: border-color 0.12s ease;
    width: 100%;
  }

  .version-item:hover {
    border-color: var(--border);
  }

  .version-item.selected {
    border-color: var(--primary);
    background: rgba(45, 125, 210, 0.06);
  }

  .version-info {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .version-date {
    font-size: 13px;
    font-weight: 500;
    color: var(--text);
  }

  .version-meta {
    font-size: 11px;
    color: var(--text-muted);
    text-transform: uppercase;
  }

  .detail-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 14px;
    margin-bottom: 20px;
  }

  .detail-item {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .detail-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.3px;
    color: var(--text-muted);
  }

  .detail-value {
    font-size: 14px;
    color: var(--text);
  }

  .file-path {
    font-family: 'Consolas', 'Monaco', monospace;
    font-size: 12px;
    word-break: break-all;
  }

  .action-row {
    display: flex;
    justify-content: flex-end;
    padding-top: 8px;
    border-top: 1px solid var(--border);
  }

  .empty-details {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    min-height: 300px;
    text-align: center;
  }

  .empty-icon {
    margin-bottom: 16px;
    opacity: 0.4;
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
