<script lang="ts">
  interface Props {
    title?: string;
    message: string;
    onClose: () => void;
  }
  // The default was «В разработке», left from when this only announced stubs —
  // and it headed the type-breakdown help and the restore-cancel refusal too.
  let { title = 'Сведения', message, onClose }: Props = $props();
</script>

<div class="overlay" onclick={onClose} role="presentation">
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- The handler only stops the overlay's click from closing the dialog when
       the user clicks INSIDE it. There is no action to give a key equivalent
       to; the dialog's own button is the keyboard path. -->
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
    background: var(--overlay);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 200;
  }
  .dialog {
    width: 420px;
    background: var(--card-bg);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 24px;
    box-shadow: var(--shadow-lg);
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
    border-radius: 6px;
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
  .btn.primary:hover {
    background: var(--primary-hover);
  }
</style>
