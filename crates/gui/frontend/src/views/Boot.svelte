<script lang="ts">
  // Как загружается эта машина: записи в памяти материнской платы и
  // загрузчики, которые лежат на разделах EFI.
  //
  // Две разные вещи в двух разных местах. Файл на разделе никуда не девается,
  // а запись о нём может исчезнуть — после обновления прошивки, сброса
  // настроек платы или переключения CSM. Тогда программа просто пропадает из
  // меню загрузки, и нигде об этом не сказано. Здесь оба списка сведены, и
  // видно, на что записи нет.
  //
  // Копия всей конфигурации снимается службой перед каждым изменением, без
  // спроса: у правки в прошивке нет своей отмены.
  import { onMount } from 'svelte';
  import { api } from '../lib/api';
  import type { BootStatus, BootEntry, Esp, EspLoader } from '../lib/types';

  let status = $state<BootStatus | null>(null);
  let busy = $state(false);
  let msg = $state<string | null>(null);
  let err = $state<string | null>(null);
  let showAll = $state(false);
  // Предупреждение живёт отдельно от msg: оно про то, куда легла копия, а не
  // про то, что сделано, и должно висеть, пока его не перекроет следующее.
  let warn = $state<string | null>(null);

  // Переименование правится на месте: номер записи, которую сейчас правят.
  let editing = $state<string | null>(null);
  let editName = $state('');

  const entries: BootEntry[] = $derived(status?.entries ?? []);
  const inMenu = $derived(entries.filter((e: BootEntry) => e.in_menu !== null));
  const notInMenu = $derived(entries.filter((e: BootEntry) => e.in_menu === null));

  // То, ради чего всё это: файл, который запускает прошивка, а записи на него
  // нет. Вторые ступени и запасной путь съёмного носителя сюда не входят —
  // своей записи у них не бывает.
  const orphans = $derived(
    (status?.esps ?? ([] as Esp[])).flatMap((esp: Esp) =>
      esp.loaders
        .filter((l: EspLoader) => !l.named_by && !l.fallback && !l.second_stage)
        .map((l: EspLoader) => ({ esp, loader: l }))
    )
  );

  async function refresh() {
    busy = true;
    err = null;
    try {
      status = await api.bootStatus();
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  async function act(action: Record<string, unknown>) {
    busy = true;
    err = null;
    msg = null;
    try {
      const r = await api.bootAction(action);
      msg = r.message;
      warn = r.safety_copy_durable
        ? null
        : `Копия прежнего состояния легла на временный диск (${r.safety_copy}) и не переживёт ` +
          'перезагрузку. Сохраните её сами, если она может понадобиться.';
      await refresh();
    } catch (e) {
      err = String(e);
    } finally {
      busy = false;
    }
  }

  function startRename(e: BootEntry) {
    editing = e.number;
    editName = e.name;
  }

  async function commitRename() {
    const n = editing;
    const name = editName.trim();
    editing = null;
    if (!n || !name) return;
    const was = entries.find((e: BootEntry) => e.number === n)?.name;
    if (was === name) return;
    await act({ action: 'rename', entry: n, name });
  }

  async function move(e: BootEntry, delta: number) {
    if (e.in_menu === null) return;
    await act({ action: 'move', entry: e.number, to: e.in_menu + delta });
  }

  async function remove(e: BootEntry) {
    const tail = e.file
      ? `\n\nЗагрузчик ${e.file} останется на разделе — удаляется только запись о нём.`
      : '';
    if (!confirm(`Удалить запись «${e.name}»?${tail}`)) return;
    await act({ action: 'delete', entry: e.number });
  }

  async function addEntry(esp: Esp, loader: EspLoader) {
    const name = prompt('Название для меню загрузки:', suggestName(loader));
    if (!name) return;
    await act({
      action: 'create',
      name,
      disk: esp.disk,
      partition: esp.partition,
      file: loader.path,
    });
  }

  // Что предложить в качестве названия. Служба уже опознала файл — её ответ
  // ближе к делу, чем путь.
  function suggestName(l: EspLoader): string {
    return l.kind === 'загрузчик' ? (l.path.split('\\').filter(Boolean)[1] ?? 'Загрузка') : l.kind;
  }

  onMount(refresh);
</script>

<div class="page">
  <div class="inner">
    <h1>Загрузка компьютера</h1>

    {#if err}<div class="banner err">{err}</div>{/if}
    {#if msg}<div class="banner ok">{msg}</div>{/if}
    {#if warn}<div class="banner warn">{warn}</div>{/if}

    {#if status && !status.uefi}
      <div class="card">
        <div class="muted">{status.message ?? 'Эта машина загружается не через UEFI.'}</div>
      </div>
    {:else if status}
      <div class="card">
        <div class="card-head">
          <div>
            <div class="card-title">Меню материнской платы</div>
            <div class="card-desc">
              То, что плата предлагает при включении, в её порядке. Название
              правится щелчком по нему.
            </div>
          </div>
          <button class="btn outline" disabled={busy} onclick={refresh}>Обновить</button>
        </div>

        <div class="meta">
          Загружена сейчас: <b>{status.current ?? '—'}</b>
          {#if status.timeout_secs != null}· меню показывается {status.timeout_secs} с{/if}
          {#if status.next}· один раз загрузится с {status.next}{/if}
        </div>

        <table class="entries">
          <tbody>
            {#each inMenu as e (e.number)}
              <tr class:inactive={!e.active}>
                <td class="pos">{e.in_menu}</td>
                <td class="name">
                  {#if editing === e.number}
                    <!-- svelte-ignore a11y_autofocus -->
                    <input
                      bind:value={editName}
                      onblur={commitRename}
                      onkeydown={(ev) => {
                        if (ev.key === 'Enter') commitRename();
                        if (ev.key === 'Escape') editing = null;
                      }}
                      autofocus
                    />
                  {:else}
                    <button
                      class="as-text"
                      title={e.editable ? 'Переименовать' : 'Эту запись править нельзя'}
                      onclick={() => startRename(e)}
                      disabled={busy || !e.editable}
                    >
                      {e.name}
                    </button>
                  {/if}
                  <div class="target">
                    {#if e.file}{e.file}{:else}устройство, а не файл{/if}
                    {#if !e.editable}· разобрана не полностью, править нельзя{/if}
                  </div>
                </td>
                <td class="acts">
                  <button class="mini" title="Выше" disabled={busy || e.in_menu === 1} onclick={() => move(e, -1)}>↑</button>
                  <button class="mini" title="Ниже" disabled={busy || e.in_menu === inMenu.length} onclick={() => move(e, 1)}>↓</button>
                  <button
                    class="mini"
                    title={e.active ? 'Убрать из меню' : 'Показывать в меню'}
                    disabled={busy}
                    onclick={() => act({ action: 'set_active', entry: e.number, on: !e.active })}
                  >{e.active ? '👁' : '🚫'}</button>
                  <button class="mini danger" title="Удалить запись" disabled={busy} onclick={() => remove(e)}>✕</button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>

        {#if notInMenu.length > 0}
          <div class="meta dim">
            Записей вне меню: {notInMenu.length}. Они существуют, но плата их не предлагает.
          </div>
        {/if}

        <div class="meta dim">
          Записи хранятся в памяти материнской платы, а не на диске, поэтому
          копия разделов их не сохраняет. Перед каждым изменением снимается
          копия в C:\ProgramData\OwnBackup\boot.
        </div>
      </div>

      {#if orphans.length > 0}
        <div class="card warn">
          <div class="card-title">Загрузчики без записи: {orphans.length}</div>
          <div class="card-desc">
            Файлы на месте, но ни одна запись на них не указывает — в меню платы
            их нет.
          </div>
          {#each orphans as o (o.esp.disk + o.loader.path)}
            <div class="orphan">
              <div>
                <b>{o.loader.kind}</b>
                <div class="target">диск {o.esp.disk} раздел {o.esp.partition} · {o.loader.path}</div>
              </div>
              <button class="btn primary" disabled={busy} onclick={() => addEntry(o.esp, o.loader)}>
                Добавить в меню
              </button>
            </div>
          {/each}
        </div>
      {:else if status.esps}
        <div class="card">
          <div class="muted">На всё, что лежит на разделах EFI, кто-то ссылается.</div>
        </div>
      {/if}

      <div class="card">
        <div class="card-head">
          <div>
            <div class="card-title">Разделы EFI</div>
            <div class="card-desc">Где лежат сами загрузчики.</div>
          </div>
          <label class="chk">
            <input type="checkbox" bind:checked={showAll} /> показать все файлы
          </label>
        </div>

        {#each status.esps ?? [] as esp (esp.disk + '-' + esp.partition)}
          <div class="esp">
            <div class="esp-title">
              Диск {esp.disk}, раздел {esp.partition} · {Math.round(esp.size_bytes / 1048576)} МиБ
            </div>
            {#if esp.unreadable}
              <div class="target">{esp.unreadable}</div>
            {:else if esp.loaders.length === 0}
              <div class="target">загрузчиков нет</div>
            {:else if !showAll}
              <div class="target">файлов: {esp.loaders.length}</div>
            {:else}
              {#each esp.loaders as l (l.path)}
                <div class="file">
                  <span class="path">{l.path}</span>
                  <span class="kind">{l.kind}</span>
                  <span class="state">
                    {#if l.named_by}запись {l.named_by}
                    {:else if l.fallback}запасной путь съёмного носителя
                    {:else if l.second_stage}запускает другой загрузчик
                    {:else}записи нет{/if}
                  </span>
                </div>
              {/each}
            {/if}
          </div>
        {/each}
      </div>
    {:else}
      <div class="card"><div class="muted">Читаем…</div></div>
    {/if}
  </div>
</div>

<style>
  /* Прокрутка принадлежит краю окна, а не колонке содержимого. */
  .page { overflow: auto; height: 100%; }
  .inner { padding: 30px 44px; }

  h1 {
    font-size: 28px;
    font-weight: 300;
    color: var(--head);
    margin-bottom: 24px;
  }

  .card {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--card-bg);
    box-shadow: var(--shadow);
    padding: 20px 22px;
    margin-bottom: 16px;
  }
  .card.warn { border-color: rgba(230, 170, 60, 0.55); }

  .card-head { display: flex; justify-content: space-between; align-items: flex-start; gap: 16px; }
  .card-title { font-size: 16px; font-weight: 600; }
  .card-desc { font-size: 13px; opacity: 0.7; margin-top: 4px; }

  .meta { margin-top: 10px; font-size: 13px; }
  .dim { opacity: 0.65; }
  .muted { opacity: 0.7; font-size: 14px; }

  .banner { padding: 10px 14px; border-radius: 6px; margin-bottom: 14px; font-size: 13px; }
  .banner.ok { background: rgba(60, 160, 90, 0.14); }
  .banner.err { background: rgba(200, 70, 70, 0.16); }
  .banner.warn { background: rgba(230, 170, 60, 0.18); }

  .entries { width: 100%; border-collapse: collapse; margin-top: 12px; }
  .entries td { padding: 8px 4px; vertical-align: top; border-bottom: 1px solid var(--border); }
  .entries tr:last-child td { border-bottom: none; }
  .entries tr.inactive .name { opacity: 0.45; }
  .pos { width: 26px; opacity: 0.55; font-variant-numeric: tabular-nums; }
  .name { font-size: 15px; }
  .acts { width: 150px; text-align: right; white-space: nowrap; }
  .target { font-size: 12px; opacity: 0.6; margin-top: 3px; word-break: break-all; }

  .as-text {
    background: none; border: none; padding: 0; cursor: pointer;
    font: inherit; color: inherit; text-align: left;
  }
  .as-text:disabled { cursor: default; }
  .as-text:hover:not(:disabled) { text-decoration: underline dotted; }

  .name input {
    font: inherit; padding: 2px 6px; width: 100%; max-width: 380px;
    border: 1px solid var(--border); border-radius: 4px;
    background: var(--input-bg, transparent); color: inherit;
  }

  .mini {
    background: none; border: 1px solid var(--border); border-radius: 4px;
    cursor: pointer; padding: 3px 7px; margin-left: 3px; font-size: 12px; color: inherit;
  }
  .mini:disabled { opacity: 0.3; cursor: default; }
  .mini:hover:not(:disabled) { background: rgba(128, 128, 128, 0.12); }
  .mini.danger:hover:not(:disabled) { border-color: rgba(200, 70, 70, 0.7); }

  .orphan {
    display: flex; align-items: center; justify-content: space-between;
    gap: 16px; margin-top: 12px;
  }

  .esp { margin-top: 12px; }
  .esp-title { font-size: 13px; font-weight: 600; opacity: 0.85; }
  .file { display: flex; gap: 12px; font-size: 12px; padding: 3px 0; }
  .path { flex: 1 1 auto; word-break: break-all; }
  .kind { flex: 0 0 210px; opacity: 0.7; }
  .state { flex: 0 0 200px; opacity: 0.7; }

  .chk { font-size: 13px; display: inline-flex; align-items: center; gap: 6px; white-space: nowrap; }

  .btn {
    padding: 7px 14px; border-radius: 6px; cursor: pointer;
    font-size: 13px; border: 1px solid var(--border); background: transparent; color: inherit;
  }
  .btn:disabled { opacity: 0.45; cursor: default; }
  .btn.primary { background: var(--accent, #6b5cff); color: #fff; border-color: transparent; }
  .btn.outline:hover:not(:disabled) { background: rgba(128, 128, 128, 0.1); }
</style>
