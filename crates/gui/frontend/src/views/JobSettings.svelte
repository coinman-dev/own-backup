<script lang="ts">
  import TabPanel from '../components/TabPanel.svelte';
  import Modal from '../components/Modal.svelte';
  import { api } from '../lib/api';
  import type { BackupJob, RetentionPolicy, ViewName } from '../lib/types';

  interface Props {
    jobId: string;
    onNavigate: (view: ViewName) => void;
  }

  let { jobId, onNavigate }: Props = $props();

  let job: BackupJob | null = $state(null);
  let isLoading = $state(true);
  let error: string | null = $state(null);
  let saveMessage: string | null = $state(null);

  let activeTab = $state('schedule');

  const tabs = [
    { id: 'schedule', label: 'Schedule' },
    { id: 'scheme', label: 'Backup Scheme' },
    { id: 'notifications', label: 'Notifications' },
    { id: 'exclusions', label: 'Exclusions' },
    { id: 'advanced', label: 'Advanced' },
  ];

  // Schedule state
  let scheduleDays = $state({
    mon: false,
    tue: false,
    wed: false,
    thu: false,
    fri: false,
    sat: false,
    sun: false,
  });
  let scheduleTime = $state('02:00');
  let scheduleEnabled = $state(false);

  // Scheme state
  let backupType = $state('incremental');
  let retention: RetentionPolicy = $state({
    job_id: '',
    max_full_backups: 3,
    max_incremental_per_full: 6,
    max_age_days: 90,
    min_keep_count: 1,
  });

  // Notifications state
  let notifyOnSuccess = $state(false);
  let notifyOnFailure = $state(true);
  let notifyEmail = $state('');

  // Exclusions state
  let exclusionPatterns = $state('');

  // Advanced state
  let compressionLevel = $state(3);
  let encryptionEnabled = $state(false);

  // Delete modal
  let showDeleteModal = $state(false);

  async function loadJob() {
    isLoading = true;
    error = null;
    try {
      const jobs = await api.listJobs();
      job = jobs.find((j) => j.id === jobId) ?? null;
      if (!job) {
        error = 'Job not found.';
        return;
      }

      compressionLevel = job.compression_level;
      encryptionEnabled = job.encryption_enabled;
      retention.job_id = job.id;

      // Load schedule
      try {
        const sched = await api.getSchedule(job.id);
        if (sched && sched.cron_expression) {
          scheduleEnabled = true;
          // Parse cron (simplified)
          // A real implementation would fully parse the cron expression
        }
      } catch {
        // No schedule configured
      }

      // Load retention
      try {
        const ret = await api.getRetention(job.id);
        if (ret) {
          retention = ret;
        }
      } catch {
        // Use defaults
      }
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    if (jobId) loadJob();
  });

  async function saveSchedule() {
    if (!job) return;
    saveMessage = null;
    try {
      if (scheduleEnabled) {
        // Build a simple cron from selected days and time
        const [hours, minutes] = scheduleTime.split(':');
        const dayNames = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'] as const;
        const dayNums: string[] = [];
        dayNames.forEach((d, i) => {
          if (scheduleDays[d]) dayNums.push(String(i + 1));
        });
        const dayStr = dayNums.length > 0 ? dayNums.join(',') : '*';
        const cron = `${minutes} ${hours} * * ${dayStr}`;
        await api.setSchedule(job.id, cron);
      } else {
        await api.setSchedule(job.id, '');
      }
      saveMessage = 'Schedule saved.';
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function saveScheme() {
    if (!job) return;
    saveMessage = null;
    try {
      await api.setRetention({
        ...retention,
        job_id: job.id,
      });
      saveMessage = 'Backup scheme saved.';
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  async function handleDeleteJob() {
    if (!job) return;
    try {
      await api.deleteJob(job.id);
      showDeleteModal = false;
      onNavigate('dashboard');
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="job-settings">
  <div class="page-header">
    <button class="back-btn" onclick={() => onNavigate('dashboard')}>
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
        <path d="M19 12H5M12 19l-7-7 7-7" />
      </svg>
      Back
    </button>
    <h1 class="page-title">
      {#if job}
        {job.name}
      {:else}
        Job Settings
      {/if}
    </h1>
    {#if job}
      <button class="btn btn-danger btn-sm" onclick={() => (showDeleteModal = true)}>
        Delete Job
      </button>
    {/if}
  </div>

  {#if error}
    <div class="error-banner">
      <span>{error}</span>
      <button class="dismiss-btn" onclick={() => (error = null)}>Dismiss</button>
    </div>
  {/if}

  {#if saveMessage}
    <div class="success-banner">
      <span>{saveMessage}</span>
      <button class="dismiss-btn" onclick={() => (saveMessage = null)}>OK</button>
    </div>
  {/if}

  {#if isLoading}
    <div class="loading-state">
      <div class="spinner"></div>
      <p>Loading job settings...</p>
    </div>
  {:else if job}
    <div class="card">
      <TabPanel {tabs} {activeTab} onTabChange={(id) => (activeTab = id)}>
        {#if activeTab === 'schedule'}
          <div class="tab-body">
            <div class="form-group">
              <label class="toggle-row">
                <input type="checkbox" bind:checked={scheduleEnabled} />
                <span>Enable scheduled backups</span>
              </label>
            </div>

            {#if scheduleEnabled}
              <div class="form-group">
                <label>Run on days</label>
                <div class="day-checkboxes">
                  {#each [
                    ['mon', 'Mon'], ['tue', 'Tue'], ['wed', 'Wed'],
                    ['thu', 'Thu'], ['fri', 'Fri'], ['sat', 'Sat'], ['sun', 'Sun'],
                  ] as [key, label]}
                    <label class="day-check">
                      <input type="checkbox" bind:checked={scheduleDays[key]} />
                      <span>{label}</span>
                    </label>
                  {/each}
                </div>
              </div>

              <div class="form-group">
                <label for="sched-time">Start time</label>
                <input
                  id="sched-time"
                  type="time"
                  class="form-input time-input"
                  bind:value={scheduleTime}
                />
              </div>
            {/if}

            <div class="form-actions">
              <button class="btn btn-primary" onclick={saveSchedule}>Save Schedule</button>
            </div>
          </div>
        {:else if activeTab === 'scheme'}
          <div class="tab-body">
            <div class="form-group">
              <label>Backup Type</label>
              <div class="radio-group">
                <label class="radio-item">
                  <input type="radio" name="backup-type" value="full" bind:group={backupType} />
                  <div class="radio-content">
                    <span class="radio-title">Full</span>
                    <span class="radio-desc">Complete copy every time (largest, slowest)</span>
                  </div>
                </label>
                <label class="radio-item">
                  <input type="radio" name="backup-type" value="incremental" bind:group={backupType} />
                  <div class="radio-content">
                    <span class="radio-title">Incremental</span>
                    <span class="radio-desc">Only changed blocks since last backup (smallest, fastest)</span>
                  </div>
                </label>
                <label class="radio-item">
                  <input type="radio" name="backup-type" value="differential" bind:group={backupType} />
                  <div class="radio-content">
                    <span class="radio-title">Differential</span>
                    <span class="radio-desc">All changes since last full backup</span>
                  </div>
                </label>
              </div>
            </div>

            <h3 class="section-heading">Retention Policy</h3>

            <div class="form-row">
              <div class="form-group">
                <label for="max-full">Max full backups</label>
                <input
                  id="max-full"
                  type="number"
                  class="form-input"
                  min="1"
                  max="100"
                  bind:value={retention.max_full_backups}
                />
              </div>
              <div class="form-group">
                <label for="max-inc">Max incremental per full</label>
                <input
                  id="max-inc"
                  type="number"
                  class="form-input"
                  min="1"
                  max="100"
                  bind:value={retention.max_incremental_per_full}
                />
              </div>
            </div>

            <div class="form-row">
              <div class="form-group">
                <label for="max-age">Max age (days)</label>
                <input
                  id="max-age"
                  type="number"
                  class="form-input"
                  min="1"
                  max="3650"
                  bind:value={retention.max_age_days}
                />
              </div>
              <div class="form-group">
                <label for="min-keep">Minimum to keep</label>
                <input
                  id="min-keep"
                  type="number"
                  class="form-input"
                  min="1"
                  max="50"
                  bind:value={retention.min_keep_count}
                />
              </div>
            </div>

            <div class="form-actions">
              <button class="btn btn-primary" onclick={saveScheme}>Save Scheme</button>
            </div>
          </div>
        {:else if activeTab === 'notifications'}
          <div class="tab-body">
            <div class="form-group">
              <label class="toggle-row">
                <input type="checkbox" bind:checked={notifyOnSuccess} />
                <span>Notify on successful backup</span>
              </label>
            </div>
            <div class="form-group">
              <label class="toggle-row">
                <input type="checkbox" bind:checked={notifyOnFailure} />
                <span>Notify on failed backup</span>
              </label>
            </div>
            <div class="form-group">
              <label for="notify-email">Email address</label>
              <input
                id="notify-email"
                type="email"
                class="form-input"
                placeholder="your@email.com"
                bind:value={notifyEmail}
              />
            </div>
            <div class="form-actions">
              <button class="btn btn-primary">Save Notifications</button>
            </div>
            <p class="placeholder-note">
              Email notifications are not yet implemented in the backend.
            </p>
          </div>
        {:else if activeTab === 'exclusions'}
          <div class="tab-body">
            <div class="form-group">
              <label for="exclusion-patterns">Exclusion patterns (one per line)</label>
              <textarea
                id="exclusion-patterns"
                class="form-input exclusion-textarea"
                placeholder="*.tmp&#10;*.log&#10;$RECYCLE.BIN"
                bind:value={exclusionPatterns}
              ></textarea>
            </div>
            <div class="form-actions">
              <button class="btn btn-primary">Save Exclusions</button>
            </div>
            <p class="placeholder-note">
              File-level exclusions will be supported in a future release. Block-level backups currently capture the entire source.
            </p>
          </div>
        {:else if activeTab === 'advanced'}
          <div class="tab-body">
            <div class="form-group">
              <label for="comp-level">Compression level (zstd): {compressionLevel}</label>
              <input
                id="comp-level"
                type="range"
                min="0"
                max="19"
                class="form-range"
                bind:value={compressionLevel}
              />
              <div class="range-labels">
                <span>0 (none)</span>
                <span>19 (max)</span>
              </div>
            </div>

            <div class="form-group">
              <label class="toggle-row">
                <input type="checkbox" bind:checked={encryptionEnabled} />
                <span>Enable encryption (AES-256-GCM)</span>
              </label>
              {#if encryptionEnabled}
                <p class="help-text">
                  You will be prompted for a password when starting each backup and restore.
                </p>
              {/if}
            </div>

            <div class="form-actions">
              <button class="btn btn-primary">Save Advanced Settings</button>
            </div>
          </div>
        {/if}
      </TabPanel>
    </div>
  {/if}
</div>

<Modal
  open={showDeleteModal}
  title="Delete Backup Job"
  onClose={() => (showDeleteModal = false)}
  onConfirm={handleDeleteJob}
  confirmLabel="Delete"
  danger
>
  <p>
    Are you sure you want to delete <strong>{job?.name}</strong>?
    This will remove the job configuration. Existing backup files will not be deleted.
  </p>
</Modal>

<style>
  .job-settings {
    padding: 28px 32px;
    height: 100%;
    overflow-y: auto;
  }

  .page-header {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 24px;
  }

  .page-title {
    font-size: 24px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
    flex: 1;
  }

  .back-btn {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 12px;
    background: none;
    border: 1px solid var(--border);
    border-radius: 6px;
    color: var(--text-secondary);
    font-size: 13px;
    cursor: pointer;
    transition: border-color 0.12s ease;
  }

  .back-btn:hover {
    border-color: var(--primary);
    color: var(--primary);
  }

  .btn-sm {
    padding: 6px 14px;
    font-size: 13px;
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

  .success-banner {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 16px;
    background: rgba(52, 168, 83, 0.08);
    border: 1px solid rgba(52, 168, 83, 0.2);
    border-radius: var(--radius);
    color: var(--success);
    margin-bottom: 20px;
    font-size: 13px;
  }

  .dismiss-btn {
    margin-left: auto;
    background: none;
    border: none;
    font-size: 12px;
    font-weight: 500;
    text-decoration: underline;
    cursor: pointer;
    color: inherit;
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

  .tab-body {
    padding: 4px 0;
  }

  .toggle-row {
    display: flex;
    align-items: center;
    gap: 10px;
    cursor: pointer;
    font-weight: 400 !important;
    color: var(--text) !important;
    font-size: 14px !important;
  }

  .toggle-row input[type='checkbox'] {
    width: 16px;
    height: 16px;
    accent-color: var(--primary);
  }

  .day-checkboxes {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
  }

  .day-check {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    padding: 8px 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    cursor: pointer;
    font-size: 12px;
    font-weight: 500;
    color: var(--text-secondary);
    transition: border-color 0.12s ease, background 0.12s ease;
    min-width: 52px;
    text-align: center;
  }

  .day-check:has(input:checked) {
    border-color: var(--primary);
    background: rgba(45, 125, 210, 0.06);
    color: var(--primary);
  }

  .day-check input {
    display: none;
  }

  .time-input {
    max-width: 160px;
  }

  .radio-group {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }

  .radio-item {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    padding: 12px;
    border: 1px solid var(--border);
    border-radius: 6px;
    cursor: pointer;
    transition: border-color 0.12s ease;
  }

  .radio-item:has(input:checked) {
    border-color: var(--primary);
    background: rgba(45, 125, 210, 0.04);
  }

  .radio-item input[type='radio'] {
    margin-top: 2px;
    accent-color: var(--primary);
  }

  .radio-content {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .radio-title {
    font-weight: 600;
    color: var(--text);
    font-size: 14px;
  }

  .radio-desc {
    font-size: 12px;
    color: var(--text-muted);
  }

  .section-heading {
    font-size: 15px;
    font-weight: 600;
    color: var(--text);
    margin: 20px 0 12px;
  }

  .form-row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
  }

  .form-actions {
    margin-top: 20px;
    display: flex;
    justify-content: flex-end;
  }

  .help-text {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 6px;
  }

  .placeholder-note {
    margin-top: 16px;
    font-size: 12px;
    color: var(--text-muted);
    font-style: italic;
    padding: 8px 12px;
    background: rgba(251, 188, 5, 0.08);
    border-radius: 6px;
  }

  .exclusion-textarea {
    min-height: 160px;
    font-family: 'Consolas', 'Monaco', monospace;
    font-size: 13px;
    resize: vertical;
  }

  .form-range {
    width: 100%;
    accent-color: var(--primary);
  }

  .range-labels {
    display: flex;
    justify-content: space-between;
    font-size: 11px;
    color: var(--text-muted);
    margin-top: 4px;
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
