<script lang="ts">
  // «Автоочистка»: what the service cleans by itself — on a schedule, and
  // before the copy of a job that asks («Очистить диск перед копией»). The set
  // is what is ticked on the page now; junk by masks is never part of it
  // (those files go only from a list someone looked through).
  import { untrack } from 'svelte';
  import Modal from './Modal.svelte';
  import { m } from '../lib/paraglide/messages';
  import { isWindows } from '../lib/platformStore.svelte';
  import type { CleanupProfile } from '../lib/types';

  interface Props {
    profile: CleanupProfile;
    /** The ticked items (ids and names) and users the set would take. */
    items: { id: string; label: string }[];
    users: { sid: string; name: string }[];
    /** Ticked items left out: cleaned only after a look at their list. */
    left: string[];
    saving: boolean;
    error: string | null;
    onSave: (schedule: CleanupProfile['schedule']) => void;
    onClose: () => void;
  }

  let { profile, items, users, left, saving, error, onSave, onClose }: Props = $props();

  // The window edits a copy of the schedule.
  let schedule = $state(untrack(() => ({ ...profile.schedule, days: [...profile.schedule.days] })));

  const kinds: [string, () => string][] = [
    ['none', m.cla_off],
    ['daily', m.wiz_sched_daily],
    ['weekly', m.wiz_sched_weekly],
    ['monthly', m.wiz_sched_monthly],
  ];
  const weekdays: [string, () => string][] = [
    ['mon', m.wiz_day_mon], ['tue', m.wiz_day_tue], ['wed', m.wiz_day_wed], ['thu', m.wiz_day_thu],
    ['fri', m.wiz_day_fri], ['sat', m.wiz_day_sat], ['sun', m.wiz_day_sun],
  ];

  function toggleDay(d: string) {
    schedule.days = schedule.days.includes(d) ? schedule.days.filter((x) => x !== d) : [...schedule.days, d];
  }

  const timed = $derived(schedule.kind === 'daily' || schedule.kind === 'weekly' || schedule.kind === 'monthly');
  const nothing = $derived(items.length === 0);
  const noDays = $derived(schedule.kind === 'weekly' && schedule.days.length === 0);
</script>

<Modal
  open
  wide
  title={m.cla_title()}
  {onClose}
  cancelLabel={m.common_cancel()}
  confirmLabel={m.cla_save()}
  confirmDisabled={saving || noDays || (nothing && schedule.kind !== 'none')}
  onConfirm={() => onSave(schedule)}
>
  <p class="lead">{m.cla_lead()}</p>

  <div class="sec">{m.cla_when()}</div>
  <div class="kinds">
    {#each kinds as [k, lbl] (k)}
      <label class="radio">
        <input type="radio" name="cla-kind" value={k} checked={schedule.kind === k} onchange={() => (schedule.kind = k as CleanupProfile['schedule']['kind'])} />
        {lbl()}
      </label>
    {/each}
  </div>
  {#if schedule.kind === 'weekly'}
    <div class="days">
      {#each weekdays as [d, lbl] (d)}
        <button class="day" class:on={schedule.days.includes(d)} onclick={() => toggleDay(d)}>{lbl()}</button>
      {/each}
    </div>
  {/if}
  {#if timed}
    <div class="row">
      {#if schedule.kind === 'monthly'}
        <label>{m.wiz_sched_day_of_month()}
          <select class="form-input" bind:value={schedule.day_of_month}>
            {#each Array.from({ length: 28 }, (_, i) => i + 1) as d (d)}<option value={d}>{d}</option>{/each}
          </select>
        </label>
      {/if}
      <label>{m.wiz_sched_at()} <input class="form-input" type="time" bind:value={schedule.time} /></label>
    </div>
    <label class="chk"><input type="checkbox" bind:checked={schedule.run_missed} /> {m.cla_run_missed()}</label>
  {/if}

  <div class="sec">{m.cla_what()}</div>
  {#if nothing}
    <p class="warn">{m.cla_nothing()}</p>
  {:else}
    <p class="what">{items.map((i) => i.label).join(', ')}</p>
    <p class="what muted">{m.cla_users({ names: users.map((u) => u.name).join(', ') || '—' })}</p>
  {/if}
  {#if left.length > 0}<p class="what muted">{m.cla_left({ names: left.join(', ') })}</p>{/if}
  <p class="note">{isWindows() ? m.cla_note() : m.cla_note_linux()}</p>
  {#if profile.last_run && profile.schedule.kind !== 'none'}
    <p class="note">{m.cla_last({ when: new Date(profile.last_run).toLocaleString() })}</p>
  {/if}
  {#if error}<p class="err">{error}</p>{/if}
</Modal>

<style>
  .lead { font-size: 13px; color: var(--text-secondary); margin: 0 0 12px; line-height: 1.5; }
  .sec { font-size: 12px; font-weight: 600; text-transform: uppercase; letter-spacing: .04em; color: var(--text-muted); margin: 14px 0 8px; }
  .kinds { display: flex; gap: 18px; flex-wrap: wrap; }
  .radio, .chk { display: flex; align-items: center; gap: 6px; font-size: 14px; color: var(--text); cursor: pointer; }
  .radio input, .chk input { width: auto; margin: 0; }
  .chk { margin-top: 10px; }
  .days { display: inline-flex; border: 1px solid var(--border-strong); border-radius: 6px; overflow: hidden; margin-top: 10px; }
  .day { padding: 7px 11px; border: none; border-right: 1px solid var(--border); background: var(--card-bg); color: var(--text); font-size: 13px; cursor: pointer; font-family: var(--font); }
  .day:last-child { border-right: none; }
  .day.on { background: var(--primary); color: #fff; }
  .row { display: flex; gap: 16px; margin-top: 10px; align-items: center; }
  .row label { display: flex; align-items: center; gap: 8px; font-size: 14px; color: var(--text); }
  .row .form-input { width: auto; }
  .what { font-size: 13px; color: var(--text); margin: 0 0 6px; line-height: 1.5; }
  .muted { color: var(--text-secondary); }
  .warn { font-size: 13px; color: var(--warning-text, var(--text)); margin: 0; }
  .note { font-size: 12px; color: var(--text-muted); margin: 8px 0 0; line-height: 1.5; }
  .err { font-size: 13px; color: var(--error-text); margin: 10px 0 0; }
</style>
