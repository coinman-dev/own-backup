<script lang="ts">
  // «Сохранить профиль как…»: what is ticked on the page now, kept under a
  // name. A job's «Очистка» tab chooses one for its cleanup before the copy.
  // The same name replaces that profile; a saved one can be opened back onto
  // the page or deleted here. Junk by masks is never part of a profile, as of
  // the automatic set (those files go only from a list someone looked through).
  import Modal from './Modal.svelte';
  import { m } from '../lib/paraglide/messages';
  import type { CleanupNamedProfile } from '../lib/types';

  interface Props {
    profiles: CleanupNamedProfile[];
    /** The ticked items and users the profile would take. */
    items: { id: string; label: string }[];
    users: { sid: string; name: string }[];
    /** Ticked items left out: cleaned only after a look at their list. */
    left: string[];
    saving: boolean;
    error: string | null;
    onSave: (name: string) => void;
    onOpen: (profile: CleanupNamedProfile) => void;
    onDelete: (profile: CleanupNamedProfile) => void;
    onClose: () => void;
  }

  let { profiles, items, users, left, saving, error, onSave, onOpen, onDelete, onClose }: Props = $props();

  let name = $state('');

  const nothing = $derived(items.length === 0);
  const trimmed = $derived(name.trim());
  const replaces = $derived(profiles.find((p) => p.name.toLowerCase() === trimmed.toLowerCase()) ?? null);
</script>

<Modal
  open
  wide
  title={m.clp_title()}
  {onClose}
  cancelLabel={m.common_cancel()}
  confirmLabel={m.cla_save()}
  confirmDisabled={saving || nothing || trimmed.length === 0 || trimmed.length > 80}
  onConfirm={() => onSave(trimmed)}
>
  <p class="lead">{m.clp_lead()}</p>

  <label class="name">
    <span class="sec">{m.clp_name()}</span>
    <!-- svelte-ignore a11y_autofocus -->
    <input class="form-input" type="text" maxlength="80" autofocus placeholder={m.clp_name_hint()} bind:value={name} />
  </label>
  {#if replaces}<p class="note">{m.clp_replaces({ name: replaces.name })}</p>{/if}

  <div class="sec">{m.cla_what()}</div>
  {#if nothing}
    <p class="warn">{m.clp_nothing()}</p>
  {:else}
    <p class="what">{items.map((i) => i.label).join(', ')}</p>
    <p class="what muted">{m.cla_users({ names: users.map((u) => u.name).join(', ') || '—' })}</p>
  {/if}
  {#if left.length > 0}<p class="what muted">{m.cla_left({ names: left.join(', ') })}</p>{/if}

  {#if profiles.length > 0}
    <div class="sec">{m.clp_saved()}</div>
    <ul class="list">
      {#each profiles as p (p.id)}
        <li>
          <button class="pick" title={m.clp_pick_title()} onclick={() => (name = p.name)}>{p.name}</button>
          <span class="count muted">{m.clp_count({ count: p.items.length })}</span>
          <button class="files-btn" disabled={saving} title={m.clp_open_title()} onclick={() => onOpen(p)}>{m.clp_open()}</button>
          <button class="x" disabled={saving} title={m.clp_delete()} aria-label={m.clp_delete()} onclick={() => onDelete(p)}>×</button>
        </li>
      {/each}
    </ul>
  {/if}
  {#if error}<p class="err">{error}</p>{/if}
</Modal>

<style>
  .lead { font-size: 13px; color: var(--text-secondary); margin: 0 0 12px; line-height: 1.5; }
  .sec { display: block; font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: .04em; color: var(--text-muted); margin: 14px 0 8px; }
  .name { display: block; }
  .name .sec { margin-top: 0; }
  .what { font-size: 13px; color: var(--text); margin: 0 0 6px; line-height: 1.5; }
  .muted { color: var(--text-secondary); }
  .warn { font-size: 13px; color: var(--warning-text, var(--text)); margin: 0; }
  .note { font-size: 12px; color: var(--text-muted); margin: 6px 0 0; line-height: 1.5; }
  .err { font-size: 13px; color: var(--error-text); margin: 10px 0 0; overflow-wrap: anywhere; }
  .list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
  .list li { display: flex; align-items: center; gap: 10px; font-size: 14px; }
  .pick { flex: 1; min-width: 0; text-align: left; background: none; border: none; padding: 2px 0; color: var(--text); font-size: 14px; font-family: var(--font); cursor: pointer; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .pick:hover { color: var(--accent-text); }
  .count { font-size: 12px; white-space: nowrap; }
  .files-btn { background: transparent; border: 1px solid var(--border-strong); border-radius: 5px; padding: 2px 9px; font-size: 12px; color: var(--accent-text); cursor: pointer; font-family: var(--font); }
  .files-btn:hover:not(:disabled) { background: var(--accent-soft); }
  .x { background: none; border: none; font-size: 16px; line-height: 1; color: var(--text-muted); cursor: pointer; padding: 0 4px; }
  .x:hover:not(:disabled) { color: var(--error-text); }
</style>
