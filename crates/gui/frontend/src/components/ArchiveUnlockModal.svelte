<script lang="ts">
  // Asking for what opens an encrypted copy, before the operation starts.
  //
  // The service will refuse a manual mount, restore or check of an encrypted
  // copy that arrives without this — deliberately, because the key it holds for
  // scheduled COPIES is not consent to hand the data back to whoever is at the
  // screen (§7.4). Asking first rather than after the refusal is not only
  // politeness: a restore that gets far enough to fail has already started
  // taking the target apart.
  //
  // Two ways in, and they are independent on purpose: the password the person
  // chose, and the recovery key the service generated when encryption was
  // turned on. Either opens the copy; neither can be derived from the other.
  import Modal from './Modal.svelte';
  import PasswordField from './PasswordField.svelte';
  import type { ArchiveSecret } from '../lib/types';

  interface Props {
    open: boolean;
    /** What the copy is called, for the line that says which one is being opened. */
    imageName: string;
    /** Shown while the key is being derived — seconds of Argon2id, by design. */
    busy?: boolean;
    /** The refusal from the last attempt, if there was one. */
    error?: string;
    onUnlock: (secret: ArchiveSecret) => void;
    onCancel: () => void;
  }

  let { open, imageName, busy = false, error = '', onUnlock, onCancel }: Props = $props();

  type Mode = 'password' | 'recovery';
  let mode = $state<Mode>('password');
  let password = $state('');
  let recovery = $state('');

  const ready = $derived(
    mode === 'password' ? password.length > 0 : recovery.trim().length > 0,
  );

  function submit() {
    if (!ready || busy) return;
    onUnlock(mode === 'password' ? { password } : { recoveryKey: recovery });
  }

  function close() {
    password = '';
    recovery = '';
    onCancel();
  }
</script>

<Modal
  {open}
  title="Копия зашифрована"
  onClose={close}
  onConfirm={submit}
  confirmDisabled={!ready || busy}
  confirmLabel={busy ? 'Открываю…' : 'Открыть'}
  cancelLabel="Отмена"
>
  <p class="what">
    Чтобы открыть <b>{imageName}</b>, нужен пароль архива или ключ восстановления.
  </p>

  <div class="tabs">
    <button class="tab" class:active={mode === 'password'} onclick={() => (mode = 'password')}>
      Пароль
    </button>
    <button class="tab" class:active={mode === 'recovery'} onclick={() => (mode = 'recovery')}>
      Ключ восстановления
    </button>
  </div>

  {#if mode === 'password'}
    <div class="wide-field">
      <PasswordField
        value={password}
        oninput={(v) => (password = v)}
        onenter={submit}
        placeholder="Пароль архива"
        invalid={!!error}
      />
    </div>
    <p class="note">Тот пароль, который был задан при включении шифрования этого задания.</p>
  {:else}
    <!-- Shown, not masked, and full width: it is copied off paper, never
         memorised, and a box that shows a third of it hides the typo. -->
    <div class="wide-field">
      <PasswordField
        value={recovery}
        oninput={(v) => (recovery = v)}
        onenter={submit}
        placeholder="obk1…"
        invalid={!!error}
        revealed={true}
      />
    </div>
    <p class="note">
      Строка, которую программа показала один раз при включении шифрования.
      Пробелы и регистр значения не имеют.
    </p>
  {/if}

  {#if busy}
    <p class="note busy">
      Вывод ключа занимает несколько секунд — так и задумано: это то, что делает
      подбор пароля дорогим.
    </p>
  {/if}
  {#if error}
    <p class="err">{error}</p>
  {/if}
</Modal>

<style>
  .what {
    margin: 0 0 14px;
    font-size: 13px;
    color: var(--text);
  }
  .tabs {
    display: flex;
    gap: 6px;
    margin-bottom: 12px;
  }
  .tab {
    padding: 6px 12px;
    font-size: 12px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--card-bg);
    color: var(--text-secondary);
    cursor: pointer;
  }
  .tab:hover:not(.active) {
    background: var(--panel2);
  }
  /* Chosen the way the design marks a chosen item: tinted, accent-edged. */
  .tab.active {
    background: var(--accent-soft);
    border-color: var(--primary);
    color: var(--accent-text);
    font-weight: 600;
  }
  .wide-field {
    display: flex;
  }
  .note {
    margin: 8px 0 0;
    font-size: 12px;
    color: var(--text-muted);
    line-height: 1.45;
  }
  .note.busy {
    color: var(--text);
  }
  .err {
    margin: 10px 0 0;
    padding: 8px 10px;
    font-size: 12px;
    border: 1px solid var(--error-line);
    border-radius: 8px;
    background: var(--error-soft);
    color: var(--error-text);
  }
</style>
