<script lang="ts">
  // The recovery key: shown once, and written down before anything is saved.
  //
  // The password is chosen on the job's own screen; this is the step that
  // cannot be skipped. The service generates a recovery key, shows it once and
  // stores it nowhere — so if the job were saved BEFORE it had been written
  // down, one forgotten password would make every copy unreadable with nothing
  // left to say so. Nothing here writes; the authorization is committed only
  // when the key comes back typed correctly.
  //
  // Typing it back is not ceremony: the key carries a checksum, so a mistyped
  // one is named as mistyped rather than quietly accepted — and a dialog
  // clicked through without reading cannot pass.
  import Modal from './Modal.svelte';
  import PasswordField from './PasswordField.svelte';
  import { api } from '../lib/api';
  import { enableArchiveEncryption } from '../lib/jobStore.svelte';
  import type { BegunArchiveEncryption, Job } from '../lib/types';

  interface Props {
    open: boolean;
    /** The job as the wizard has it — saved by the service as part of the
     *  same transaction that stores the key. */
    job: Job;
    /** What the operator typed on the job's «Дополнительно» screen. */
    password: string;
    /** The stored job, once encryption is on. */
    onDone: (saved: Job) => void;
    onCancel: () => void;
  }

  let { open, job, password, onDone, onCancel }: Props = $props();

  let begun = $state<BegunArchiveEncryption | null>(null);
  let echo = $state('');
  let busy = $state(false);
  let error = $state('');
  let preparing = $state(false);

  // Opening the dialog is what starts the (slow) preparation: the service
  // seals the key with Argon2id at the production profile, seconds rather than
  // milliseconds. Nothing is written by it.
  $effect(() => {
    if (open && !begun && !preparing) {
      void prepare();
    }
    if (!open && (begun || error)) {
      // Left behind by a close that went through `cancel`; reset for next time.
      begun = null;
      echo = '';
      error = '';
    }
  });

  async function prepare() {
    preparing = true;
    busy = true;
    error = '';
    try {
      begun = await api.beginArchiveEncryption(job, password);
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
      preparing = false;
    }
  }

  async function commit() {
    if (!begun || echo.trim().length === 0 || busy) return;
    busy = true;
    error = '';
    try {
      // Through the store, so the job list shows the job the moment it exists
      // — for a new job this commit IS its creation.
      const saved = await enableArchiveEncryption(begun.token, echo);
      const token = begun.token;
      begun = null;
      echo = '';
      onDone(saved);
      void token;
    } catch (e) {
      // The service keeps the request alive for a survivable typo and says how
      // many tries are left; when they run out it is gone and says so. Either
      // way the text it sends is the one to show.
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function cancel() {
    // Nothing was written, but the service is holding a prepared key in memory
    // — say it is no longer wanted rather than leaving it to time out.
    if (begun) {
      try {
        await api.cancelArchiveEncryption(begun.token);
      } catch {
        // It expires by itself; a failure here changes nothing.
      }
    }
    begun = null;
    echo = '';
    error = '';
    onCancel();
  }

  async function copyKey() {
    if (!begun) return;
    try {
      await navigator.clipboard.writeText(begun.recovery_key);
    } catch {
      // No clipboard (a boot medium may have none) — the key is on screen.
    }
  }
</script>

<Modal
  {open}
  title="Ключ восстановления"
  onClose={cancel}
  onConfirm={commit}
  confirmDisabled={!begun || echo.trim().length === 0 || busy}
  confirmLabel={busy ? 'Включаю…' : 'Включить шифрование'}
  cancelLabel="Отмена"
>
  {#if !begun}
    <p class="what">
      {#if error}
        Ключ подготовить не удалось.
      {:else}
        Готовлю ключ задания — это несколько секунд.
      {/if}
    </p>
  {:else}
    <p class="what">
      Запишите ключ восстановления <b>сейчас</b>. Он показывается один раз и нигде
      не сохраняется — ни у нас, ни в службе.
    </p>

    <div class="key-box">
      <code>{begun.recovery_key}</code>
      <button class="copy" onclick={copyKey} title="Скопировать в буфер обмена">
        Скопировать
      </button>
    </div>

    <p class="note">
      Отпечаток ключа задания: <code class="fp">{begun.fingerprint.slice(0, 16)}…</code>
    </p>

    <div class="field-label">Введите ключ восстановления, чтобы подтвердить:</div>
    <!-- Full width: it is sixty characters of bech32 copied off paper, and a
         box that shows a third of it hides the typo being looked for. -->
    <div class="wide-field">
      <PasswordField
        value={echo}
        oninput={(v) => (echo = v)}
        onenter={commit}
        placeholder="obk1…"
        invalid={!!error}
        revealed={true}
      />
    </div>
    <p class="note">
      Пока он не введён, ничего не сохранено: задание останется без шифрования.
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
    line-height: 1.5;
    color: var(--text);
  }
  .field-label {
    font-size: 12px;
    color: var(--text-muted);
    margin: 14px 0 5px;
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
  .key-box {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px;
    margin: 4px 0 2px;
    border: 1px solid var(--primary);
    border-radius: 6px;
    background: var(--card-bg);
  }
  .key-box code {
    flex: 1 1 auto;
    font-family: ui-monospace, 'Cascadia Mono', 'Consolas', monospace;
    font-size: 13px;
    line-height: 1.6;
    word-break: break-all;
    color: var(--text);
    user-select: all;
  }
  .copy {
    flex: 0 0 auto;
    padding: 6px 10px;
    font-size: 12px;
    border: 1px solid var(--border);
    border-radius: 5px;
    background: transparent;
    color: var(--text-muted);
    cursor: pointer;
  }
  .copy:hover {
    color: var(--text);
  }
  .fp {
    font-family: ui-monospace, 'Cascadia Mono', 'Consolas', monospace;
    font-size: 11px;
  }
  .err {
    margin: 10px 0 0;
    padding: 8px 10px;
    font-size: 12px;
    border-radius: 5px;
    background: rgba(216, 74, 74, 0.12);
    color: #d84a4a;
  }
</style>
