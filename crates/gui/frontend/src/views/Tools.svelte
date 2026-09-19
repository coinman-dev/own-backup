<script lang="ts">
  import { onMount } from 'svelte';
  import { pickPath } from '../lib/pickPath.svelte';
  import BootMediaCard from '../components/BootMediaCard.svelte';
  import { api } from '../lib/api';
  import { engine, runVerify, toolsState, refreshVolumes } from '../lib/engineStore.svelte';
  import type { ActiveMount } from '../lib/types';

  let mountMsg: string | null = $state(null);
  let mountErr: string | null = $state(null);
  let mountBusy = $state(false);

  let activeMounts: ActiveMount[] = $state([]);

  let verifyMsg: string | null = $state(null);
  let verifyErr: string | null = $state(null);
  let verifyBusy = $state(false);
  const verifyProgress = $derived(engine.verifyProgress);

  let isRefreshingDisks = $state(false);
  let hasLoadedOnce = false;

  const mountedItems = $derived(
    activeMounts.map(m => {
      // Prefer the letters the service resolved on the mounted disk; fall back
      // to the disk inventory, then to the bare disk number.
      let letters: string[] = (m.volume_letters ?? []).map(l => l.toUpperCase());
      if (letters.length === 0) {
        const disk = engine.disks.find(d =>
          d.device_id.toLowerCase() === m.physical_path.toLowerCase()
        );
        if (disk) {
          for (const p of disk.partitions) {
            if (p.volume_letter) {
              letters.push(p.volume_letter.toUpperCase() + ":");
            }
          }
        }
      }
      if (letters.length === 0) {
        const mMatch = m.physical_path.match(/PhysicalDrive(\d+)/i);
        letters.push(mMatch ? `Диск ${mMatch[1]}` : 'Диск');
      }
      const active = toolsState.mountPath.toLowerCase() === m.image_path.toLowerCase();
      return {
        image_path: m.image_path,
        letters,
        active
      };
    })
  );

  const isCurrentPathMounted = $derived(
    activeMounts.some(m => m.image_path.toLowerCase() === toolsState.mountPath.toLowerCase())
  );

  function mountedLettersFor(imagePath: string): string[] {
    const item = mountedItems.find(
      m => m.image_path.toLowerCase() === imagePath.toLowerCase()
    );
    return item ? item.letters : [];
  }

  async function loadData() {
    if (!hasLoadedOnce) {
      isRefreshingDisks = true;
    }
    try {
      const status = await api.getServiceStatus();
      activeMounts = status.active_mounts || [];
      hasLoadedOnce = true;
    } catch (e) {
      console.error("Failed to load active mounts:", e);
    } finally {
      isRefreshingDisks = false;
    }
  }

  onMount(() => {
    loadData();
    const interval = setInterval(loadData, 5000);
    return () => clearInterval(interval);
  });

  async function browse(setPath: (p: string) => void, extensions = ['vhdx', 'avhdx', 'qcow2']) {
    try {
      const picked = await pickPath({ directory: false, title: 'Выберите образ', extensions });
      if (typeof picked === 'string') setPath(picked);
    } catch {
      /* dialog cancelled */
    }
  }

  async function mount() {
    mountBusy = true;
    mountMsg = null;
    mountErr = null;
    isRefreshingDisks = true;
    try {
      await api.mountBackup(toolsState.mountPath, toolsState.rwMode);
      await Promise.all([loadData(), refreshVolumes()]);
      // The disk may need a moment for Windows to assign a letter; poll briefly.
      let letters = mountedLettersFor(toolsState.mountPath);
      for (let i = 0; i < 10 && letters.every(l => l.startsWith('Диск')); i++) {
        await new Promise(r => setTimeout(r, 500));
        await loadData();
        letters = mountedLettersFor(toolsState.mountPath);
      }
      const mode = toolsState.rwMode ? 'чтение и запись' : 'только чтение';
      const where = letters.length ? letters.join(', ') : 'см. проводник';
      mountMsg = `Подключено (${mode}): ${where}. Открыть можно в проводнике.`;
    } catch (e) {
      mountErr = String(e);
    } finally {
      mountBusy = false;
      isRefreshingDisks = false;
    }
  }

  async function unmount() {
    mountBusy = true;
    mountMsg = null;
    mountErr = null;
    isRefreshingDisks = true;
    try {
      await api.unmountBackup(toolsState.mountPath);
      mountMsg = 'Образ отключён.';
      await Promise.all([loadData(), refreshVolumes()]);
    } catch (e) {
      mountErr = String(e);
    } finally {
      mountBusy = false;
      isRefreshingDisks = false;
    }
  }

  async function verify() {
    verifyBusy = true;
    verifyMsg = null;
    verifyErr = null;
    try {
      const v = await runVerify(toolsState.verifyPath);
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
  <div class="inner">
  <h1>Инструменты</h1>

  <div class="tool-card">
    <div class="tool-head">
      <span class="glyph">💽</span>
      <div>
        <div class="tool-title">Подключить образ как диск</div>
        <div class="tool-desc">
          Образ подключается как диск; тома внутри получают буквы и
          открываются в проводнике.
        </div>
      </div>
    </div>
    <div class="row">
      <input
        type="text"
        bind:value={toolsState.mountPath}
        placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
        spellcheck="false"
        disabled={mountBusy}
      />
      <button class="btn outline btn-browse" onclick={() => browse((p) => (toolsState.mountPath = p), ['vhdx', 'avhdx', 'qcow2'])} disabled={mountBusy}>
        Обзор…
      </button>
    </div>
    <div class="row row-actions">
      <div class="mounted-info">
        {#if isRefreshingDisks}
          <span class="spinner-container">
            <span class="spinner-icon">🔄</span>
            <span>Поиск примонтированных дисков...</span>
          </span>
        {:else if mountedItems.length === 0}
          <span class="muted-text">Нет примонтированных дисков</span>
        {:else}
          <span>Примонтировано: </span>
          {#each mountedItems as item, i}
            {#each item.letters as letter, j}
              <button
                class="btn btn-letter {item.active ? 'primary' : 'outline'}"
                onclick={() => { toolsState.mountPath = item.image_path; }}
                disabled={mountBusy}
              >
                {letter}
              </button>
            {/each}
          {/each}
        {/if}
      </div>
      <div class="actions-right">
        <label class="rw-checkbox">
          <input type="checkbox" bind:checked={toolsState.rwMode} disabled={mountBusy || isCurrentPathMounted} />
          RW Mode
        </label>
        <button class="btn primary" onclick={mount} disabled={mountBusy || !toolsState.mountPath || isCurrentPathMounted}>
          Подключить
        </button>
        <!-- Nothing mounted, nothing to detach: the button says so by being
             unavailable, instead of failing on the second press. -->
        <button class="btn outline" onclick={unmount} disabled={mountBusy || !isCurrentPathMounted}>
          Отключить
        </button>
      </div>
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
          Сверяет контрольные суммы каждого блока с метаданными в образе (или файлом .obkm).
        </div>
      </div>
    </div>
    <div class="row">
      <input
        type="text"
        bind:value={toolsState.verifyPath}
        placeholder="D:\OwnBackup\disk_C_20260702.vhdx"
        spellcheck="false"
        disabled={verifyBusy}
      />
      <button class="btn outline" onclick={() => browse((p) => (toolsState.verifyPath = p))} disabled={verifyBusy}>
        Обзор…
      </button>
      <button class="btn primary" onclick={verify} disabled={verifyBusy || !toolsState.verifyPath}>
        {verifyBusy ? 'Проверка…' : 'Проверить'}
      </button>
    </div>
    {#if verifyBusy && verifyProgress}
      <div class="bar"><div class="bar-fill" style="width:{verifyProgress.percent}%"></div></div>
    {/if}
    {#if verifyMsg}<div class="banner ok">{verifyMsg}</div>{/if}
    {#if verifyErr}<div class="banner err">{verifyErr}</div>{/if}
  </div>

  <BootMediaCard />
</div>
</div>

<style>
  /* The scrollbar belongs to the window's edge, not to the column of content:
     with max-width on the scrolling element it appeared 860 px in, floating in
     the middle of the page. */
  .page {
    overflow: auto;
    height: 100%;
  }

  .inner {
    padding: 30px 44px;
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

  .row-actions {
    justify-content: space-between;
    align-items: center;
    margin-top: 12px;
  }

  .btn-browse {
    min-width: 95px;
  }

  .mounted-info {
    font-size: 13px;
    color: var(--text);
  }

  .muted-text {
    color: var(--text-muted);
  }

  .btn-letter {
    min-width: 36px !important;
    height: 36px !important;
    padding: 0 8px !important;
    display: inline-flex !important;
    align-items: center !important;
    justify-content: center !important;
    font-size: 13px !important;
    margin-right: 6px;
    border-radius: 4px;
    font-weight: 600;
    box-sizing: border-box !important;
  }

  @keyframes spin {
    from { transform: rotate(0deg); }
    to { transform: rotate(360deg); }
  }

  .spinner-container {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--text-muted);
  }

  .spinner-icon {
    display: inline-block;
    animation: spin 1s linear infinite;
  }

  .actions-right {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .rw-checkbox {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    user-select: none;
    cursor: pointer;
    color: var(--text);
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
