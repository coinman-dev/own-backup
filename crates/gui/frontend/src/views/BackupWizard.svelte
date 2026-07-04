<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
  import { api } from '../lib/api';
  import { engine, volumeLabel } from '../lib/engineStore.svelte';
  import { saveJob, runJob } from '../lib/jobStore.svelte';
  import InfoModal from '../components/InfoModal.svelte';
  import type { Job } from '../lib/types';

  interface Props {
    mode: 'wizard' | 'edit';
    // Shared draft owned by the parent (BackupSection) so the name is also
    // editable inline in the jobs list. Cancel discards it in the parent.
    job: Job;
    onCancel: () => void;
    onDone: (job: Job) => void;
  }
  let { mode, job, onCancel, onDone }: Props = $props();

  let step = $state(0);
  let error: string | null = $state(null);
  let stubMsg: string | null = $state(null);
  const stub = () =>
    (stubMsg = 'Эта настройка сохраняется, но пока не применяется — реализация позже.');

  const steps = [
    { key: 'source', label: 'Источник' },
    { key: 'schedule', label: 'Расписание' },
    { key: 'scheme', label: 'Схема' },
    { key: 'exclusions', label: 'Исключения' },
    { key: 'advanced', label: 'Дополнительно' },
  ];

  const weekdays = [
    ['mon', 'Пн'], ['tue', 'Вт'], ['wed', 'Ср'], ['thu', 'Чт'],
    ['fri', 'Пт'], ['sat', 'Сб'], ['sun', 'Вс'],
  ];
  const scheduleKinds = [
    ['daily', 'Ежедневно'], ['weekly', 'Еженедельно'], ['monthly', 'Ежемесячно'],
    ['event', 'По событию'], ['nonstop', 'Непрерывно'], ['none', 'Не планировать'],
  ];

  function toggleDay(d: string) {
    job.schedule.days = job.schedule.days.includes(d)
      ? job.schedule.days.filter((x) => x !== d)
      : [...job.schedule.days, d];
  }

  function pickSource(dev: string) {
    job.source = dev;
    job.source_label = volumeLabel(dev);
  }

  async function browseDest() {
    try {
      const picked = await open({ directory: true, title: 'Папка для образов' });
      if (typeof picked === 'string') job.dest_folder = picked;
    } catch (e) {
      error = String(e);
    }
  }

  // Exclusions
  let newPattern = $state('');
  function addPattern() {
    const p = newPattern.trim();
    if (p) {
      job.exclusions.patterns = [...job.exclusions.patterns, p];
      newPattern = '';
    }
  }
  function removePattern(i: number) {
    job.exclusions.patterns = job.exclusions.patterns.filter((_, idx) => idx !== i);
  }
  async function resetExclusions() {
    job.exclusions.patterns = await api.defaultExclusionPatterns();
  }

  // Uppercase drive letter of a path/device, or null (UNC/no letter).
  function driveLetter(s: string): string | null {
    const t = s.trim().replace(/^\\\\[.?]\\/, '');
    return t.length >= 2 && /[a-zA-Z]/.test(t[0]) && t[1] === ':'
      ? t[0].toUpperCase()
      : null;
  }

  const sameDrive = $derived.by(() => {
    const src = driveLetter(job.source);
    const dst = driveLetter(job.dest_folder);
    return !!(src && dst && src === dst);
  });

  function validate(): boolean {
    if (!job.name.trim()) {
      error = 'Укажите имя задания.';
      step = 0;
      return false;
    }
    if (!job.source) {
      error = 'Выберите источник (том).';
      step = 0;
      return false;
    }
    if (!job.dest_folder.trim()) {
      error = 'Выберите папку назначения.';
      step = 0;
      return false;
    }
    const src = driveLetter(job.source);
    const dst = driveLetter(job.dest_folder);
    if (src && dst && src === dst) {
      error = `Нельзя сохранять копию на тот же диск, который копируется (${src}:). Выберите для назначения другой диск.`;
      step = 0;
      return false;
    }
    error = null;
    return true;
  }

  async function finish(run: boolean) {
    if (!validate()) return;
    try {
      const saved = await saveJob(job);
      if (run) await runJob(saved.id);
      onDone(saved);
    } catch (e) {
      error = String(e);
    }
  }

  // Default the source to the first volume in wizard mode.
  $effect(() => {
    if (mode === 'wizard' && !job.source && engine.volumes.length > 0) {
      pickSource(engine.volumes[0].device);
    }
  });
</script>

<div class="wizard">
    <div class="wz-head">
      <span class="wz-title">Параметры резервного копирования диска</span>
      <span class="wz-name">{job.name}</span>
    </div>

    <!-- tab bar -->
    <div class="tabs">
      {#each steps as s, i}
        <button class="tab" class:active={step === i} onclick={() => (step = i)}>{s.label}</button>
      {/each}
    </div>

    <div class="body">
      <!-- 0: source → dest -->
      {#if step === 0}
        <div class="flow">
          <div class="card">
            <svg width="80" height="80" viewBox="0 0 24 24" fill="none" stroke="#9aa0b4" stroke-width="1.1"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="4.4"/><circle cx="12" cy="12" r="1" fill="#9aa0b4"/></svg>
            <div class="card-title">Источник</div>
            {#if engine.volumesLoading && engine.volumes.length === 0}
              <div class="loading-note">Загрузка списка томов…</div>
            {:else}
              <select value={job.source} onchange={(e) => pickSource((e.target as HTMLSelectElement).value)}>
                <option value="" disabled>— выберите том —</option>
                {#each engine.volumes as v}<option value={v.device}>{v.label}</option>{/each}
              </select>
            {/if}
          </div>
          <div class="arrow">→</div>
          <div class="card">
            <svg width="80" height="74" viewBox="0 0 24 22" fill="#e6b34a"><path d="M2 5a2 2 0 0 1 2-2h5l2 2.4h7a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2z"/></svg>
            <div class="card-title">Папка назначения</div>
            <input type="text" bind:value={job.dest_folder} placeholder="D:\OwnBackup" spellcheck="false" />
            <button class="link" onclick={browseDest}>Обзор…</button>
          </div>
        </div>
        <div class="note">Каждый запуск создаёт файл <code>&lt;имя&gt;_&lt;буква&gt;_&lt;дата&gt;.vhdx</code> в этой папке.</div>
        {#if sameDrive}
          <div class="warn-note">⛔ Назначение на том же диске ({driveLetter(job.source)}:), что и источник. Копию нужно писать на другой диск — выберите другую папку.</div>
        {/if}
      {/if}

      <!-- 1: schedule -->
      {#if step === 1}
        <div class="sched">
          <div class="sched-kinds">
            {#each scheduleKinds as [k, lbl]}
              <label class="radio"><input type="radio" name="skind" value={k} checked={job.schedule.kind === k} onchange={() => (job.schedule.kind = k as Job['schedule']['kind'])} /> {lbl}</label>
            {/each}
          </div>
          <div class="sched-right">
            {#if job.schedule.kind === 'weekly'}
              <div class="days">
                {#each weekdays as [d, lbl]}
                  <button class="day" class:on={job.schedule.days.includes(d)} onclick={() => toggleDay(d)}>{lbl}</button>
                {/each}
              </div>
            {/if}
            {#if job.schedule.kind === 'daily' || job.schedule.kind === 'weekly' || job.schedule.kind === 'monthly'}
              <label class="at">В <input type="time" bind:value={job.schedule.time} /></label>
            {/if}
          </div>
        </div>
        <div class="dev-note">⚙ Автозапуск по расписанию — в разработке (сохраняется, но пока не срабатывает).</div>
        <div class="adv-schedule">
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.only_when_locked} onchange={stub} /> Копировать только если компьютер заблокирован</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.wake_to_run} onchange={stub} /> Пробуждать компьютер из спящего режима</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.prevent_sleep} onchange={stub} /> Предотвращать переход в спящий режим</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.run_missed} onchange={stub} /> Выполнять пропущенные операции при запуске</label>
        </div>
      {/if}

      <!-- 2: scheme -->
      {#if step === 2}
        <div class="field-label">Метод резервного копирования:</div>
        <select bind:value={job.scheme.method} class="wide">
          <option value="full">Полное</option>
          <option value="incremental">Инкрементное</option>
          <option value="differential">Дифференциальное</option>
        </select>
        {#if job.scheme.method === 'incremental'}
          <div class="note">Каждый запуск сохраняет только изменения относительно предыдущей версии (цепочка полная → И1 → И2 …).</div>
        {:else if job.scheme.method === 'differential'}
          <div class="note">Каждый запуск сохраняет изменения относительно полной копии (полная → Д1, Д2 …).</div>
        {/if}
        {#if job.scheme.method !== 'full'}
          <label class="radio blk"><input type="radio" checked={job.scheme.only_incremental} onchange={() => (job.scheme.only_incremental = true)} /> Только инкрементные версии после первой полной</label>
          <label class="radio blk"><input type="radio" checked={!job.scheme.only_incremental} onchange={() => (job.scheme.only_incremental = false)} /> Полная версия после каждых <input type="number" min="1" bind:value={job.scheme.full_every_n} class="num" /> версий</label>
        {/if}
        <div class="field-label mt">Правила очистки старых версий:</div>
        <label class="chk"><input type="checkbox" checked={job.scheme.keep_chains !== null} onchange={(e) => (job.scheme.keep_chains = (e.target as HTMLInputElement).checked ? 3 : null)} /> Хранить не более <input type="number" min="1" value={job.scheme.keep_chains ?? 3} oninput={(e) => (job.scheme.keep_chains = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.keep_chains === null} /> последних цепочек</label>
        <label class="chk"><input type="checkbox" checked={job.scheme.cleanup_older_than_days !== null} onchange={(e) => (job.scheme.cleanup_older_than_days = (e.target as HTMLInputElement).checked ? 90 : null)} /> Удалять цепочки старше <input type="number" min="1" value={job.scheme.cleanup_older_than_days ?? 90} oninput={(e) => (job.scheme.cleanup_older_than_days = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.cleanup_older_than_days === null} /> дней</label>
        <label class="chk"><input type="checkbox" bind:checked={job.scheme.keep_first} /> Не удалять первую (самую старую) цепочку</label>
        <div class="note">Цепочка удаляется целиком (полная копия вместе с её инкрементами) после успешного завершения очередного копирования. Цепочка, к которой относится свежая копия, не удаляется никогда.</div>
      {/if}

      <!-- 3: exclusions -->
      {#if step === 3}
        <label class="chk"><input type="checkbox" bind:checked={job.exclusions.enabled} /> Исключить файлы по следующим критериям:</label>
        <div class="note">Маска без пути (<code>*.tmp</code>, <code>pagefile.sys</code>) действует по всему тому; маска с путём (<code>Temp\*.log</code>) — от корня тома. Работает для NTFS; в посекторном режиме исключения не применяются.</div>
        <div class="excl-list">
          {#each job.exclusions.patterns as p, i}
            <div class="excl-row"><span>{p}</span><button class="x" onclick={() => removePattern(i)}>×</button></div>
          {/each}
        </div>
        <div class="excl-add">
          <input type="text" bind:value={newPattern} placeholder="Путь или маска, напр. *.tmp" spellcheck="false" onkeydown={(e) => e.key === 'Enter' && addPattern()} />
          <button class="btn small" onclick={addPattern}>+ Добавить</button>
          <button class="link" onclick={resetExclusions}>Исходный список</button>
        </div>
      {/if}

      <!-- 4: advanced -->
      {#if step === 4}
        <div class="adv">
          <div class="adv-group">
            <div class="adv-title">Режим создания образа</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.sector_by_sector} /> Архивировать в посекторном режиме <span class="ok-badge">работает</span></label>
          </div>
          <div class="adv-group">
            <div class="adv-title">Проверка</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.validate_after} /> Проверять резервную копию после создания <span class="ok-badge">работает</span></label>
          </div>
          <button class="adv-row" onclick={stub}><span>🔒 Защита резервной копии (пароль)</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⌘ Pre/Post-команды</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⚠ Обработка ошибок</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⏻ Выключение компьютера</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⏱ Производительность</span><span class="dev-badge">в разработке</span></button>
        </div>
      {/if}
    </div>

    {#if error}<div class="wz-error">{error}</div>{/if}

    <!-- footer -->
    <div class="footer">
      <div class="spacer"></div>
      <button class="btn ghost" onclick={onCancel}>Отмена</button>
      {#if mode === 'wizard'}
        {#if step > 0}<button class="btn ghost" onclick={() => (step -= 1)}>Назад</button>{/if}
        {#if step < steps.length - 1}
          <button class="btn primary" onclick={() => (step += 1)}>Дальше</button>
        {:else}
          <button class="btn green" onclick={() => finish(true)}>Создать копию</button>
        {/if}
      {:else}
        <button class="btn primary" onclick={() => finish(false)}>Сохранить</button>
      {/if}
    </div>
</div>

{#if stubMsg}<InfoModal message={stubMsg} onClose={() => (stubMsg = null)} />{/if}

<style>
  .wizard { height: 100%; background: var(--content-bg); display: flex; flex-direction: column; overflow: hidden; }
  .wz-head { padding: 22px 34px 14px; display: flex; align-items: baseline; gap: 14px; }
  .wz-title { font-size: 24px; font-weight: 300; color: var(--head); }
  .wz-name { font-size: 13px; color: var(--text-muted); }
  .tabs { display: flex; padding: 0 34px; border-bottom: 1px solid var(--border); }
  .tab { padding: 10px 4px 12px; margin-right: 28px; background: none; border: none; border-bottom: 3px solid transparent; color: var(--text-muted); font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; }
  .tab.active { color: var(--head); border-bottom-color: var(--primary); }
  .body { flex: 1; overflow: auto; padding: 26px 34px; }
  .flow { display: flex; align-items: stretch; justify-content: center; gap: 24px; }
  .card { flex: 1; max-width: 300px; border: 1px solid var(--border); border-radius: 6px; padding: 20px 18px; text-align: center; background: var(--card-bg); display: flex; flex-direction: column; align-items: center; gap: 10px; }
  .card-title { font-size: 16px; font-weight: 600; color: var(--head); }
  .card select, .card input, select.wide { width: 100%; padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: #fff; color: var(--text); }
  .arrow { display: flex; align-items: center; color: var(--primary); font-size: 26px; }
  .link { background: none; border: none; color: var(--primary); font-size: 13px; cursor: pointer; padding: 0; }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .note, .dev-note { font-size: 12px; color: var(--text-muted); margin-top: 18px; }
  .dev-note { color: #a06a10; background: rgba(251,188,5,.1); padding: 8px 12px; border-radius: 5px; }
  .warn-note { font-size: 13px; color: #8c1d1d; background: rgba(216,74,74,.1); padding: 10px 14px; border-radius: 5px; margin-top: 14px; font-weight: 600; }
  code { background: var(--panel2); padding: 1px 5px; border-radius: 3px; }
  .sched { display: flex; gap: 50px; }
  .sched-kinds { display: flex; flex-direction: column; gap: 14px; }
  .sched-right { display: flex; flex-direction: column; gap: 16px; padding-top: 2px; }
  .radio { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--head); cursor: pointer; }
  .radio.blk { margin: 10px 0; }
  .days { display: flex; border: 1px solid var(--border); border-radius: 5px; overflow: hidden; }
  .day { padding: 9px 12px; border: none; border-right: 1px solid var(--border); background: #fff; color: var(--head); font-size: 13px; cursor: pointer; font-family: var(--font); }
  .day.on { background: var(--primary); color: #fff; }
  .at { display: flex; align-items: center; gap: 10px; font-size: 14px; color: var(--head); }
  .at input { padding: 7px 10px; border: 1px solid var(--border); border-radius: 4px; }
  .adv-schedule { margin-top: 20px; display: flex; flex-direction: column; gap: 12px; }
  .chk { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--head); cursor: pointer; }
  .field-label { font-size: 15px; font-weight: 600; color: var(--head); margin-bottom: 10px; }
  .field-label.mt { margin-top: 26px; }
  .num { width: 60px; padding: 5px 8px; border: 1px solid var(--border); border-radius: 4px; margin: 0 4px; }
  .excl-list { border: 1px solid var(--border); border-radius: 5px; background: #fff; max-height: 260px; overflow: auto; margin-top: 12px; }
  .excl-row { display: flex; justify-content: space-between; align-items: center; padding: 6px 14px; font-size: 13px; color: var(--head); border-bottom: 1px solid var(--border); }
  .x { background: none; border: none; color: var(--text-muted); font-size: 18px; cursor: pointer; line-height: 1; }
  .excl-add { display: flex; gap: 10px; align-items: center; margin-top: 12px; }
  .excl-add input { flex: 1; padding: 8px 12px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; }
  .adv { display: flex; flex-direction: column; gap: 4px; }
  .adv-group { padding: 12px 0; border-bottom: 1px solid var(--border); }
  .adv-title { font-size: 13px; font-weight: 600; color: var(--primary); margin-bottom: 8px; }
  .adv-row { display: flex; align-items: center; justify-content: space-between; padding: 14px 4px; border-bottom: 1px solid var(--border); background: none; border-top: none; border-left: none; border-right: none; cursor: pointer; font-size: 14px; color: var(--head); font-family: var(--font); width: 100%; }
  .dev-badge { font-size: 11px; color: #a06a10; background: rgba(251,188,5,.15); padding: 3px 9px; border-radius: 10px; }
  .ok-badge { font-size: 11px; color: #2c5b12; background: rgba(90,176,47,.15); padding: 2px 8px; border-radius: 10px; margin-left: 6px; }
  .wz-error { padding: 8px 34px; color: #8c1d1d; font-size: 13px; background: rgba(216,74,74,.08); }
  .footer { display: flex; align-items: center; gap: 12px; padding: 14px 34px; border-top: 1px solid var(--border); background: var(--card-bg); }
  .spacer { flex: 1; }
  .btn { border-radius: 4px; padding: 10px 26px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn.small { padding: 7px 14px; font-size: 13px; }
  .btn.ghost { background: none; border: 1px solid var(--border); color: var(--head); }
  .btn.primary { background: var(--primary); color: #fff; }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover { background: var(--green-hover); }
</style>
