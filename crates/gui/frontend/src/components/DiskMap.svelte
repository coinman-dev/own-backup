<script lang="ts">
  // Paragon-style disk map: one physical disk drawn as proportional partition
  // segments. Extracted from the backup wizard's source tab so the Restore
  // screen can reuse the exact same visuals (read-only for the backed-up disk,
  // selectable for picking a restore target). No decryption/boot-toggle logic —
  // that stays in the wizard; here segments are shown, and optionally clickable.
  import type { Snippet } from 'svelte';
  import { formatBytes } from '../lib/format';
  import { diskSegments as computeSegments } from '../lib/diskLayout';
  import type { DiskInfo, PartitionInfo } from '../lib/types';
  import { m } from '../lib/paraglide/messages';

  /** One cell of an explicit layout grid (see the `slots` prop).
   *
   *  `share` is the cell's width as a percentage of the bar — a *layout budget*
   *  rather than a raw proportion. Tiny partitions would be a few pixels wide if
   *  drawn to scale, so the budget is allocated once (small cells get a readable
   *  floor, the rest split what is left proportionally) and then only ever moves
   *  BETWEEN cells: deleting a partition hands its share to the free cell that
   *  replaces it, creating one takes its share back. The total is always 100%,
   *  so untouched partitions never move or change width. */
  export type MapSlot =
    | { kind: 'part'; p: PartitionInfo; share: number }
    | { kind: 'free'; offset: number; size: number; share: number };

  interface Props {
    disk: DiskInfo;
    /** Explicit layout grid. When given it replaces the geometry derived from
     *  `disk.partitions`, so a deleted partition can leave a free cell with the
     *  SAME boundaries — the remaining partitions never shift or resize. */
    slots?: MapSlot[];
    /** Optional content rendered in the header centre (e.g. a copy navigator). */
    center?: Snippet;
    /** Devices to render as selected/checked. */
    selected?: string[];
    /** Partition indices to render as selected — used when highlighting comes
     *  from embedded metadata (works for letterless partitions too). */
    selectedIndices?: number[];
    /** When true, segments are shown but not clickable. */
    readonly?: boolean;
    /** Show the "загрузочный" badge and highlight the header. */
    boot?: boolean;
    /** State of the checkbox next to the "загрузочный" badge (off by default). */
    bootChecked?: boolean;
    /** Called when that checkbox is toggled. */
    onBootToggle?: (checked: boolean) => void;
    /** Called with (device, label) when a selectable segment is clicked. */
    onToggle?: (dev: string, label: string) => void;
    /** Restore-mapping mode: click anywhere on the disk to pick it. */
    onDisk?: () => void;
    /** Restore-mapping mode: click a partition (source or destination). */
    onPart?: (p: PartitionInfo) => void;
    /** Restore-mapping colour for a partition: yellow (picked source), green
     *  (mapped ok), red (bad destination). `pair` is the mapping's ordinal —
     *  each pair gets a slightly lighter green so it's clear what maps to what. */
    stateOf?: (index: number) => { kind: 'yellow' | 'green' | 'red' | 'preview'; pair?: number } | undefined;
    /** Partitions whose GPT entry the copy keeps although they hold no data —
     *  the Microsoft Reserved partition above all. Windows expects the slot to
     *  exist, so the restore recreates it; there is simply nothing inside to
     *  copy. Drawn as part of the copy, because "unchecked" reads as "left
     *  out" and would be wrong. */
    layoutOnly?: number[];
    /** Partition indices actually present in the backup. When given, any other
     *  partition is drawn as an empty "не в бэкапе" slot (no data, not clickable). */
    inBackup?: number[];
    /** Partitions whose fate is already decided (mapped / placed): they keep
     *  their colour but no longer respond to clicks — undo or "Отмена" to redo.
     *  The right-click editor menu still works. */
    lockedIndices?: number[];
    /** Click a stretch of unallocated space. Receives the gap's offset and size.
     *  What that means depends on the mode — see `freeAction`. */
    onFree?: (offset: number, size: number) => void;
    /** What clicking free space does, so the tooltip can say it: drop the whole
     *  backup layout there ('place'), or open the create-partition dialog
     *  ('create'). */
    freeAction?: 'place' | 'create';
    /** Bytes the copy needs: free regions smaller than this highlight in red on
     *  hover, so it is obvious before clicking that they do not fit. */
    needBytes?: number;
    /** Right-click on a partition (simple partition editor menu). */
    onPartMenu?: (p: PartitionInfo, ev: MouseEvent) => void;
    /** Right-click on unallocated space (to create a partition there). */
    onFreeMenu?: (offset: number, size: number, ev: MouseEvent) => void;
  }
  let {
    disk,
    slots,
    center,
    selected = [],
    selectedIndices = [],
    readonly = false,
    boot = false,
    bootChecked = false,
    onBootToggle,
    onToggle,
    onDisk,
    onPart,
    stateOf,
    inBackup,
    layoutOnly = [],
    lockedIndices = [],
    onFree,
    freeAction = 'place',
    needBytes,
    onPartMenu,
    onFreeMenu,
  }: Props = $props();

  // Each mapped pair gets its own shade: 5% lighter AND 10% of the way from
  // green towards blue (≈12° of hue), so source and destination are easy to
  // match up even with several pairs on screen.
  function pairStyle(st: { kind: string; pair?: number } | undefined): string {
    if (!st || st.kind !== 'green') return '';
    const i = st.pair ?? 0;
    return `--pair-l: ${28 + i * 5}%; --pair-h: ${96 + i * 12}`;
  }

  const FS_LABELS: Record<string, string> = {
    ntfs: 'NTFS', fat32: 'FAT32', ex_fat: 'exFAT', ext4: 'ext4', xfs: 'XFS',
    btrfs: 'Btrfs', re_fs: 'ReFS', unknown: '',
  };
  function fsLabel(fs: string): string {
    return FS_LABELS[fs] ?? (fs === 'unknown' ? '' : fs);
  }
  /** Partitions the system needs but never mounts: no filesystem, no letter, no
   *  label — and previously drawn as a nameless "Раздел 128 MiB" that looks like
   *  a mistake. The GPT type GUID is what they are, so say it. */
  const GPT_TYPE_NAMES: Record<string, () => string> = {
    'C12A7328-F81F-11D2-BA4B-00A0C93EC93B': () => 'EFI (ESP)',
    'E3C9E316-0B5C-4DB8-817D-F92DF00215AE': m.dmap_gpt_msr,
    'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC': m.dmap_gpt_winre,
    '0FC63DAF-8483-4772-8E79-3D69D8477DE4': () => 'Linux',
    '0657FD6D-A4AB-43C4-84E5-0933C84B4F4F': () => 'Linux swap',
    'E6D6D379-F507-44C2-A23C-238F2A3DF928': () => 'Linux LVM',
    'A19D880F-05FC-4D3B-A006-743F0F84911E': () => 'Linux RAID',
    'CA7D7CCB-63ED-4C53-861C-1742536059CC': () => 'LUKS',
    '21686148-6449-6E6F-744E-656564454649': () => 'BIOS boot',
    '024DEE41-33E7-11D3-9D69-0008C781F39F': m.dmap_gpt_mbr_in_gpt,
  };
  function gptTypeName(p: PartitionInfo): string {
    const guid = (p.gpt_type_guid || '').replace(/[{}]/g, '').toUpperCase();
    return GPT_TYPE_NAMES[guid]?.() ?? '';
  }
  function tableLabel(t: string): string {
    return t === 'gpt' ? 'GPT' : t === 'mbr' ? 'MBR' : t.toUpperCase();
  }
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  function partLabel(p: PartitionInfo): string {
    if (p.volume_letter) {
      const name = p.volume_label || fsLabel(p.filesystem) || m.dmap_volume();
      return `${p.volume_letter}: ${name}`;
    }
    return p.volume_label || fsLabel(p.filesystem) || gptTypeName(p) || m.dmap_partition();
  }
  function partFs(p: PartitionInfo): string {
    return fsLabel(p.filesystem);
  }
  function usedPct(p: PartitionInfo): number {
    if (p.used_bytes <= 0 || p.size_bytes <= 0) return 0;
    return Math.min(100, (p.used_bytes / p.size_bytes) * 100);
  }
  function segTitle(p: PartitionInfo): string {
    let t = partLabel(p) + ' — ' + formatBytes(p.size_bytes);
    if (p.used_bytes > 0) t += ` (${m.dmap_used({ size: formatBytes(p.used_bytes) })})`;
    const fs = partFs(p);
    if (fs) t += ` ${fs}`;
    if (layoutOnly.includes(p.index)) {
      t += ' · ' + m.dmap_layout_only_title();
    }
    return t;
  }

  // Shape of the disk — from `lib/diskLayout`, so this map, the backup wizard
  // and the restore designer all draw the same disk the same way.
  interface DiskSeg { kind: 'part' | 'free'; size: number; offset?: number; share?: number; p?: PartitionInfo }

  // Cells to draw: the explicit grid when supplied, else derived from the disk.
  const segments = $derived.by((): DiskSeg[] =>
    slots
      ? slots.map((s) =>
          s.kind === 'part'
            ? { kind: 'part' as const, size: s.p.size_bytes, offset: s.p.offset_bytes, share: s.share, p: s.p }
            : { kind: 'free' as const, size: s.size, offset: s.offset, share: s.share },
        )
      : computeSegments(disk),
  );
  // With an explicit budget the width is exact — no flexing, no min-width.
  const flexOf = (seg: DiskSeg) =>
    seg.share != null ? `flex: 0 0 ${seg.share}%` : `flex: ${seg.size} 1 0`;

  const diskDigits = $derived(disk.device_id.replace(/\D+/g, '') || '');
  // A partition is highlighted if its device is in `selected` OR its index is
  // in `selectedIndices` (the latter also covers letterless partitions).
  function isSelP(p: PartitionInfo, dev: string | null): boolean {
    return (!!dev && selected.includes(dev)) || selectedIndices.includes(p.index);
  }
</script>

{#snippet segInner(p: PartitionInfo, sel: boolean)}
  {#if usedPct(p) > 0}<div class="usedfill" style="width: {usedPct(p)}%"></div>{/if}
  <div class="seg-top">
    {#if p.is_system || p.is_boot || p.has_os}
      <svg class="lock" width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg>
    {/if}
    <!-- The same padlock as the system one, but in the info colour — as on the
         design's «Копия зашифрована» banner — so the two stay distinguishable. -->
    {#if p.is_encrypted}<span class="enc" title={m.dmap_encrypted()}><svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="var(--info)" stroke-width="2.4" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg></span>{/if}
    <span class="seg-label">{partLabel(p)}</span>
  </div>
  <div class="seg-meta">{formatBytes(p.size_bytes)}{#if p.used_bytes > 0}<span class="muted">&nbsp;({m.dmap_used({ size: formatBytes(p.used_bytes) })})</span>{/if}{#if partFs(p)}<span class="muted">&nbsp;{partFs(p)}</span>{/if}{#if layoutOnly.includes(p.index)}<span class="muted">&nbsp;· {m.dmap_layout_only()}</span>{/if}</div>
  {#if sel}<div class="check"><svg width="10" height="10" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg></div>{/if}
{/snippet}

<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
<!-- The tab stop and the role appear together or not at all: without `onDisk`
     the card is inert and gets neither. The checker cannot see that they are
     the same condition, so it reads the tabindex as sitting on a plain div. -->
<div
  class="disk-card"
  class:clickable={onDisk}
  role={onDisk ? 'button' : undefined}
  tabindex={onDisk ? 0 : undefined}
  onclick={onDisk}
  onkeydown={onDisk ? (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onDisk(); } } : undefined}
>
  <div class="disk-head" class:whole={boot}>
    <span class="disk-model">{disk.model || disk.device_id}</span>
    {#if center}<div class="disk-head-center">{@render center()}</div>{/if}
    <span class="disk-head-right">
      {#if boot}
        <!-- Whole-disk (bootable) restore opt-in — the checkbox lives inside the
             badge itself, so it reads as one control. -->
        <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
        <!-- A guard, not an action: it keeps a click on the badge from also
             selecting the card behind it. The checkbox inside is the control,
             and it answers the keyboard by itself. -->
        <label class="whole-badge boot-check" onclick={(e) => e.stopPropagation()}>
          <input
            type="checkbox"
            checked={bootChecked}
            onchange={(e) => onBootToggle?.((e.currentTarget as HTMLInputElement).checked)}
          />
          {m.dmap_bootable()}
        </label>
      {/if}
      <span class="disk-meta">{formatBytes(disk.size_bytes)} · {tableLabel(disk.partition_table)}</span>
    </span>
  </div>
  <div class="disk-body">
    <div class="disk-ico">
      <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.6"><ellipse cx="12" cy="6" rx="8" ry="3"/><path d="M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6"/><path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/></svg>
      <div class="disk-name">{m.dmap_disk({ n: diskDigits })}</div>
    </div>
    <div class="bar" class:grid={!!slots}>
      {#each segments as seg}
        {#if seg.kind === 'free'}
          {#if onFree || onFreeMenu}
            <button
              type="button"
              class="seg free"
              class:free-pick={!!onFree}
              class:too-small={!!onFree && needBytes != null && seg.size < needBytes}
              style={flexOf(seg)}
              title={!onFree
                ? m.dmap_unalloc({ size: formatBytes(seg.size) })
                : needBytes != null && seg.size < needBytes
                  ? m.dmap_unalloc_short({ size: formatBytes(seg.size), short: formatBytes(needBytes - seg.size), need: formatBytes(needBytes) })
                  : freeAction === 'create'
                    ? m.dmap_unalloc_create({ size: formatBytes(seg.size) })
                    : m.dmap_unalloc_place({ size: formatBytes(seg.size) })}
              onclick={(e) => { e.stopPropagation(); onFree?.(seg.offset ?? 0, seg.size); }}
              oncontextmenu={(e) => { if (onFreeMenu) { e.preventDefault(); e.stopPropagation(); onFreeMenu(seg.offset ?? 0, seg.size, e); } }}
            ><span class="free-lbl">{m.dmap_free_size({ size: formatBytes(seg.size) })}</span></button>
          {:else}
            <div class="seg free" style={flexOf(seg)}><span class="free-lbl">{m.dmap_free()}</span></div>
          {/if}
        {:else if seg.p}
          {@const dev = partDevice(seg.p)}
          {@const sel = isSelP(seg.p, dev)}
          {@const st = stateOf ? stateOf(seg.p.index) : undefined}
          {@const absent = !!inBackup && !inBackup.includes(seg.p.index)}
          {#if absent}
            <!-- Not part of this backup: an empty slot, no data, not clickable.
                 It still says WHICH partition it is. Two nameless "раздел не в
                 бэкапе" cells side by side read as one thing drawn twice, and
                 the question that follows is "where did the second one come
                 from" — when they are simply two different partitions. -->
            <div
              class="seg absent"
              style={flexOf(seg)}
              title={m.dmap_absent_title({ name: partLabel(seg.p), size: formatBytes(seg.p.size_bytes) })}
            >
              <span class="absent-lbl">{m.dmap_absent()}</span>
              <span class="absent-who">{partLabel(seg.p)} · {formatBytes(seg.p.size_bytes)}</span>
            </div>
          {:else if onPart || onPartMenu}
            {@const locked = lockedIndices.includes(seg.p.index)}
            <button
              type="button"
              class="seg selectable"
              class:locked
              class:selected={sel}
              class:st-yellow={st?.kind === 'yellow'}
              class:st-green={st?.kind === 'green'}
              class:st-red={st?.kind === 'red'}
              class:st-preview={st?.kind === 'preview'}
              style="{flexOf(seg)}; {pairStyle(st)}"
              title={segTitle(seg.p)}
              onclick={(e) => { e.stopPropagation(); if (!locked) onPart?.(seg.p!); }}
              oncontextmenu={(e) => { if (onPartMenu) { e.preventDefault(); e.stopPropagation(); onPartMenu(seg.p!, e); } }}
            >{@render segInner(seg.p, sel)}</button>
          {:else if layoutOnly.includes(seg.p.index)}
            <!-- No volume to select and nothing to copy, but the copy does keep
                 its table entry. Drawn like any other partition in the copy —
                 tick and all — because that is what it is. -->
            <div class="seg ro layout-only selected" style={flexOf(seg)} title={segTitle(seg.p)}>
              {@render segInner(seg.p, true)}
            </div>
          {:else if dev && !readonly && onToggle}
            <button
              type="button"
              class="seg selectable"
              class:selected={sel}
              style={flexOf(seg)}
              title={segTitle(seg.p)}
              onclick={() => onToggle(dev, partLabel(seg.p!))}
            >{@render segInner(seg.p, sel)}</button>
          {:else}
            <div class="seg ro" class:selected={sel} style={flexOf(seg)} title={segTitle(seg.p)}>
              {@render segInner(seg.p, sel)}
            </div>
          {/if}
        {/if}
      {/each}
    </div>
  </div>
</div>

<style>
  .disk-card { border: 1px solid var(--border); border-radius: 8px; overflow: hidden; background: var(--card-bg); }
  .disk-card.clickable { cursor: pointer; }
  .disk-card.clickable:hover { border-color: var(--primary); box-shadow: 0 0 0 1px var(--primary); }
  /* Restore-mapping partition states. Green pairs share a hue but step 5% in
     lightness per pair (--pair-l), so source and destination visibly match.
     The green is translucent (.22) over the card, so in the dark theme it stays
     a dim tint — no pair lightness makes it glow — and the text on it keeps
     the theme's own colours. */
  .seg.st-yellow { background: var(--warning-soft); box-shadow: inset 0 0 0 2px var(--warning); }
  .seg.st-green { background: hsl(var(--pair-h, 96) 58% calc(var(--pair-l, 28%) + var(--pair-lift)) / .22); box-shadow: inset 0 0 0 2px hsl(var(--pair-h, 96) 58% calc(var(--pair-l, 28%) + var(--pair-lift))); }
  .seg.st-red { background: var(--error-soft); box-shadow: inset 0 0 0 2px var(--error); }
  /* Partitions that will be created by the restore (preview, not on disk yet). */
  .seg.st-preview { background: var(--accent-soft); box-shadow: inset 0 0 0 2px var(--primary); border-style: dashed; }
  .seg.free.free-pick { cursor: pointer; }
  .seg.free.free-pick:hover { box-shadow: inset 0 0 0 2px var(--primary); }
  /* Region too small for the copy: red on hover so it reads as "won't fit". */
  .seg.free.free-pick.too-small:hover { box-shadow: inset 0 0 0 2px var(--error); }
  /* Partition that is not in the backup: an empty, inert slot. */
  /* Two lines, centred: what it is not, and which partition it is. */
  .seg.absent { align-items: center; justify-content: center; flex-direction: column; gap: 2px; background: var(--panel2); cursor: default; overflow: hidden; }
  .absent-lbl { font-size: 11px; color: var(--text-muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 100%; }
  .absent-who { font-size: 11px; color: var(--text-muted); opacity: .75; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 100%; }
  .disk-head { width: 100%; display: flex; align-items: center; justify-content: space-between; padding: 6px 12px; background: var(--panel2); border: 0; border-bottom: 1px solid var(--border); margin: 0; font-family: inherit; text-align: left; }
  .disk-head.whole { background: var(--accent-soft); color: var(--accent-text); }
  .disk-head.whole .disk-model { color: var(--accent-text); }
  .disk-head-center { display: flex; align-items: center; }
  .disk-head-right { display: flex; align-items: center; gap: 10px; }
  .whole-badge { font-size: 11px; font-weight: 700; color: #fff; background: var(--primary); padding: 2px 8px; border-radius: 10px; }
  /* The checkbox sits inside the violet pill, alongside the label text. */
  .boot-check { display: inline-flex; align-items: center; gap: 6px; cursor: pointer; user-select: none; }
  .boot-check input { margin: 0; cursor: pointer; width: 12px; height: 12px; accent-color: #fff; }
  .disk-model { font-size: 13px; font-weight: 600; color: var(--head); }
  .disk-meta { font-size: 12px; color: var(--text-muted); }
  .disk-body { display: flex; align-items: center; gap: 14px; padding: 7px 12px; }
  .disk-ico { width: 52px; flex-shrink: 0; text-align: center; color: var(--text-muted); }
  .disk-name { font-size: 12px; color: var(--text-secondary); margin-top: 1px; }
  .bar { flex: 1; display: flex; height: 44px; border: 1px solid var(--border); border-radius: 4px; overflow: hidden; background: var(--card-bg); }
  /* Segments are parted by a 2px strip of the card itself (the design's 2px
     gap). A border, not a gap: box-sizing keeps the widths exactly as before. */
  .seg { position: relative; padding: 5px 8px; min-width: 84px; border: 0; border-right: 2px solid var(--card-bg); margin: 0; display: flex; flex-direction: column; gap: 3px; background: var(--seg); overflow: hidden; text-align: left; font-family: inherit; color: inherit; appearance: none; -webkit-appearance: none; }
  .seg:last-child { border-right: none; }
  .seg.free { min-width: 40px; align-items: center; justify-content: center; background: repeating-linear-gradient(135deg, var(--hatch) 0 6px, transparent 6px 12px); }
  /* Explicit budget: widths are exact percentages, so the readable floor is
     already baked into each share — min-width would distort them again. */
  .bar.grid .seg { min-width: 0; }
  .free-lbl { font-size: 11px; color: var(--text-muted); white-space: nowrap; max-width: 100%; overflow: hidden; text-overflow: ellipsis; }
  .seg.selectable { cursor: pointer; }
  /* Hover lays one more thin accent tint over whatever the state colour is
     (only the image layer, so the state's own background-color stays). The old
     brightness(0.97) was invisible on the dark theme's dark segments. */
  .seg.selectable:hover { background-image: linear-gradient(var(--seg), var(--seg)); }
  /* Already mapped/placed: shown, but no longer a click target. */
  .seg.selectable.locked { cursor: default; }
  .seg.selectable.locked:hover { background-image: none; }
  .seg.ro { cursor: default; }
  /* Kept in the copy as a table entry only. It looks selected because it IS in
     the copy; the cursor says it is not something to click. */
  .seg.layout-only { cursor: default; }
  .seg.selected { background: var(--seg-sel); box-shadow: inset 0 0 0 2px var(--primary); }
  .usedfill { position: absolute; top: 0; bottom: 0; left: 0; z-index: 0; background: var(--seg-used); pointer-events: none; }
  .seg-top { position: relative; z-index: 1; display: flex; align-items: center; gap: 5px; color: var(--text); }
  .lock { color: var(--text-muted); flex-shrink: 0; }
  .enc { font-size: 10px; line-height: 1; flex-shrink: 0; display: flex; }
  .seg-label { font-size: 12px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta { position: relative; z-index: 1; font-size: 11px; color: var(--text-secondary); line-height: 1.35; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta .muted { color: var(--text-muted); }
  .check { position: absolute; top: 5px; right: 6px; z-index: 2; width: 16px; height: 16px; border-radius: 3px; background: var(--primary); display: flex; align-items: center; justify-content: center; }
</style>
