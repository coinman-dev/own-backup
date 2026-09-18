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
    await act({ action: 'rename', entry: n, name });
  }

  async function move(e: BootEntry, delta: number) {
    if (e.in_menu === null) return;
    await act({ action: 'move', entry: e.number, to: e.in_menu + delta });
  }

  async function remove(e: BootEntry) {
    const what = e.file ? `\n\nЗагрузчик ${e.file} останется на разделе — удаляется только запись о нём.` : '';
    if (!confirm(`Удалить запись «${e.name}»?${what}`)) return;
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
    return l.kind === 'загрузчик' ? l.path.split('\\').filter(Boolean)[1] ?? 'Загрузка' : l.kind;
  }

  onMount(refresh);
</script>

<div class="tool-card">
  <div class="tool-head">
    <span class="glyph">🖥️</span>
    <div>
      <div class="tool-title">Загрузка компьютера</div>
      <div class="tool-desc">
        Что материнская плата предлагает в меню при включении, и что лежит на
        разделах EFI. Записи хранятся в памяти платы, а не на диске, поэтому
        копия разделов их не сохраняет.
      </div>
    </div>
  </div>

  {#if err}
    <div class="note bad">{err}</div>
  {/if}
  {#if msg}
    <div class="note ok">{msg}</div>
  {/if}

  {#if status && !status.uefi}
    <div class="note">{status.message ?? 'Эта машина загружается не через UEFI.'}</div>
  {:else if status}
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
                <button class="as-text" onclick={() => startRename(e)} disabled={busy || !e.editable}>
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
              <button class="mini danger" title="Удалить" disabled={busy} onclick={() => remove(e)}>✕</button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>

    {#if notInMenu.length > 0}
      <div class="meta dim">
        Есть записи вне меню ({notInMenu.length}): существуют, но плата их не предлагает.
      </div>
    {/if}

    {#if orphans.length > 0}
      <div class="orphans">
        <div class="orphans-title">
          Загрузчики без записи: {orphans.length}
        </div>
        <div class="tool-desc">
          Файлы на месте, но ни одна запись на них не указывает — в меню платы
          их нет.
        </div>
        {#each orphans as o (o.esp.disk + o.loader.path)}
          <div class="orphan">
            <div>
              <b>{o.loader.kind}</b>
              <div class="target">диск {o.esp.disk} раздел {o.esp.partition} · {o.loader.path}</div>
            </div>
            <button class="btn outline" disabled={busy} onclick={() => addEntry(o.esp, o.loader)}>
              Добавить в меню
            </button>
          </div>
        {/each}
      </div>
    {:else if status.esps}
      <div class="meta dim">
        На всё, что лежит на разделах EFI, кто-то ссылается.
      </div>
    {/if}

    <div class="row row-actions">
      <button class="btn outline" disabled={busy} onclick={refresh}>Обновить</button>
      <label class="chk">
        <input type="checkbox" bind:checked={showAll} /> показать все файлы на разделах EFI
      </label>
    </div>

    {#if showAll}
      {#each status.esps ?? [] as esp (esp.disk + '-' + esp.partition)}
        <div class="esp">
          <div class="esp-title">
            Раздел EFI: диск {esp.disk} раздел {esp.partition} ·
            {Math.round(esp.size_bytes / 1048576)} МиБ
          </div>
          {#if esp.unreadable}
            <div class="target">{esp.unreadable}</div>
          {:else if esp.loaders.length === 0}
            <div class="target">загрузчиков нет</div>
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
    {/if}
  {:else}
    <div class="muted-text">Читаем…</div>
  {/if}
</div>

<style>
  .meta { margin: 6px 0; font-size: 13px; }
  .dim { opacity: 0.7; }
  .note { margin: 8px 0; padding: 8px 10px; border-radius: 6px; font-size: 13px; }
  .note.ok { background: rgba(60, 160, 90, 0.12); }
  .note.bad { background: rgba(200, 70, 70, 0.14); }

  .entries { width: 100%; border-collapse: collapse; margin: 8px 0; }
  .entries td { padding: 6px 4px; vertical-align: top; border-bottom: 1px solid rgba(128,128,128,0.15); }
  .entries tr.inactive .name { opacity: 0.5; }
  .pos { width: 24px; opacity: 0.6; font-variant-numeric: tabular-nums; }
  .acts { width: 140px; text-align: right; white-space: nowrap; }
  .target { font-size: 12px; opacity: 0.65; margin-top: 2px; word-break: break-all; }

  .as-text {
    background: none; border: none; padding: 0; cursor: pointer;
    font: inherit; color: inherit; text-align: left;
  }
  .as-text:disabled { cursor: default; }
  .as-text:hover:not(:disabled) { text-decoration: underline dotted; }

  .mini {
    background: none; border: 1px solid rgba(128,128,128,0.3); border-radius: 4px;
    cursor: pointer; padding: 2px 6px; margin-left: 2px; font-size: 12px; color: inherit;
  }
  .mini:disabled { opacity: 0.3; cursor: default; }
  .mini.danger:hover:not(:disabled) { border-color: rgba(200,70,70,0.6); }

  .orphans { margin: 10px 0; padding: 10px; border-radius: 6px; background: rgba(230,170,60,0.10); }
  .orphans-title { font-weight: 600; }
  .orphan { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-top: 8px; }

  .esp { margin-top: 10px; }
  .esp-title { font-size: 13px; font-weight: 600; opacity: 0.85; }
  .file { display: flex; gap: 10px; font-size: 12px; padding: 2px 0; }
  .path { flex: 1 1 auto; word-break: break-all; }
  .kind { flex: 0 0 200px; opacity: 0.7; }
  .state { flex: 0 0 190px; opacity: 0.7; }

  .chk { font-size: 13px; display: inline-flex; align-items: center; gap: 6px; }
</style>
