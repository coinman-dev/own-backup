<script lang="ts">
  import { pickPath } from '../lib/pickPath.svelte';
  import { api } from '../lib/api';
  import { engine } from '../lib/engineStore.svelte';
  import { media, ensureMediaMode } from '../lib/mediaStore.svelte';
  import { saveJob, runJob, decryptVolume, decryptedVolumes } from '../lib/jobStore.svelte';
  import { formatBytes } from '../lib/format';
  import { diskSegments } from '../lib/diskLayout';
  import InfoModal from '../components/InfoModal.svelte';
  import PasswordField from '../components/PasswordField.svelte';
  import type { Job, DiskInfo, PartitionInfo } from '../lib/types';

  interface Props {
    mode: 'wizard' | 'edit';
    // Shared draft owned by the parent (BackupSection) so the name is also
    // editable inline in the jobs list. Cancel discards it in the parent.
    job: Job;
    onCancel: () => void;
    onDone: (job: Job) => void;
  }
  let { mode, job, onCancel, onDone }: Props = $props();

  let step = $state(0);
  let error: string | null = $state(null);
  let stubMsg: string | null = $state(null);
  const stub = () =>
    (stubMsg = 'Эта настройка сохраняется, но пока не применяется — реализация позже.');

  // On the boot medium the schedule step is dropped rather than disabled: the
  // session ends at power-off, so any schedule set here is a promise nothing
  // can keep. The job is saved with kind='none' (see buildJob), which is what
  // "run it now, once" means everywhere else in the program.
  const steps = $derived(
    [
      { key: 'source', label: 'Источник' },
      { key: 'schedule', label: 'Расписание' },
      { key: 'scheme', label: 'Схема' },
      { key: 'exclusions', label: 'Исключения' },
      { key: 'advanced', label: 'Дополнительно' },
    ].filter((s) => !(media.active && s.key === 'schedule')),
  );
  // The tab list is filtered (no schedule on the boot medium), so the body must
  // be keyed by the step's KEY, never by its index: on the medium index 2 said
  // «Схема» in the tab bar and drew the exclusions, and «Дополнительно» — the
  // last index — could not be reached at all.
  const stepKey = $derived(steps[step]?.key ?? 'source');
  // A step can also vanish under us (the medium check answers after the first
  // render), so never leave `step` pointing past the end of the list.
  $effect(() => {
    if (step > steps.length - 1) step = steps.length - 1;
  });

  const weekdays = [
    ['mon', 'Пн'], ['tue', 'Вт'], ['wed', 'Ср'], ['thu', 'Чт'],
    ['fri', 'Пт'], ['sat', 'Сб'], ['sun', 'Вс'],
  ];
  const scheduleKinds = [
    ['daily', 'Ежедневно'], ['weekly', 'Еженедельно'], ['monthly', 'Ежемесячно'],
    ['event', 'По событию'], ['none', 'Не планировать'],
  ];

  // Event triggers for kind='event'. Third field = badge text ('' = works).
  const eventTriggers: [string, string, string][] = [
    ['usb_connect', 'После подключения внешнего устройства', 'в разработке'],
    ['logon', 'При входе пользователя', ''],
    ['logoff', 'При выходе пользователя', ''],
    ['startup', 'При запуске системы (с задержкой)', ''],
  ];

  function toggleDay(d: string) {
    job.schedule.days = job.schedule.days.includes(d)
      ? job.schedule.days.filter((x) => x !== d)
      : [...job.schedule.days, d];
  }

  // The wizard can be opened before App's health check has answered, and it is
  // the schedule step that depends on the answer — ask, then force the draft to
  // "run manually" so a job carried over from an edit cannot smuggle a schedule
  // onto a medium that has no scheduler running.
  $effect(() => {
    ensureMediaMode();
    if (media.active && job.schedule.kind !== 'none') job.schedule.kind = 'none';
  });

  // ---- source selection (Paragon-style disk map, multi-volume) ----------

  const FS_LABELS: Record<string, string> = {
    ntfs: 'NTFS', fat32: 'FAT32', ex_fat: 'exFAT', ext4: 'ext4', xfs: 'XFS',
    btrfs: 'Btrfs', re_fs: 'ReFS', unknown: '',
  };
  // Unknown / unformatted → empty string (the UI shows nothing, not "—").
  function fsLabel(fs: string): string {
    return FS_LABELS[fs] ?? (fs === 'unknown' ? '' : fs);
  }
  function tableLabel(t: string): string {
    return t === 'gpt' ? 'GPT' : t === 'mbr' ? 'MBR' : t.toUpperCase();
  }

  // Openable device for a partition (letter "C:" or a `\\?\Volume{GUID}` path
  // for letterless system/hidden volumes). Empty ⇒ not backable ⇒ not clickable.
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  /** Partitions the system needs but never mounts: no filesystem, no letter, no
   *  label. Their GPT type GUID is what they are, so say it instead of drawing
   *  a nameless "Раздел 128 MiB" that reads like a mistake. */
  const GPT_TYPE_NAMES: Record<string, string> = {
    'C12A7328-F81F-11D2-BA4B-00A0C93EC93B': 'EFI (ESP)',
    'E3C9E316-0B5C-4DB8-817D-F92DF00215AE': 'Зарезервировано системой (MSR)',
    'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC': 'Среда восстановления Windows',
    '0FC63DAF-8483-4772-8E79-3D69D8477DE4': 'Linux',
    '0657FD6D-A4AB-43C4-84E5-0933C84B4F4F': 'Linux swap',
    'E6D6D379-F507-44C2-A23C-238F2A3DF928': 'Linux LVM',
    'CA7D7CCB-63ED-4C53-861C-1742536059CC': 'LUKS',
    '21686148-6449-6E6F-744E-656564454649': 'BIOS boot',
  };
  function gptTypeName(p: PartitionInfo): string {
    const guid = (p.gpt_type_guid || '').replace(/[{}]/g, '').toUpperCase();
    return GPT_TYPE_NAMES[guid] ?? '';
  }

  function partLabel(p: PartitionInfo): string {
    // A decrypted encrypted volume shows its real inner label.
    const dec = p.volume_device ? decryptedVolumes[p.volume_device] : undefined;
    if (dec?.label) return dec.label;
    if (p.volume_letter) {
      const name = p.volume_label || fsLabel(p.filesystem) || 'Том';
      return `${p.volume_letter}: ${name}`;
    }
    // Letterless (EFI / ext4 / hidden): its on-disk label, else the filesystem,
    // else what its GPT type says it is.
    return p.volume_label || fsLabel(p.filesystem) || gptTypeName(p) || 'Раздел';
  }
  // Used bytes / filesystem for a partition, preferring decrypted info when the
  // volume has been unlocked.
  function partUsed(p: PartitionInfo): number {
    const dec = p.volume_device ? decryptedVolumes[p.volume_device] : undefined;
    return dec ? dec.used_bytes : p.used_bytes;
  }
  function partFs(p: PartitionInfo): string {
    const dec = p.volume_device ? decryptedVolumes[p.volume_device] : undefined;
    return fsLabel(dec ? dec.filesystem : p.filesystem);
  }
  // Per-source token used in the file-name preview (mirrors core's source_token:
  // letter for lettered volumes, V+8 hex of the GUID for letterless volumes,
  // P<disk><offsetHex> for physical-offset partitions).
  function sourceToken(device: string): string {
    const pm = device.match(/^part:(\d+):(\d+):/);
    if (pm) return `P${pm[1]}${BigInt(pm[2]).toString(16).toUpperCase()}`;
    const m = device.match(/Volume\{([0-9a-fA-F]+)/);
    if (m) return 'V' + m[1].slice(0, 8).toUpperCase();
    return driveLetter(device) ?? 'X';
  }

  function isSelected(dev: string): boolean {
    return job.sources.includes(dev);
  }
  function toggleSource(dev: string, label: string) {
    if (sourceReadonly) return; // fixed source on the "Параметры" screen
    if (job.sources.includes(dev)) {
      // A boot-essential volume (ESP / OS) cannot be dropped while its disk is
      // in bootable-copy mode — the restore would not boot without it.
      if (isLockedSource(dev)) return;
      const i = job.sources.indexOf(dev);
      job.sources = job.sources.filter((x) => x !== dev);
      job.source_labels = job.source_labels.filter((_, idx) => idx !== i);
      // Keep the decrypted state (session-wide) so re-selecting it stays unlocked.
    } else {
      job.sources = [...job.sources, dev];
      job.source_labels = [...job.source_labels, label];
    }
    // Keep the primary source in sync (used for file naming / back-compat).
    job.source = job.sources[0] ?? '';
    job.source_label = job.source_labels[0] ?? '';
    // Decoupled from bootable-copy mode: picking a data volume no longer turns
    // the boot layout off — it just updates which partitions the captured GPT
    // keeps (behaviour B: a deselected volume drops out of the layout).
    const d = diskOfDevice(dev);
    if (d) syncDiskSourceParts(d);
  }

  // Proportional segments of a disk: partitions plus hatched gaps for
  // unallocated space, ordered by on-disk offset. Shared with the restore
  // screens (`lib/diskLayout`) — the same disk has to look the same wherever it
  // is drawn. This screen used to hide any gap under 1% of the disk, which on a
  // 894 GiB drive meant 8.9 GiB of free space simply was not there.

  // Fraction (0..100) of a partition that is in use, for the fill overlay.
  function usedPct(p: PartitionInfo): number {
    const used = partUsed(p);
    if (used <= 0 || p.size_bytes <= 0) return 0;
    return Math.min(100, (used / p.size_bytes) * 100);
  }
  // Full text for the segment tooltip (shown when the label/size is truncated).
  function segTitle(p: PartitionInfo): string {
    let t = partLabel(p) + ' — ' + formatBytes(p.size_bytes);
    if (partUsed(p) > 0) t += ` (${formatBytes(partUsed(p))} занято)`;
    const fs = partFs(p);
    if (fs) t += ` ${fs}`;
    return t;
  }

  // Physical drive number from the device id (`\\.\PhysicalDrive3` → 3).
  function diskNumber(d: DiskInfo): number {
    const m = d.device_id.match(/(\d+)\s*$/);
    return m ? parseInt(m[1], 10) : 0;
  }
  // A disk can host a bootable OS only if it has an EFI System Partition; the
  // "bootable copy" option is disabled (read-only) for pure data disks.
  function isBootableDisk(d: DiskInfo): boolean {
    return d.partitions.some((p) => p.is_boot);
  }
  // The ESP and the OS/system partition are required for a bootable restore, so
  // while bootable-copy is on they are auto-included and cannot be deselected.
  /** Partitions a bootable disk copy cannot leave out: the loader's partition
   *  and the one holding the system itself. `is_system` alone was not enough —
   *  booted from the medium nothing is "the running system", so the copy came
   *  out with the EFI partition and without Windows. */
  function isBootEssential(p: PartitionInfo): boolean {
    return p.is_boot || p.is_system || p.has_os;
  }
  /** Microsoft Reserved: Windows expects the slot, there is no filesystem in it.
   *  A whole-disk copy keeps its GPT entry (`DiskLayout::retain_for_backup`)
   *  whatever the user selected — so the map must show it as part of the copy,
   *  not as something left out. */
  const MSR_GUID = 'E3C9E316-0B5C-4DB8-817D-F92DF00215AE';
  /** Is this disk's layout captured — i.e. does the job write ONE image of it?
   *  True for every disk a new job picks; false for jobs saved before that
   *  became unconditional, which still hold one image per volume. */
  function isDiskCaptured(d: DiskInfo): boolean {
    return job.disk_source != null && job.disk_source.disk_number === diskNumber(d);
  }
  function layoutOnlyParts(d: DiskInfo): number[] {
    // Keyed on the captured layout, not on bootability: `retain_for_backup`
    // keeps the MSR slot in ANY whole-disk copy, so a non-bootable one must
    // show it as included too.
    if (!isDiskCaptured(d)) return [];
    return d.partitions
      .filter((p) => (p.gpt_type_guid || '').replace(/[{}]/g, '').toUpperCase() === MSR_GUID)
      .map((p) => p.index);
  }
  // The disk a selected source device lives on (matched by volume device).
  function diskOfDevice(dev: string): DiskInfo | null {
    return engine.disks.find((d) => d.partitions.some((p) => partDevice(p) === dev)) ?? null;
  }
  // "Bootable copy of this disk" is on when every partition a restore needs to
  // boot is in the copy.
  //
  // It used to mean "disk_source is captured", i.e. "this job makes ONE image
  // of the whole disk" — two different ideas behind one checkbox. With one
  // image per disk now unconditional (2026-08-21), `disk_source` says nothing
  // about bootability, so the question is asked of the selection itself.
  // Derived, not stored: a flag could disagree with the partitions actually
  // selected, and then the copy would promise a boot it cannot deliver.
  function isBootableCopy(d: DiskInfo): boolean {
    if (!isBootableDisk(d)) return false;
    const essential = d.partitions.filter(isBootEssential);
    if (essential.length === 0) return false;
    return essential.every((p) => {
      const dev = partDevice(p);
      return !!dev && job.sources.includes(dev);
    });
  }
  // A selected volume that is boot-essential AND on a disk in bootable-copy
  // mode is locked: its checkbox stays checked and can't be turned off.
  function isLockedSource(dev: string): boolean {
    const d = diskOfDevice(dev);
    if (!d || !isBootableCopy(d)) return false;
    const p = d.partitions.find((pp) => partDevice(pp) === dev);
    return !!p && isBootEssential(p);
  }

  // "Параметры" screen (edit mode): the source is fixed once a job is saved —
  // changing which disks/volumes it captures would break the incremental chain.
  // So show ONLY the disks this job actually backs up, and render the whole map
  // read-only (for a different source the user creates a new job).
  const sourceReadonly = $derived(mode === 'edit');
  function diskHasSelection(d: DiskInfo): boolean {
    if (isBootableCopy(d)) return true;
    return d.partitions.some((p) => {
      const dev = partDevice(p);
      return !!dev && job.sources.includes(dev);
    });
  }
  const visibleDisks = $derived(
    sourceReadonly ? engine.disks.filter(diskHasSelection) : engine.disks,
  );

  // ---- one job = one disk = one image file (2026-08-19, enforced 2026-08-21)
  //
  // The source step asks its questions one at a time, the way "Восстановление"
  // does: FIRST which disk this copy is about, THEN which of its volumes go in.
  // Everything that belongs to the second question — the volume segments, the
  // "Загрузочная копия диска" switch, the password panels, the arrow down and
  // the destination folder — stays off the screen until a disk is picked, so
  // the first screen has exactly one thing to click and no way to build a job
  // that spans two disks.
  //
  // The "one image file" half of that rule was only half true until 2026-08-21:
  // it held for a bootable copy and not otherwise, so two selected partitions
  // quietly produced two files with two independent chains. `pickDisk` now
  // captures `disk_source` for every disk, and the bootable switch is back to
  // meaning only what it says.
  let pickedDiskId = $state<string | null>(null);
  // A saved job already named its disk, and its source cannot be changed.
  const diskChosen = $derived(sourceReadonly || pickedDiskId !== null);
  const activeDisk = $derived(engine.disks.find((d) => d.device_id === pickedDiskId) ?? null);
  // Stage 1 lists every disk to choose from; stage 2 draws only the chosen one
  // — a copy that holds one disk must not show the others as if they could
  // still join it. A read-only job keeps showing whatever it captured,
  // including the multi-disk jobs saved before this rule.
  const shownDisks = $derived(
    sourceReadonly ? visibleDisks : activeDisk ? [activeDisk] : visibleDisks,
  );

  /** Forget the volume selection: a different disk is a different copy, and a
   *  leftover source device from the previous one would put the job back across
   *  two disks — the thing this flow exists to prevent. */
  function clearSourceSelection() {
    job.sources = [];
    job.source_labels = [];
    job.source = '';
    job.source_label = '';
    job.disk_source = null;
    autoIncluded.clear();
  }
  /** One job = one disk = ONE image file (2026-08-21).
   *
   *  Capturing `disk_source` is what makes the engine write a single image
   *  holding the real GPT with each selected partition at its original offset,
   *  instead of a separate image per volume. It is captured for every disk, not
   *  only bootable ones: two selected partitions used to silently produce two
   *  files with two independent chains, which is not what "a copy of this disk"
   *  means to anyone reading the folder. A restore can still take a single
   *  partition out of such an image (`RestoreOp::RestorePartition`).
   */
  function captureDiskSource(d: DiskInfo) {
    job.disk_source = {
      disk_number: diskNumber(d),
      disk_model: d.model || d.device_id,
      parts: [],
    };
    syncDiskSourceParts(d);
  }

  function pickDisk(d: DiskInfo) {
    if (sourceReadonly || pickedDiskId === d.device_id) return;
    pickedDiskId = d.device_id;
    clearSourceSelection();
    captureDiskSource(d);
  }
  /** Back to stage 1. Without it the first click would be final — and the disk
   *  is the one choice on this screen that cannot be corrected further down. */
  function pickAnotherDisk() {
    pickedDiskId = null;
    clearSourceSelection();
  }

  // Kept-files breakdown of the last run (same data the dashboard shows), drawn
  // under the disk map on the read-only "Параметры" screen so the user sees what
  // kind of files the copy holds without leaving the settings.
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
    const c = job.last_run?.categories;
    if (!c) return null;
    const total = CAT_META.reduce((s, m) => s + (c[m.key] ?? 0), 0);
    if (total <= 0) return null;
    return CAT_META.map((m) => ({
      ...m,
      bytes: c[m.key] ?? 0,
      pct: ((c[m.key] ?? 0) / total) * 100,
    }));
  });

  // The bar covers the whole scanned volume: what the copy holds, plus what the
  // exclusions left out (last segment). Shares are recomputed against that grand
  // total so the bar still adds up to 100%.
  const barData = $derived.by(() => {
    if (!catData) return null;
    const kept = catData.reduce((s, c) => s + c.bytes, 0);
    const ex = job.last_run?.excluded_bytes ?? 0;
    const total = kept + ex;
    if (total <= 0) return null;
    const out: { name: string; color: string; bytes: number; pct: number }[] = catData.map((c) => ({
      name: c.name,
      color: c.color,
      bytes: c.bytes,
      pct: (c.bytes / total) * 100,
    }));
    if (ex > 0) out.push({ name: 'Исключено', color: '#c0463f', bytes: ex, pct: (ex / total) * 100 });
    return out;
  });

  // Keep disk_source.parts in sync with the currently-selected backable
  // partitions of a disk (behaviour B: only kept volumes stay in the layout).
  function syncDiskSourceParts(d: DiskInfo) {
    if (!job.disk_source || job.disk_source.disk_number !== diskNumber(d)) return;
    const parts = d.partitions
      .filter((p) => { const dev = partDevice(p); return !!dev && job.sources.includes(dev); })
      .map((p) => ({ part_index: p.index, device: partDevice(p)!, filesystem: p.filesystem }));
    job.disk_source = { ...job.disk_source, parts };
  }

  // ---- Scheme mode: chains that roll over, or one chain that consolidates --
  //
  // The mode is DERIVED, not stored: `rolling_incrementals` being set is what
  // makes the engine consolidate, so a separate "mode" field could only ever
  // disagree with it. Switching mode rewrites the fields the other mode owns,
  // so a job never carries settings that its mode ignores but the engine still
  // reads — the way `only_incremental` used to quietly outlive the radio that
  // set it.
  //
  // `cleanup_older_than_days` is deliberately shared: it means "delete chains
  // older than X" in the standard mode and "start the chain over after X" in
  // consolidation, and both are the same user intent — do not let this archive
  // get older than X.
  const isConsolidation = $derived(
    job.scheme.method === 'incremental' && job.scheme.rolling_incrementals !== null,
  );

  function setSchemeMode(mode: 'standard' | 'consolidation') {
    if (mode === 'consolidation') {
      job.scheme.rolling_incrementals = job.scheme.rolling_incrementals ?? 3;
      // One chain: it never rolls over on a version count, and there is no
      // second chain for "keep N chains" to count.
      job.scheme.only_incremental = true;
      job.scheme.keep_chains = null;
      job.scheme.keep_first = false;
    } else {
      job.scheme.rolling_incrementals = null;
      job.scheme.only_incremental = false;
      job.scheme.full_every_n = job.scheme.full_every_n || 3;
      job.scheme.keep_chains = job.scheme.keep_chains ?? 3;
    }
    // The age box carries a DIFFERENT action in each mode — "начать цепочку
    // заново" against "удалить цепочки" — so it is not carried across. A
    // checkbox that stays ticked while its meaning changes underneath is worse
    // than one the user has to tick again.
    job.scheme.cleanup_older_than_days = null;
  }

  // A job saved before the modes existed can hold "only incremental versions",
  // which is one endless chain with nothing bounding it. That is the shape the
  // consolidation mode replaced, so the standard mode must not keep offering
  // it: normalise on the way in.
  /** Leaving the incremental method drops out of the consolidation mode, and
   *  that mode owns three fields the standard one needs back.
   *
   *  Without this, picking "Дифференциальное" while consolidation was on left
   *  `keep_chains` null and `cleanup_older_than_days` at 90 — so the standard
   *  block came up with "хранить не более N цепочек" unticked and "удалять
   *  цепочки старше 90 дней" ticked, the opposite of the defaults, and the
   *  ticked one now meant DELETE where a moment ago it meant START OVER. */
  function onMethodChange() {
    if (job.scheme.method !== 'incremental' && job.scheme.rolling_incrementals !== null) {
      setSchemeMode('standard');
    }
  }

  // A job saved before the modes existed can hold "only incremental versions",
  // which is one endless chain with nothing bounding it — the shape the
  // consolidation mode replaced. Normalise it on the way in, whatever the
  // method: a differential job left with the flag would never roll over
  // either, and the engine reads it regardless of method.
  $effect(() => {
    if (!isConsolidation && job.scheme.only_incremental) {
      job.scheme.only_incremental = false;
      job.scheme.full_every_n = job.scheme.full_every_n || 3;
    }
  });

  // Toggle the disk's "bootable copy": capture the GPT layout + boot partitions
  // so the disk can be restored bootable to a new SSD. Independent of which
  // data volumes are selected; disabled for non-bootable disks.
  function toggleBootableCopy(d: DiskInfo) {
    if (sourceReadonly) return; // fixed source on the "Параметры" screen
    if (!isBootableDisk(d)) return; // read-only for pure data disks
    if (isBootableCopy(d)) {
      // Turn off: drop the partitions a boot needs, keep the rest. The copy
      // stays one image of this disk — it just stops promising a boot.
      for (const p of d.partitions) {
        const dev = partDevice(p);
        if (!dev || !isBootEssential(p)) continue;
        const i = job.sources.indexOf(dev);
        if (i >= 0) {
          job.sources = job.sources.filter((x) => x !== dev);
          job.source_labels = job.source_labels.filter((_, idx) => idx !== i);
        }
      }
      job.source = job.sources[0] ?? '';
      job.source_label = job.source_labels[0] ?? '';
      syncDiskSourceParts(d);
      return;
    }
    // Turn on: include EVERY volume of the disk that can be copied.
    //
    // Not "the boot-essential ones", because we cannot tell which those are.
    // `has_os` is set by looking for `Windows\System32\ntoskrnl.exe`
    // (`win_disks.rs`), so it answers "is Windows installed here" and nothing
    // else — a Linux root, encrypted or not, is never recognised from Windows.
    // On a Kubuntu disk that left the bootable copy holding the EFI partition
    // and not the system (2026-08-21), which is not a bootable copy at all.
    //
    // Guessing harder (GPT type GUIDs, "encrypted so probably the root") only
    // moves the guess around: Ubuntu writes the generic "Linux filesystem"
    // type for data partitions too. So take everything and let the user drop
    // what they know is data — the tooltip has always promised exactly that,
    // and the cost of one extra partition is space, while the cost of one
    // missing partition is a copy that will not boot.
    for (const p of d.partitions) {
      const dev = partDevice(p);
      if (dev && !job.sources.includes(dev)) {
        job.sources = [...job.sources, dev];
        job.source_labels = [...job.source_labels, partLabel(p)];
      }
    }
    job.source = job.sources[0] ?? '';
    job.source_label = job.source_labels[0] ?? '';
    captureDiskSource(d);
    syncDiskSourceParts(d);
  }

  async function browseDest() {
    try {
      const picked = await pickPath({ directory: true, title: 'Папка для образов' });
      if (typeof picked === 'string') job.dest_folder = picked;
    } catch (e) {
      error = String(e);
    }
  }

  // Exclusions — two OS-specific lists behind a Windows / Linux tab. The engine
  // applies Windows masks to NTFS/ReFS/FAT partitions and Linux masks to
  // ext/btrfs/xfs ones (Linux-FS enforcement is a follow-up).
  let exclTab = $state<'windows' | 'linux'>('windows');
  let newPattern = $state('');
  // Old jobs deserialize without patterns_linux — keep it an array defensively.
  function exclList(): string[] {
    if (exclTab === 'linux') return (job.exclusions.patterns_linux ??= []);
    return job.exclusions.patterns;
  }
  function setExclList(v: string[]) {
    if (exclTab === 'linux') job.exclusions.patterns_linux = v;
    else job.exclusions.patterns = v;
  }
  function addPattern() {
    const p = newPattern.trim();
    if (p) {
      setExclList([...exclList(), p]);
      newPattern = '';
    }
  }
  function removePattern(i: number) {
    setExclList(exclList().filter((_, idx) => idx !== i));
  }
  async function resetExclusions() {
    const d = await api.defaultExclusionPatterns();
    setExclList(exclTab === 'linux' ? d.linux : d.windows);
  }

  // Uppercase drive letter of a path/device, or null (UNC/no letter).
  function driveLetter(s: string): string | null {
    const t = s.trim().replace(/^\\\\[.?]\\/, '');
    return t.length >= 2 && /[a-zA-Z]/.test(t[0]) && t[1] === ':'
      ? t[0].toUpperCase()
      : null;
  }

  // The drive letter of any selected source that collides with the
  // destination folder, or null when the destination is safe for all sources.
  const collisionLetter = $derived.by(() => {
    const dst = driveLetter(job.dest_folder);
    if (!dst) return null;
    for (const s of job.sources) {
      if (driveLetter(s) === dst) return dst;
    }
    return null;
  });

  // Representative timestamp for the file-name preview (fixed at open — the
  // real one is stamped when the backup actually runs).
  const pad2 = (n: number) => String(n).padStart(2, '0');
  const previewStamp = (() => {
    const d = new Date();
    return `${d.getFullYear()}${pad2(d.getMonth() + 1)}${pad2(d.getDate())}-${pad2(d.getHours())}${pad2(d.getMinutes())}${pad2(d.getSeconds())}`;
  })();

  // Full path of the file the first selected volume will produce, so the user
  // sees the concrete auto-generated name (mirrors the engine's naming; the
  // <тип> here is shown as "full" — the real type/date are decided at run time).
  const previewPath = $derived.by(() => {
    const folder = job.dest_folder.trim();
    if (!folder || job.sources.length === 0) return '';
    const safe = (job.name || 'backup').replace(/[^\p{L}\p{N}]/gu, '_');
    const token = sourceToken(job.sources[0]);
    const sep = folder.endsWith('\\') || folder.endsWith('/') ? '' : '\\';
    return `${folder}${sep}${safe}_${token}_full_${previewStamp}.vhdx`;
  });

  // Universal "folder + file name" field (2-in-1). At rest it shows the full
  // generated path so the whole name is visible; while focused it collapses to
  // the editable "folder\<имя>", and BOTH the folder and the base name can be
  // typed freely (last path segment = base name → job.name, the rest = folder →
  // job.dest_folder). The engine still appends _<буква>_<тип>_<дата>.vhdx.
  let destFocused = $state(false);
  let pathText = $state('');

  function editablePath(): string {
    const folder = job.dest_folder.trim();
    const name = job.name.trim();
    if (!folder && !name) return '';
    const sep = folder && !/[\\/]$/.test(folder) ? '\\' : '';
    return `${folder}${sep}${name}`;
  }
  $effect(() => {
    if (!destFocused) pathText = editablePath();
  });
  const destDisplay = $derived(destFocused ? pathText : previewPath || editablePath());

  function onDestInput(e: Event) {
    pathText = (e.target as HTMLInputElement).value;
    const idx = Math.max(pathText.lastIndexOf('\\'), pathText.lastIndexOf('/'));
    const folder = idx >= 0 ? pathText.slice(0, idx) : '';
    const name = (idx >= 0 ? pathText.slice(idx + 1) : pathText).replace(/\.(a?vhdx)$/i, '');
    job.dest_folder = folder;
    job.name = name;
  }

  function validate(): boolean {
    if (!job.name.trim()) {
      error = 'Укажите имя задания.';
      step = 0;
      return false;
    }
    if (job.sources.length === 0) {
      error = diskChosen
        ? 'Выберите источник (хотя бы один том на диске).'
        : 'Выберите диск, с которым будет работать копия.';
      step = 0;
      return false;
    }
    if (!job.dest_folder.trim()) {
      error = 'Выберите папку назначения.';
      step = 0;
      return false;
    }
    if (collisionLetter) {
      error = `Нельзя сохранять копию на тот же диск, который копируется (${collisionLetter}:). Выберите для назначения другой диск.`;
      step = 0;
      return false;
    }
    error = null;
    return true;
  }

  // ---- encrypted sources: password entry -------------------------------

  // Per-device password inputs for encrypted (LUKS) sources.
  let pwInputs = $state<Record<string, { password: string; remember: boolean }>>({});

  function findPart(dev: string): PartitionInfo | null {
    for (const d of engine.disks) {
      for (const p of d.partitions) if (p.volume_device === dev) return p;
    }
    return null;
  }
  /// An encrypted volume, and what its own metadata says opens it.
  interface EncryptedVolume {
    device: string;
    label: string;
    /// "пароль", "ключ восстановления (48 цифр)", "TPM + PIN"… Empty when the
    /// service could not read the volume's metadata (or is an older build).
    methods: string[];
    /// Can we open it with something the user can type?
    unlockable: boolean;
  }

  // Selected sources that are encrypted, paired with their display label.
  function encryptedSelected(): { device: string; label: string }[] {
    return job.sources
      .map((dev, i) => ({ device: dev, label: job.source_labels[i] ?? dev }))
      .filter(({ device }) => findPart(device)?.is_encrypted);
  }
  // Encrypted volumes shown in the password panel UNDER a disk: those already
  // selected, plus — when the disk is a bootable copy — any still-unselected
  // encrypted volume, so its Linux root can be decrypted and auto-included.
  // Encrypted volumes to OFFER a password for. On a bootable-copy disk that is
  // all of them: the system volume usually has to be unlocked before it can be
  // recognised and chosen at all, so waiting for the user to tick it first
  // would be backwards.
  //
  // Offering is not the same as requiring — see `encryptedBlocking`.
  function encryptedForDisk(d: DiskInfo): EncryptedVolume[] {
    const boot = isBootableCopy(d);
    return d.partitions
      .filter((p) => p.is_encrypted && partDevice(p))
      .filter((p) => boot || job.sources.includes(partDevice(p)!))
      .map((p) => {
        const dev = partDevice(p)!;
        const i = job.sources.indexOf(dev);
        return {
          device: dev,
          label: i >= 0 ? (job.source_labels[i] ?? partLabel(p)) : partLabel(p),
          methods: p.unlock_methods ?? [],
          // "Открыть нечем" is only believed when the volume said WHY. A bare
          // `false` with no reason means the service could not read the
          // container (or is an older build) — then the user gets the password
          // field, as they did before the field existed.
          unlockable: (p.unlockable ?? true) || (p.unlock_methods?.length ?? 0) === 0,
        };
      });
  }
  // A volume whose only typeable way in is the 48-digit key. Windows does not
  // allow a passphrase protector on the system volume, so this is the normal
  // state of C: — and asking it for "пароль" is asking for something that does
  // not exist on that volume.
  function needsRecoveryKey(e: EncryptedVolume): boolean {
    return (
      !e.methods.includes('пароль') &&
      e.methods.some((m) => m.startsWith('ключ восстановления'))
    );
  }
  // What to ask this volume for, in the words of its own metadata.
  function promptFor(e: EncryptedVolume): string {
    return needsRecoveryKey(e) ? '123456-123456-123456-… (48 цифр)' : 'Пароль';
  }

  // The 48-digit key as a person actually has it: printed in eight groups,
  // copied off a phone with the dashes lost, read aloud with spaces. All three
  // become the canonical `123456-…-123456` the engine parses.
  //
  // The checks are the format's own, and they are the same ones
  // `bitlocker::parse_recovery_key` runs: every group is a 16-bit number times
  // eleven. Running them here means a typo is answered in the instant it is
  // made, instead of after a round trip and a million rounds of SHA-256 that
  // can only report "не подошёл" — which for a mistyped digit is not the
  // useful half of the truth.
  function normalizeRecoveryKey(text: string): { key: string } | { error: string } {
    const digits = text.replace(/\D/g, '');
    if (digits.length !== 48) {
      return {
        error: `Ключ восстановления — 48 цифр (8 групп по 6), а введено ${digits.length}`,
      };
    }
    const groups = digits.match(/.{6}/g) as string[];
    const bad = groups.findIndex((g) => Number(g) % 11 !== 0 || Number(g) / 11 > 65535);
    if (bad >= 0) {
      return { error: `Группа ${bad + 1} («${groups[bad]}») не сходится — опечатка в цифрах` };
    }
    return { key: groups.join('-') };
  }

  // Linux OS filesystems: a decrypted volume of one of these on a bootable-copy
  // disk is the system root and is auto-included.
  // Unlocked-volume info lives in a GUI-session store (`decryptedVolumes`), so
  // it survives closing and reopening the wizard. Per-device wrong-password
  // markers and the in-flight device are local to this wizard instance.
  let pwErrors = $state<Record<string, boolean>>({});
  // "What you typed is not a 48-digit key" — said before anything is sent, and
  // kept apart from "the key was right in shape and wrong in fact".
  let pwFormat = $state<Record<string, string>>({});
  let decrypting = $state<string | null>(null);

  // Keep the "needs a password" job flag in sync with the selection.
  $effect(() => {
    job.requires_password = encryptedSelected().length > 0;
  });

  // Auto-include an UNLOCKED volume on a disk marked "Загрузочная копия": the
  // password was typed for exactly one reason, and the copy is of the whole
  // disk. Reactive, so it works no matter the order (toggle-then-decrypt or
  // decrypt-then-toggle) and self-corrects if the disk was toggled after the
  // volume was unlocked. Converges: once a volume is in `sources` the guard
  // skips it (no loop).
  //
  // Until 2026-08-19 this fired only for a Linux root (ext4/xfs/btrfs), because
  // a Windows system volume was supposed to be caught at toggle time by
  // `isBootEssential`. It is not: `is_system`/`has_os` are read from a MOUNTED
  // filesystem, and Windows never mounts a locked BitLocker volume, so both
  // come back false while it is locked — and a locked volume is exactly the
  // state the wizard meets it in. The bootable copy of a 954 GiB disk then went
  // through with the EFI partition alone: 43 MB written in 0 s, reported as
  // success.
  //
  // Unlocking is the user's "yes" and leaving a volume locked is the "no" —
  // `retain_for_backup` then drops it from the layout, so its space comes back
  // free on restore. Both halves are said out loud in the password panel.
  //
  // Included ONCE per volume, not held as an invariant: a user who unlocked a
  // volume to see what is in it must still be able to untick it, and an effect
  // that re-adds on every run would make that impossible. Per wizard instance,
  // like the rest of this screen's scratch state.
  const autoIncluded = new Set<string>();
  $effect(() => {
    if (sourceReadonly) return; // the source of a saved job is fixed
    for (const d of engine.disks) {
      if (!isBootableCopy(d)) continue;
      for (const p of d.partitions) {
        const dev = partDevice(p);
        if (!dev || job.sources.includes(dev) || autoIncluded.has(dev)) continue;
        const dec = decryptedVolumes[dev];
        if (!dec) continue;
        autoIncluded.add(dev);
        job.sources = [...job.sources, dev];
        job.source_labels = [...job.source_labels, dec.label || partLabel(p)];
        syncDiskSourceParts(d);
      }
    }
  });

  function setPw(dev: string, value: string) {
    pwInputs[dev] = { password: value, remember: pwInputs[dev]?.remember ?? false };
    delete pwErrors[dev]; // editing clears a previous "wrong password"
    delete pwFormat[dev]; // …and a previous "that is not 48 digits"
  }
  function setRemember(dev: string, remember: boolean) {
    pwInputs[dev] = { password: pwInputs[dev]?.password ?? '', remember };
  }
  // Are all encrypted sources set to "remember"? (Scheduling needs this.)
  function allRemembered(): boolean {
    const enc = encryptedSelected();
    return enc.length > 0 && enc.every(({ device }) => pwInputs[device]?.remember && pwInputs[device]?.password);
  }

  // Unlock one encrypted volume: verify the password and read its label + used
  // bytes. On success the segment then shows them like any other volume, the
  // label flows into the file name, and "Далее" unlocks.
  async function decrypt(device: string) {
    const typed = pwInputs[device]?.password;
    if (!typed) { pwErrors[device] = true; return; }
    // A volume that takes only the 48-digit key gets the key, in the shape the
    // engine parses — whatever shape it was typed in. Only input that is trying
    // to BE a key (digits and separators) is held to the format; anything with
    // letters in it goes through untouched, so a volume whose protector list we
    // read wrong is never a dead end.
    let pw = typed;
    const vol = encryptedOffered().find((e) => e.device === device);
    if (vol && needsRecoveryKey(vol) && /^[\d\s-]+$/.test(typed)) {
      const parsed = normalizeRecoveryKey(typed);
      if ('error' in parsed) { pwFormat[device] = parsed.error; return; }
      pw = parsed.key;
    }
    decrypting = device;
    delete pwErrors[device];
    delete pwFormat[device];
    try {
      const r = await decryptVolume(device, pw, pwInputs[device]?.remember ?? false);
      if (!r.ok) { pwErrors[device] = true; return; }
      // Remember for the whole GUI session (survives reopening the wizard). A
      // decrypted Linux OS volume on a "Загрузочная копия" disk is then
      // auto-included by the reactive effect below (any order of actions).
      decryptedVolumes[device] = { label: r.label, used_bytes: r.used_bytes, filesystem: r.filesystem };

      // Carry the volume label into the job (file name + display), like C:/D:.
      const i = job.sources.indexOf(device);
      if (i >= 0 && r.label) {
        job.source_labels[i] = r.label;
        if (i === 0) job.source_label = r.label;
      }
      job.source = job.sources[0] ?? '';
      job.source_label = job.source_labels[0] ?? '';
    } catch (e) {
      error = String(e);
    } finally {
      decrypting = null;
    }
  }

  // ---- one password for every volume ("Общий пароль") -------------------

  // Typing the same BitLocker/LUKS password into three boxes in a row is the
  // normal case, not the exception: one policy, one passphrase, several
  // volumes. With the box ticked the panels collapse into a single prompt that
  // unlocks every locked volume on every disk in turn; per-volume entry is one
  // click away again for the machine where the passwords really do differ.
  let sharedPw = $state(false);
  let sharedInput = $state({ password: '', remember: false });
  let sharedBusy = $state(false);

  // Every encrypted volume a password is offered for, across all shown disks.
  function encryptedOffered(): EncryptedVolume[] {
    return shownDisks.flatMap((d) => encryptedForDisk(d));
  }
  // The shared prompt is drawn once — under the first disk that still has a
  // locked volume — instead of once per disk panel.
  const sharedFormDiskId = $derived(
    shownDisks.find((d) =>
      encryptedForDisk(d).some(({ device }) => !decryptedVolumes[device]),
    )?.device_id ?? null,
  );
  // Volumes one password can be tried on. A volume that takes only the 48-digit
  // recovery key is not one of them: that key is unique per volume, so feeding
  // it the shared password would mark it "не подошёл" for a reason that has
  // nothing to do with the password. It keeps its own field instead.
  function sharedCandidates(): EncryptedVolume[] {
    return encryptedOffered().filter((e) => e.unlockable && !needsRecoveryKey(e));
  }
  // Volumes the shared password did not open: they need individual entry, so
  // say so rather than leaving "Дальше" greyed out with no explanation.
  const sharedFailed = $derived(sharedCandidates().filter(({ device }) => pwErrors[device]));

  function setSharedPw(value: string) {
    sharedInput.password = value;
    pwErrors = {}; // editing clears the previous "wrong password" markers
  }
  async function decryptAllShared() {
    if (!sharedInput.password) return;
    sharedBusy = true;
    try {
      for (const { device } of sharedCandidates()) {
        if (decryptedVolumes[device]) continue;
        pwInputs[device] = { password: sharedInput.password, remember: sharedInput.remember };
        await decrypt(device);
      }
    } finally {
      sharedBusy = false;
    }
  }

  // Every encrypted volume shown in a per-disk password panel — selected, OR on
  // a disk marked "Загрузочная копия" — must be unlocked before leaving the
  // Source step. Otherwise an undecrypted LUKS root gives an incomplete
  // bootable copy, so "Далее" stays disabled until every shown lock is opened.
  // Encrypted volumes that actually stand in the way: the ones this copy will
  // read. A 347 GiB BitLocker volume the user unchecked used to hold "Дальше"
  // greyed out on a bootable-copy disk, with no way forward but to type a
  // password for data nobody was going to touch.
  function encryptedBlocking(): EncryptedVolume[] {
    return engine.disks
      .flatMap((d) => encryptedForDisk(d))
      .filter(({ device }) => job.sources.includes(device));
  }
  function allEncryptedDecrypted(): boolean {
    return encryptedBlocking().every(({ device }) => decryptedVolumes[device]);
  }

  // The source step is complete: a source is chosen, any encrypted volume is
  // unlocked, and a destination folder is set. "Далее" stays disabled until then.
  function canLeaveSource(): boolean {
    return job.sources.length > 0 && allEncryptedDecrypted() && !!job.dest_folder.trim();
  }
  function nextHint(): string {
    // The hint names the question the screen is actually asking: at stage 1 it
    // is the disk, and "выберите том" would point at volumes that are not drawn
    // yet.
    if (!diskChosen) return 'Выберите диск, с которым будет работать копия';
    if (job.sources.length === 0) return 'Выберите том-источник';
    // Name the volume: "расшифруйте зашифрованный том" is no help at all when
    // the screen shows two of them and one is already unlocked.
    const locked = encryptedBlocking().find(({ device }) => !decryptedVolumes[device]);
    // A volume with nothing we can type is not waiting for a password: telling
    // the user to enter one is a dead end, since no field is shown for it.
    if (locked) {
      if (!locked.unlockable) return `Том «${locked.label}» открыть нечем — снимите с него галочку`;
      return needsRecoveryKey(locked)
        ? `Введите ключ восстановления для тома «${locked.label}»`
        : `Введите пароль для тома «${locked.label}»`;
    }
    if (!job.dest_folder.trim()) return 'Выберите папку назначения';
    return '';
  }

  async function finish(run: boolean) {
    if (!validate()) return;
    // A run needs every encrypted source unlocked first — the password is
    // stored (and verified) when the user clicks "Расшифровать".
    if (run && !allEncryptedDecrypted()) {
      error = nextHint() || 'Сначала расшифруйте выбранный зашифрованный том.';
      step = 0;
      return;
    }
    // Snapshot the source-disk layout(s) into the job once ("если её ещё нет"),
    // so the restore screen can render the archived disk instantly later without
    // querying live disks — and even if that disk is no longer attached.
    if (!job.source_disks || job.source_disks.length === 0) {
      const snap = engine.disks.filter(diskHasSelection);
      if (snap.length > 0) job.source_disks = $state.snapshot(snap) as DiskInfo[];
    }
    try {
      const saved = await saveJob(job);
      if (run) await runJob(saved.id);
      onDone(saved);
    } catch (e) {
      error = String(e);
    }
  }

  // Migrate an old single-source job (only `source` set) into the multi-source
  // array. Nothing is selected by default — the user picks volumes explicitly.
  $effect(() => {
    if (job.sources.length === 0 && job.source.trim()) {
      job.sources = [job.source];
      job.source_labels = [job.source_label || job.source];
    }
  });
</script>

{#snippet segInner(p: PartitionInfo, selected: boolean, layoutOnly = false)}
  {#if usedPct(p) > 0}
    <div class="usedfill" style="width: {usedPct(p)}%"></div>
  {/if}
  <div class="seg-top">
    {#if p.is_system || p.is_boot || p.has_os}
      <svg class="lock" width="10" height="10" viewBox="0 0 24 24" fill="currentColor"><path d="M12 2a5 5 0 0 0-5 5v3H6a1 1 0 0 0-1 1v9a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-9a1 1 0 0 0-1-1h-1V7a5 5 0 0 0-5-5zm-3 8V7a3 3 0 0 1 6 0v3z"/></svg>
    {/if}
    {#if p.is_encrypted}
      <span class="enc" title="Зашифрован — потребуется пароль">🔒</span>
    {/if}
    <span class="seg-label">{partLabel(p)}</span>
  </div>
  <div class="seg-meta">{formatBytes(p.size_bytes)}{#if partUsed(p) > 0}<span class="muted">&nbsp;({formatBytes(partUsed(p))} занято)</span>{/if}{#if partFs(p)}<span class="muted">&nbsp;{partFs(p)}</span>{/if}{#if layoutOnly}<span class="muted">&nbsp;· в копии (только разметка)</span>{/if}</div>
  {#if selected}
    <div class="check"><svg width="10" height="10" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></div>
  {/if}
{/snippet}

{#snippet pwPanel(list: EncryptedVolume[], showShared: boolean, whole: boolean)}
  <div class="pw-panel under-disk">
    <div class="pw-head">
      <span>
        🔒 Зашифрованные тома — {list.every(needsRecoveryKey)
          ? 'введите ключ восстановления'
          : 'введите пароль для расшифровки'}
      </span>
      <!-- One toggle for the whole screen: it lives next to the single shared
           prompt, not once per disk panel. -->
      {#if showShared && sharedCandidates().length > 1}
        <label class="pw-shared-tgl" title="Один и тот же пароль у всех зашифрованных томов — ввести его один раз и расшифровать все">
          <input type="checkbox" bind:checked={sharedPw} /> Общий пароль
        </label>
      {/if}
    </div>

    {#if sharedPw && showShared}
      <div class="pw-row shared">
        <span class="pw-vol">Пароль для всех томов</span>
        <PasswordField
          value={sharedInput.password}
          oninput={setSharedPw}
          onenter={() => { if (!sharedBusy && sharedInput.password) decryptAllShared(); }}
          invalid={sharedFailed.length > 0}
        />
        <label class="pw-remember">
          <input type="checkbox" bind:checked={sharedInput.remember} /> Запомнить
        </label>
        <button
          type="button"
          class="btn small primary"
          onclick={decryptAllShared}
          disabled={sharedBusy || !sharedInput.password}
        >{sharedBusy ? 'Расшифровка…' : 'Расшифровать все'}</button>
      </div>
      {#if sharedFailed.length > 0 && !sharedBusy}
        <div class="pw-err">
          Пароль не подошёл: {sharedFailed.map((f) => f.label).join(', ')}. Снимите
          «Общий пароль», чтобы ввести пароль для этих томов отдельно.
        </div>
      {/if}
    {/if}

    {#each list as e (e.device)}
      <!-- A volume asking for the 48-digit key gets two lines: the question on
           its own, the answer under it. On one line the label, the words "ключ
           восстановления", a field wide enough to type into and two controls do
           not fit — the "Расшифровать" button was pushed off the panel. -->
      {@const stacked = needsRecoveryKey(e) && !decryptedVolumes[e.device] && e.unlockable}
      <div
        class="pw-row"
        class:done={!!decryptedVolumes[e.device]}
        class:stacked
      >
        {#if stacked}
          <div class="pw-ask">{e.label}, введите ключ восстановления:</div>
        {:else}
          <span class="pw-vol" title={e.device}>{e.label}</span>
        {/if}
        {#if !e.unlockable && !decryptedVolumes[e.device]}
          <!-- Asking for a password a volume does not have, and then calling
               it wrong, is not an error message — it is the wrong question.
               The volume's own metadata says what it does have. -->
          <span class="pw-nokey">
            Открыть нечем: {e.methods.join(', ')}. Own Backup открывает том паролем
            или ключом восстановления — снимите блокировку в системе или исключите
            том из копии.
          </span>
        {:else if decryptedVolumes[e.device]}
          <span class="pw-done">✓ Расшифровано</span>
        {:else if sharedPw && !needsRecoveryKey(e)}
          <!-- One prompt for all: the per-volume row is only a status line. -->
          <span class="pw-wait">
            {#if decrypting === e.device}Расшифровка…
            {:else if pwErrors[e.device]}Пароль не подошёл
            {:else}Ожидает общего пароля{/if}
          </span>
        {:else}
          <!-- The entry line: the field takes every spare pixel, the two
               controls keep their width at the right edge. It is a row of its
               own when stacked; otherwise `display: contents` dissolves the
               wrapper and the three controls sit in the parent row as before. -->
          <div class="pw-line">
            <PasswordField
              value={pwInputs[e.device]?.password ?? ''}
              oninput={(v) => setPw(e.device, v)}
              placeholder={promptFor(e)}
              revealed={needsRecoveryKey(e)}
              onenter={() => {
                if (decrypting !== e.device && pwInputs[e.device]?.password) decrypt(e.device);
              }}
              invalid={pwErrors[e.device] || !!pwFormat[e.device]}
            />
            <label class="pw-remember">
              <input
                type="checkbox"
                checked={pwInputs[e.device]?.remember ?? false}
                onchange={(ev) => setRemember(e.device, (ev.target as HTMLInputElement).checked)}
              /> Запомнить
            </label>
            <button
              type="button"
              class="btn small primary"
              onclick={() => decrypt(e.device)}
              disabled={decrypting === e.device || !pwInputs[e.device]?.password}
            >{decrypting === e.device ? 'Расшифровка…' : 'Расшифровать'}</button>
          </div>
        {/if}
      </div>
      {#if pwFormat[e.device]}
        <div class="pw-err">{pwFormat[e.device]}</div>
      {:else if pwErrors[e.device] && !sharedPw}
        <div class="pw-err">
          {needsRecoveryKey(e) ? 'Ключ восстановления не подходит этому тому' : 'Пароль не подходит'}
        </div>
      {/if}
    {/each}
    {#if list.some(needsRecoveryKey)}
      <!-- Where to get the key, since the answer is on the machine the user is
           standing at — but in the system they are not currently running. -->
      <div class="pw-hint">
        У системного тома пароля нет — его открывает только 48-значный ключ
        восстановления. Узнать его в работающей Windows:
        <code>manage-bde -protectors -get C:</code> — либо на странице учётной
        записи Microsoft, в AD/Intune, или с распечатки. Вводить можно с дефисами,
        пробелами или подряд.
      </div>
    {/if}
    {#if whole && list.some((e) => !decryptedVolumes[e.device])}
      <!-- Said before the copy runs, because afterwards it is a 43 MB image
           where 128 GiB were expected. A volume left locked is dropped from the
           partition table too (`DiskLayout::retain_for_backup`), not kept as an
           empty slot. -->
      <div class="pw-hint warn">
        Тома, оставшиеся зашифрованными, в загрузочную копию НЕ войдут — ни
        данными, ни записью в таблице разделов: при восстановлении их место
        останется свободным. Расшифрованные войдут в копию расшифрованными.
      </div>
    {/if}
    <div class="pw-hint">
      Подходит пароль тома или 48-значный ключ восстановления. Пароль хранится в
      памяти до закрытия программы. «Запомнить» сохраняет его в защищённом
      хранилище ОС — это нужно, чтобы копия работала по расписанию.
    </div>
  </div>
{/snippet}

<div class="wizard">
    <div class="wz-head">
      <span class="wz-title">Параметры резервного копирования диска</span>
      <span class="wz-name">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.4"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.2"/></svg>
        {job.name}
      </span>
    </div>

    <!-- tab bar -->
    <div class="tabs">
      {#each steps as s, i}
        <button class="tab" class:active={step === i} onclick={() => (step = i)}>{s.label}</button>
      {/each}
    </div>

    <div class="body">
      <!-- source (top) → destination (bottom) -->
      {#if stepKey === 'source'}
        <div class="src-wrap">
          <!-- SOURCE -->
          <div class="sec-head">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="2"/><circle cx="12" cy="12" r="3.4"/></svg>
            <span class="sec-title">{sourceReadonly ? 'Что скопировано' : 'Что копировать'}</span>
            <span class="sec-sub">{sourceReadonly
              ? 'разделы, включённые в эту копию (менять нельзя)'
              : diskChosen
                ? 'теперь выберите один или несколько томов на диске'
                : 'вначале выберите диск, с которым будет работать'}</span>
            {#if diskChosen && !sourceReadonly}
              <button class="link back-disk" onclick={pickAnotherDisk}>← другой диск</button>
            {/if}
          </div>

          {#if engine.volumesLoading && engine.disks.length === 0}
            <div class="loading-note">Загрузка списка дисков…</div>
          {:else if engine.volumesError && engine.disks.length === 0}
            <div class="warn-note">{engine.volumesError}</div>
          {:else if visibleDisks.length === 0}
            <div class="warn-note">
              {sourceReadonly
                ? 'Диски этой копии сейчас недоступны (не подключены) — источник показать нельзя.'
                : 'Диски не найдены.'}
            </div>
          {:else}
            <div class="disks">
              {#each shownDisks as d (d.device_id)}
                {#if !diskChosen}
                  <!-- Stage 1. The whole card is one button: the only question
                       here is WHICH disk, and nothing inside it can be answered
                       before that. The map is drawn all the same — a disk is
                       recognised by what is on it, not by its model string. -->
                  <button type="button" class="disk-card pickable" onclick={() => pickDisk(d)}>
                    <div class="disk-head">
                      <span class="disk-model">{d.model || d.device_id}</span>
                      <span class="disk-head-right">
                        <span class="disk-meta">{formatBytes(d.size_bytes)} · {tableLabel(d.partition_table)}</span>
                      </span>
                    </div>
                    <div class="disk-body">
                      <div class="disk-ico">
                        <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.3"><path d="M3 8a9 3 0 0 0 18 0M3 8a9 3 0 0 1 18 0v8a9 3 0 0 1-18 0z"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>
                        <div class="disk-name">Диск {d.device_id.replace(/\D+/g, '') || ''}</div>
                      </div>
                      <div class="bar">
                        {#each diskSegments(d) as seg}
                          {#if seg.kind === 'free'}
                            <div class="seg free" style="flex: {seg.size} 1 0"><span class="free-lbl">Не занято</span></div>
                          {:else if seg.p}
                            <div class="seg" style="flex: {seg.size} 1 0" title={segTitle(seg.p)}>
                              {@render segInner(seg.p, false, false)}
                            </div>
                          {/if}
                        {/each}
                      </div>
                    </div>
                  </button>
                {:else}
                <div class="disk-card">
                  <div class="disk-head" class:whole={isBootableCopy(d)}>
                    <span class="disk-model">{d.model || d.device_id}</span>
                    <span class="disk-head-right">
                      {#if isBootableCopy(d)}<span class="whole-badge">загрузочный</span>{/if}
                      <label
                        class="boot-toggle"
                        class:disabled={!isBootableDisk(d)}
                        title={isBootableDisk(d)
                          ? 'Включить в копию загрузочные разделы (EFI и системный), чтобы систему можно было восстановить на новый диск с сохранением загрузки. Таблица разделов (GPT) сохраняется в любом случае — копия диска всегда один файл.'
                          : 'Диск не является загрузочным (нет EFI-раздела) — загрузочная копия недоступна.'}
                      >
                        <input
                          type="checkbox"
                          checked={isBootableCopy(d)}
                          disabled={sourceReadonly || !isBootableDisk(d)}
                          onchange={() => toggleBootableCopy(d)}
                        />
                        <span>Загрузочная копия диска</span>
                      </label>
                      <span class="disk-meta">{formatBytes(d.size_bytes)} · {tableLabel(d.partition_table)}</span>
                    </span>
                  </div>
                  <div class="disk-body">
                    <div class="disk-ico">
                      <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--text-muted)" stroke-width="1.3"><path d="M3 8a9 3 0 0 0 18 0M3 8a9 3 0 0 1 18 0v8a9 3 0 0 1-18 0z"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>
                      <div class="disk-name">Диск {d.device_id.replace(/\D+/g, '') || ''}</div>
                    </div>
                    <div class="bar">
                      {#each diskSegments(d) as seg}
                        {#if seg.kind === 'free'}
                          <div class="seg free" style="flex: {seg.size} 1 0"><span class="free-lbl">Не занято</span></div>
                        {:else if seg.p}
                          {@const dev = partDevice(seg.p)}
                          {#if dev}
                            <button
                              type="button"
                              class="seg selectable"
                              class:selected={isSelected(dev)}
                              class:locked={isLockedSource(dev)}
                              class:ro={sourceReadonly}
                              style="flex: {seg.size} 1 0"
                              title={isLockedSource(dev) ? segTitle(seg.p) + ' · обязателен для загрузки' : segTitle(seg.p)}
                              onclick={() => toggleSource(dev, partLabel(seg.p!))}
                            >
                              {@render segInner(seg.p, isSelected(dev), false)}
                            </button>
                          {:else}
                            {@const inLayout = layoutOnlyParts(d).includes(seg.p.index)}
                            <!-- No volume to select and nothing inside to copy,
                                 but a whole-disk copy does keep its table entry.
                                 Drawn exactly like a selected partition — tick
                                 and all — because that is what it is: part of
                                 the copy. It just cannot be toggled, since
                                 there is no volume to include or leave out. -->
                            <div
                              class="seg"
                              class:selected={inLayout}
                              class:layout-only={inLayout}
                              style="flex: {seg.size} 1 0"
                              title={inLayout
                                ? segTitle(seg.p) + ' · входит в копию: файловой системы нет, сохраняется только запись в таблице разделов'
                                : segTitle(seg.p)}
                            >
                              {@render segInner(seg.p, inLayout, inLayout)}
                            </div>
                          {/if}
                        {/if}
                      {/each}
                    </div>
                  </div>
                </div>
                {#if isBootableCopy(d) && !d.partitions.some((p) => p.has_os)}
                  <div class="boot-hint">
                    Система на этом диске не опознана — Own Backup определяет только
                    установленную Windows. В копию включены все разделы диска, чтобы
                    ничего нужного для загрузки не потерялось. Разделы с данными можно
                    снять, но <b>раздел с системой снимать нельзя</b> — без него копия не загрузится.
                  </div>
                {/if}
                {#if encryptedForDisk(d).length > 0}
                  {@render pwPanel(encryptedForDisk(d), d.device_id === sharedFormDiskId, isBootableCopy(d))}
                {/if}
                {/if}
              {/each}
            </div>
          {/if}

          {#if sourceReadonly && catData}
            <div class="cat-block">
              <div class="cat-head">Что в копии — по типам файлов</div>
              <div class="catbar">
                {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}
                  <div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>
                {/each}
              </div>
              <div class="cats">
                {#each catData as c}
                  <div class="cat">
                    <span class="dot" style="background:{c.color}"></span>
                    <span class="cat-name">{c.name}</span>
                    <span class="cat-size">{formatBytes(c.bytes)}</span>
                  </div>
                {/each}
                {#if job.last_run && job.last_run.excluded_files > 0}
                  <div class="cat">
                    <span class="dot" style="background:#c0463f"></span>
                    <span class="cat-name">Исключено</span>
                    <span class="cat-size">{formatBytes(job.last_run.excluded_bytes)}</span>
                  </div>
                {/if}
              </div>
            </div>
          {/if}

          <!-- Stage 2 only: where the copy goes is a question about a copy
               that has a source. Until a disk is picked the screen holds one
               question, not three. -->
          {#if diskChosen}
          <!-- connector -->
          <div class="connector">
            <div class="dash"></div>
            <div class="arrow-circle"><svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.6" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M19 12l-7 7-7-7"/></svg></div>
            <div class="dash"></div>
          </div>

          <!-- DESTINATION -->
          <div class="sec-head">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--primary)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 6a2 2 0 0 1 2-2h5l2 2h5a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z"/></svg>
            <span class="sec-title">Куда сохранять</span>
            <span class="sec-sub">папка и имя файла резервной копии</span>
          </div>
          <div class="dest-card">
            <svg width="42" height="40" viewBox="0 0 24 22" fill="#e6b34a" style="flex-shrink:0"><path d="M4 2h9l2 3h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1z"/><rect x="3" y="9" width="18" height="10" rx="1" fill="#f0c268"/></svg>
            <div class="dest-fields">
              <input
                type="text"
                value={destDisplay}
                oninput={onDestInput}
                onfocus={() => (destFocused = true)}
                onblur={() => (destFocused = false)}
                placeholder="D:\OwnBackup"
                spellcheck="false"
                title={destDisplay}
              />
              <button class="link" onclick={browseDest}>Обзор…</button>
            </div>
          </div>
          {#if collisionLetter}
            <div class="warn-note">⛔ Назначение на том же диске ({collisionLetter}:), что и один из источников. Копию нужно писать на другой диск — выберите другую папку.</div>
          {/if}
          {/if}
        </div>
      {/if}

      <!-- schedule -->
      {#if stepKey === 'schedule'}
        <div class="sched">
          <div class="sched-kinds">
            {#each scheduleKinds as [k, lbl]}
              <label class="radio"><input type="radio" name="skind" value={k} checked={job.schedule.kind === k} onchange={() => (job.schedule.kind = k as Job['schedule']['kind'])} /> {lbl}</label>
            {/each}
          </div>
          <div class="sched-right">
            {#if job.schedule.kind === 'weekly'}
              <div class="days">
                {#each weekdays as [d, lbl]}
                  <button class="day" class:on={job.schedule.days.includes(d)} onclick={() => toggleDay(d)}>{lbl}</button>
                {/each}
              </div>
            {/if}
            {#if job.schedule.kind === 'monthly'}
              <label class="at">Число
                <select bind:value={job.schedule.day_of_month}>
                  {#each Array.from({ length: 28 }, (_, i) => i + 1) as d}
                    <option value={d}>{d}</option>
                  {/each}
                </select>
              </label>
            {/if}
            {#if job.schedule.kind === 'daily' || job.schedule.kind === 'weekly' || job.schedule.kind === 'monthly'}
              <label class="at">В <input type="time" bind:value={job.schedule.time} /></label>
            {/if}
            {#if job.schedule.kind === 'event'}
              <div class="event-opts">
                <label class="chk"><input type="checkbox" bind:checked={job.schedule.event_once_per_day} /> Только один раз в день</label>
                {#each eventTriggers as [val, lbl, badge]}
                  <label class="radio">
                    <input type="radio" name="evtrig" value={val} checked={job.schedule.event_trigger === val} onchange={() => (job.schedule.event_trigger = val)} />
                    {lbl}{#if badge}<span class="dev-badge">{badge}</span>{/if}
                  </label>
                  {#if val === 'startup' && job.schedule.event_trigger === 'startup'}
                    <label class="delay-min">Задержка:
                      <select bind:value={job.schedule.event_startup_delay_min}>
                        {#each Array.from({ length: 61 }, (_, i) => i) as mn}<option value={mn}>{mn}</option>{/each}
                      </select> мин
                    </label>
                  {/if}
                {/each}
              </div>
            {/if}
          </div>
        </div>
        {#if job.schedule.kind === 'monthly'}
          <div class="hint-sm">Число месяца — только 1–28: 29–31 есть не в каждом месяце, поэтому такие даты не выбираются.</div>
        {/if}
        {#if job.schedule.kind === 'event'}
          <div class="note">✓ Работают: вход/выход пользователя и запуск системы. Завершение работы фиксируется, но копия может не успеть создаться при выключении. Подключение устройства — в разработке. Все триггеры выполняет служба «Own Backup».</div>
        {:else if job.schedule.kind !== 'none'}
          <div class="note">✓ Автозапуск выполняет служба «Own Backup» (должна быть установлена и запущена). Для зашифрованных томов сохраните пароль на шаге «Источник».</div>
        {/if}
        <div class="adv-schedule">
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.only_when_locked} onchange={stub} /> Копировать только если компьютер заблокирован</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.wake_to_run} onchange={stub} /> Пробуждать компьютер из спящего режима</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.prevent_sleep} onchange={stub} /> Предотвращать переход в спящий режим</label>
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.run_missed} /> Выполнять пропущенные операции при запуске системы с задержкой (в минутах)</label>
          {#if job.schedule.run_missed}
            <label class="delay-min">Задержка после запуска системы:
              <select bind:value={job.schedule.missed_delay_min}>
                {#each Array.from({ length: 60 }, (_, i) => i + 1) as mn}
                  <option value={mn}>{mn}</option>
                {/each}
              </select> мин
            </label>
          {/if}
        </div>
      {/if}

      <!-- scheme -->
      {#if stepKey === 'scheme'}
        <div class="field-label">Формат образа:</div>
        <select
          bind:value={job.image_format}
          class="wide"
          disabled={mode === 'edit' && job.last_run !== null}
        >
          <option value="vhdx">VHDX — стандарт Windows (по умолчанию)</option>
          <option value="qcow2">qcow2 — универсальный, со сжатием</option>
        </select>
        {#if mode === 'edit' && job.last_run !== null}
          <div class="note">У задания уже есть резервные копии — менять формат существующего архива нельзя. Для другого формата создайте новое задание.</div>
        {/if}
        {#if job.image_format === 'qcow2'}
          <div class="note">Встроенное сжатие (zstd): копия обычно на 30–50 % меньше, экономия видна сразу. Один и тот же файл одинаково открывается на Windows и Linux. Инкрементные и дифференциальные цепочки поддерживаются. Восстановление, проверка и монтирование копии как диска работают. Исключённые файлы удаляются из копии полностью, в том числе на NTFS/ReFS — как в VHDX.</div>
        {:else}
          <div class="note">Копия открывается самой Windows двойным кликом и видна как обычный диск — даже без Own Backup. Инкрементные цепочки и полное удаление исключённых файлов (NTFS/ReFS). Сжатия нет — файл больше; на Linux открывается через qemu-nbd.</div>
        {/if}
        <div class="field-label mt">Метод резервного копирования:</div>
        <select bind:value={job.scheme.method} onchange={onMethodChange} class="wide">
          <option value="full">Полное</option>
          <option value="incremental">Инкрементное</option>
          <option value="differential">Дифференциальное</option>
        </select>
        {#if job.scheme.method === 'incremental'}
          <div class="note">Каждый запуск сохраняет только изменения относительно предыдущей версии (цепочка: полная → И1 → И2 …).</div>
        {:else if job.scheme.method === 'differential'}
          <div class="note">Каждый запуск сохраняет изменения относительно полной копии (полная → Д1, Д2 …). Объединение для этого метода недоступно: все версии опираются прямо на полную копию, и сдвинуть её нельзя, не сделав остальные неверными.</div>
        {/if}

        {#if job.scheme.method === 'incremental'}
          <div class="field-label mt">Режим хранения:</div>
          <label class="radio blk"><input type="radio" checked={!isConsolidation} onchange={() => setSchemeMode('standard')} /> Стандартный — цепочки сменяются</label>
          <label class="radio blk"><input type="radio" checked={isConsolidation} onchange={() => setSchemeMode('consolidation')} /> Объединение (консолидация) — одна цепочка</label>
        {/if}

        {#if job.scheme.method !== 'full' && !isConsolidation}
          <!-- Стандартный режим: цепочка растёт до предела, затем начинается новая. -->
          <div class="field-label mt">Длина цепочки:</div>
          <label class="chk-line">Делать не более <input type="number" min="1" max="64" bind:value={job.scheme.full_every_n} class="num" /> {job.scheme.method === 'differential' ? 'дифференциальных' : 'инкрементных'} версий в цепочке</label>
          <div class="note">Когда предел достигнут, следующий запуск снимает новую полную копию и начинает новую цепочку. Старые цепочки остаются целыми и удаляются по правилам ниже.<br />Точек восстановления: до {job.scheme.full_every_n + 1} в каждой цепочке.</div>

          <div class="field-label mt">Правила очистки старых цепочек:</div>
          <label class="chk"><input type="checkbox" checked={job.scheme.keep_chains !== null} onchange={(e) => (job.scheme.keep_chains = (e.target as HTMLInputElement).checked ? 3 : null)} /> Хранить не более <input type="number" min="1" value={job.scheme.keep_chains ?? 3} oninput={(e) => (job.scheme.keep_chains = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.keep_chains === null} /> последних цепочек</label>
          <label class="chk"><input type="checkbox" checked={job.scheme.cleanup_older_than_days !== null} onchange={(e) => (job.scheme.cleanup_older_than_days = (e.target as HTMLInputElement).checked ? 90 : null)} /> Удалять цепочки старше <input type="number" min="1" value={job.scheme.cleanup_older_than_days ?? 90} oninput={(e) => (job.scheme.cleanup_older_than_days = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.cleanup_older_than_days === null} /> дней</label>
          <label class="chk sub"><input type="checkbox" bind:checked={job.scheme.keep_first} disabled={job.scheme.cleanup_older_than_days === null} /> Не удалять первую (самую старую) цепочку</label>
          <div class="note">Цепочка удаляется целиком — полная копия вместе с её версиями — и только после успешного завершения очередного копирования. Цепочка, к которой относится свежая копия, не удаляется никогда.</div>
        {/if}

        {#if isConsolidation}
          <!-- Консолидация: цепочка одна, её длину держит объединение. -->
          <div class="field-label mt">Длина цепочки:</div>
          <label class="chk-line">Делать объединение после <input type="number" min="1" max="64" value={job.scheme.rolling_incrementals ?? 3} oninput={(e) => (job.scheme.rolling_incrementals = +(e.target as HTMLInputElement).value)} class="num" /> инкрементных версий</label>
          <div class="note">Новая полная копия не создаётся: когда версий становится больше предела, самая старая вливается в полную копию, а следующая начинает опираться прямо на неё. Место экономится, а восстановление идёт по короткой цепочке.<br />Точек восстановления: {(job.scheme.rolling_incrementals ?? 3) + 1} — полная плюс {job.scheme.rolling_incrementals ?? 3}. Но <b>полная копия перестаёт быть копией того дня, когда её сняли</b>: она сдвигается вперёд, на день влитой версии, и откатиться глубже уже нельзя.</div>

          <div class="field-label mt">Обновление цепочки:</div>
          <label class="chk"><input type="checkbox" checked={job.scheme.cleanup_older_than_days !== null} onchange={(e) => (job.scheme.cleanup_older_than_days = (e.target as HTMLInputElement).checked ? 90 : null)} /> Начинать цепочку заново, когда ей больше <input type="number" min="1" value={job.scheme.cleanup_older_than_days ?? 90} oninput={(e) => (job.scheme.cleanup_older_than_days = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.cleanup_older_than_days === null} /> дней</label>
          {#if job.scheme.cleanup_older_than_days !== null}
            <div class="note">Снимается новая полная копия, и только после её успешного завершения старая цепочка удаляется — момента без резервной копии не бывает. Считается возраст цепочки с её начала, а не дата полной копии: объединение сдвигает её вперёд каждый раз.</div>
          {:else}
            <div class="note warn">Цепочка не будет обновляться никогда: одна и та же полная копия останется единственным экземпляром этих данных и будет переписываться при каждом объединении. Один сбойный блок в ней заберёт сразу все точки восстановления. Рекомендуется включить обновление.</div>
          {/if}
        {/if}
      {/if}

      <!-- exclusions -->
      {#if stepKey === 'exclusions'}
        <label class="chk"><input type="checkbox" bind:checked={job.exclusions.enabled} /> Исключить файлы по следующим критериям:</label>
        <div class="note">Маска без пути (<code>*.tmp</code>, <code>pagefile.sys</code>) действует по всему тому; маска с путём (<code>Temp\*.log</code>) — от корня тома. В посекторном режиме исключения не применяются.<br />Исключённые файлы удаляются из копии полностью — как в Acronis: место в архиве и на восстановленном томе освобождается. На <b>NTFS и ReFS</b> данные исключённых файлов в копию не попадают вовсе, а записи каталога в готовом образе удаляет сам драйвер файловой системы Windows; если этот шаг не удался, файлы остаются в каталоге копии пустыми (нулевыми) — резервная копия при этом полностью рабочая.</div>

        <!-- Windows / Linux списки: движок применяет их по типу ФС раздела. -->
        <div class="excl-tabs">
          <button class="excl-tab" class:active={exclTab === 'windows'} onclick={() => (exclTab = 'windows')}>Windows</button>
          <button class="excl-tab" class:active={exclTab === 'linux'} onclick={() => (exclTab = 'linux')}>Linux</button>
        </div>
        {#if exclTab === 'windows'}
          <div class="note sub">Для разделов Windows (NTFS / ReFS / FAT).</div>
        {:else}
          <div class="note sub">Для разделов Linux. Маски от корня тома (<code>/</code>), напр. <code>var/cache/**</code>. <b>ext2/3/4</b> и <b>btrfs</b> — полностью (данные не попадают в копию + файл удаляется из каталога). <b>xfs</b> — данные исключённых файлов не попадают в копию (сам файл пока остаётся пустым в каталоге). Для btrfs/xfs файлы с общими экстентами (снапшоты/reflink) сохраняются целиком.</div>
        {/if}

        <div class="excl-list">
          {#each exclList() as p, i (p + i)}
            <div class="excl-row"><span>{p}</span><button class="x" onclick={() => removePattern(i)}>×</button></div>
          {/each}
        </div>
        <div class="excl-add">
          <input type="text" bind:value={newPattern} placeholder={exclTab === 'linux' ? 'Путь или маска, напр. var/tmp/**' : 'Путь или маска, напр. *.tmp'} spellcheck="false" onkeydown={(e) => e.key === 'Enter' && addPattern()} />
          <button class="btn small" onclick={addPattern}>+ Добавить</button>
          <button class="link" onclick={resetExclusions}>Исходный список</button>
        </div>
      {/if}

      <!-- advanced -->
      {#if stepKey === 'advanced'}
        <div class="adv">
          <div class="adv-group">
            <div class="adv-title">Режим создания образа</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.sector_by_sector} /> Архивировать в посекторном режиме <span class="ok-badge">работает</span></label>
          </div>
          <div class="adv-group">
            <div class="adv-title">Проверка</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.validate_after} /> Проверять резервную копию после создания <span class="ok-badge">работает</span></label>
          </div>
          <button class="adv-row" onclick={stub}><span>🔒 Защита резервной копии (пароль)</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⌘ Pre/Post-команды</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⚠ Обработка ошибок</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⏻ Выключение компьютера</span><span class="dev-badge">в разработке</span></button>
          <button class="adv-row" onclick={stub}><span>⏱ Производительность</span><span class="dev-badge">в разработке</span></button>
        </div>
      {/if}
    </div>

    {#if error}<div class="wz-error">{error}</div>{/if}

    {#if mode === 'wizard' && stepKey === 'schedule' && job.schedule.kind !== 'none' && job.requires_password && !allRemembered()}
      <div class="warn-note footer-warn">
        Среди источников есть зашифрованный том. Чтобы копия запускалась по
        расписанию, отметьте «Запомнить» для его пароля на шаге «Источник» —
        иначе служба пропустит запланированный запуск.
      </div>
    {/if}

    <!-- footer -->
    <div class="footer">
      <div class="spacer"></div>
      <button class="btn ghost" onclick={onCancel}>Отмена</button>
      {#if mode === 'wizard'}
        {#if step > 0}<button class="btn ghost" onclick={() => (step -= 1)}>Назад</button>{/if}
        {#if step < steps.length - 1}
          <button
            class="btn primary"
            onclick={() => (step += 1)}
            disabled={stepKey === 'source' && !canLeaveSource()}
            title={stepKey === 'source' ? nextHint() : ''}
          >Дальше</button>
        {:else}
          <button class="btn green" onclick={() => finish(true)}>Создать копию</button>
        {/if}
      {:else}
        <button class="btn primary" onclick={() => finish(false)}>Сохранить</button>
      {/if}
    </div>
</div>

{#if stubMsg}<InfoModal message={stubMsg} onClose={() => (stubMsg = null)} />{/if}

<style>
  .wizard { height: 100%; background: var(--content-bg); display: flex; flex-direction: column; overflow: hidden; }
  .wz-head { padding: 22px 34px 14px; display: flex; align-items: center; gap: 14px; }
  .wz-title { font-size: 24px; font-weight: 300; color: var(--head); }
  .wz-name { margin-left: auto; display: inline-flex; align-items: center; gap: 8px; font-size: 15px; font-weight: 600; color: var(--head); white-space: nowrap; }
  .wz-name svg { flex-shrink: 0; }
  .tabs { display: flex; padding: 0 34px; border-bottom: 1px solid var(--border); }
  .tab { padding: 10px 4px 12px; margin-right: 28px; background: none; border: none; border-bottom: 3px solid transparent; color: var(--text-muted); font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; }
  .tab.active { color: var(--head); border-bottom-color: var(--primary); }
  .body { flex: 1; overflow: auto; padding: 26px 34px; }
  select.wide { width: 100%; padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: var(--card-bg); color: var(--text); }
  .link { background: none; border: none; color: var(--primary); font-size: 13px; cursor: pointer; padding: 0; }

  /* --- Paragon-style source map (step 0) --- */
  .src-wrap { max-width: 940px; margin: 0 auto; }
  .sec-head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .sec-title { font-size: 15px; font-weight: 700; color: var(--head); }
  .sec-sub { font-size: 13px; color: var(--text-muted); }
  .disks { display: flex; flex-direction: column; gap: 14px; }
  /* Kept-files category breakdown, shown under the disk map (read-only). */
  .cat-block { margin-top: 14px; padding: 14px 16px; border: 1px solid var(--border); border-radius: 6px; background: var(--card-bg); }
  .cat-head { font-size: 13px; font-weight: 600; color: var(--head); margin-bottom: 4px; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; margin-top: 10px; }
  /* Every present category stays visible: tiny slices keep 5px and the
     large ones shrink to make room, so the bar still totals 100%. */
  .catseg { min-width: 5px; }
  .cats { display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap; }
  .cat { flex: 1; min-width: 130px; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--head); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .disk-card { border: 1px solid var(--border); border-radius: 6px; overflow: hidden; background: var(--card-bg); }
  /* Stage 1: the same card, clickable as a whole. Button defaults (centred
     text, own font, native border) would undo the layout, so they go. */
  button.disk-card { display: block; width: 100%; padding: 0; text-align: left; font: inherit; color: inherit; cursor: pointer; }
  button.disk-card:hover { border-color: var(--primary); }
  button.disk-card:hover .disk-head { background: var(--hover-bg, rgba(127, 127, 127, 0.1)); }
  /* Back to the disk question — small, next to the instruction it undoes. */
  .back-disk { margin-left: auto; font-size: 12px; }
  .disk-head { width: 100%; display: flex; align-items: center; justify-content: space-between; padding: 6px 12px; background: var(--panel2); border: 0; border-bottom: 1px solid var(--border); margin: 0; font-family: inherit; text-align: left; }
  .disk-head.whole { background: rgba(45,125,210,.12); color: var(--primary); }
  .disk-head.whole .disk-model { color: var(--primary); }
  .disk-head-right { display: flex; align-items: center; gap: 10px; }
  .boot-toggle { display: flex; align-items: center; gap: 6px; font-size: 12px; font-weight: 600; color: var(--head); cursor: pointer; user-select: none; }
  .boot-toggle input { cursor: pointer; margin: 0; }
  .boot-toggle.disabled { color: var(--text-muted); cursor: not-allowed; opacity: .55; }
  .boot-toggle.disabled input { cursor: not-allowed; }
  .whole-badge { font-size: 11px; font-weight: 700; color: #fff; background: var(--primary); padding: 2px 8px; border-radius: 10px; }
  .seg.locked { cursor: default; }
  .seg.locked:hover { filter: none; }
  .disk-model { font-size: 13px; font-weight: 600; color: var(--head); }
  .disk-meta { font-size: 12px; color: var(--text-muted); }
  .disk-body { display: flex; align-items: center; gap: 14px; padding: 7px 12px; }
  .disk-ico { width: 52px; flex-shrink: 0; text-align: center; color: var(--text-muted); }
  .disk-name { font-size: 12px; color: var(--head); margin-top: 1px; }
  .bar { flex: 1; display: flex; height: 44px; border: 1px solid var(--border); border-radius: 3px; overflow: hidden; background: var(--panel2); }
  /* Smallest readable segment width: tiny partitions never shrink below this
     (so their size/FS text stays legible), larger ones grow proportionally. */
  .seg { position: relative; padding: 5px 8px; min-width: 84px; border: 0; border-right: 1px solid var(--border); margin: 0; display: flex; flex-direction: column; gap: 3px; background: rgba(45,125,210,.06); overflow: hidden; text-align: left; font-family: inherit; color: inherit; appearance: none; -webkit-appearance: none; }
  .seg:last-child { border-right: none; }
  .seg.free { min-width: 40px; align-items: center; justify-content: center; background: repeating-linear-gradient(135deg, var(--panel2) 0 6px, var(--border) 6px 7px); }
  .free-lbl { font-size: 11px; color: var(--text-muted); white-space: nowrap; }
  .seg.selectable { cursor: pointer; }
  .seg.selectable:hover { filter: brightness(0.97); }
  /* Kept in the copy as a table entry only. It looks selected because it IS in
     the copy; the cursor says it is not something to click. */
  .seg.layout-only { cursor: default; }
  /* Read-only source map ("Параметры"): the selection is shown, not editable. */
  .seg.ro { cursor: default; }
  .seg.ro:hover { filter: none; }
  .seg.selected { background: rgba(45,125,210,.16); box-shadow: inset 0 0 0 2px var(--primary); }
  /* Left-to-right "used space" fill inside a partition — a subtle shaded strip
     with a defined right edge, so occupancy is readable at a glance. */
  .usedfill { position: absolute; top: 0; bottom: 0; left: 0; z-index: 0; background: rgba(45,125,210,.14); border-right: 1px solid rgba(45,125,210,.45); pointer-events: none; }
  .seg.selected .usedfill { background: rgba(45,125,210,.24); border-right-color: rgba(45,125,210,.6); }
  .seg-top { position: relative; z-index: 1; display: flex; align-items: center; gap: 5px; color: var(--head); }
  .lock { color: var(--text-muted); flex-shrink: 0; }
  .enc { font-size: 10px; line-height: 1; flex-shrink: 0; }

  .pw-panel { margin-top: 16px; padding: 16px 18px; border: 1px solid var(--border); border-radius: 8px; background: var(--surface-2, rgba(127,127,127,.06)); }
  /* Password panel rendered directly under its disk card: tighter top margin
     and a primary-tinted border so the association with the disk is clear. */
  .pw-panel.under-disk { margin-top: 6px; margin-bottom: 4px; border-color: rgba(45,125,210,.35); background: rgba(45,125,210,.05); }
  .pw-head { display: flex; align-items: center; gap: 16px; font-size: 13px; font-weight: 700; margin-bottom: 12px; }
  .pw-shared-tgl { margin-left: auto; display: flex; align-items: center; gap: 6px; font-size: 13px; font-weight: 600; white-space: nowrap; cursor: pointer; }
  .pw-row { display: flex; align-items: center; gap: 12px; margin-bottom: 6px; }
  /* The single "one password for all volumes" prompt, set off from the
     per-volume status lines below it. */
  .pw-row.shared { padding-bottom: 10px; margin-bottom: 10px; border-bottom: 1px dashed var(--border); }
  .pw-wait { font-size: 13px; color: var(--text-muted); }
  /* Once decrypted, centre the "<метка> ✓ Расшифровано" pair. */
  .pw-row.done { justify-content: center; gap: 16px; }
  .pw-row.done .pw-vol { flex: 0 0 auto; }
  .pw-vol { flex: 0 0 auto; max-width: 240px; font-size: 13px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  /* Two lines for the 48-digit key: the question, then the answer. Everything
     it needs — a long label, a field worth typing into, "Запомнить" and the
     button — cannot share one line without pushing the button off the panel. */
  .pw-row.stacked { flex-direction: column; align-items: stretch; gap: 8px; margin-bottom: 10px; }
  .pw-ask { font-size: 13px; font-weight: 600; }
  /* The entry line proper. In the one-line layout the wrapper dissolves and its
     three children become items of `.pw-row` itself, exactly as before. */
  .pw-line { display: contents; }
  .pw-row.stacked .pw-line { display: flex; align-items: center; gap: 12px; }
  /* The field takes the slack; the checkbox and button hold the right edge. */
  .pw-row.stacked .pw-remember { margin-left: auto; }
  .pw-row.stacked + .pw-err { margin-left: 0; }
  /* One row: label + wide password field + remember + button. The field grows
     to fill (see PasswordField); label, checkbox and button keep their
     natural width. */
  .pw-remember { flex-shrink: 0; display: flex; align-items: center; gap: 6px; font-size: 13px; white-space: nowrap; }
  .pw-row .btn { flex-shrink: 0; white-space: nowrap; }
  .pw-done { font-size: 13px; font-weight: 600; color: var(--green, #2e7d32); }
  .pw-err { font-size: 12px; color: #d84a4a; font-weight: 600; margin: 0 0 6px 204px; }
  /* A volume we cannot open: a sentence in place of the password box. */
  .pw-nokey { flex: 1 1 auto; font-size: 12px; color: var(--text-muted); line-height: 1.4; }
  .pw-hint { font-size: 12px; color: var(--text-muted); margin-top: 10px; }
  /* The consequence of leaving a lock closed reads as a warning, not as
     small print: it changes what the copy contains. */
  .pw-hint.warn { color: var(--warning); }
  .pw-hint code { font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 11.5px; padding: 1px 5px; border-radius: 3px; background: rgba(127, 127, 127, 0.16); color: var(--head); }
  .btn:disabled { opacity: .5; cursor: default; }
  .seg-label { font-size: 12px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta { position: relative; z-index: 1; font-size: 11px; color: var(--head); line-height: 1.35; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta .muted { color: var(--text-muted); }
  .check { position: absolute; top: 5px; right: 6px; z-index: 2; width: 16px; height: 16px; border-radius: 3px; background: var(--primary); display: flex; align-items: center; justify-content: center; }
  .connector { display: flex; flex-direction: column; align-items: center; margin: 20px 0; }
  .connector .dash { width: 2px; height: 14px; background: repeating-linear-gradient(180deg, var(--text-muted) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 44px; height: 44px; border-radius: 50%; background: var(--green); display: flex; align-items: center; justify-content: center; box-shadow: 0 4px 14px rgba(90,176,47,.35); margin: 4px 0; }
  .dest-card { display: flex; align-items: center; gap: 14px; padding: 12px; border: 1px solid var(--border); border-radius: 6px; background: var(--card-bg); }
  .dest-fields { flex: 1; display: flex; align-items: center; gap: 12px; min-width: 0; }
  .dest-fields input { flex: 1; min-width: 0; padding: 8px 10px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; font-family: var(--font); background: var(--card-bg); color: var(--text); }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .note, .dev-note { font-size: 12px; color: var(--text-muted); margin-top: 18px; }
  .dev-note { color: #a06a10; background: rgba(251,188,5,.1); padding: 8px 12px; border-radius: 5px; }
  .warn-note { font-size: 13px; color: #8c1d1d; background: rgba(216,74,74,.1); padding: 10px 14px; border-radius: 5px; margin-top: 14px; font-weight: 600; }
  /* Same warning, docked just above the Отмена/Назад/Далее footer. */
  .footer-warn { margin: 0 34px 12px; }
  code { background: var(--panel2); padding: 1px 5px; border-radius: 3px; }
  .sched { display: flex; gap: 50px; }
  .sched-kinds { display: flex; flex-direction: column; gap: 14px; }
  .sched-right { display: flex; flex-direction: column; gap: 16px; padding-top: 2px; }
  .radio { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--head); cursor: pointer; }
  .radio.blk { margin: 10px 0; }
  .days { display: flex; border: 1px solid var(--border); border-radius: 5px; overflow: hidden; }
  .day { padding: 9px 12px; border: none; border-right: 1px solid var(--border); background: #fff; color: var(--head); font-size: 13px; cursor: pointer; font-family: var(--font); }
  .day.on { background: var(--primary); color: #fff; }
  .at { display: flex; align-items: center; gap: 10px; font-size: 14px; color: var(--head); }
  .hint-sm { font-size: 12px; color: var(--text-muted); margin-top: 8px; }
  .delay-min { display: flex; align-items: center; gap: 8px; font-size: 13px; color: var(--head); margin-left: 26px; }
  .delay-min select, .at select { padding: 3px 6px; }
  .event-opts { display: flex; flex-direction: column; gap: 12px; }
  .event-opts .dev-badge { margin-left: 8px; }
  .at input { padding: 7px 10px; border: 1px solid var(--border); border-radius: 4px; }
  .adv-schedule { margin-top: 20px; display: flex; flex-direction: column; gap: 12px; }
  .chk { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--head); cursor: pointer; }
  .field-label { font-size: 15px; font-weight: 600; color: var(--head); margin-bottom: 10px; }
  .field-label.mt { margin-top: 26px; }
  .num { width: 60px; padding: 5px 8px; border: 1px solid var(--border); border-radius: 4px; margin: 0 4px; }
  .excl-tabs { display: flex; gap: 24px; margin-top: 18px; border-bottom: 1px solid var(--border); }
  .excl-tab { padding: 8px 2px 10px; background: none; border: none; border-bottom: 2px solid transparent; color: var(--text-muted); font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; }
  .excl-tab.active { color: var(--head); border-bottom-color: var(--primary); }
  .note.sub { margin-top: 10px; }
  /* A line of prose that happens to contain a number field — reads as a
     sentence, not as a labelled input. */
  .chk-line { display: flex; align-items: center; font-size: 14px; color: var(--head); }
  /* A checkbox that only makes sense under the one above it. */
  .chk.sub { margin-left: 26px; }
  .chk:has(input:disabled) { color: var(--text-muted); }
  .note.warn { color: #a06a10; background: rgba(251,188,5,.10); border-left: 3px solid rgba(251,188,5,.55); padding: 8px 12px; border-radius: 0 4px 4px 0; }
  .boot-hint { margin-top: 10px; font-size: 12.5px; line-height: 1.5; color: #a06a10; background: rgba(251,188,5,.10); border-left: 3px solid rgba(251,188,5,.55); padding: 8px 12px; border-radius: 0 4px 4px 0; }
  .excl-list { border: 1px solid var(--border); border-radius: 5px; background: #fff; max-height: 260px; overflow: auto; margin-top: 12px; }
  .excl-row { display: flex; justify-content: space-between; align-items: center; padding: 6px 14px; font-size: 13px; color: var(--head); border-bottom: 1px solid var(--border); }
  .x { background: none; border: none; color: var(--text-muted); font-size: 18px; cursor: pointer; line-height: 1; }
  .excl-add { display: flex; gap: 10px; align-items: center; margin-top: 12px; }
  .excl-add input { flex: 1; padding: 8px 12px; border: 1px solid var(--border); border-radius: 4px; font-size: 13px; }
  .adv { display: flex; flex-direction: column; gap: 4px; }
  .adv-group { padding: 12px 0; border-bottom: 1px solid var(--border); }
  .adv-title { font-size: 13px; font-weight: 600; color: var(--primary); margin-bottom: 8px; }
  .adv-row { display: flex; align-items: center; justify-content: space-between; padding: 14px 4px; border-bottom: 1px solid var(--border); background: none; border-top: none; border-left: none; border-right: none; cursor: pointer; font-size: 14px; color: var(--head); font-family: var(--font); width: 100%; }
  .dev-badge { font-size: 11px; color: #a06a10; background: rgba(251,188,5,.15); padding: 3px 9px; border-radius: 10px; }
  .ok-badge { font-size: 11px; color: #2c5b12; background: rgba(90,176,47,.15); padding: 2px 8px; border-radius: 10px; margin-left: 6px; }
  .wz-error { padding: 8px 34px; color: #8c1d1d; font-size: 13px; background: rgba(216,74,74,.08); }
  /* .footer is global (styles/global.css) — same bar on every page. */
  .spacer { flex: 1; }
  .btn { border-radius: 4px; padding: 10px 26px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: none; }
  .btn.small { padding: 7px 14px; font-size: 13px; }
  .btn.ghost { background: none; border: 1px solid var(--border); color: var(--head); }
  .btn.primary { background: var(--primary); color: #fff; }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover { background: var(--green-hover); }
</style>
