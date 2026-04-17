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
    font-size: 12px;
    font-weight: 500;
    padding: 3px 10px;
    border-radius: 12px;
    white-space: nowrap;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }

  .badge-success {
    background: rgba(52, 168, 83, 0.1);
    color: var(--success);
  }
  .badge-success .dot {
    background: var(--success);
  }

  .badge-warning {
    background: rgba(251, 188, 5, 0.12);
    color: #c49200;
  }
  .badge-warning .dot {
    background: var(--warning);
  }

  .badge-error {
    background: rgba(234, 67, 53, 0.1);
    color: var(--error);
  }
  .badge-error .dot {
    background: var(--error);
  }

  .badge-running {
    background: rgba(45, 125, 210, 0.1);
    color: var(--primary);
  }
  .badge-running .dot {
    background: var(--primary);
    animation: pulse 1.2s ease-in-out infinite;
  }

  .badge-idle {
    background: rgba(153, 153, 153, 0.1);
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
