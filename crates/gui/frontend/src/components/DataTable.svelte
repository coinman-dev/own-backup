<script lang="ts">
  interface Column {
    id: string;
    label: string;
    sortable?: boolean;
    width?: string;
  }

  interface Props {
    columns: Column[];
    data: Record<string, unknown>[];
    sortColumn?: string;
    sortDirection?: 'asc' | 'desc';
    onSort?: (columnId: string) => void;
  }

  let {
    columns,
    data,
    sortColumn = '',
    sortDirection = 'asc',
    onSort,
  }: Props = $props();
</script>

<div class="table-wrapper">
  <table class="data-table">
    <thead>
      <tr>
        {#each columns as col}
          <th style={col.width ? `width: ${col.width}` : ''}>
            {#if col.sortable && onSort}
              <button class="sort-btn" onclick={() => onSort(col.id)}>
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
      {#if data.length === 0}
        <tr>
          <td colspan={columns.length} class="empty-row">
            No data to display.
          </td>
        </tr>
      {:else}
        {#each data as row}
          <tr>
            {#each columns as col}
              <td>{row[col.id] ?? ''}</td>
            {/each}
          </tr>
        {/each}
      {/if}
    </tbody>
  </table>
</div>

<style>
  .table-wrapper {
    width: 100%;
    overflow-x: auto;
  }

  .data-table {
    width: 100%;
    border-collapse: collapse;
    font-size: 13px;
  }

  .data-table th {
    text-align: left;
    padding: 10px 14px;
    font-weight: 600;
    color: var(--text-secondary);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    border-bottom: 2px solid var(--border);
    white-space: nowrap;
    background: var(--content-bg);
  }

  .data-table td {
    padding: 10px 14px;
    border-bottom: 1px solid var(--border);
    color: var(--text);
    vertical-align: middle;
  }

  .data-table tbody tr:hover {
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

  .empty-row {
    text-align: center;
    color: var(--text-muted);
    padding: 40px 14px !important;
    font-style: italic;
  }
</style>
