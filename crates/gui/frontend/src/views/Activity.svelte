<script lang="ts">
  import DataTable from '../components/DataTable.svelte';
  import StatusBadge from '../components/StatusBadge.svelte';
  import { api } from '../lib/api';
  import { formatBytes, formatDate, formatDuration } from '../lib/format';
  import type { BackupJob, BackupSet } from '../lib/types';

  let jobs: BackupJob[] = $state([]);
  let allSets: (BackupSet & { jobName: string })[] = $state([]);
  let isLoading = $state(true);
  let error: string | null = $state(null);
  let sortColumn = $state('date');
  let sortDirection: 'asc' | 'desc' = $state('desc');

  const columns = [
    { id: 'date', label: 'Date', sortable: true, width: '170px' },
    { id: 'jobName', label: 'Job', sortable: true },
    { id: 'type', label: 'Type', sortable: true, width: '100px' },
    { id: 'status', label: 'Status', sortable: true, width: '120px' },
    { id: 'size', label: 'Size', sortable: true, width: '100px' },
    { id: 'duration', label: 'Duration', sortable: true, width: '100px' },
  ];

  async function loadActivity() {
    isLoading = true;
    error = null;
    try {
      jobs = await api.listJobs();
      const jobMap = new Map(jobs.map((j) => [j.id, j.name]));

      const setsPromises = jobs.map(async (j) => {
        try {
          const sets = await api.listSets(j.id);
          return sets.map((s) => ({ ...s, jobName: jobMap.get(s.job_id) ?? 'Unknown' }));
        } catch {
          return [];
        }
      });

      const results = await Promise.all(setsPromises);
      allSets = results.flat();
    } catch (e: unknown) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      isLoading = false;
    }
  }

  $effect(() => {
    loadActivity();
  });

  function handleSort(colId: string) {
    if (sortColumn === colId) {
      sortDirection = sortDirection === 'asc' ? 'desc' : 'asc';
    } else {
      sortColumn = colId;
      sortDirection = 'desc';
    }
  }

  let sortedData = $derived(() => {
    const rows = allSets.map((s) => {
      const started = s.started_at ? new Date(s.started_at).getTime() : 0;
      const completed = s.completed_at ? new Date(s.completed_at).getTime() : 0;
      const durationSec = started && completed ? (completed - started) / 1000 : 0;

      return {
        date: s.started_at ? formatDate(s.started_at) : '---',
        _dateRaw: started,
        jobName: s.jobName,
        type: s.backup_type,
        status: s.status,
        size: s.file_size != null ? formatBytes(s.file_size) : '---',
        _sizeRaw: s.file_size ?? 0,
        duration: durationSec > 0 ? formatDuration(durationSec) : '---',
        _durationRaw: durationSec,
      };
    });

    rows.sort((a, b) => {
      let cmp = 0;
      switch (sortColumn) {
        case 'date':
          cmp = a._dateRaw - b._dateRaw;
          break;
        case 'size':
          cmp = a._sizeRaw - b._sizeRaw;
          break;
        case 'duration':
          cmp = a._durationRaw - b._durationRaw;
          break;
        default:
          cmp = String(a[sortColumn as keyof typeof a] ?? '').localeCompare(
            String(b[sortColumn as keyof typeof b] ?? ''),
          );
      }
      return sortDirection === 'asc' ? cmp : -cmp;
    });

    return rows;
  });

  function statusToBadge(
    status: string,
  ): 'success' | 'warning' | 'error' | 'running' | 'idle' {
    switch (status) {
      case 'completed':
        return 'success';
      case 'failed':
        return 'error';
      case 'running':
      case 'in_progress':
        return 'running';
      case 'cancelled':
        return 'warning';
      default:
        return 'idle';
    }
  }
</script>

<div class="activity">
  <div class="page-header">
    <h1 class="page-title">Activity</h1>
    <button class="btn btn-secondary" onclick={loadActivity}>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
        <path d="M4 4v5h5M20 20v-5h-5M4 9a8 8 0 0 1 14-4M20 15a8 8 0 0 1-14 4" />
      </svg>
      Refresh
    </button>
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
      <p>Loading activity log...</p>
    </div>
  {:else}
    <div class="card">
      <div class="table-container">
        <table class="activity-table">
          <thead>
            <tr>
              {#each columns as col}
                <th style={col.width ? `width: ${col.width}` : ''}>
                  {#if col.sortable}
                    <button class="sort-btn" onclick={() => handleSort(col.id)}>
                      {col.label}
                      <span class="sort-indicator">
                        {#if sortColumn === col.id}
                          {sortDirection === 'asc' ? '\u25B2' : '\u25BC'}
                        {:else}
                          <span class="sort-inactive">\u25B4</span>
                        {/if}
                      </span>
                    </button>
                  {:else}
                    {col.label}
                  {/if}
                </th>
              {/each}
            </tr>
          </thead>
          <tbody>
            {#if sortedData().length === 0}
              <tr>
                <td colspan={columns.length} class="empty-row">
                  No backup activity recorded yet.
                </td>
              </tr>
            {:else}
              {#each sortedData() as row}
                <tr>
                  <td>{row.date}</td>
                  <td>{row.jobName}</td>
                  <td>
                    <span class="type-badge">{row.type}</span>
                  </td>
                  <td>
                    <StatusBadge status={statusToBadge(row.status)} />
                  </td>
                  <td>{row.size}</td>
                  <td>{row.duration}</td>
                </tr>
              {/each}
            {/if}
          </tbody>
        </table>
      </div>
    </div>
  {/if}
</div>

<style>
  .activity {
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

  .table-container {
    overflow-x: auto;
  }

  .activity-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  .activity-table th {
    text-align: left;
    padding: 12px 14px;
    font-weight: 600;
    color: var(--text-secondary);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    border-bottom: 2px solid var(--border);
    white-space: nowrap;
  }

  .activity-table td {
    padding: 12px 14px;
    border-bottom: 1px solid var(--border);
    color: var(--text);
    vertical-align: middle;
  }

  .activity-table tbody tr:hover {
    background: rgba(45, 125, 210, 0.04);
  }

  .sort-btn {
    background: none;
    border: none;
    font: inherit;
    font-weight: 600;
    color: var(--text-secondary);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 0;
  }

  .sort-btn:hover {
    color: var(--primary);
  }

  .sort-indicator {
    font-size: 10px;
    line-height: 1;
  }

  .sort-inactive {
    opacity: 0.3;
  }

  .type-badge {
    display: inline-block;
    padding: 2px 8px;
    background: rgba(45, 125, 210, 0.08);
    color: var(--primary);
    font-size: 11px;
    font-weight: 600;
    border-radius: 4px;
    text-transform: uppercase;
  }

  .empty-row {
    text-align: center;
    color: var(--text-muted);
    padding: 40px 14px !important;
    font-style: italic;
  }

  @keyframes spin {
    to { transform: rotate(360deg); }
  }
</style>
