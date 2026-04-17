<script lang="ts">
  import { api } from '../lib/api';
  import type { ServiceStatus } from '../lib/types';

  let dataDirectory = $state('');
  let compressionLevel = $state(3);
  let logLevel = $state('info');
  let serviceStatus: ServiceStatus | null = $state(null);
  let isLoading = $state(true);
  let error: string | null = $state(null);
  let saveMessage: string | null = $state(null);

  async function loadSettings() {
    isLoading = true;
    error = null;
    try {
      const [configResult, statusResult] = await Promise.allSettled([
        api.getConfig(),
        api.getServiceStatus(),
      ]);

      if (configResult.status === 'fulfilled') {
        const cfg = configResult.value;
        dataDirectory = (cfg.data_directory as string) ?? '';
        compressionLevel = (cfg.compression_level as number) ?? 3;
        logLevel = (cfg.log_level as string) ?? 'info';
      }

      serviceStatus = statusResult.status === 'fulfilled' ? statusResult.value : null;
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    loadSettings();
  });

  async function handleSave() {
    saveMessage = null;
    error = null;
    try {
      await api.saveConfig({
        data_directory: dataDirectory,
        compression_level: compressionLevel,
        log_level: logLevel,
      });
      saveMessage = 'Settings saved successfully.';
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
</script>

<div class="settings-view">
  <div class="page-header">
    <h1 class="page-title">Settings</h1>
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
      <p>Loading settings...</p>
    </div>
  {:else}
    <div class="settings-grid">
      <div class="card">
        <h2 class="section-title">General</h2>

        <div class="form-group">
          <label for="data-dir">Data Directory</label>
          <input
            id="data-dir"
            type="text"
            class="form-input"
            placeholder="e.g. C:\ProgramData\OwnBackup or /var/lib/own-backup"
            bind:value={dataDirectory}
          />
          <p class="help-text">
            Directory where backup metadata and database are stored.
          </p>
        </div>

        <div class="form-group">
          <label for="comp-default">Default Compression Level: {compressionLevel}</label>
          <input
            id="comp-default"
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
          <p class="help-text">
            Zstd compression level for new backup jobs. Higher values compress more but are slower.
          </p>
        </div>

        <div class="form-group">
          <label for="log-level">Log Level</label>
          <select id="log-level" class="form-select" bind:value={logLevel}>
            <option value="error">Error</option>
            <option value="warn">Warning</option>
            <option value="info">Info</option>
            <option value="debug">Debug</option>
            <option value="trace">Trace</option>
          </select>
        </div>

        <div class="form-actions">
          <button class="btn btn-primary" onclick={handleSave}>Save Settings</button>
        </div>
      </div>

      <div class="card">
        <h2 class="section-title">Service Status</h2>

        {#if serviceStatus}
          <div class="status-grid">
            <div class="status-item">
              <span class="status-label">Status</span>
              <span class="status-value running">
                <span class="status-dot"></span>
                Running
              </span>
            </div>
            <div class="status-item">
              <span class="status-label">Version</span>
              <span class="status-value">{serviceStatus.version}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Uptime</span>
              <span class="status-value">{serviceStatus.uptime}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Active Jobs</span>
              <span class="status-value">{serviceStatus.active_jobs}</span>
            </div>
            <div class="status-item">
              <span class="status-label">Total Completed</span>
              <span class="status-value">{serviceStatus.total_backups_completed}</span>
            </div>
          </div>
        {:else}
          <div class="status-offline">
            <span class="status-dot offline"></span>
            <span>Service is not running or unreachable.</span>
          </div>
        {/if}
      </div>
    </div>
  {/if}
</div>

<style>
  .settings-view {
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

  .settings-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 24px;
    align-items: start;
  }

  .section-title {
    font-size: 16px;
    font-weight: 600;
    color: var(--text);
    margin: 0 0 16px;
  }

  .help-text {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 4px;
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

  .form-actions {
    margin-top: 20px;
    display: flex;
    justify-content: flex-end;
  }

  .status-grid {
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .status-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding-bottom: 10px;
    border-bottom: 1px solid var(--border);
  }

  .status-item:last-child {
    border-bottom: none;
    padding-bottom: 0;
  }

  .status-label {
    font-size: 13px;
    color: var(--text-muted);
  }

  .status-value {
    font-size: 14px;
    font-weight: 500;
    color: var(--text);
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .status-value.running {
    color: var(--success);
  }

  .status-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--success);
    flex-shrink: 0;
  }

  .status-dot.offline {
    background: var(--error);
  }

  .status-offline {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 16px;
    background: rgba(234, 67, 53, 0.06);
    border-radius: 6px;
    font-size: 13px;
    color: var(--text-secondary);
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
