<script lang="ts">
  interface Props {
    percent: number;
    speed?: string;
    eta?: string;
  }

  let { percent, speed = '', eta = '' }: Props = $props();

  let clampedPercent = $derived(Math.max(0, Math.min(100, percent)));

  let overlayText = $derived(() => {
    const parts: string[] = [`${Math.round(clampedPercent)}%`];
    if (speed) parts.push(speed);
    if (eta) parts.push(`ETA ${eta}`);
    return parts.join(' \u2014 ');
  });
</script>

<div class="progress-container">
  <div class="progress-track">
    <div
      class="progress-fill"
      style="width: {clampedPercent}%"
    ></div>
  </div>
  <div class="progress-text">
    {overlayText()}
  </div>
</div>

<style>
  .progress-container {
    width: 100%;
  }

  .progress-track {
    width: 100%;
    height: 8px;
    background: var(--track);
    border-radius: 4px;
    overflow: hidden;
  }

  .progress-fill {
    height: 100%;
    background: var(--success);
    border-radius: 4px;
    transition: width 0.3s ease;
    position: relative;
  }

  .progress-fill::after {
    content: '';
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    bottom: 0;
    background: linear-gradient(
      90deg,
      transparent 0%,
      rgba(255, 255, 255, 0.25) 50%,
      transparent 100%
    );
    animation: shimmer 1.5s ease-in-out infinite;
  }

  .progress-text {
    margin-top: 6px;
    font-size: 12px;
    color: var(--text-secondary);
    text-align: center;
  }

  @keyframes shimmer {
    0% { transform: translateX(-100%); }
    100% { transform: translateX(100%); }
  }
</style>
