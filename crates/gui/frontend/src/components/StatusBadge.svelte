<script lang="ts">
  interface Props {
    status: 'success' | 'warning' | 'error' | 'running' | 'idle';
  }

  let { status }: Props = $props();

  const labelMap: Record<string, string> = {
    success: 'Completed',
    warning: 'Warning',
    error: 'Failed',
    running: 'Running',
    idle: 'Idle',
  };

  let label = $derived(labelMap[status] ?? status);
</script>

<span class="badge badge-{status}">
  <span class="dot"></span>
  {label}
</span>

<style>
  .badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    font-weight: 700;
    padding: 2px 8px;
    border-radius: 10px;
    white-space: nowrap;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .badge-success {
    background: var(--success-soft);
    color: var(--success-text);
  }
  .badge-success .dot {
    background: var(--success);
  }

  .badge-warning {
    background: var(--warning-soft);
    color: var(--warning-text);
  }
  .badge-warning .dot {
    background: var(--warning);
  }

  .badge-error {
    background: var(--error-soft);
    color: var(--error-text);
  }
  .badge-error .dot {
    background: var(--error);
  }

  .badge-running {
    background: var(--accent-soft);
    color: var(--accent-text);
  }
  .badge-running .dot {
    background: var(--primary);
    animation: pulse 1.2s ease-in-out infinite;
  }

  .badge-idle {
    background: var(--sunken);
    color: var(--text-muted);
  }
  .badge-idle .dot {
    background: var(--text-muted);
  }

  @keyframes pulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.3; }
  }
</style>
