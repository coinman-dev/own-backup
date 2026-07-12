<script lang="ts">
  // Paragon-style disk map: one physical disk drawn as proportional partition
  // segments. Extracted from the backup wizard's source tab so the Restore
  // screen can reuse the exact same visuals (read-only for the backed-up disk,
  // selectable for picking a restore target). No decryption/boot-toggle logic —
  // that stays in the wizard; here segments are shown, and optionally clickable.
  import { formatBytes } from '../lib/format';
  import type { DiskInfo, PartitionInfo } from '../lib/types';

  interface Props {
    disk: DiskInfo;
    /** Devices to render as selected/checked. */
    selected?: string[];
    /** Partition indices to render as selected — used when highlighting comes
     *  from embedded metadata (works for letterless partitions too). */
    selectedIndices?: number[];
    /** When true, segments are shown but not clickable. */
    readonly?: boolean;
    /** Show the "загрузочный" badge and highlight the header. */
    boot?: boolean;
    /** Called with (device, label) when a selectable segment is clicked. */
    onToggle?: (dev: string, label: string) => void;
  }
  let { disk, selected = [], selectedIndices = [], readonly = false, boot = false, onToggle }: Props = $props();

  const FS_LABELS: Record<string, string> = {
    ntfs: 'NTFS', fat32: 'FAT32', ex_fat: 'exFAT', ext4: 'ext4', xfs: 'XFS',
    btrfs: 'Btrfs', re_fs: 'ReFS', unknown: '',
  };
  function fsLabel(fs: string): string {
    return FS_LABELS[fs] ?? (fs === 'unknown' ? '' : fs);
  }
  function tableLabel(t: string): string {
    return t === 'gpt' ? 'GPT' : t === 'mbr' ? 'MBR' : t.toUpperCase();
  }
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  function partLabel(p: PartitionInfo): string {
    if (p.volume_letter) {
      const name = p.volume_label || fsLabel(p.filesystem) || 'Том';
      return `${p.volume_letter}: ${name}`;
    }
    return p.volume_label || fsLabel(p.filesystem) || 'Раздел';
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
    if (p.used_bytes > 0) t += ` (${formatBytes(p.used_bytes)} занято)`;
    const fs = partFs(p);
    if (fs) t += ` ${fs}`;
    return t;
  }

  interface DiskSeg { kind: 'part' | 'free'; size: number; p?: PartitionInfo }
  function diskSegments(d: DiskInfo): DiskSeg[] {
    const parts = [...d.partitions].sort((a, b) => a.offset_bytes - b.offset_bytes);
    const segs: DiskSeg[] = [];
    const minGap = d.size_bytes * 0.01;
    let cursor = 0;
    for (const p of parts) {
      const gap = p.offset_bytes - cursor;
      if (gap > minGap) segs.push({ kind: 'free', size: gap });
      segs.push({ kind: 'part', size: p.size_bytes, p });
      cursor = Math.max(cursor, p.offset_bytes + p.size_bytes);
    }
    const tail = d.size_bytes - cursor;
    if (tail > minGap) segs.push({ kind: 'free', size: tail });
    return segs;
  }

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
    {#if p.is_system || p.is_boot}
      <svg class="lock" width="10" height="10" viewBox="0 0 24 24" fill="currentColor"><path d="M12 2a5 5 0 0 0-5 5v3H6a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-9a1 1 0 0 0-1-1h-1V7a5 5 0 0 0-5-5zm-3 8V7a3 3 0 0 1 6 0v3z"/></svg>
    {/if}
    {#if p.is_encrypted}<span class="enc" title="Зашифрован">🔒</span>{/if}
    <span class="seg-label">{partLabel(p)}</span>
  </div>
  <div class="seg-meta">{formatBytes(p.size_bytes)}{#if p.used_bytes > 0}<span class="muted">&nbsp;({formatBytes(p.used_bytes)} занято)</span>{/if}{#if partFs(p)}<span class="muted">&nbsp;{partFs(p)}</span>{/if}</div>
  {#if sel}<div class="check"><svg width="10" height="10" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></div>{/if}
{/snippet}

<div class="disk-card">
  <div class="disk-head" class:whole={boot}>
    <span class="disk-model">{disk.model || disk.device_id}</span>
    <span class="disk-head-right">
      {#if boot}<span class="whole-badge">загрузочный</span>{/if}
      <span class="disk-meta">{formatBytes(disk.size_bytes)} · {tableLabel(disk.partition_table)}</span>
    </span>
  </div>
  <div class="disk-body">
    <div class="disk-ico">
      <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.3"><path d="M3 8a9 3 0 0 0 18 0M3 8a9 3 0 0 1 18 0v8a9 3 0 0 1-18 0z"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>
      <div class="disk-name">Диск {diskDigits}</div>
    </div>
    <div class="bar">
      {#each diskSegments(disk) as seg}
        {#if seg.kind === 'free'}
          <div class="seg free" style="flex: {seg.size} 1 0"><span class="free-lbl">Не занято</span></div>
        {:else if seg.p}
          {@const dev = partDevice(seg.p)}
          {@const sel = isSelP(seg.p, dev)}
          {#if dev && !readonly && onToggle}
            <button
              type="button"
              class="seg selectable"
              class:selected={sel}
              style="flex: {seg.size} 1 0"
              title={segTitle(seg.p)}
              onclick={() => onToggle(dev, partLabel(seg.p!))}
            >{@render segInner(seg.p, sel)}</button>
          {:else}
            <div class="seg ro" class:selected={sel} style="flex: {seg.size} 1 0" title={segTitle(seg.p)}>
              {@render segInner(seg.p, sel)}
            </div>
          {/if}
        {/if}
      {/each}
    </div>
  </div>
</div>

<style>
  .disk-card { border: 1px solid var(--border); border-radius: 6px; overflow: hidden; background: var(--card-bg); }
  .disk-head { width: 100%; display: flex; align-items: center; justify-content: space-between; padding: 6px 12px; background: var(--panel2); border: 0; border-bottom: 1px solid var(--border); margin: 0; font-family: inherit; text-align: left; }
  .disk-head.whole { background: rgba(45,125,210,.12); color: var(--primary); }
  .disk-head.whole .disk-model { color: var(--primary); }
  .disk-head-right { display: flex; align-items: center; gap: 10px; }
  .whole-badge { font-size: 11px; font-weight: 700; color: #fff; background: var(--primary); padding: 2px 8px; border-radius: 10px; }
  .disk-model { font-size: 13px; font-weight: 600; color: var(--head); }
  .disk-meta { font-size: 12px; color: var(--text-muted); }
  .disk-body { display: flex; align-items: center; gap: 14px; padding: 7px 12px; }
  .disk-ico { width: 52px; flex-shrink: 0; text-align: center; color: var(--text-muted); }
  .disk-name { font-size: 12px; color: var(--head); margin-top: 1px; }
  .bar { flex: 1; display: flex; height: 44px; border: 1px solid var(--border); border-radius: 3px; overflow: hidden; background: var(--panel2); }
  .seg { position: relative; padding: 5px 8px; min-width: 84px; border: 0; border-right: 1px solid var(--border); margin: 0; display: flex; flex-direction: column; gap: 3px; background: rgba(45,125,210,.06); overflow: hidden; text-align: left; font-family: inherit; color: inherit; appearance: none; -webkit-appearance: none; }
  .seg:last-child { border-right: none; }
  .seg.free { min-width: 40px; align-items: center; justify-content: center; background: repeating-linear-gradient(135deg, var(--panel2) 0 6px, var(--border) 6px 7px); }
  .free-lbl { font-size: 11px; color: var(--text-muted); white-space: nowrap; }
  .seg.selectable { cursor: pointer; }
  .seg.selectable:hover { filter: brightness(0.97); }
  .seg.ro { cursor: default; }
  .seg.selected { background: rgba(45,125,210,.16); box-shadow: inset 0 0 0 2px var(--primary); }
  .usedfill { position: absolute; top: 0; bottom: 0; left: 0; z-index: 0; background: rgba(45,125,210,.14); border-right: 1px solid rgba(45,125,210,.45); pointer-events: none; }
  .seg.selected .usedfill { background: rgba(45,125,210,.24); border-right-color: rgba(45,125,210,.6); }
  .seg-top { position: relative; z-index: 1; display: flex; align-items: center; gap: 5px; color: var(--head); }
  .lock { color: var(--text-muted); flex-shrink: 0; }
  .enc { font-size: 10px; line-height: 1; flex-shrink: 0; }
  .seg-label { font-size: 12px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta { position: relative; z-index: 1; font-size: 11px; color: var(--head); line-height: 1.35; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta .muted { color: var(--text-muted); }
  .check { position: absolute; top: 5px; right: 6px; z-index: 2; width: 16px; height: 16px; border-radius: 3px; background: var(--primary); display: flex; align-items: center; justify-content: center; }
</style>
