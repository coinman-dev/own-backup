<script lang="ts">
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  // DESIGN STAGE: `runRestore` is deliberately NOT imported — this screen must
  // not be able to write to a disk. The partition editor and the mapping are
  // in-memory only; "Восстановить" just lists the queued operations.
  import { engine, cancelCurrent, clearRestoreResult } from '../lib/engineStore.svelte';
  import { jobs } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import InfoModal from '../components/InfoModal.svelte';
  import DiskMap from '../components/DiskMap.svelte';
  import type { MapSlot } from '../components/DiskMap.svelte';
  import type { Job, DiskInfo, PartitionInfo, ImageInfo } from '../lib/types';

  interface Props {
    job: Job;
  }
  let { job }: Props = $props();

  let confirmOpen = $state(false);
  let confirmText = $state('');
  let formError: string | null = $state(null);
  let stubMsg: string | null = $state(null);

  const lastRun = $derived(job.last_run);

  // The archived-disk card read from the IMAGE FILE's embedded metadata, so it
  // works WITHOUT jobs.json (e.g. a Boot-CD restore). Reloaded whenever the
  // selected copy's image path changes; null until loaded / for old images.
  let imageInfo = $state<ImageInfo | null>(null);
  $effect(() => {
    const path = lastRun?.status === 'success' ? lastRun.image_path : '';
    if (!path) {
      imageInfo = null;
      return;
    }
    let cancelled = false;
    api.readImageInfo(path).then(
      (info) => { if (!cancelled) imageInfo = info; },
      () => { if (!cancelled) imageInfo = null; },
    );
    return () => { cancelled = true; };
  });

  // All point-in-time copies of THIS job's archive, discovered by scanning its
  // folder for images with a matching embedded job id (no jobs.json), newest
  // first — with a navigator to page between them. Empty for a single imported
  // file, in which case the run-record image (imageInfo) is used instead.
  let copies = $state<ImageInfo[]>([]);
  let copyIndex = $state(0);
  $effect(() => {
    const folder = job.dest_folder;
    const jid = job.id;
    if (!folder || jid.startsWith('file:')) {
      copies = [];
      copyIndex = 0;
      return;
    }
    let cancelled = false;
    api.listImageBackups(folder).then(
      (list) => {
        if (cancelled) return;
        copies = list
          .filter((i) => i.job_id === jid)
          .sort((a, b) => (a.created_at < b.created_at ? 1 : -1)); // newest first
        copyIndex = 0;
      },
      () => {
        if (!cancelled) copies = [];
      },
    );
    return () => { cancelled = true; };
  });

  // The copy currently shown & restored: the selected point-in-time from the
  // scan, or the single run-record image when there's no scan.
  const activeInfo = $derived(copies.length > 0 ? copies[copyIndex] : imageInfo);
  const hasMultiple = $derived(copies.length > 1);

  // Excluded-files line: prefer the count embedded in the active image (works
  // from a file without jobs.json), fall back to the run record.
  const excluded = $derived({
    files: activeInfo?.excluded_files ?? lastRun?.excluded_files ?? 0,
    bytes: activeInfo?.excluded_bytes ?? lastRun?.excluded_bytes ?? 0,
  });

  // Real file-category breakdown of the archived data — same source as the
  // backup dashboard (collected during the backup's file walk).
  const CAT_META = [
    { key: 'images', name: 'Изображения', color: '#5ab02f' },
    { key: 'video', name: 'Видео', color: '#d8c81e' },
    { key: 'audio', name: 'Аудио', color: '#e07a2f' },
    { key: 'documents', name: 'Документы', color: '#8b5cf6' },
    { key: 'windows', name: 'Windows', color: '#4f9be8' },
    { key: 'linux', name: 'Linux', color: '#14b8a6' },
    { key: 'other', name: 'Другое', color: '#9aa0ad' },
  ] as const;
  const catData = $derived.by(() => {
    // Prefer the breakdown embedded in the image (works without jobs.json);
    // fall back to the run record for images taken before it was embedded.
    const c = activeInfo?.categories ?? lastRun?.categories;
    if (!c) return null;
    const total = CAT_META.reduce((s, m) => s + (c[m.key] ?? 0), 0);
    if (total <= 0) return null;
    return CAT_META.map((m) => ({
      ...m,
      bytes: c[m.key] ?? 0,
      pct: ((c[m.key] ?? 0) / total) * 100,
    }));
  });
  // --- Disk maps: top = the disk this copy captured (read-only), bottom = all
  // system disks to pick a restore target from (same visuals as the source tab).
  function diskNumber(d: DiskInfo): number {
    const m = d.device_id.match(/(\d+)\s*$/);
    return m ? parseInt(m[1], 10) : 0;
  }
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  function isBootableCopy(d: DiskInfo): boolean {
    return job.disk_source != null && job.disk_source.disk_number === diskNumber(d);
  }
  function diskHasSelection(d: DiskInfo): boolean {
    if (isBootableCopy(d)) return true;
    return d.partitions.some((p) => {
      const dev = partDevice(p);
      return !!dev && job.sources.includes(dev);
    });
  }
  // The archived disk map, in priority order:
  //  1. the snapshot embedded in the IMAGE FILE (works without jobs.json / on a
  //     Boot-CD; also correct when the original disk is detached),
  //  2. the snapshot stored on the job at creation (jobs.json),
  //  3. a live query of currently-attached disks (pre-snapshot backups).
  const backedUpDisks = $derived(
    activeInfo?.source_disk_layout
      ? [activeInfo.source_disk_layout]
      : job.source_disks && job.source_disks.length > 0
        ? job.source_disks
        : engine.disks.filter(diskHasSelection),
  );
  const systemDisks = $derived(engine.disks);

  // --- Restore-mapping wizard (what → where) --------------------------------
  // Step 1: pick a target disk below (others collapse; source selection clears).
  // Step 2: click a source partition (turns yellow), then a destination one —
  //         it fits → both green; too small → destination red + a message.
  // Step 3: repeat, then Восстановить.
  let targetDiskId = $state<string | null>(null);
  let srcPart = $state<number | null>(null); // yellow source partition index
  let mappings = $state<{ src: number; dst: number }[]>([]);
  let badTarget = $state<number | null>(null); // last destination that didn't fit
  let fitMsg = $state<string | null>(null);
  // "загрузочный" checkbox — restore the whole bootable set (all partitions of
  // the copy + boot data). Pre-checked while no target disk is chosen when the
  // copy actually holds boot data; picking a target clears it so the user opts
  // in deliberately.
  let wholeDiskRestore = $state(false);
  $effect(() => {
    if (!targetDiskId) wholeDiskRestore = activeInfo?.is_whole_disk ?? false;
  });

  // Partitions actually inside this copy — from the image's embedded metadata,
  // falling back to the job's selected source devices. Anything else on the disk
  // is drawn as an empty "не в бэкапе" slot.
  const backedUpParts = $derived.by(() => {
    const fromImage = activeInfo?.backed_up_parts ?? [];
    if (fromImage.length > 0) return fromImage;
    const d = backedUpDisks[0];
    if (!d) return [] as number[];
    return d.partitions
      .filter((p) => !!p.volume_device && job.sources.includes(p.volume_device))
      .map((p) => p.index);
  });

  const targetDisk = $derived(systemDisks.find((d) => d.device_id === targetDiskId) ?? null);
  const srcDisk = $derived(backedUpDisks[0] ?? null);
  // Source partitions that are actually inside this copy.
  const srcBackupParts = $derived(
    (srcDisk?.partitions ?? []).filter((p) => backedUpParts.includes(p.index)),
  );
  const srcTotalBytes = $derived(srcBackupParts.reduce((s, p) => s + p.size_bytes, 0));

  // --- Simple partition editor on the target disk ---------------------------
  // The layout is an explicit list of cells. Deleting a partition turns ITS cell
  // into a free cell with the same boundaries, so nothing around it moves;
  // creating splits a free cell in place. Nothing is written to the disk.
  let slots = $state<MapSlot[] | null>(null);
  let previewIdx = $state<number[]>([]); // partitions the restore would create
  let menu = $state<{ x: number; y: number; part?: PartitionInfo; offset?: number; size?: number } | null>(null);
  let delConfirm = $state<PartitionInfo | null>(null);
  const MIB = 1024 * 1024;
  const GIB = 1024 * 1024 * 1024;
  const TIB = 1024 * GIB;
  /** GPT partition-type GUIDs (the same ones `common::gpt` uses). The firmware
   *  finds the boot partition by this GUID, so it is not cosmetic. */
  const GPT_TYPES = [
    { guid: 'EBD0A0A2-B9E5-4433-87C0-68B6B72699C7', name: 'Данные (Basic Data)' },
    { guid: 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B', name: 'EFI System (ESP)' },
    { guid: 'E3C9E316-0B5C-4DB8-817D-F92DF00215AE', name: 'Microsoft Reserved (MSR)' },
    { guid: 'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC', name: 'Windows Recovery' },
    { guid: '0FC63DAF-8483-4772-8E79-3D69D8477DE4', name: 'Linux filesystem' },
    { guid: '0657FD6D-A4AB-43C4-84E5-0933C84B4F4F', name: 'Linux swap' },
  ];
  const isGpt = $derived((targetDisk?.partition_table ?? 'gpt') !== 'mbr');
  /** Drive letters already taken anywhere in the system. */
  const usedLetters = $derived(
    new Set(
      systemDisks.flatMap((d) => d.partitions.map((p) => p.volume_letter).filter(Boolean) as string[]),
    ),
  );
  const freeLetters = $derived(
    'DEFGHIJKLMNOPQRSTUVWXYZ'.split('').filter((l) => !usedLetters.has(l)),
  );

  let createDlg = $state<{
    offset: number;
    /** 1 MiB-aligned start of the new partition. */
    start: number;
    max: number;
    size: string;
    unit: 'MiB' | 'GiB' | 'TiB';
    fs: string;
    label: string;
    gptType: string;
    mbrKind: 'primary' | 'logical';
    active: boolean;
    letter: string;
    quickFormat: boolean;
  } | null>(null);
  let opsOpen = $state(false);
  /** Extra attributes of hand-created partitions, for the operations queue. */
  let newPartMeta = $state(
    new Map<number, { gptType: string; mbrKind: string; active: boolean; letter: string; quickFormat: boolean }>(),
  );

  const curParts = $derived(
    (slots ?? []).filter((s) => s.kind === 'part').map((s) => (s as { p: PartitionInfo }).p),
  );

  // Undo stack: every editing action snapshots the state first, so the back
  // arrow rolls back exactly one step (delete / create / place / map).
  type Snapshot = {
    slots: MapSlot[] | null;
    mappings: { src: number; dst: number }[];
    previewIdx: number[];
    srcPart: number | null;
  };
  let history = $state<Snapshot[]>([]);
  const canUndo = $derived(history.length > 0);
  function pushHistory() {
    history = [...history, { slots: slots ? [...slots] : null, mappings: [...mappings], previewIdx: [...previewIdx], srcPart }];
  }
  function undo() {
    const h = history[history.length - 1];
    if (!h) return;
    slots = h.slots;
    mappings = h.mappings;
    previewIdx = h.previewIdx;
    srcPart = h.srcPart;
    history = history.slice(0, -1);
    badTarget = null;
    fitMsg = null;
  }

  /** Smallest readable cell width in PIXELS (same floor the non-edited map uses
   *  via `min-width`), converted to a share of the measured bar. Hard-coding a
   *  percentage would shrink small partitions on wide windows and clip their
   *  labels on narrow ones. */
  const MIN_PX = 84;
  let dashWidth = $state(0);
  const minShare = $derived.by(() => {
    // Bar width = content width minus the disk icon column, gaps and paddings.
    const bar = Math.max(240, (dashWidth || 900) - 180);
    return Math.min(25, (MIN_PX / bar) * 100);
  });

  /** Split 100% between cells: anything whose true proportion is below the
   *  readable floor gets the floor, the rest share what remains in proportion
   *  (water-filling). Computed ONCE per disk — afterwards the shares only move
   *  between cells, which is what keeps untouched partitions perfectly still. */
  function allocateShares(sizes: number[], floor: number = minShare): number[] {
    const n = sizes.length;
    if (n === 0) return [];
    const shares = new Array(n).fill(0);
    const pinned = new Array(n).fill(false);
    for (;;) {
      const pinnedShare = shares.reduce((a, s, i) => a + (pinned[i] ? s : 0), 0);
      const restShare = Math.max(0, 100 - pinnedShare);
      const restSize = sizes.reduce((a, s, i) => a + (pinned[i] ? 0 : s), 0) || 1;
      let changed = false;
      for (let i = 0; i < n; i++) {
        if (pinned[i]) continue;
        const s = (sizes[i] / restSize) * restShare;
        if (s < floor) {
          shares[i] = floor;
          pinned[i] = true;
          changed = true;
        } else {
          shares[i] = s;
        }
      }
      if (!changed || pinned.every(Boolean)) break;
    }
    // Normalise (many tiny cells can overshoot once they all hit the floor).
    const sum = shares.reduce((a, b) => a + b, 0) || 1;
    return shares.map((s) => (s * 100) / sum);
  }

  // Build the initial grid: partitions in offset order, plus the real gaps.
  function buildSlots(d: DiskInfo): MapSlot[] {
    const parts = [...d.partitions].sort((a, b) => a.offset_bytes - b.offset_bytes);
    const raw: { kind: 'part' | 'free'; size: number; offset: number; p?: PartitionInfo }[] = [];
    // Absolute threshold: a percentage of the disk hid real free space (1% of a
    // 477 GiB disk is 4.77 GiB — and 20 GiB on a 2 TB one). Only sub-MiB
    // alignment slivers are not worth a cell.
    const minGap = MIB;
    let cursor = 0;
    for (const p of parts) {
      const gap = p.offset_bytes - cursor;
      if (gap > minGap) raw.push({ kind: 'free', size: gap, offset: cursor });
      raw.push({ kind: 'part', size: p.size_bytes, offset: p.offset_bytes, p });
      cursor = Math.max(cursor, p.offset_bytes + p.size_bytes);
    }
    const tail = d.size_bytes - cursor;
    if (tail > minGap) raw.push({ kind: 'free', size: tail, offset: cursor });
    const shares = allocateShares(raw.map((r) => r.size));
    return raw.map((r, i) =>
      r.kind === 'part'
        ? { kind: 'part', p: r.p!, share: shares[i] }
        : { kind: 'free', offset: r.offset, size: r.size, share: shares[i] },
    );
  }

  const slotOffset = (s: MapSlot) => (s.kind === 'part' ? s.p.offset_bytes : s.offset);

  /** The layout invariant, re-established after every edit: cells are ordered by
   *  their position on the disk and unallocated space is always a single merged
   *  region. Without it a new partition could end up drawn in the middle of a
   *  free area instead of at its left edge. */
  function normalize(list: MapSlot[]): MapSlot[] {
    return mergeFree([...list].sort((a, b) => slotOffset(a) - slotOffset(b)));
  }

  /** Merge neighbouring free cells into one region: their sizes AND their width
   *  shares add up, so the freed space becomes a single labelled block that
   *  occupies exactly the pixels its partitions did. */
  function mergeFree(list: MapSlot[]): MapSlot[] {
    const out: MapSlot[] = [];
    for (const s of list) {
      const prev = out[out.length - 1];
      if (s.kind === 'free' && prev && prev.kind === 'free') {
        out[out.length - 1] = {
          kind: 'free',
          offset: Math.min(prev.offset, s.offset),
          size: prev.size + s.size,
          share: prev.share + s.share,
        };
      } else {
        out.push(s);
      }
    }
    return out;
  }

  const step = $derived(
    !targetDiskId ? 1 : wholeDiskRestore ? (previewIdx.length > 0 ? 3 : 2) : mappings.length === 0 ? 2 : 3,
  );
  const instr = $derived(
    step === 1
      ? 'Шаг 1. Выберите диск, на который восстановить — кликните на него ниже.'
      : step === 2
        ? wholeDiskRestore
          ? 'Шаг 2. Кликните свободное (не распределённое) место внизу — туда лягут все разделы копии.'
          : 'Шаг 2. Кликните раздел сверху (станет жёлтым), затем раздел ниже — куда его скопировать.'
        : 'Шаг 3. Повторите для остальных разделов и нажмите «Восстановить» или нажмите «Отмена» чтобы начать заново.',
  );

  function resetMapping() {
    srcPart = null;
    mappings = [];
    badTarget = null;
    fitMsg = null;
    previewIdx = [];
  }
  function selectTargetDisk(d: DiskInfo) {
    targetDiskId = d.device_id;
    slots = normalize(buildSlots(d));
    history = [];
    // The user opts in to a whole-disk restore themselves at this point.
    wholeDiskRestore = false;
    resetMapping();
  }
  // "Отмена": undo everything and go back to step 1.
  function pickAnotherDisk() {
    targetDiskId = null;
    slots = null;
    history = [];
    wholeDiskRestore = false;
    resetMapping();
  }
  // Toggling "загрузочный" switches between whole-disk and per-partition modes.
  function toggleWholeDisk(v: boolean) {
    wholeDiskRestore = v;
    resetMapping();
  }
  function clickSourcePart(p: PartitionInfo) {
    if (wholeDiskRestore) return; // whole-disk: the selection is fixed
    badTarget = null;
    fitMsg = null;
    srcPart = p.index;
  }

  // Whole-disk mode: drop the entire backup layout into a free stretch. The run
  // of adjacent free cells is replaced by the copy's partitions (plus whatever
  // space is left over), so the surrounding cells keep their exact geometry.
  function clickFreeSpace(offset: number, _size: number) {
    if (!wholeDiskRestore || !slots) return;
    const i = slots.findIndex((s) => s.kind === 'free' && s.offset === offset);
    if (i < 0) return;
    const cell = slots[i] as { offset: number; size: number; share: number };
    if (cell.size < srcTotalBytes) {
      // Exact numbers + the shortfall: rounded ones read as "нужно 130 GiB,
      // свободно 130 GiB", which tells the user nothing.
      const short = srcTotalBytes - cell.size;
      fitMsg =
        `Недостаточно места: нужно ${preciseSize(srcTotalBytes)}, свободно ${preciseSize(cell.size)}` +
        ` — не хватает ${preciseSize(short)}. Удалите разделы или выберите другой участок.`;
      return;
    }
    pushHistory();
    // The region's width budget is split between the placed partitions (and any
    // leftover free space), so the cells around it do not move.
    const sizes = srcBackupParts.map((p) => p.size_bytes);
    const rest = cell.size - srcTotalBytes;
    if (rest > 0) sizes.push(rest);
    // The sub-shares are scaled by the cell's own share, so the readable floor
    // has to be scaled up by the same factor — otherwise a cell that owns 40% of
    // the bar would give its smallest partition 0.4 × the floor.
    const sub = allocateShares(sizes, (minShare * 100) / cell.share).map(
      (s) => (s * cell.share) / 100,
    );

    let cursor = cell.offset;
    let next = Math.max(0, ...curParts.map((p) => p.index)) + 1;
    const placed: MapSlot[] = [];
    srcBackupParts.forEach((sp, k) => {
      placed.push({
        kind: 'part',
        p: { ...sp, index: next, offset_bytes: cursor, volume_device: '', volume_letter: null },
        share: sub[k],
      });
      cursor += sp.size_bytes;
      next += 1;
    });
    previewIdx = placed.map((s) => (s as { p: PartitionInfo }).p.index);
    if (rest > 0) placed.push({ kind: 'free', offset: cursor, size: rest, share: sub[sub.length - 1] });
    slots = normalize([...slots.slice(0, i), ...placed, ...slots.slice(i + 1)]);
    fitMsg = null;
  }

  // --- Partition editor actions --------------------------------------------
  function openPartMenu(p: PartitionInfo, ev: MouseEvent) {
    menu = { x: ev.clientX, y: ev.clientY, part: p };
  }
  function openFreeMenu(offset: number, size: number, ev: MouseEvent) {
    menu = { x: ev.clientX, y: ev.clientY, offset, size };
  }
  // Delete = the partition's cell becomes a free cell with the SAME offset and
  // size, so every other partition keeps its position and width on the map.
  function doDelete() {
    const p = delConfirm;
    delConfirm = null;
    if (!p || !slots) return;
    pushHistory();
    // The cell keeps its width budget and becomes free; adjacent free cells then
    // merge into ONE region, so its label sits at the region's start and the
    // partitions around it stay exactly where they were.
    slots = normalize(
      slots.map((s) =>
        s.kind === 'part' && s.p.index === p.index
          ? { kind: 'free' as const, offset: p.offset_bytes, size: p.size_bytes, share: s.share }
          : s,
      ),
    );
    previewIdx = previewIdx.filter((i) => i !== p.index);
    mappings = mappings.filter((m) => m.dst !== p.index);
  }
  function openCreate(offset: number, max: number) {
    menu = null;
    // Alignment is not a user choice: a partition that does not start on a 1 MiB
    // boundary makes every cluster straddle two physical 4 KiB sectors (and SSD
    // pages), costing a read-modify-write on each write. We always align up.
    const start = Math.ceil(offset / MIB) * MIB;
    const avail = Math.floor((offset + max - start) / MIB) * MIB;
    createDlg = {
      offset,
      start,
      max: Math.max(0, avail),
      // Pre-fill with the EXACT maximum. Rounding it down to whole GiB left a
      // leftover sliver (130.28 GiB of free space became a "130" default and a
      // 287 MiB gap the user never asked for).
      size: avail >= GIB ? (avail / GIB).toFixed(2) : String(Math.round(avail / MIB)),
      unit: avail >= GIB ? 'GiB' : 'MiB',
      fs: 'ntfs',
      label: '',
      gptType: GPT_TYPES[0].guid,
      mbrKind: 'primary',
      active: false,
      letter: '',
      quickFormat: true,
    };
  }
  /** Size without the rounding `formatBytes` does — used where a rounded value
   *  would mislead (a 17 KiB shortfall must not read as "0 MiB"). */
  function preciseSize(bytes: number): string {
    if (bytes >= GIB) return `${(bytes / GIB).toFixed(2)} GiB`;
    if (bytes >= MIB) return `${(bytes / MIB).toFixed(2)} MiB`;
    if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
    return `${bytes} B`;
  }
  /** Keep the partition type in step with the chosen filesystem. */
  function onFsChange(fs: string) {
    if (!createDlg) return;
    createDlg.fs = fs;
    createDlg.gptType = ['ext4', 'xfs', 'btrfs'].includes(fs) ? GPT_TYPES[4].guid : GPT_TYPES[0].guid;
  }
  // Create = split that free cell in place: [new partition][leftover free].
  function doCreate() {
    const d = createDlg;
    if (!d || !slots) return;
    const mult = d.unit === 'TiB' ? TIB : d.unit === 'GiB' ? GIB : MIB;
    const raw = parseFloat(d.size.replace(',', '.')) * mult;
    if (!Number.isFinite(raw) || raw <= 0) return;
    // Round the size down to whole MiB so the NEXT partition also starts aligned.
    let bytes = Math.floor(raw / MIB) * MIB;
    if (bytes <= 0) {
      fitMsg = 'Размер слишком мал (минимум 1 МиБ).';
      return;
    }
    if (bytes > d.max) {
      fitMsg = `Размер больше свободного места (${formatBytes(d.max)}).`;
      return;
    }
    // Normally the region still starts where the menu was opened; if it has been
    // merged/changed since, fall back to the region that CONTAINS that point, so
    // the new partition still lands at that region's left edge.
    let i = slots.findIndex((s) => s.kind === 'free' && s.offset === d.offset);
    if (i < 0) {
      i = slots.findIndex(
        (s) => s.kind === 'free' && d.offset >= s.offset && d.offset < s.offset + s.size,
      );
    }
    if (i < 0) return;
    pushHistory();
    const cell = slots[i] as { offset: number; size: number; share: number };
    const next = Math.max(0, ...curParts.map((p) => p.index)) + 1;
    // Partitions always fill a free region from its LEFT edge — the start comes
    // from the region itself, never from wherever inside it the user clicked.
    // Only the 1 MiB alignment may skip a few bytes at the very beginning.
    const start = Math.ceil(cell.offset / MIB) * MIB;
    const head = start - cell.offset;
    // Snap to the region's right edge: a leftover smaller than SNAP is unusable
    // anyway, so absorb it instead of leaving a sliver nobody asked for.
    const SNAP = 16 * MIB;
    const avail = cell.size - head;
    if (avail - bytes < SNAP) bytes = avail;
    const tail = avail - bytes;
    const showHead = head >= MIB; // a sub-MiB sliver is not worth its own cell
    const parts = [showHead ? head : 0, bytes, tail].filter((b) => b > 0);
    const sub = allocateShares(parts, (minShare * 100) / cell.share).map(
      (s) => (s * cell.share) / 100,
    );
    let si = 0;
    const made: MapSlot[] = [];
    if (showHead) made.push({ kind: 'free', offset: cell.offset, size: head, share: sub[si++] });
    made.push(
      {
        share: sub[si++],
        kind: 'part',
        p: {
          index: next,
          offset_bytes: start,
          size_bytes: bytes,
          used_bytes: 0,
          gpt_type_guid: isGpt ? d.gptType : '',
          gpt_partition_guid: '',
          filesystem: d.fs,
          volume_letter: d.letter || null,
          volume_device: '',
          volume_label: d.label,
          fs_uuid: '',
          is_system: false,
          is_boot: isGpt && d.gptType === GPT_TYPES[1].guid,
          is_encrypted: false,
        } as PartitionInfo,
      },
    );
    // Remember the extra attributes for the operations queue.
    newPartMeta.set(next, {
      gptType: d.gptType,
      mbrKind: d.mbrKind,
      active: d.active,
      letter: d.letter,
      quickFormat: d.quickFormat,
    });
    if (tail > 0) made.push({ kind: 'free', offset: start + bytes, size: tail, share: sub[si] });
    slots = normalize([...slots.slice(0, i), ...made, ...slots.slice(i + 1)]);
    createDlg = null;
    fitMsg = null;
  }

  // Human-readable queue of what "Восстановить" would do (design preview only).
  const operations = $derived.by(() => {
    const ops: string[] = [];
    const orig = targetDisk?.partitions ?? [];
    const cur = curParts;
    for (const p of orig) {
      if (!cur.some((c) => c.index === p.index)) {
        ops.push(`Удалить раздел ${p.volume_letter ? p.volume_letter + ': ' : ''}${p.volume_label || '(без метки)'} — ${formatBytes(p.size_bytes)}`);
      }
    }
    for (const p of cur) {
      if (orig.some((o) => o.index === p.index) || previewIdx.includes(p.index)) continue;
      const m = newPartMeta.get(p.index);
      const bits = [formatBytes(p.size_bytes)];
      if (p.filesystem) bits.push(p.filesystem.toUpperCase());
      if (m) {
        bits.push(isGpt ? (GPT_TYPES.find((t) => t.guid === m.gptType)?.name ?? 'тип') : m.mbrKind === 'primary' ? 'основной' : 'логический');
        if (!isGpt && m.active) bits.push('активный');
        if (m.letter) bits.push(`буква ${m.letter}:`);
        bits.push(m.quickFormat ? 'быстрое форматирование' : 'полное форматирование');
      }
      bits.push('выравнивание 1 МиБ');
      ops.push(`Создать раздел ${p.volume_label || '(без метки)'} — ${bits.join(', ')}`);
    }
    if (wholeDiskRestore && previewIdx.length > 0) {
      ops.push(`Восстановить все разделы копии (${srcBackupParts.length} шт., ${formatBytes(srcTotalBytes)}) и загрузочные данные`);
    }
    for (const m of mappings) {
      const sp = srcDisk?.partitions.find((x) => x.index === m.src);
      const dp = cur.find((x) => x.index === m.dst);
      ops.push(
        `Восстановить «${sp?.volume_label || sp?.volume_letter || 'раздел ' + m.src}» → «${dp?.volume_label || dp?.volume_letter || 'раздел ' + m.dst}» (${formatBytes(sp?.size_bytes ?? 0)})`,
      );
    }
    return ops;
  });
  function clickTargetPart(p: PartitionInfo) {
    if (wholeDiskRestore) {
      fitMsg = 'В режиме «загрузочный» выберите свободное место, а не существующий раздел.';
      return;
    }
    if (srcPart == null) {
      fitMsg = 'Сначала выберите раздел сверху.';
      return;
    }
    const sp = srcDisk?.partitions.find((x) => x.index === srcPart);
    if (!sp) {
      srcPart = null;
      return;
    }
    const need = sp.used_bytes > 0 ? sp.used_bytes : sp.size_bytes;
    if (p.size_bytes < need) {
      badTarget = p.index;
      fitMsg = `Раздел не подходит: нужно ≥ ${formatBytes(need)}, а доступно ${formatBytes(p.size_bytes)}.`;
      return;
    }
    pushHistory();
    mappings = [...mappings.filter((m) => m.src !== srcPart && m.dst !== p.index), { src: srcPart, dst: p.index }];
    srcPart = null;
    badTarget = null;
    fitMsg = null;
  }
  // Both sides return the mapping's ordinal as `pair`, so a source partition and
  // its destination get the exact same green shade.
  function srcState(index: number) {
    // Whole-disk mode: every partition of the copy is taken and locked — yellow
    // while waiting for a destination, then green (paired with the partition it
    // will be written to) once the layout has been placed.
    if (wholeDiskRestore) {
      const k = srcBackupParts.findIndex((p) => p.index === index);
      if (k < 0) return undefined;
      return previewIdx.length > 0 ? { kind: 'green' as const, pair: k } : { kind: 'yellow' as const };
    }
    if (index === srcPart) return { kind: 'yellow' as const };
    const i = mappings.findIndex((m) => m.src === index);
    return i >= 0 ? { kind: 'green' as const, pair: i } : undefined;
  }
  function dstState(index: number) {
    // Placed partitions share the exact green shade of their source partition.
    const k = previewIdx.indexOf(index);
    if (k >= 0) return { kind: 'green' as const, pair: k };
    const i = mappings.findIndex((m) => m.dst === index);
    if (i >= 0) return { kind: 'green' as const, pair: i };
    if (index === badTarget) return { kind: 'red' as const };
    return undefined;
  }

  const canRestore = $derived(lastRun?.status === 'success' && !!lastRun.image_path);
  const backupBusy = $derived(
    jobs.queue.current !== null || jobs.queue.pending.length > 0,
  );

  const running = $derived(engine.op === 'restore');
  const progress = $derived(engine.restoreProgress);
  const summary = $derived(engine.restoreSummary);
  const errorMsg = $derived(formError ?? engine.restoreError);

  // Elapsed + ETA from recent throughput.
  let nowMs = $state(Date.now());
  let etaMs = $state(NaN);
  let startMs = $state(0);
  let samples: { t: number; blocks: number }[] = [];

  $effect(() => {
    const p = engine.restoreProgress;
    if (!running || !p) {
      samples = [];
      return;
    }
    if (samples.length === 0) startMs = Date.now();
    const last = samples[samples.length - 1];
    if (last && last.blocks === p.blocks_done) return;
    const t = Date.now();
    samples.push({ t, blocks: p.blocks_done });
    while (samples.length > 2 && samples[0].t < t - 12000) samples.shift();
  });

  $effect(() => {
    if (!running) {
      etaMs = NaN;
      return;
    }
    const tick = () => {
      nowMs = Date.now();
      const p = engine.restoreProgress;
      if (!p || samples.length < 2) {
        etaMs = NaN;
        return;
      }
      const first = samples[0];
      const last = samples[samples.length - 1];
      const dt = last.t - first.t;
      const db = last.blocks - first.blocks;
      if (dt < 1500 || db <= 0) {
        etaMs = NaN;
        return;
      }
      etaMs = ((p.blocks_total - p.blocks_done) * dt) / db;
    };
    tick();
    const id = setInterval(tick, 500);
    return () => clearInterval(id);
  });

  const elapsedMs = $derived(running && startMs ? nowMs - startMs : 0);

  // Design stage: "Восстановить" only shows the queue of operations that WOULD
  // run. Nothing is written until the restore logic is wired up.
  function askConfirm() {
    formError = null;
    if (!targetDiskId) {
      fitMsg = 'Шаг 1: выберите диск, на который восстановить.';
      return;
    }
    if (operations.length === 0) {
      fitMsg = wholeDiskRestore
        ? 'Шаг 2: выберите свободное место для разделов копии.'
        : 'Шаг 2: сопоставьте хотя бы один раздел.';
      return;
    }
    opsOpen = true;
  }

  async function doRestore() {
    if (confirmText !== 'ДА') return;
    confirmOpen = false;
    // NOTE: per-partition mapped restore is not yet wired to the engine (which
    // restores a whole volume/disk). Shown as a preview until the backend lands.
    stubMsg =
      'Восстановление по разделам пока в разработке: движок восстанавливает том/диск целиком. ' +
      'Сопоставление разделов показано как предпросмотр и ещё не выполняется.';
  }

  const targetLabel = $derived(targetDisk?.model ?? '');
</script>

{#snippet copyNav()}
  <div class="copynav">
    <button class="cn-btn" title="Более старая копия" aria-label="Более старая копия"
      disabled={!hasMultiple || copyIndex >= copies.length - 1} onclick={() => { copyIndex += 1; resetMapping(); }}>‹</button>
    <span class="cn-date">{activeInfo ? formatDate(activeInfo.created_at) : lastRun ? formatDate(lastRun.finished_at) : ''}</span>
    <button class="cn-btn" title="Более новая копия" aria-label="Более новая копия"
      disabled={!hasMultiple || copyIndex <= 0} onclick={() => { copyIndex -= 1; resetMapping(); }}>›</button>
  </div>
{/snippet}

<div class="page">
<div class="dash" bind:clientWidth={dashWidth}>
  {#if !running}
    <!-- WHAT: the disk this copy captured — same read-only map as Параметры → Источник -->
    <div class="sec-head">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
      <span class="sec-title">Что восстановить</span>
      <span class="sec-sub">разделы, вошедшие в эту копию</span>
    </div>

    {#if backedUpDisks.length > 0}
      <!-- Map is available (embedded in the file / job snapshot / live) — show it
           without waiting on the slow live-disk enumeration. -->
      <div class="disks">
        {#each backedUpDisks as d, i (d.device_id)}
          <DiskMap
            disk={d}
            center={i === 0 ? copyNav : undefined}
            selected={targetDiskId ? [] : job.sources}
            selectedIndices={targetDiskId ? [] : (activeInfo?.backed_up_parts ?? [])}
            readonly
            boot={activeInfo?.is_whole_disk ?? isBootableCopy(d)}
            bootChecked={wholeDiskRestore}
            onBootToggle={toggleWholeDisk}
            inBackup={backedUpParts.length > 0 ? backedUpParts : undefined}
            onPart={i === 0 && targetDiskId ? clickSourcePart : undefined}
            stateOf={i === 0 && targetDiskId ? srcState : undefined}
          />
        {/each}
      </div>
    {:else if engine.volumesLoading && engine.disks.length === 0}
      <div class="loading-note">Загрузка списка дисков…</div>
    {:else}
      <div class="warn-note">Диск этой копии сейчас не подключён — карту показать нельзя.</div>
    {/if}

    {#if catData}
      <div class="cat-block">
        <div class="cat-head">Что в копии — по типам файлов</div>
        <div class="catbar">
          {#each catData as c}<div style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>{/each}
        </div>
        <div class="cats">
          {#each catData as c}
            <div class="cat"><span class="dot" style="background:{c.color}"></span><span class="cat-name">{c.name}</span><span class="cat-size">{formatBytes(c.bytes)}</span></div>
          {/each}
          {#if excluded.files > 0}
            <div class="cat"><span class="dot" style="background:#c0463f"></span><span class="cat-name">Исключено</span><span class="cat-size">{formatBytes(excluded.bytes)}</span></div>
          {/if}
        </div>
      </div>
    {/if}

    <!-- arrow (at 25%) + step instruction (what to do now) -->
    <div class="connector">
      <div class="conn-arrow">
        <div class="v-dash"></div>
        <div class="arrow-circle"><svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M19 12l-7 7-7-7"/></svg></div>
        <div class="v-dash"></div>
      </div>
      <div class="conn-instr">{instr}</div>
    </div>

    <!-- WHERE: step 1 = pick a disk; step 2/3 = map partitions onto it -->
    <div class="sec-head">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
      <span class="sec-title">Куда восстановить</span>
      <span class="sec-sub">
        {#if !targetDiskId}кликните диск, на который восстановить{:else}кликните раздел — куда скопировать{/if}
      </span>
    </div>
    {#if systemDisks.length === 0}
      <div class="loading-note searching">
        <span class="spinner" aria-hidden="true"></span>
        Поиск дисков для восстановления…
      </div>
    {:else if !targetDiskId}
      <div class="disks">
        {#each systemDisks as d (d.device_id)}
          <DiskMap disk={d} onDisk={() => selectTargetDisk(d)} />
        {/each}
      </div>
    {:else if targetDisk}
      <div class="disks">
        <DiskMap
          disk={targetDisk}
          {slots}
          onPart={clickTargetPart}
          stateOf={dstState}
          onFree={wholeDiskRestore ? clickFreeSpace : undefined}
          needBytes={wholeDiskRestore ? srcTotalBytes : undefined}
          onPartMenu={openPartMenu}
          onFreeMenu={openFreeMenu}
        />
      </div>
      <div class="hint-small">Правый клик по разделу или свободному месту — создать / удалить раздел.</div>
      <!-- Undo everything and go back to step 1 (pick a disk). -->
      <div class="under-card">
        <button
          class="btn ghost small icon"
          title="Отменить последнее действие"
          aria-label="Отменить последнее действие"
          disabled={!canUndo}
          onclick={undo}
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14 4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-4"/></svg>
        </button>
        <button class="btn ghost small" onclick={pickAnotherDisk}>Отмена</button>
      </div>
      {#if fitMsg}<div class="fit-msg" class:bad={badTarget !== null}>{fitMsg}</div>{/if}
    {/if}
  {/if}

  {#if running}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>Восстановление…</span>
        {#if progress}
          <span>{progress.blocks_done} / {progress.blocks_total} блоков · {progress.speed_mbps.toFixed(1)} МБ/с</span>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="progress-meta">
        <span class="route">{job.name} → {targetLabel}</span>
        <span class="times">Прошло {formatClock(elapsedMs)} · Осталось {Number.isFinite(etaMs) ? '~' + formatClock(etaMs) : 'оценка…'}</span>
      </div>
    </div>
  {/if}

  {#if summary && !running}
    <div class="banner ok"><span class="bdot"></span><span>Восстановление завершено: {formatBytes(summary.bytes_restored)} за {summary.duration_secs.toFixed(1)} с.</span></div>
  {/if}
  {#if errorMsg}<div class="banner err"><span class="bdot red"></span><span>{errorMsg}</span></div>{/if}
</div>

  <div class="footer">
    <div class="hint">
      {#if backupBusy}
        Идёт резервное копирование — восстановление недоступно.
      {:else}
        Восстанавливается последняя копия. Отдельные файлы — через «Инструменты → Подключить как диск».
      {/if}
    </div>
    {#if running}
      <button class="btn danger" onclick={cancelCurrent}>Отмена</button>
    {:else}
      <button class="btn green" onclick={askConfirm} disabled={!canRestore || backupBusy}>Восстановить</button>
    {/if}
  </div>
</div>

{#if stubMsg}<InfoModal message={stubMsg} onClose={() => (stubMsg = null)} />{/if}

<!-- Right-click menu of the simple partition editor -->
{#if menu}
  <div class="menu-backdrop" role="presentation" oncontextmenu={(e) => { e.preventDefault(); menu = null; }} onclick={() => (menu = null)}></div>
  <div class="ctx-menu" style="left:{menu.x}px; top:{menu.y}px">
    <div class="ctx-title">Действия</div>
    {#if menu.part}
      <button onclick={() => { const p = menu!.part!; menu = null; delConfirm = p; }}>Удалить раздел</button>
    {:else}
      <button onclick={() => openCreate(menu!.offset ?? 0, menu!.size ?? 0)}>Создать раздел</button>
    {/if}
  </div>
{/if}

<!-- Delete confirmation -->
{#if delConfirm}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">Удалить раздел?</div>
      <p>
        Раздел <b>{delConfirm.volume_letter ? delConfirm.volume_letter + ': ' : ''}{delConfirm.volume_label || '(без метки)'}</b>
        — {formatBytes(delConfirm.size_bytes)}. Место станет не распределённым.
      </p>
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (delConfirm = null)}>Нет</button>
        <button class="btn danger-solid" onclick={doDelete}>Да, удалить</button>
      </div>
    </div>
  </div>
{/if}

<!-- Create-partition dialog -->
{#if createDlg}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">Создать раздел</div>
      <!-- Exact, not rounded: `formatBytes` shows 130.28 GiB as "130 GiB", which
           made the pre-filled size look like it covered everything. -->
      <p>Свободно: <b>{preciseSize(createDlg.max)}</b></p>
      <label class="fld">
        <span>Размер</span>
        <span class="fld-row">
          <input type="text" bind:value={createDlg.size} spellcheck="false" />
          <select class="unit" bind:value={createDlg.unit}>
            <option value="MiB">MiB</option>
            <option value="GiB">GiB</option>
            <option value="TiB">TiB</option>
          </select>
        </span>
      </label>
      <div class="row2">
        <label class="fld">
          <span>Файловая система</span>
          <select value={createDlg.fs} onchange={(e) => onFsChange((e.currentTarget as HTMLSelectElement).value)}>
            <option value="ntfs">NTFS</option>
            <option value="fat32">FAT32</option>
            <option value="ex_fat">exFAT</option>
            <option value="ext4">ext4</option>
            <option value="re_fs">ReFS</option>
          </select>
        </label>
        {#if isGpt}
          <label class="fld">
            <span>Тип раздела (GPT)</span>
            <select bind:value={createDlg.gptType}>
              {#each GPT_TYPES as t}<option value={t.guid}>{t.name}</option>{/each}
            </select>
          </label>
        {:else}
          <label class="fld">
            <span>Тип раздела (MBR)</span>
            <select bind:value={createDlg.mbrKind}>
              <option value="primary">Основной</option>
              <option value="logical">Логический</option>
            </select>
          </label>
        {/if}
      </div>
      {#if !isGpt}
        <label class="chk">
          <input type="checkbox" bind:checked={createDlg.active} />
          Активный (загрузка BIOS с этого раздела)
        </label>
      {/if}
      <div class="row2">
        <label class="fld">
          <span>Метка тома</span>
          <input type="text" bind:value={createDlg.label} placeholder="например, Data" spellcheck="false" />
        </label>
        <label class="fld">
          <span>Буква диска</span>
          <select bind:value={createDlg.letter}>
            <option value="">— не назначать —</option>
            {#each freeLetters as l}<option value={l}>{l}:</option>{/each}
          </select>
        </label>
      </div>
      <label class="chk">
        <input type="checkbox" bind:checked={createDlg.quickFormat} />
        Быстрое форматирование
      </label>
      <p class="fld-note">Начало раздела выравнивается на 1 МиБ — иначе каждая запись задевала бы два физических сектора и работала бы вдвое медленнее.</p>
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (createDlg = null)}>Отмена</button>
        <button class="btn primary-solid" onclick={doCreate}>Создать</button>
      </div>
    </div>
  </div>
{/if}

<!-- What "Восстановить" would do (design preview — nothing is executed yet) -->
{#if opsOpen}
  <div class="overlay">
    <div class="dialog wide">
      <div class="dialog-title">Будут выполнены следующие операции</div>
      <ol class="ops">
        {#each operations as op}<li>{op}</li>{/each}
      </ol>
      <p class="ops-note">Пока это только предпросмотр — восстановление ещё не запускается.</p>
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (opsOpen = false)}>Закрыть</button>
      </div>
    </div>
  </div>
{/if}

{#if confirmOpen}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">Подтверждение восстановления</div>
      <p>На диске <b>{targetDisk?.model ?? ''}</b> будут перезаписаны сопоставленные разделы копией <b>{job.name}</b> от {activeInfo ? formatDate(activeInfo.created_at) : lastRun ? formatDate(lastRun.finished_at) : ''} ({mappings.length} разд.).</p>
      <p>Введите <b>ДА</b> (заглавными), чтобы продолжить:</p>
      <input type="text" bind:value={confirmText} spellcheck="false" />
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (confirmOpen = false)}>Отмена</button>
        <button class="btn danger-solid" disabled={confirmText !== 'ДА'} onclick={doRestore}>Восстановить</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .page { height: 100%; display: flex; flex-direction: column; }
  .dash { flex: 1; overflow: auto; width: 100%; padding: 34px 44px; max-width: 900px; margin: 0 auto; display: flex; flex-direction: column; }
  .sec-head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .sec-title { font-size: 15px; font-weight: 700; color: var(--head); }
  .sec-sub { font-size: 13px; color: var(--text-muted); }
  .disks { display: flex; flex-direction: column; gap: 14px; }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .loading-note.searching { display: flex; align-items: center; gap: 10px; }
  .spinner { width: 16px; height: 16px; flex-shrink: 0; border: 2px solid var(--border); border-top-color: var(--primary); border-radius: 50%; animation: spin 0.7s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .warn-note { font-size: 13px; color: #8c1d1d; background: rgba(216,74,74,.08); padding: 10px 14px; border-radius: 5px; }
  .cat-block { margin-top: 14px; padding: 14px 16px; border: 1px solid var(--border); border-radius: 6px; background: var(--card-bg); }
  .cat-head { font-size: 13px; font-weight: 600; color: var(--head); margin-bottom: 4px; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; margin-top: 10px; }
  .cats { display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap; }
  .cat { flex: 1; min-width: 130px; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--head); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  /* Copy navigator in the disk-card header: ‹ date › to page between the copies
     of one archive. Buttons disable at the ends / when there's only one copy. */
  .copynav { display: flex; align-items: center; gap: 6px; }
  .cn-btn { width: 22px; height: 20px; display: flex; align-items: center; justify-content: center; border: 1px solid var(--border); border-radius: 4px; background: var(--card-bg); color: var(--head); font-size: 18px; font-weight: 800; line-height: 1; cursor: pointer; padding: 0 0 3px; font-family: var(--font); }
  .cn-btn:hover:not(:disabled) { border-color: var(--primary); color: var(--primary); }
  .cn-btn:disabled { opacity: .4; cursor: default; }
  .cn-date { font-size: 12px; font-weight: 600; color: var(--head); white-space: nowrap; }
  /* Arrow occupies the left half so it sits at ~25%; the instruction fills the
     right half. */
  /* The arrow's centre sits at 25% of the width (half its own 44px width is
     subtracted); the instruction starts right after it, not at the midpoint. */
  .connector { display: flex; align-items: center; margin: 20px 0; gap: 14px; }
  .conn-arrow { flex: 0 0 auto; margin-left: calc(25% - 22px); display: flex; flex-direction: column; align-items: center; }
  .conn-instr { flex: 1; font-size: 15px; font-weight: 600; color: var(--head); line-height: 1.45; }
  .connector .v-dash { width: 2px; height: 14px; background: repeating-linear-gradient(180deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 44px; height: 44px; border-radius: 50%; background: var(--green); display: flex; align-items: center; justify-content: center; box-shadow: 0 4px 14px rgba(90,176,47,.35); margin: 4px 0; }
  .under-card { display: flex; justify-content: flex-end; gap: 10px; margin-top: 10px; }
  .btn.small { padding: 7px 18px; font-size: 13px; }
  .btn.icon { display: inline-flex; align-items: center; justify-content: center; padding: 7px 12px; }
  .hint-small { margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  /* Right-click partition menu */
  .menu-backdrop { position: fixed; inset: 0; z-index: 190; }
  .ctx-menu { position: fixed; z-index: 200; min-width: 180px; background: var(--card-bg); border: 1px solid var(--border); border-radius: 6px; box-shadow: 0 10px 30px rgba(0,0,0,.18); padding: 6px; }
  .ctx-title { font-size: 11px; font-weight: 700; text-transform: uppercase; color: var(--text-muted); padding: 4px 10px 6px; }
  .ctx-menu button { display: block; width: 100%; text-align: left; padding: 8px 10px; border: none; background: none; border-radius: 4px; font-size: 13px; font-family: var(--font); color: var(--head); cursor: pointer; }
  .ctx-menu button:hover { background: var(--accent-soft); color: var(--primary); }
  /* Create-partition form */
  .fld { display: block; margin-bottom: 12px; font-size: 13px; color: var(--head); }
  .fld > span:first-child { display: block; margin-bottom: 5px; font-weight: 600; }
  .fld input, .fld select { padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: var(--card-bg); color: var(--text); }
  .fld > input { width: 100%; }
  /* Size row: the number takes the whole width, the unit is a narrow box on the
     right. Both are forced to the same box height so they line up exactly. */
  .fld-row { display: flex; gap: 8px; align-items: stretch; }
  .fld-row input { flex: 1 1 auto; min-width: 0; }
  .fld-row .unit { flex: 0 0 92px; }
  .fld-row input, .fld-row .unit { height: 36px; box-sizing: border-box; }
  .fld > select { width: 100%; height: 36px; box-sizing: border-box; }
  .fld > input { height: 36px; box-sizing: border-box; }
  /* Two fields side by side, half the width each. */
  .row2 { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin-bottom: 12px; }
  .row2 .fld { margin-bottom: 0; min-width: 0; }
  .chk { display: flex; align-items: center; gap: 8px; margin-bottom: 12px; font-size: 13px; color: var(--head); cursor: pointer; white-space: nowrap; }
  .chk input { margin: 0; flex-shrink: 0; cursor: pointer; }
  .fld-note { font-size: 12px; color: var(--text-muted); line-height: 1.45; margin-bottom: 4px; }
  .dialog.wide { width: 560px; }
  .ops { margin: 0 0 12px 18px; padding: 0; font-size: 13px; color: var(--head); line-height: 1.7; }
  .ops-note { font-size: 12px; color: var(--text-muted); }
  .btn.primary-solid { background: var(--primary); color: #fff; }
  .fit-msg { margin-top: 10px; padding: 9px 14px; font-size: 13px; border-radius: 5px; background: var(--accent-soft); color: var(--head); }
  .fit-msg.bad { background: rgba(216,74,74,.1); color: #8c1d1d; font-weight: 600; }
  .progress-wrap { margin-top: 30px; }
  .progress-info { display: flex; justify-content: space-between; font-size: 13px; color: var(--head); margin-bottom: 8px; }
  .bar { height: 8px; border-radius: 4px; background: var(--tabbg); overflow: hidden; }
  .bar-fill { height: 100%; background: var(--primary); transition: width .15s linear; }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 60%; }
  .banner { margin-top: 22px; border-radius: 6px; padding: 13px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); font-size: 13px; color: var(--head); }
  .banner.err { background: rgba(216,74,74,.08); }
  .bdot { width: 18px; height: 18px; border-radius: 50%; background: var(--green); flex-shrink: 0; }
  .bdot.red { background: var(--error); }
  /* .footer is global (styles/global.css) — full-width bar, same on every page. */
  .hint { flex: 1; font-size: 12px; color: var(--text-muted); }
  .btn { border-radius: 4px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn:disabled { opacity: .5; cursor: default; }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover:not(:disabled) { background: var(--green-hover); }
  .btn.danger { background: transparent; border: 1px solid var(--error); color: var(--error); }
  .btn.ghost { background: none; border: 1px solid var(--border); color: var(--head); }
  .btn.danger-solid { background: var(--error); color: #fff; }
  .overlay { position: fixed; inset: 0; background: rgba(20,17,44,.5); display: flex; align-items: center; justify-content: center; z-index: 150; }
  .dialog { width: 400px; background: #fff; border-radius: 8px; padding: 24px; box-shadow: 0 20px 60px rgba(0,0,0,.3); }
  .dialog-title { font-size: 17px; font-weight: 600; color: var(--head); margin-bottom: 12px; }
  .dialog p { font-size: 13px; color: var(--text); margin-bottom: 10px; line-height: 1.5; }
  /* Only the confirm dialog's own text box — a bare `.dialog input` would also
     stretch checkboxes to full width and push their labels out of the dialog. */
  .dialog > input { width: 100%; padding: 9px 12px; border: 1px solid var(--border); border-radius: 4px; font-size: 14px; margin-bottom: 16px; }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 10px; }
</style>
