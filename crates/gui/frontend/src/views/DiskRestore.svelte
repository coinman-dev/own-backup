<script lang="ts">
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';
  import { onMount, onDestroy } from 'svelte';
  import { api } from '../lib/api';
  import { engine } from '../lib/engineStore.svelte';
  import { jobs } from '../lib/jobStore.svelte';
  import { formatBytes, formatDate } from '../lib/format';
  import type { DiskBackup, DiskRestoreProgress } from '../lib/types';

  let backups: DiskBackup[] = $state([]);
  let loading = $state(true);
  let selected: DiskBackup | null = $state(null);
  let targetDisk: number | null = $state(null);
  let force = $state(false);
  let confirmText = $state('');

  let running = $state(false);
  let progress: DiskRestoreProgress | null = $state(null);
  let error: string | null = $state(null);
  let done: string | null = $state(null);

  let unlisten: UnlistenFn | null = null;

  // Distinct destination folders across all jobs → scan each for `.obdm`.
  async function reload() {
    loading = true;
    error = null;
    const folders = [...new Set(jobs.list.map((j) => j.dest_folder).filter(Boolean))];
    const all: DiskBackup[] = [];
    for (const f of folders) {
      try {
        all.push(...(await api.listDiskBackups(f)));
      } catch {
        /* folder gone / unreadable — skip */
      }
    }
    all.sort((a, b) => b.created_at.localeCompare(a.created_at));
    backups = all;
    if (selected && !all.some((b) => b.manifest_path === selected!.manifest_path)) selected = null;
    loading = false;
  }

  onMount(async () => {
    await reload();
    unlisten = await listen<DiskRestoreProgress>('restore-disk-progress', (e) => {
      progress = e.payload;
    });
  });
  onDestroy(() => unlisten?.());

  function diskNumber(deviceId: string): number {
    const m = deviceId.match(/(\d+)\s*$/);
    return m ? parseInt(m[1], 10) : 0;
  }
  // The target disk, if it is the SOURCE of the selected backup (restoring onto
  // the very disk that was backed up is fine, but worth flagging).
  const targetIsSource = $derived(
    selected != null && targetDisk != null && selected.disk_number === targetDisk,
  );

  const canRun = $derived(
    !running && selected != null && targetDisk != null && confirmText.trim() === 'ДА',
  );

  async function run() {
    if (!selected || targetDisk == null) return;
    error = null;
    done = null;
    progress = null;
    running = true;
    try {
      const s = await api.restoreDiskNow(selected.manifest_path, targetDisk, force);
      done = `Готово: ${s.partitions} раздел(ов), ${formatBytes(s.bytes_written)} за ${s.duration_secs.toFixed(0)} с.`;
    } catch (e) {
      error = String(e);
    } finally {
      running = false;
      progress = null;
      confirmText = '';
    }
  }
</script>

<div class="disk-restore">
  <div class="head">
    <h2>Восстановить диск целиком</h2>
    <p class="sub">
      Разворачивает таблицу разделов и все разделы из резервной копии диска
      (<code>.obdm</code>) на выбранный физический диск — система становится загрузочной.
    </p>
  </div>

  {#if loading}
    <div class="note">Поиск резервных копий диска…</div>
  {:else if backups.length === 0}
    <div class="note">
      Резервные копии диска не найдены. Создайте их в «Резервном копировании»,
      выбрав диск целиком (клик по шапке диска).
    </div>
  {:else}
    <!-- 1. pick a disk backup -->
    <div class="section-title">1 · Что восстанавливать</div>
    <div class="cards">
      {#each backups as b (b.manifest_path)}
        <button class="card" class:sel={selected?.manifest_path === b.manifest_path} onclick={() => (selected = b)}>
          <div class="card-top">
            <span class="model">💽 {b.disk_model || `Диск ${b.disk_number}`}</span>
            <span class="size">{formatBytes(b.total_bytes)}</span>
          </div>
          <div class="card-sub">{formatDate(b.created_at)} · {b.partitions.length} раздел(ов)</div>
          <div class="parts">
            {#each b.partitions as p}
              <span class="chip">{p.name || `#${p.index}`} · {p.filesystem}</span>
            {/each}
          </div>
        </button>
      {/each}
    </div>

    <!-- 2. pick a target disk -->
    <div class="section-title">2 · Куда восстанавливать <span class="danger">(диск будет полностью перезаписан)</span></div>
    <select class="target" bind:value={targetDisk} disabled={running}>
      <option value={null} disabled>— выберите целевой диск —</option>
      {#each engine.disks as d}
        <option value={diskNumber(d.device_id)}>
          Диск {diskNumber(d.device_id)} · {d.model || d.device_id} · {formatBytes(d.size_bytes)}
        </option>
      {/each}
    </select>
    {#if targetIsSource}
      <div class="warn">⚠ Это тот же диск, с которого сделана копия — данные на нём будут перезаписаны копией.</div>
    {/if}

    <label class="force"><input type="checkbox" bind:checked={force} disabled={running} /> Обойти защиту системного диска (опасно — только осознанно)</label>

    <!-- 3. confirm + run -->
    <div class="section-title">3 · Подтверждение</div>
    <div class="confirm-row">
      <span>Введите <b>ДА</b>, чтобы стереть целевой диск и восстановить:</span>
      <input class="confirm" bind:value={confirmText} placeholder="ДА" disabled={running} spellcheck="false" />
      <button class="btn danger" disabled={!canRun} onclick={run}>Восстановить диск</button>
    </div>

    {#if running}
      <div class="progress">
        <div class="pbar"><div class="pfill" style="width:{progress?.percent ?? 0}%"></div></div>
        <div class="pmeta">
          {#if progress?.phase === 'gpt'}Запись таблицы разделов…
          {:else if progress}Раздел {progress.partition_index + 1}/{progress.partition_count}: {progress.part_label} — {progress.percent}% ({formatBytes(progress.bytes_written)})
          {:else}Запуск…{/if}
        </div>
      </div>
    {/if}
    {#if done}<div class="ok">{done} Для чистого диска/нового железа выполните <code>bcdboot</code> из WinPE.</div>{/if}
    {#if error}<div class="err">{error}</div>{/if}
  {/if}
</div>

<style>
  .disk-restore { padding: 24px 28px; max-width: 900px; }
  .head h2 { font-size: 22px; font-weight: 300; color: var(--head); margin: 0 0 6px; }
  .sub { font-size: 13px; color: var(--text-muted); margin: 0 0 18px; line-height: 1.5; }
  .note { color: var(--text-muted); font-size: 14px; padding: 24px 0; }
  .section-title { font-size: 13px; font-weight: 700; color: var(--head); margin: 20px 0 10px; }
  .danger { color: var(--error); font-weight: 600; }
  .cards { display: flex; flex-direction: column; gap: 10px; }
  .card { text-align: left; border: 1px solid var(--border); border-radius: 8px; padding: 12px 14px; background: var(--card-bg); cursor: pointer; font-family: var(--font); }
  .card:hover { border-color: var(--primary); }
  .card.sel { border-color: var(--primary); background: var(--accent-soft); }
  .card-top { display: flex; justify-content: space-between; align-items: baseline; }
  .model { font-size: 14px; font-weight: 600; color: var(--head); }
  .size { font-size: 13px; color: var(--text-muted); }
  .card-sub { font-size: 12px; color: var(--text-muted); margin: 3px 0 8px; }
  .parts { display: flex; flex-wrap: wrap; gap: 6px; }
  .chip { font-size: 11px; color: var(--head); background: var(--panel2); border-radius: 10px; padding: 2px 8px; }
  .target { width: 100%; max-width: 560px; padding: 9px 10px; border: 1px solid var(--border); border-radius: 6px; font-size: 13px; font-family: var(--font); background: var(--card-bg); color: var(--text); }
  .warn { margin-top: 8px; font-size: 13px; color: #a06a10; background: rgba(251,188,5,.12); padding: 8px 12px; border-radius: 6px; }
  .force { display: flex; align-items: center; gap: 8px; font-size: 12px; color: var(--text-muted); margin-top: 10px; cursor: pointer; }
  .confirm-row { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; font-size: 13px; color: var(--head); }
  .confirm { width: 90px; padding: 8px 10px; border: 1px solid var(--border); border-radius: 6px; font-weight: 700; text-align: center; font-family: var(--font); }
  .btn { border: none; border-radius: 6px; padding: 10px 22px; font-size: 14px; font-weight: 600; cursor: pointer; font-family: var(--font); }
  .btn.danger { background: var(--error); color: #fff; }
  .btn.danger:disabled { opacity: .5; cursor: default; }
  .progress { margin-top: 18px; }
  .pbar { height: 8px; border-radius: 4px; background: var(--panel2); overflow: hidden; }
  .pfill { height: 100%; background: var(--primary); transition: width .2s; }
  .pmeta { font-size: 13px; color: var(--text-muted); margin-top: 8px; }
  .ok { margin-top: 16px; font-size: 13px; color: var(--green); background: rgba(90,176,47,.1); padding: 10px 14px; border-radius: 6px; }
  .err { margin-top: 16px; font-size: 13px; color: #8c1d1d; background: rgba(216,74,74,.1); padding: 10px 14px; border-radius: 6px; }
  code { background: var(--panel2); padding: 1px 5px; border-radius: 3px; }
</style>
