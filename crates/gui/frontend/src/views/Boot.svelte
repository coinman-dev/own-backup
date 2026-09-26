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
  import { engine, refreshVolumes } from '../lib/engineStore.svelte';
  import type { BootStatus, BootEntry, DiskInfo, Esp, EspLoader } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  let status = $state<BootStatus | null>(null);
  let busy = $state(false);
  let msg = $state<string | null>(null);
  let err = $state<string | null>(null);
  let showAll = $state(false);
  // Предупреждение живёт отдельно от msg: оно про то, куда легла копия, а не
  // про то, что сделано, и должно висеть, пока его не перекроет следующее.
  // A function, so it follows a change of language while it hangs there.
  let warn = $state<(() => string) | null>(null);

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
      const copy = r.safety_copy;
      warn = r.safety_copy_durable ? null : () => m.boot_copy_not_durable({ path: copy });
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
    const tail = e.file ? '\n\n' + m.boot_delete_confirm_file({ file: e.file }) : '';
    if (!confirm(m.boot_delete_confirm({ name: e.name }) + tail)) return;
    await act({ action: 'delete', entry: e.number });
  }

  async function addEntry(esp: Esp, loader: EspLoader) {
    const name = prompt(m.boot_new_entry_prompt(), suggestName(loader));
    if (!name) return;
    await act({
      action: 'create',
      name,
      disk: esp.disk,
      partition: esp.partition,
      file: loader.path,
    });
  }

  // «диск 3» ничего не говорит человеку, у которого их пять. Модель берём из
  // того же перечисления дисков, что показывает мастер копирования: служба
  // отдаёт в записях только номер, а номер — это то, чем диски отличаются друг
  // от друга меньше всего.
  function diskLabel(n: number): string {
    const d = engine.disks.find((x: DiskInfo) => {
      const tail = x.device_id.match(/(\d+)$/);
      return tail ? Number(tail[1]) === n : false;
    });
    return d ? m.boot_disk_model({ n, model: d.model }) : m.boot_disk({ n });
  }

  // Что предложить в качестве названия. Служба уже опознала файл — её ответ
  // ближе к делу, чем путь.
  function suggestName(l: EspLoader): string {
    // `generic`: the service could not tell what the file is. A service from
    // before the flag said so with the Russian word, always.
    const generic = l.generic ?? l.kind === 'загрузчик';
    return generic ? (l.path.split('\\').filter(Boolean)[1] ?? m.boot_default_entry_name()) : l.kind;
  }

  onMount(() => {
    refresh();
    // Список дисков нужен только ради их названий; если мастер копирования
    // его уже загрузил, второй раз не спрашиваем.
    if (!engine.volumesLoaded && !engine.volumesLoading) refreshVolumes();
  });
</script>

<div class="page">
  <div class="inner">
    <h1>{m.boot_title()}</h1>

    {#if err}<div class="banner err">{err}</div>{/if}
    {#if msg}<div class="banner ok">{msg}</div>{/if}
    {#if warn}<div class="banner warn">{warn()}</div>{/if}

    {#if status && !status.uefi}
      <div class="card">
        <div class="muted">{status.message ?? m.boot_not_uefi()}</div>
      </div>
    {:else if status}
      <div class="card">
        <div class="card-head">
          <div>
            <div class="card-title">{m.boot_priority()}</div>
            <div class="card-desc">{m.boot_priority_desc()}</div>
          </div>
          <button class="btn outline" disabled={busy} onclick={refresh}>{m.common_refresh()}</button>
        </div>

        <div class="meta">
          {m.boot_current_label()} <b>{status.current ?? '—'}</b>
          {#if status.timeout_secs != null}· {m.boot_menu_timeout({ secs: status.timeout_secs })}{/if}
          {#if status.next}· {m.boot_next_once({ name: status.next })}{/if}
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
                      title={e.editable ? m.boot_rename() : m.boot_not_editable()}
                      onclick={() => startRename(e)}
                      disabled={busy || !e.editable}
                    >
                      {e.name}
                    </button>
                  {/if}
                  <div class="target">
                    {#if e.file}{e.file}{:else}{m.boot_device_not_file()}{/if}
                    {#if !e.editable}· {m.boot_partly_parsed()}{/if}
                  </div>
                </td>
                <td class="acts">
                  <button class="mini" title={m.boot_move_up()} disabled={busy || e.in_menu === 1} onclick={() => move(e, -1)}>
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5M6 11l6-6 6 6" /></svg>
                  </button>
                  <button class="mini" title={m.boot_move_down()} disabled={busy || e.in_menu === inMenu.length} onclick={() => move(e, 1)}>
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M6 13l6 6 6-6" /></svg>
                  </button>
                  <button
                    class="mini"
                    title={e.active ? m.boot_hide_from_menu() : m.boot_show_in_menu()}
                    disabled={busy}
                    onclick={() => act({ action: 'set_active', entry: e.number, on: !e.active })}
                  >
                    {#if e.active}
                      <!-- open eye: shown in the menu -->
                      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z" /><circle cx="12" cy="12" r="3" /></svg>
                    {:else}
                      <!-- struck-through eye: hidden from the menu -->
                      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9.9 5.6A9.6 9.6 0 0 1 12 5.4c6.6 0 10.2 6.6 10.2 6.6a17 17 0 0 1-3.4 4.2M6.2 7.8A17 17 0 0 0 1.8 12S5.4 18.6 12 18.6c1.9 0 3.5-.5 4.9-1.3" /><path d="M10 10a2.8 2.8 0 0 0 4 4" /><path d="M3 3l18 18" /></svg>
                    {/if}
                  </button>
                  <button class="mini danger" title={m.boot_delete_entry()} disabled={busy} onclick={() => remove(e)}>
                    <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>

        {#if notInMenu.length > 0}
          <div class="meta dim">{m.boot_not_in_menu({ n: notInMenu.length })}</div>
        {/if}

        <div class="meta dim">{m.boot_one_time_menu_note()}</div>

        <div class="meta dim">{m.boot_stored_in_firmware_note()}</div>
      </div>

      {#if orphans.length > 0}
        <div class="card warn">
          <div class="card-title">{m.boot_orphans_title({ n: orphans.length })}</div>
          <div class="card-desc">{m.boot_orphans_desc()}</div>
          {#each orphans as o (o.esp.disk + o.loader.path)}
            <div class="orphan">
              <div>
                <b>{o.loader.kind}</b>
                <div class="target">
                  {m.boot_disk_partition({ disk: diskLabel(o.esp.disk), part: o.esp.partition })} · {o.loader.path}
                </div>
              </div>
              <button class="btn accent" disabled={busy} onclick={() => addEntry(o.esp, o.loader)}>
                {m.boot_add_to_menu()}
              </button>
            </div>
          {/each}
        </div>
      {:else if status.esps}
        <div class="card">
          <div class="muted">{m.boot_no_orphans()}</div>
        </div>
      {/if}

      <div class="card">
        <div class="card-head">
          <div>
            <div class="card-title">{m.boot_esps_title()}</div>
            <div class="card-desc">{m.boot_esps_desc()}</div>
          </div>
          <label class="chk">
            <input type="checkbox" bind:checked={showAll} /> {m.boot_show_all_files()}
          </label>
        </div>

        {#each status.esps ?? [] as esp (esp.disk + '-' + esp.partition)}
          <div class="esp">
            <div class="esp-title">
              {m.boot_disk_partition({ disk: diskLabel(esp.disk), part: esp.partition })} ·
              {m.boot_size_mib({ n: Math.round(esp.size_bytes / 1048576) })}
            </div>
            {#if esp.unreadable}
              <div class="target">{esp.unreadable}</div>
            {:else if esp.loaders.length === 0}
              <div class="target">{m.boot_no_loaders()}</div>
            {:else if !showAll}
              <div class="target">{m.boot_files_count({ n: esp.loaders.length })}</div>
            {:else}
              {#each esp.loaders as l (l.path)}
                <div class="file">
                  <span class="path">{l.path}</span>
                  <span class="kind">{l.kind}</span>
                  <span class="state">
                    {#if l.named_by}{m.boot_state_entry({ entry: l.named_by })}
                    {:else if l.fallback}{m.boot_state_fallback()}
                    {:else if l.second_stage}{m.boot_state_second_stage()}
                    {:else}{m.boot_state_no_entry()}{/if}
                  </span>
                </div>
              {/each}
            {/if}
          </div>
        {/each}
      </div>
    {:else}
      <div class="card"><div class="muted">{m.boot_reading()}</div></div>
    {/if}
  </div>
</div>

<style>
  /* Прокрутка принадлежит краю окна, а не колонке содержимого. */
  .page { overflow: auto; height: 100%; }
  .inner { padding: 30px 44px; }

  h1 {
    font-size: 24px;
    font-weight: 300;
    color: var(--head);
    margin-bottom: 24px;
  }

  .card {
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--card-bg);
    padding: 20px 22px;
    margin-bottom: 16px;
  }
  .card.warn { border-color: var(--warning-line); }

  .card-head { display: flex; justify-content: space-between; align-items: flex-start; gap: 16px; }
  .card-title { font-size: 16px; font-weight: 600; color: var(--head); }
  .card-desc { font-size: 13px; color: var(--text-secondary); margin-top: 4px; }

  .meta { margin-top: 10px; font-size: 13px; color: var(--text-secondary); }
  .meta b { font-weight: 600; color: var(--head); }
  .dim { color: var(--text-muted); }
  .muted { color: var(--text-secondary); font-size: 14px; }

  .banner { padding: 10px 14px; border-radius: 8px; margin-bottom: 14px; font-size: 13px; border: 1px solid transparent; }
  .banner.ok { background: var(--success-soft); border-color: var(--success-line); color: var(--success-text); }
  .banner.err { background: var(--error-soft); border-color: var(--error-line); color: var(--error-text); }
  .banner.warn { background: var(--warning-soft); border-color: var(--warning-line); color: var(--warning-text); }

  .entries { width: 100%; border-collapse: collapse; margin-top: 12px; }
  .entries td { padding: 8px 4px; vertical-align: top; border-bottom: 1px solid var(--border); }
  .entries tr:last-child td { border-bottom: none; }
  .entries tr.inactive .name { opacity: 0.45; }
  .pos { width: 26px; color: var(--text-muted); font-variant-numeric: tabular-nums; }
  .name { font-size: 15px; color: var(--head); }
  .acts { width: 150px; text-align: right; white-space: nowrap; }
  .target { font-size: 12px; color: var(--text-muted); margin-top: 3px; word-break: break-all; }

  .as-text {
    background: none; border: none; padding: 0; cursor: pointer;
    font: inherit; color: inherit; text-align: left;
  }
  .as-text:disabled { cursor: default; }
  .as-text:hover:not(:disabled) { text-decoration: underline dotted; }

  .name input {
    font: inherit; padding: 2px 6px; width: 100%; max-width: 380px;
    border: 1px solid var(--border-strong); border-radius: 6px;
    background: var(--input-bg); color: var(--text);
  }
  .name input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }

  .mini {
    background: var(--card-bg); border: 1px solid var(--border); border-radius: 6px;
    cursor: pointer; padding: 3px 7px; margin-left: 3px; font-size: 12px; color: var(--text-secondary);
  }
  .mini svg { vertical-align: middle; }
  .mini:disabled { opacity: 0.45; cursor: default; }
  .mini:hover:not(:disabled) { background: var(--panel2); color: var(--head); }
  .mini.danger { color: var(--error); }
  .mini.danger:hover:not(:disabled) { border-color: var(--error); background: var(--error-soft); color: var(--error); }

  .orphan {
    display: flex; align-items: center; justify-content: space-between;
    gap: 16px; margin-top: 12px;
  }
  .orphan b { font-weight: 600; color: var(--head); }

  .esp { margin-top: 12px; }
  .esp-title { font-size: 13px; font-weight: 600; color: var(--head); }
  .file { display: flex; gap: 12px; font-size: 12px; padding: 3px 0; }
  .path { flex: 1 1 auto; word-break: break-all; }
  .kind { flex: 0 0 210px; color: var(--text-muted); }
  .state { flex: 0 0 200px; color: var(--text-muted); }

  .chk { font-size: 13px; display: inline-flex; align-items: center; gap: 6px; white-space: nowrap; color: var(--text-secondary); }

  /* «Обновить»: the design's secondary button. */
  .btn {
    padding: 7px 14px; border-radius: 6px; cursor: pointer;
    font-size: 13px; border: 1px solid var(--border-strong); background: var(--card-bg); color: var(--text);
  }
  .btn:disabled { opacity: 0.45; cursor: default; }
  .btn.outline:hover:not(:disabled) { background: var(--panel2); }
  /* «Добавить в меню»: outlined in the accent — one per orphan, so a column
     of solid buttons would shout. */
  .btn.accent { background: transparent; border-color: var(--primary); color: var(--accent-text); }
  .btn.accent:hover:not(:disabled) { background: var(--accent-soft); }
</style>
