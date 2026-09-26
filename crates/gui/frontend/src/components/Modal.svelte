<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    open: boolean;
    title: string;
    onClose: () => void;
    onConfirm?: () => void;
    confirmLabel?: string;
    /** Keep the confirm button on screen but unavailable (nothing typed yet,
     *  or already working) — a button that appears only once the form is
     *  filled in leaves no way to tell what the form is for. */
    confirmDisabled?: boolean;
    /** An empty string hides the button — a window that only informs has one
     *  answer, and a second one labelled "Cancel" had nothing to cancel. */
    cancelLabel?: string;
    danger?: boolean;
    children: Snippet;
  }

  let {
    open,
    title,
    onClose,
    onConfirm,
    confirmLabel = 'OK',
    confirmDisabled = false,
    cancelLabel = 'Отмена',
    danger = false,
    children,
  }: Props = $props();

  function handleBackdropClick(e: MouseEvent) {
    if (e.target === e.currentTarget) {
      onClose();
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      onClose();
    }
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <!-- tabindex="-1": a dialog has to be focusable for Escape to reach the
       handler below at all. Without it the key went to whatever was focused
       behind the dialog. -->
  <div
    class="modal-backdrop"
    role="dialog"
    aria-modal="true"
    aria-label={title}
    tabindex="-1"
    onclick={handleBackdropClick}
    onkeydown={handleKeydown}
  >
    <div class="modal-card">
      <div class="modal-header">
        <h2 class="modal-title">{title}</h2>
        <button class="modal-close" onclick={onClose} aria-label="Закрыть">
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round">
            <path d="M18 6L6 18M6 6l12 12" />
          </svg>
        </button>
      </div>

      <div class="modal-body">
        {@render children()}
      </div>

      <div class="modal-footer">
        {#if cancelLabel}
          <button class="btn btn-secondary" onclick={onClose}>
            {cancelLabel}
          </button>
        {/if}
        {#if onConfirm}
          <button
            class="btn {danger ? 'btn-danger' : 'btn-primary'}"
            onclick={onConfirm}
            disabled={confirmDisabled}
          >
            {confirmLabel}
          </button>
        {/if}
      </div>
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.45);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    animation: fadeIn 0.15s ease;
  }

  .modal-card {
    background: var(--card-bg);
    border-radius: 12px;
    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.2);
    width: 90%;
    max-width: 520px;
    max-height: 85vh;
    display: flex;
    flex-direction: column;
    animation: scaleIn 0.15s ease;
  }

  .modal-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 20px 24px 16px;
    border-bottom: 1px solid var(--border);
  }

  .modal-title {
    font-size: 18px;
    font-weight: 600;
    color: var(--text);
    margin: 0;
  }

  .modal-close {
    background: none;
    border: none;
    color: var(--text-muted);
    padding: 4px;
    border-radius: 4px;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: color 0.12s ease;
  }

  .modal-close:hover {
    color: var(--text);
  }

  .modal-body {
    padding: 20px 24px;
    overflow-y: auto;
    flex: 1;
  }

  .modal-footer {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
    padding: 16px 24px 20px;
    border-top: 1px solid var(--border);
  }

  @keyframes fadeIn {
    from { opacity: 0; }
    to { opacity: 1; }
  }

  @keyframes scaleIn {
    from { transform: scale(0.95); opacity: 0; }
    to { transform: scale(1); opacity: 1; }
  }
</style>
