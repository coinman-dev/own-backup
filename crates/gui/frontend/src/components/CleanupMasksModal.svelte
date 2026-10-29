<script lang="ts">
  // The junk masks, as Ace Utilities lists them: each with its kind and what
  // it finds, ticked or not; the person's own masks below. Masks too broad
  // are refused here as the service refuses them.
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import { m } from '../lib/paraglide/messages';
  import { maskProblem } from '../lib/junkMasks';
  import type { CleanupMask } from '../lib/types';

  interface Props {
    offered: CleanupMask[];
    chosen: string[];
    onSave: (masks: string[]) => void;
    onClose: () => void;
  }

  let { offered, chosen, onSave, onClose }: Props = $props();

  const lower = (s: string) => s.toLowerCase();
  const known = $derived(new Set(offered.map((o) => lower(o.mask))));
  // The window edits a copy; the page gets it back on «Готово».
  let on = $state(untrack(() => new Set(chosen.map(lower))));
  let own = $state(untrack(() => chosen.filter((c) => !known.has(lower(c)))));
  let adding = $state('');
  let error = $state<string | null>(null);

  function toggle(mask: string) {
    const next = new Set(on);
    if (next.has(lower(mask))) next.delete(lower(mask));
    else next.add(lower(mask));
    on = next;
  }

  function add() {
    const mask = adding.trim();
    const problem = maskProblem(mask);
    if (problem) {
      error = problem === 'empty' ? null : m.cl_junk_bad_mask({ mask });
      return;
    }
    if (known.has(lower(mask))) {
      on = new Set([...on, lower(mask)]);
    } else if (!own.some((o) => lower(o) === lower(mask))) {
      own = [...own, mask];
      on = new Set([...on, lower(mask)]);
    }
    adding = '';
    error = null;
  }

  function removeOwn(mask: string) {
    own = own.filter((o) => o !== mask);
    const next = new Set(on);
    next.delete(lower(mask));
    on = next;
  }

  function defaults() {
    own = [];
    on = new Set(offered.filter((o) => o.default_on).map((o) => lower(o.mask)));
  }

  const picked = $derived([
    ...offered.filter((o) => on.has(lower(o.mask))).map((o) => o.mask),
    ...own.filter((o) => on.has(lower(o))),
  ]);

  function save() {
    if (picked.length === 0) {
      error = m.cl_junk_no_masks();
      return;
    }
    onSave(picked);
  }
</script>

<Modal
  open
  xwide
  title={m.clm_title()}
  {onClose}
  cancelLabel={m.common_cancel()}
  confirmLabel={m.clm_save({ count: picked.length })}
  onConfirm={save}
>
  <div class="list">
    <div class="row head">
      <span></span><span>{m.clm_mask()}</span><span>{m.clm_kind()}</span><span>{m.clm_about()}</span>
    </div>
    {#each offered as o (o.mask)}
      <label class="row" class:risky={!o.default_on}>
        <input type="checkbox" checked={on.has(lower(o.mask))} onchange={() => toggle(o.mask)} />
        <span class="mask">{o.mask}</span>
        <span class="kind">{o.kind_label}</span>
        <span class="about">{o.about}</span>
      </label>
    {/each}
    {#each own as o (o)}
      <label class="row">
        <input type="checkbox" checked={on.has(lower(o))} onchange={() => toggle(o)} />
        <span class="mask">{o}</span>
        <span class="kind">{m.clm_own()}</span>
        <span class="about">
          <button class="x" title={m.clm_remove()} aria-label={m.clm_remove()} onclick={() => removeOwn(o)}>×</button>
        </span>
      </label>
    {/each}
  </div>
  <div class="add">
    <input
      class="form-input"
      placeholder={m.clm_add_placeholder()}
      bind:value={adding}
      onkeydown={(e) => e.key === 'Enter' && add()}
    />
    <button class="btn-add" onclick={add}>{m.clm_add()}</button>
    <button class="linkish" onclick={defaults}>{m.cl_junk_defaults()}</button>
  </div>
  {#if error}<p class="err">{error}</p>{/if}
  <p class="note">{m.clm_note()}</p>
</Modal>

<style>
  .list { max-height: 52vh; overflow: auto; border: 1px solid var(--border); border-radius: 6px; }
  .row { display: grid; grid-template-columns: 28px 150px 120px 1fr; gap: 10px; align-items: center; padding: 5px 10px; font-size: 13px; border-bottom: 1px solid var(--border); color: var(--text); cursor: pointer; }
  .row:last-child { border-bottom: none; }
  .row.head { position: sticky; top: 0; background: var(--panel2); font-size: 12px; font-weight: 600; color: var(--text-muted); cursor: default; }
  .row input { width: auto; margin: 0; }
  .mask { font-family: var(--mono, monospace); }
  .kind { color: var(--text-secondary); }
  .row.risky .kind { color: var(--warning-text, var(--text-secondary)); }
  .about { color: var(--text-secondary); font-size: 12.5px; }
  .add { display: flex; gap: 10px; align-items: center; margin-top: 10px; }
  .add .form-input { flex: 0 1 260px; }
  .btn-add { background: transparent; border: 1px solid var(--primary); color: var(--accent-text); border-radius: 6px; padding: 7px 14px; font-size: 13px; font-weight: 600; cursor: pointer; font-family: var(--font); }
  .linkish { background: none; border: none; padding: 0; font-size: 12.5px; color: var(--accent-text); cursor: pointer; font-family: var(--font); margin-left: auto; }
  .x { background: none; border: none; font-size: 16px; line-height: 1; color: var(--text-muted); cursor: pointer; padding: 0 4px; }
  .x:hover { color: var(--error-text); }
  .err { font-size: 13px; color: var(--error-text); margin: 8px 0 0; }
  .note { font-size: 12px; color: var(--text-muted); margin: 10px 0 0; }
</style>
