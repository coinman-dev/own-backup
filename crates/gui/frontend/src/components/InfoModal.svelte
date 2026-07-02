<script lang="ts">
  interface Props {
    title?: string;
    message: string;
    onClose: () => void;
  }
  let { title = 'В разработке', message, onClose }: Props = $props();
</script>

<div class="overlay" onclick={onClose} role="presentation">
  <div class="dialog" onclick={(e) => e.stopPropagation()} role="dialog" tabindex="-1">
    <div class="dialog-title">{title}</div>
    <p>{message}</p>
    <div class="dialog-actions">
      <button class="btn primary" onclick={onClose}>Понятно</button>
    </div>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(20, 17, 44, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 200;
  }
  .dialog {
    width: 420px;
    background: #fff;
    border-radius: 8px;
    padding: 24px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.3);
  }
  .dialog-title {
    font-size: 17px;
    font-weight: 600;
    color: var(--head);
    margin-bottom: 12px;
  }
  .dialog p {
    font-size: 14px;
    color: var(--text);
    line-height: 1.5;
    margin-bottom: 20px;
  }
  .dialog-actions {
    display: flex;
    justify-content: flex-end;
  }
  .btn {
    border-radius: 4px;
    padding: 9px 22px;
    font-size: 14px;
    font-weight: 600;
    font-family: var(--font);
    cursor: pointer;
    border: none;
  }
  .btn.primary {
    background: var(--primary);
    color: #fff;
  }
</style>
