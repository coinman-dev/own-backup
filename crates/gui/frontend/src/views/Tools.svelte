<script lang="ts">
  import { open } from '@tauri-apps/plugin-dialog';
  import { api } from '../lib/api';
  import { engine, runVerify } from '../lib/engineStore.svelte';

  let mountPath = $state('');
  let mountMsg: string | null = $state(null);
  let mountErr: string | null = $state(null);
  let mountBusy = $state(false);

  let verifyPath = $state('');
  let verifyMsg: string | null = $state(null);
  let verifyErr: string | null = $state(null);
  let verifyBusy = $state(false);
  const verifyProgress = $derived(engine.verifyProgress);

  async function browse(setPath: (p: string) => void, extensions = ['vhdx', 'avhdx', 'qcow2']) {
    try {
      const picked = await open({
        title: 'Выберите образ',
        multiple: false,
        filters: [{ name: 'Образ резервной копии', extensions }],
      });
      if (typeof picked === 'string') setPath(picked);
    } catch {
      /* dialog cancelled */
    }
  }

  async function mount() {
    mountBusy = true;
    mountMsg = null;
    mountErr = null;
    // qcow2 mounts through the Arsenal Image Mounter driver (must be installed);
    // the service holds the connection open. VHDX mounts natively via vhdmp.
    try {
      const physical = await api.mountBackup(mountPath);
      mountMsg = `Подключено (только чтение): ${physical}. Буквы назначит Windows — смотрите проводник.`;
    } catch (e) {
      mountErr = String(e);
    } finally {
      mountBusy = false;
    }
  }

  async function unmount() {
    mountBusy = true;
    mountMsg = null;
    mountErr = null;
    try {
      await api.unmountBackup(mountPath);
      mountMsg = 'Образ отключён.';
    } catch (e) {
      mountErr = String(e);
    } finally {
      mountBusy = false;
    }
  }

  async function verify() {
    verifyBusy = true;
    verifyMsg = null;
    verifyErr = null;
    try {
      const v = await runVerify(verifyPath);
      verifyMsg = v.valid
        ? `Целостность подтверждена: ${v.blocks_checked} блоков за ${v.duration_secs.toFixed(1)} с.`
        : `ОШИБКА: повреждено блоков — ${v.mismatched_blocks.length} из ${v.blocks_checked}.`;
    } catch (e) {
      verifyErr = String(e);
    } finally {
      verifyBusy = false;
    }
  }
</script>

<div class="page">
  <h1>Инструменты</h1>

  <div class="tool-card">
    <div class="tool-head">
      <span class="glyph">💽</span>
      <div>
        <div class="tool-title">Подключить образ как диск</div>
        <div class="tool-desc">
          Образ подключается только для чтения; тома внутри получают буквы и
          открываются в проводнике.
        </div>
      </div>
    </div>
    <div class="row">
      <input
        type="text"
        bind:value={mountPath}
        placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
        spellcheck="false"
        disabled={mountBusy}
      />
      <button class="btn outline" onclick={() => browse((p) => (mountPath = p), ['vhdx', 'avhdx', 'qcow2'])} disabled={mountBusy}>
        Обзор…
      </button>
      <button class="btn primary" onclick={mount} disabled={mountBusy || !mountPath}>
        Подключить
      </button>
      <button class="btn outline" onclick={unmount} disabled={mountBusy || !mountPath}>
        Отключить
      </button>
    </div>
    {#if mountMsg}<div class="banner ok">{mountMsg}</div>{/if}
    {#if mountErr}<div class="banner err">{mountErr}</div>{/if}
  </div>

  <div class="tool-card">
    <div class="tool-head">
      <span class="glyph">🏅</span>
      <div>
        <div class="tool-title">Проверить образ</div>
        <div class="tool-desc">
          Сверяет контрольные суммы каждого блока с файлом метаданных (.obkm).
        </div>
      </div>
    </div>
    <div class="row">
      <input
        type="text"
        bind:value={verifyPath}
        placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
        spellcheck="false"
        disabled={verifyBusy}
      />
      <button class="btn outline" onclick={() => browse((p) => (verifyPath = p))} disabled={verifyBusy}>
        Обзор…
      </button>
      <button class="btn primary" onclick={verify} disabled={verifyBusy || !verifyPath}>
        {verifyBusy ? 'Проверка…' : 'Проверить'}
      </button>
    </div>
    {#if verifyBusy && verifyProgress}
      <div class="bar"><div class="bar-fill" style="width:{verifyProgress.percent}%"></div></div>
    {/if}
    {#if verifyMsg}<div class="banner ok">{verifyMsg}</div>{/if}
    {#if verifyErr}<div class="banner err">{verifyErr}</div>{/if}
  </div>

  <div class="tool-card muted">
    <div class="tool-head">
      <span class="glyph">💿</span>
      <div>
        <div class="tool-title">Загрузочный носитель</div>
        <div class="tool-desc">Создание аварийного USB-носителя восстановления — в разработке.</div>
      </div>
    </div>
  </div>
</div>

<style>
  .page {
    padding: 30px 44px;
    max-width: 860px;
    overflow: auto;
    height: 100%;
  }

  h1 {
    font-size: 28px;
    font-weight: 300;
    color: var(--head);
    margin-bottom: 24px;
  }

  .tool-card {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--card-bg);
    box-shadow: var(--shadow);
    padding: 20px 22px;
    margin-bottom: 16px;
  }

  .tool-card.muted {
    opacity: 0.6;
  }

  .tool-head {
    display: flex;
    gap: 14px;
    align-items: flex-start;
    margin-bottom: 14px;
  }

  .glyph {
    font-size: 22px;
    color: var(--primary);
  }

  .tool-title {
    font-size: 15px;
    font-weight: 600;
    color: var(--head);
  }

  .tool-desc {
    font-size: 12px;
    color: var(--text-muted);
    margin-top: 3px;
    line-height: 1.5;
  }

  .row {
    display: flex;
    gap: 10px;
  }

  .row input {
    flex: 1;
    padding: 9px 12px;
    border: 1px solid var(--border);
    border-radius: 4px;
    font-size: 13px;
    font-family: var(--font);
  }

  .btn {
    border-radius: 4px;
    padding: 9px 20px;
    font-size: 13px;
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

  .btn.primary {
    background: var(--primary);
    color: #fff;
  }

  .btn.outline {
    background: transparent;
    border: 1px solid var(--primary);
    color: var(--primary);
  }

  .bar {
    height: 8px;
    border-radius: 4px;
    background: var(--tabbg);
    overflow: hidden;
    margin-top: 12px;
  }

  .bar-fill {
    height: 100%;
    background: var(--green);
    transition: width 0.15s linear;
  }

  .banner {
    margin-top: 12px;
    border-radius: 6px;
    padding: 11px 14px;
    font-size: 13px;
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
</style>
