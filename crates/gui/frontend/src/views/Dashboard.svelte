<script lang="ts">
  import DiskCard from '../components/DiskCard.svelte';
  import Modal from '../components/Modal.svelte';
  import { api } from '../lib/api';
  import { formatBytes } from '../lib/format';
  import type { BackupJob, BackupProgress, BackupSet, DiskInfo, ServiceStatus, ViewName } from '../lib/types';

  interface Props {
    onNavigate: (view: ViewName, jobId?: string) => void;
  }

  let { onNavigate }: Props = $props();

  let jobs: BackupJob[] = $state([]);
  let disks: DiskInfo[] = $state([]);
  let serviceStatus: ServiceStatus | null = $state(null);
  let latestSets: Record<string, BackupSet | null> = $state({});
  let activeProgress: Record<string, BackupProgress> = $state({});
  let isLoading = $state(true);
  let error: string | null = $state(null);

  // Add-backup modal
  let showAddModal = $state(false);
  let newJobName = $state('');
  let newJobSourceDevice = $state('');
  let newJobTargetPath = $state('');
  let newJobSourceType = $state('full_disk');

  async function loadData() {
    isLoading = true;
    error = null;
    try {
      const [jobsResult, disksResult, statusResult] = await Promise.allSettled([
        api.listJobs(),
        api.listDisks(),
        api.getServiceStatus(),
      ]);

      jobs = jobsResult.status === 'fulfilled' ? jobsResult.value : [];
      disks = disksResult.status === 'fulfilled' ? disksResult.value : [];
      serviceStatus = statusResult.status === 'fulfilled' ? statusResult.value : null;

      // Load latest set for each job
      const setPromises = jobs.map(async (j) => {
        try {
          const sets = await api.listSets(j.id);
          const completed = sets
            .filter((s) => s.completed_at)
            .sort((a, b) => (b.completed_at! > a.completed_at! ? 1 : -1));
          return [j.id, completed[0] ?? null] as const;
        } catch {
          return [j.id, null] as const;
        }
      });

      const setResults = await Promise.all(setPromises);
      const setsMap: Record<string, BackupSet | null> = {};
      for (const [id, set] of setResults) {
        setsMap[id] = set;
      }
      latestSets = setsMap;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    loadData();
  });

  function getJobStatus(
    job: BackupJob,
  ): 'success' | 'warning' | 'error' | 'running' | 'idle' {
    if (activeProgress[job.id]?.status === 'running') return 'running';
    const latest = latestSets[job.id];
    if (!latest) return 'idle';
    if (latest.status === 'completed') return 'success';
    if (latest.status === 'failed') return 'error';
    return 'idle';
  }

  function openJobSettings(jobId: string) {
    onNavigate('job-settings', jobId);
  }

  async function handleBackupNow(jobId: string) {
    try {
      await api.startBackup(jobId, 'incremental', '');
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function handleCreateJob() {
    if (!newJobName.trim() || !newJobSourceDevice.trim() || !newJobTargetPath.trim()) return;
    try {
      await api.createJob({
        name: newJobName,
        source_type: newJobSourceType,
        source_device: newJobSourceDevice,
        target_path: newJobTargetPath,
      });
      showAddModal = false;
      newJobName = '';
      newJobSourceDevice = '';
      newJobTargetPath = '';
      await loadData();
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="dashboard">
  <div class="page-header">
    <h1 class="page-title">Dashboard</h1>
    {#if serviceStatus?.connected}
      <div class="service-info">
        <span class="service-dot"></span>
        Service running
        <span class="service-detail">
          | {serviceStatus.active_jobs} active
          | {serviceStatus.total_backups_completed} completed
        </span>
      </div>
    {:else}
      <div class="service-info service-stopped">
        <span class="service-dot service-dot-stopped"></span>
        Service stopped
      </div>
    {/if}
  </div>

  {#if error}
    <div class="error-banner">
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
        <circle cx="12" cy="12" r="10" /><path d="M12 8v4m0 4h.01" />
      </svg>
      <span>{error}</span>
      <button class="dismiss-btn" onclick={() => (error = null)}>Dismiss</button>
    </div>
  {/if}

  {#if isLoading}
    <div class="loading-state">
      <div class="spinner"></div>
      <p>Loading backup jobs...</p>
    </div>
  {:else if jobs.length === 0}
    <div class="empty-state">
      <div class="empty-icon">
        <svg width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1" stroke-linecap="round" stroke-linejoin="round">
          <rect x="3" y="4" width="18" height="16" rx="2" />
          <circle cx="12" cy="12" r="4" />
          <circle cx="12" cy="12" r="1" />
        </svg>
      </div>
      <h2 class="empty-title">No backup jobs configured</h2>
      <p class="empty-text">Create your first backup job to protect your data.</p>
      <button class="btn btn-primary" onclick={() => (showAddModal = true)}>
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M12 5v14M5 12h14" />
        </svg>
        Add Backup
      </button>
    </div>
  {:else}
    <div class="jobs-grid">
      {#each jobs as job (job.id)}
        <DiskCard
          {job}
          lastBackupDate={latestSets[job.id]?.completed_at}
          lastBackupStatus={getJobStatus(job)}
          nextBackup={job.schedule ? 'Scheduled' : null}
          progress={activeProgress[job.id] ?? null}
          onClick={() => openJobSettings(job.id)}
          onBackupNow={() => handleBackupNow(job.id)}
        />
      {/each}
    </div>

    <div class="add-section">
      <button class="add-job-btn" onclick={() => (showAddModal = true)}>
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M12 5v14M5 12h14" />
        </svg>
        Add backup job
      </button>
    </div>
  {/if}
</div>

<Modal
  open={showAddModal}
  title="Add Backup Job"
  onClose={() => (showAddModal = false)}
  onConfirm={handleCreateJob}
  confirmLabel="Create"
>
  <div class="form-group">
    <label for="job-name">Job Name</label>
    <input
      id="job-name"
      class="form-input"
      type="text"
      placeholder="e.g. System Drive Backup"
      bind:value={newJobName}
    />
  </div>
  <div class="form-group">
    <label for="source-type">Source Type</label>
    <select id="source-type" class="form-select" bind:value={newJobSourceType}>
      <option value="full_disk">Entire Disk</option>
      <option value="partition">Partition</option>
      <option value="volume">Volume</option>
    </select>
  </div>
  <div class="form-group">
    <label for="source-device">Source Device</label>
    <select id="source-device" class="form-select" bind:value={newJobSourceDevice}>
      {#if disks.length === 0}
        <option value="">No disks detected</option>
      {:else}
        <option value="">Select a disk...</option>
        {#each disks as disk}
          <option value={disk.device_id}>
            {disk.model || disk.device_id} ({formatBytes(disk.size_bytes)})
          </option>
        {/each}
      {/if}
    </select>
  </div>
  <div class="form-group">
    <label for="target-path">Target Path</label>
    <input
      id="target-path"
      class="form-input"
      type="text"
      placeholder="e.g. D:\Backups or /mnt/backups"
      bind:value={newJobTargetPath}
    />
  </div>
</Modal>

<style>
  .dashboard {
    padding: 28px 32px;
    height: 100%;
    overflow-y: auto;
  }

  .page-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 24px;
  }

  .page-title {
    font-size: 24px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
  }

  .service-info {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--success);
    font-weight: 500;
  }

  .service-stopped {
    color: var(--text-muted);
  }

  .service-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--success);
  }

  .service-dot-stopped {
    background: var(--text-muted);
  }

  .service-detail {
    color: var(--text-muted);
    font-weight: 400;
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

  .empty-state {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 80px 20px;
    text-align: center;
  }

  .empty-icon {
    margin-bottom: 20px;
    opacity: 0.5;
  }

  .empty-title {
    font-size: 20px;
    font-weight: 600;
    color: var(--text);
    margin: 0 0 8px;
  }

  .empty-text {
    color: var(--text-muted);
    margin: 0 0 24px;
  }

  .jobs-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(340px, 1fr));
    gap: 20px;
    margin-bottom: 24px;
  }

  .add-section {
    display: flex;
    justify-content: center;
    padding-top: 8px;
  }

  .add-job-btn {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 24px;
    background: none;
    border: 2px dashed var(--border);
    border-radius: var(--radius);
    color: var(--text-muted);
    font-size: 14px;
    font-weight: 500;
    cursor: pointer;
    transition: border-color 0.15s ease, color 0.15s ease;
  }

  .add-job-btn:hover {
    border-color: var(--primary);
    color: var(--primary);
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
