<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
  import { api } from '../lib/api';
  import { formatBytes, formatDate } from '../lib/format';
  import { engine, runRestore, cancelCurrent, clearRestoreResult } from '../lib/engineStore.svelte';
  import type { ImageInfo } from '../lib/types';

  let imagePath = $state('');
  let target = $state('');
  let info: ImageInfo | null = $state(null);
  let infoError: string | null = $state(null);

  let confirmOpen = $state(false);
  let confirmText = $state('');

  const running = $derived(engine.op === 'restore');
  const progress = $derived(engine.restoreProgress);
  const summary = $derived(engine.restoreSummary);
  let formError: string | null = $state(null);
  const errorMsg = $derived(formError ?? engine.restoreError);

  async function browseImage() {
    try {
      const picked = await open({
        title: 'Выберите образ',
        multiple: false,
        filters: [{ name: 'Образ VHDX', extensions: ['vhdx', 'avhdx'] }],
      });
      if (typeof picked === 'string') {
        imagePath = picked;
        await loadInfo();
      }
    } catch (e) {
      infoError = String(e);
    }
  }

  async function loadInfo() {
    info = null;
    infoError = null;
    clearRestoreResult();
    try {
      info = await api.readImageInfo(imagePath);
    } catch (e) {
      infoError = String(e);
    }
  }

  function askConfirm() {
    if (!info || !target) {
      formError = 'Загрузите образ и укажите целевой том.';
      return;
    }
    formError = null;
    confirmText = '';
    confirmOpen = true;
  }

  async function doRestore() {
    if (confirmText !== 'ДА') return;
    confirmOpen = false;
    await runRestore(imagePath, target);
  }
</script>

<div class="page">
  <div class="row">
    <label for="img">Образ (.vhdx):</label>
    <input
      id="img"
      type="text"
      bind:value={imagePath}
      placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
      spellcheck="false"
      disabled={running}
    />
    <button class="btn outline" onclick={browseImage} disabled={running}>Обзор…</button>
    <button class="btn outline" onclick={loadInfo} disabled={running || !imagePath}>
      Загрузить
    </button>
  </div>

  {#if infoError}
    <div class="banner err">{infoError}</div>
  {/if}

  {#if info}
    <div class="info-card">
      <div class="info-grid">
        <div><span class="k">Файл:</span> {info.image_file_name}</div>
        <div><span class="k">Создан:</span> {formatDate(info.created_at)}</div>
        <div><span class="k">Источник:</span> {info.source_device}</div>
        <div><span class="k">Размер тома:</span> {formatBytes(info.source_bytes)}</div>
        <div>
          <span class="k">Занято:</span>
          {info.used_blocks} из {info.total_blocks} блоков
          ({formatBytes(info.used_blocks * info.block_size)})
        </div>
        <div><span class="k">Режим:</span> {info.backup_mode} · {info.chain_role}</div>
      </div>
    </div>

    <div class="row">
      <label for="tgt">Восстановить в:</label>
      <select id="tgt" bind:value={target} disabled={running}>
        <option value="" disabled selected>— выберите том (будет ПОЛНОСТЬЮ перезаписан) —</option>
        {#each engine.volumes as v}
          <option value={v.device}>{v.label}</option>
        {/each}
      </select>
      {#if running}
        <button class="btn danger" onclick={cancelCurrent}>Отмена</button>
      {:else}
        <button class="btn primary-green" onclick={askConfirm}>Восстановить</button>
      {/if}
    </div>
  {/if}

  {#if running && progress}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>Восстановление… (контрольные суммы проверяются)</span>
        <span>
          {progress.blocks_done} / {progress.blocks_total} блоков ·
          {progress.speed_mbps.toFixed(1)} МБ/с
        </span>
      </div>
      <div class="bar"><div class="bar-fill" style="width:{progress.percent}%"></div></div>
    </div>
  {/if}

  {#if summary}
    <div class="banner ok">
      Восстановление завершено: {summary.blocks_restored} блоков,
      {formatBytes(summary.bytes_restored)} за {summary.duration_secs.toFixed(1)} с.
    </div>
  {/if}

  {#if errorMsg}
    <div class="banner err">{errorMsg}</div>
  {/if}

  <div class="note">
    Отдельные файлы: подключите образ как диск (Инструменты → Подключить) и
    скопируйте нужное в проводнике.
  </div>
</div>

{#if confirmOpen}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">Подтверждение восстановления</div>
      <p>
        Все данные на томе <b>{target}</b> будут безвозвратно перезаписаны
        содержимым образа <b>{info?.image_file_name}</b>.
      </p>
      <p>Введите <b>ДА</b> (заглавными), чтобы продолжить:</p>
      <input type="text" bind:value={confirmText} spellcheck="false" />
      <div class="dialog-actions">
        <button class="btn outline" onclick={() => (confirmOpen = false)}>Отмена</button>
        <button class="btn danger-solid" disabled={confirmText !== 'ДА'} onclick={doRestore}>
          Восстановить
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .page {
    padding: 30px 44px;
    max-width: 860px;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 18px;
  }

  .row label {
    font-size: 14px;
    color: var(--head);
    min-width: 130px;
  }

  .row input,
  .row select {
    flex: 1;
    padding: 9px 12px;
    border: 1px solid var(--border);
    border-radius: 4px;
    font-size: 13px;
    font-family: var(--font);
    background: #fff;
    color: var(--text);
  }

  .info-card {
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--panel2);
    padding: 16px 20px;
    margin-bottom: 20px;
  }

  .info-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px 24px;
    font-size: 13px;
    color: var(--text);
  }

  .k {
    color: var(--text-muted);
  }

  .progress-wrap {
    margin-top: 10px;
    margin-bottom: 16px;
  }

  .progress-info {
    display: flex;
    justify-content: space-between;
    font-size: 13px;
    color: var(--head);
    margin-bottom: 8px;
  }

  .bar {
    height: 8px;
    border-radius: 4px;
    background: var(--tabbg);
    overflow: hidden;
  }

  .bar-fill {
    height: 100%;
    background: var(--green);
    transition: width 0.15s linear;
  }

  .banner {
    border-radius: var(--radius);
    padding: 13px 16px;
    font-size: 13px;
    margin-bottom: 14px;
    word-break: break-all;
  }

  .banner.ok {
    background: rgba(90, 176, 47, 0.1);
    color: #2c5b12;
  }

  .banner.err {
    background: rgba(216, 74, 74, 0.08);
    color: #8c1d1d;
  }

  .note {
    margin-top: 20px;
    font-size: 12px;
    color: var(--text-muted);
  }

  .btn {
    border-radius: 4px;
    padding: 9px 22px;
    font-size: 14px;
    font-weight: 600;
    font-family: var(--font);
    cursor: pointer;
    border: none;
    white-space: nowrap;
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }

  .btn.outline {
    background: transparent;
    border: 1px solid var(--primary);
    color: var(--primary);
  }

  .btn.primary-green {
    background: var(--green);
    color: #fff;
  }

  .btn.danger {
    background: transparent;
    border: 1px solid var(--error);
    color: var(--error);
  }

  .btn.danger-solid {
    background: var(--error);
    color: #fff;
  }

  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(20, 17, 44, 0.5);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 100;
  }

  .dialog {
    width: 440px;
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
    font-size: 13px;
    color: var(--text);
    margin-bottom: 10px;
    line-height: 1.5;
  }

  .dialog input {
    width: 100%;
    padding: 9px 12px;
    border: 1px solid var(--border);
    border-radius: 4px;
    font-size: 14px;
    margin-bottom: 16px;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
  }
</style>
