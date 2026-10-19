<script lang="ts">
  import { pickPath } from '../lib/pickPath.svelte';
  import { api } from '../lib/api';
  import { engine } from '../lib/engineStore.svelte';
  import { media, ensureMediaMode } from '../lib/mediaStore.svelte';
  import { saveJob, runJob, decryptVolume, decryptedVolumes } from '../lib/jobStore.svelte';
  import { formatBytes } from '../lib/format';
  import { diskSegments, msrKept } from '../lib/diskLayout';
  // Per-source token of the file-name preview — core's source_token, mirrored.
  import { sourceToken } from '../lib/sourceToken';
  import PasswordField from '../components/PasswordField.svelte';
  import ArchiveKeyModal from '../components/ArchiveKeyModal.svelte';
  import type { Job, DiskInfo, PartitionInfo } from '../lib/types';
  import { m } from '../lib/paraglide/messages';
  import { examplePath, joinPath } from '../lib/paths';
  import { caps, ensureCapabilities, dropUnsupported, canChain, offersWholeDisk } from '../lib/capabilitiesStore.svelte';
  import { isWindows } from '../lib/platformStore.svelte';
  import { diskCaption, diskTableText } from '../lib/diskNames';
  import type { DestinationCheck } from '../lib/types';

  interface Props {
    mode: 'wizard' | 'edit';
    // Shared draft owned by the parent (BackupSection) so the name is also
    // editable inline in the jobs list. Cancel discards it in the parent.
    job: Job;
    onCancel: () => void;
    onDone: (job: Job) => void;
    /** The draft was cloned from an encrypted job: the format starts at OBK.
     *  The key is not cloned — a job has its own, so the password is asked. */
    startEncrypted?: boolean;
  }
  let { mode, job, onCancel, onDone, startEncrypted = false }: Props = $props();

  let step = $state(0);
  // What the service's engine carries out: anything else is not offered, since
  // a job holding it is refused at run time (Linux, for now — see
  // `own_backup_core::capabilities`, the table the run refuses by).
  $effect(() => {
    void ensureCapabilities();
  });
  const cap = $derived(caps.value);
  /** Whole-disk jobs are built here only where a disk is named by number —
   *  see `offersWholeDisk`. */
  const wholeDisk = $derived(offersWholeDisk(cap));
  // A new draft (template or clone) starts as a job this engine runs: the
  // template turns exclusions on, a clone may carry a chain. A saved job is
  // left as it is until it is saved again (`finish`).
  $effect(() => {
    if (mode === 'wizard') dropUnsupported(job, cap);
  });
  // A function, not a string: the text is drawn in whatever language the
  // window speaks when it is drawn. The service's own errors are wrapped as is.
  let error: (() => string) | null = $state(null);

  // --- Archive encryption (§7.5) ---
  // A job that already has a key stays encrypted: the copies written with it
  // cannot be un-encrypted by a setting, and replacing the key is a separate
  // operation the service refuses from here. So the switch is only ever an
  // on-switch, and it is locked once it is on.
  const alreadyEncrypted = $derived(!!job.archive_encryption);
  // svelte-ignore state_referenced_locally
  let wantEncryption = $state(!!job.archive_encryption || startEncrypted);
  /** This engine writes chains in the chosen container (Linux: not VHDX). */
  const chainOk = $derived(canChain(cap, wantEncryption ? 'obk' : job.image_format));
  /** The containers it does write chains in, as the format list names them;
   *  OBK only where it is offered at all. */
  const chainFormatNames = $derived(
    cap.chain_formats
      .filter((f) => f !== 'obk' || cap.archive_encryption)
      .map((f) => (f === 'vhdx' ? 'VHDX' : f === 'obk' ? 'OBK' : f))
      .join(', '),
  );
  let encPassword = $state('');
  let encPasswordAgain = $state('');
  /** Matches the service's own floor; a slow KDF does not rescue a short one. */
  const MIN_ARCHIVE_PASSWORD = 8;
  const encryptionReady = $derived(
    encPassword.length >= MIN_ARCHIVE_PASSWORD && encPassword === encPasswordAgain,
  );
  /** What the archive password still lacks, or '' once it is ready. */
  function encryptionHint(): string {
    return encryptionProblem()?.() ?? '';
  }
  // The same, as a message rather than a string — for `error`, which keeps it
  // and draws it in the language of the moment.
  function encryptionProblem(): (() => string) | null {
    if (!wantEncryption || alreadyEncrypted || encryptionReady) return null;
    return encPassword.length < MIN_ARCHIVE_PASSWORD
      ? () => m.wiz_enc_hint_short({ count: MIN_ARCHIVE_PASSWORD })
      : m.wiz_enc_hint_mismatch;
  }
  /** The format is fixed once copies exist — a chain cannot change container
   *  midway, and the runner refuses a job whose format no longer matches its
   *  copies. Encryption is a format (.obk), so it is fixed the same way. */
  const formatLocked = $derived((mode === 'edit' && job.last_run !== null) || alreadyEncrypted);
  /** OBK is our sealed container, qcow2 underneath — the service sets the same
   *  when it commits the key. */
  function setFormat(v: string) {
    if (formatLocked) return;
    wantEncryption = v === 'obk';
    job.image_format = v === 'vhdx' ? 'vhdx' : 'qcow2';
    // A chain this engine does not write in the new container (Linux: VHDX)
    // falls back to full copies here, visibly, not at run time.
    if (job.scheme.method !== 'full' && !canChain(cap, v)) {
      job.scheme.method = 'full';
      job.scheme.rolling_incrementals = null;
      job.scheme.only_incremental = false;
    }
    error = null;
  }
  /** The settings' zstd level, which a job without its own follows. Read once;
   *  only shown, so a stale value costs a wrong number on screen, nothing more. */
  let defaultLevel = $state(3);
  $effect(() => {
    api.getConfig().then(
      (c) => {
        const v = Number(c.default_compression_level);
        if (v >= 1 && v <= 19) defaultLevel = v;
      },
      () => {},
    );
  });
  /** Levels offered for a job. The ends and the default get a word; the rest
   *  are just numbers. */
  const LEVEL_HINT: Record<number, () => string> = {
    1: m.wiz_level_fastest,
    3: m.wiz_level_balance,
    19: m.wiz_level_smallest,
  };
  let keyModalOpen = $state(false);
  /** Set when the save was interrupted to ask for a key: run the job after. */
  let runAfterKey = $state(false);

  // On the boot medium the schedule step is dropped rather than disabled: the
  // session ends at power-off, so any schedule set here is a promise nothing
  // can keep. The job is saved with kind='none' (see buildJob), which is what
  // "run it now, once" means everywhere else in the program.
  const steps = $derived(
    [
      { key: 'source', label: m.wiz_tab_source() },
      { key: 'schedule', label: m.wiz_tab_schedule() },
      { key: 'scheme', label: m.wiz_tab_scheme() },
      { key: 'exclusions', label: m.wiz_tab_exclusions() },
      { key: 'advanced', label: m.wiz_tab_advanced() },
    ].filter((s) => !(media.active && s.key === 'schedule')),
  );
  // The tab list is filtered (no schedule on the boot medium), so the body must
  // be keyed by the step's KEY, never by its index: on the medium index 2 said
  // «Схема» in the tab bar and drew the exclusions, and «Дополнительно» — the
  // last index — could not be reached at all.
  const stepKey = $derived(steps[step]?.key ?? 'source');
  /** Open a step by its key. A step missing from the list stays put rather
   *  than leaving the wizard on no step at all. */
  function goToStep(key: string) {
    const at = steps.findIndex((s) => s.key === key);
    if (at >= 0) step = at;
  }
  // A step can also vanish under us (the medium check answers after the first
  // render), so never leave `step` pointing past the end of the list.
  $effect(() => {
    if (step > steps.length - 1) step = steps.length - 1;
  });

  const weekdays: [string, () => string][] = [
    ['mon', m.wiz_day_mon], ['tue', m.wiz_day_tue], ['wed', m.wiz_day_wed], ['thu', m.wiz_day_thu],
    ['fri', m.wiz_day_fri], ['sat', m.wiz_day_sat], ['sun', m.wiz_day_sun],
  ];
  const scheduleKinds: [string, () => string][] = [
    ['daily', m.wiz_sched_daily], ['weekly', m.wiz_sched_weekly], ['monthly', m.wiz_sched_monthly],
    ['event', m.wiz_sched_event], ['none', m.wiz_sched_none],
  ];

  // Event triggers for kind='event'. Third field = badge text ('' = works).
  // Only those the service's scheduler fires: logon/logoff come from the
  // Windows service control handler and do not exist on Linux.
  const allEventTriggers: [string, () => string, string][] = [
    ['dest_connected', m.wiz_evt_dest_connected, ''],
    ['logon', m.wiz_evt_logon, ''],
    ['logoff', m.wiz_evt_logoff, ''],
    ['startup', m.wiz_evt_startup, ''],
  ];
  const eventTriggers = $derived(allEventTriggers.filter(([v]) => cap.schedule_events.includes(v)));

  // «После подключения внешнего устройства» (usb_connect) was never wired;
  // what it was for — the backup drive being plugged in — is dest_connected.
  // The service reads both the same; the radio shows the new one.
  $effect(() => {
    if (job.schedule.event_trigger === 'usb_connect') job.schedule.event_trigger = 'dest_connected';
  });

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
    if (media.active) {
      job.schedule.kind = 'none';
      if (job.verification) job.verification.schedule.kind = 'none';
    }
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

  // Openable device for a partition (letter "C:" or a `\\?\Volume{GUID}` path
  // for letterless system/hidden volumes). Empty ⇒ not backable ⇒ not clickable.
  function partDevice(p: PartitionInfo): string | null {
    return p.volume_device ? p.volume_device : null;
  }
  /** Partitions the system needs but never mounts: no filesystem, no letter, no
   *  label. Their GPT type GUID is what they are, so say it instead of drawing
   *  a nameless "Раздел 128 MiB" that reads like a mistake. */
  const GPT_TYPE_NAMES: Record<string, () => string> = {
    'C12A7328-F81F-11D2-BA4B-00A0C93EC93B': () => 'EFI (ESP)',
    'E3C9E316-0B5C-4DB8-817D-F92DF00215AE': m.wiz_gpt_msr,
    'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC': m.wiz_gpt_winre,
    '0FC63DAF-8483-4772-8E79-3D69D8477DE4': () => 'Linux',
    '0657FD6D-A4AB-43C4-84E5-0933C84B4F4F': () => 'Linux swap',
    'E6D6D379-F507-44C2-A23C-238F2A3DF928': () => 'Linux LVM',
    'CA7D7CCB-63ED-4C53-861C-1742536059CC': () => 'LUKS',
    '21686148-6449-6E6F-744E-656564454649': () => 'BIOS boot',
  };
  function gptTypeName(p: PartitionInfo): string {
    const guid = (p.gpt_type_guid || '').replace(/[{}]/g, '').toUpperCase();
    return GPT_TYPE_NAMES[guid]?.() ?? '';
  }

  function partLabel(p: PartitionInfo): string {
    // A decrypted encrypted volume shows its real inner label.
    const dec = p.volume_device ? decryptedVolumes[p.volume_device] : undefined;
    if (dec?.label) return dec.label;
    if (p.volume_letter) {
      const name = p.volume_label || fsLabel(p.filesystem) || m.wiz_volume_fallback();
      return `${p.volume_letter}: ${name}`;
    }
    // Letterless (EFI / ext4 / hidden): its on-disk label, else the filesystem,
    // else what its GPT type says it is.
    return p.volume_label || fsLabel(p.filesystem) || gptTypeName(p) || m.wiz_partition_fallback();
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
    if (partUsed(p) > 0) t += ' ' + m.wiz_seg_used({ size: formatBytes(partUsed(p)) });
    const fs = partFs(p);
    if (fs) t += ` ${fs}`;
    // Linux: the name the system's own tools show (`/dev/nvme0n1p2`).
    if (p.kernel_device) t += ` · ${p.kernel_device}`;
    return t;
  }

  // Physical drive number from the device id (`\\.\PhysicalDrive3` → 3).
  function diskNumber(d: DiskInfo): number {
    const m = d.device_id.match(/(\d+)\s*$/);
    return m ? parseInt(m[1], 10) : 0;
  }
  // ---- What belongs to booting (rewritten from scratch 2026-08-21) ----------
  //
  // The old switch answered this by taking EVERY volume of the disk, on the
  // reasoning that a Linux system cannot be recognised from Windows and one
  // partition too many only costs space. On a real disk that put a 347 GiB NTFS
  // data volume into a "bootable copy", which is not what the words say and not
  // what the user wants backed up.
  //
  // So the question is asked properly, of the partition TYPE — the one thing
  // written on the disk itself, by the installer, in a form both worlds agree
  // on. `has_os` (a Windows kernel file) stays as a second opinion; it is the
  // only signal that needs a mounted filesystem, and it is never the only one.

  /** EFI System Partition — the loader lives here, on Windows and on Linux. */
  const GPT_ESP = 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B';
  /** Microsoft Reserved: Windows expects the slot, there is no filesystem in it.
   *  A whole-disk copy that carries Windows keeps its GPT entry
   *  (`DiskLayout::retain_for_backup`) — so the map must show it as part of
   *  that copy, not as something left out. */
  const MSR_GUID = 'E3C9E316-0B5C-4DB8-817D-F92DF00215AE';
  /** Types that ARE the boot chain, whoever installed the system. */
  const BOOT_TYPES = new Set([
    GPT_ESP,
    MSR_GUID,
    'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC', // Windows Recovery (WinRE)
    'BC13C2FF-59E6-4262-A352-B275FD6F7172', // Linux extended boot (XBOOTLDR, /boot)
    '4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709', // Linux root, x86-64
    '44479540-F297-41B2-9AF7-D131D5F0458A', // Linux root, x86
    'B921B045-1DF0-41C3-AF44-4C6F280D3FAE', // Linux root, arm64
    '8484680C-9521-48C6-9C11-B0720656F69E', // Linux /usr, x86-64
    'E6D6D379-F507-44C2-A23C-238F2A3DF928', // Linux LVM — the root usually lives inside
  ]);
  /** Filesystems a Linux system is installed on. A partition carrying one is
   *  part of the system unless something says otherwise — Windows data lives on
   *  NTFS/exFAT, never on these. */
  const LINUX_FS = new Set(['ext2', 'ext3', 'ext4', 'xfs', 'btrfs', 'f2fs']);

  function typeGuid(p: PartitionInfo): string {
    return (p.gpt_type_guid || '').replace(/[{}]/g, '').toUpperCase();
  }
  function fsName(p: PartitionInfo): string {
    return String(p.filesystem ?? '').toLowerCase();
  }

  /** Why a partition is (not) part of the boot chain. `locked` is the honest
   *  third answer: an encrypted volume cannot be judged until it is open, so it
   *  is neither included nor dismissed — its password is asked for instead. */
  type BootRole = 'boot' | 'data' | 'locked';
  function bootRole(p: PartitionInfo): BootRole {
    // Sealed containers first: nothing inside is readable, so nothing about
    // them is decidable. Judged again the moment they are unlocked.
    if (p.is_encrypted && !decryptedVolumes[partDevice(p) ?? '']) return 'locked';
    if (BOOT_TYPES.has(typeGuid(p))) return 'boot';
    // A running Windows, or a Windows found by its kernel file. Both need a
    // mounted filesystem, which is why they cannot carry this alone.
    if (p.is_system || p.has_os) return 'boot';
    // The generic "Linux filesystem" type (0FC63DAF…) is what Ubuntu writes for
    // the root — and also for a data partition, so the type alone decides
    // nothing, and neither does "it is ext4". What DOES decide is the mount
    // point the filesystem itself recorded: a root says "/", a data volume says
    // "/home" or "/mnt/…". Read straight from the superblock, no mounting.
    const mnt = lastMounted(p);
    if (mnt) return LINUX_SYSTEM_MOUNTS.has(mnt) ? 'boot' : 'data';
    // Never mounted, or not an ext filesystem (btrfs and xfs keep no such
    // record). Falling back to "a Linux filesystem is part of the system" errs
    // towards including it: a copy with one partition too many costs space, a
    // copy missing the root does not boot.
    if (LINUX_FS.has(decryptedFs(p) || fsName(p))) return 'boot';
    return 'data';
  }
  /** Mount points that make a Linux volume part of the system. `/` is the root;
   *  a separate `/boot`, `/boot/efi` or `/usr` is just as load-bearing. */
  const LINUX_SYSTEM_MOUNTS = new Set(['/', '/boot', '/boot/efi', '/usr', '/etc', '/var']);
  /** What the filesystem says about its own last mount point — from the
   *  unlocked view when there is one, else from the disk inventory. */
  function lastMounted(p: PartitionInfo): string {
    const dev = partDevice(p);
    const dec = dev ? decryptedVolumes[dev]?.linux_last_mounted : '';
    return (dec || p.linux_last_mounted || '').trim();
  }
  /** Filesystem seen INSIDE an unlocked container, if it has been opened. */
  function decryptedFs(p: PartitionInfo): string {
    const dev = partDevice(p);
    return dev ? String(decryptedVolumes[dev]?.filesystem ?? '').toLowerCase() : '';
  }
  /** Every partition of the disk that the boot chain needs, as judged right
   *  now — encrypted ones only once they are open. */
  function bootParts(d: DiskInfo): PartitionInfo[] {
    return d.partitions.filter((p) => bootRole(p) === 'boot');
  }
  /** A disk worth offering the switch for: something on it boots. An encrypted
   *  volume counts — it may well be the system, and that is exactly the case
   *  the switch has to handle. */
  function isBootableDisk(d: DiskInfo): boolean {
    return d.partitions.some((p) => bootRole(p) !== 'data');
  }
  /** Is this disk's layout captured — i.e. does the job write ONE image of it?
   *  True for every disk a new job picks; false for jobs saved before that
   *  became unconditional, which still hold one image per volume. */
  function isDiskCaptured(d: DiskInfo): boolean {
    return job.disk_source != null && job.disk_source.disk_number === diskNumber(d);
  }
  function layoutOnlyParts(d: DiskInfo): number[] {
    // Keyed on the captured layout, not on bootability: `retain_for_backup`
    // keeps the MSR slot in a non-bootable whole-disk copy too, so one must
    // show it as included.
    //
    // But only once there IS a copy — something on the disk chosen. Picking
    // the disk alone selects nothing, and a map that opened with the MSR
    // already ticked (and no way to untick it) read as a selection made for
    // the user (2026-09-17). Every partition starts unselected.
    //
    // And only with Windows in it (`msrKept`): D: alone is not Windows.
    if (!isDiskCaptured(d) || !diskHasSelection(d)) return [];
    return msrKept(d, (p) => {
      const dev = partDevice(p);
      return !!dev && job.sources.includes(dev);
    });
  }
  // The disk a selected source device lives on (matched by volume device).
  function diskOfDevice(dev: string): DiskInfo | null {
    return engine.disks.find((d) => d.partitions.some((p) => partDevice(p) === dev)) ?? null;
  }
  /** Disks whose "Загрузочная копия" switch the user turned ON.
   *
   *  STORED, not derived from the selection. Deriving it was what made the
   *  switch unusable with encryption: an encrypted root cannot be selected
   *  until it is open, so a derived switch reads "off" exactly while it is
   *  waiting for the password it asked for, and turns itself on and off under
   *  the user's hand as volumes unlock. */
  let bootCopyOn = $state<string[]>([]);
  function isBootableCopy(d: DiskInfo): boolean {
    // A saved job cannot be edited, so there is no switch to read — say what
    // the copy actually holds.
    if (sourceReadonly) {
      const need = bootParts(d);
      return need.length > 0 && need.every((p) => {
        const dev = partDevice(p);
        return !!dev && job.sources.includes(dev);
      });
    }
    return bootCopyOn.includes(d.device_id);
  }
  // While the switch is on, the boot chain stays in: a "загрузочная копия"
  // missing its ESP is a copy that will not boot, and there is a switch for
  // saying that — turning this one off.
  function isLockedSource(dev: string): boolean {
    const d = diskOfDevice(dev);
    if (!d || !isBootableCopy(d)) return false;
    const p = d.partitions.find((pp) => partDevice(pp) === dev);
    return !!p && bootRole(p) === 'boot';
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
    // An engine without whole-disk copies writes one image per volume; a
    // `disk_source` would only get the job refused.
    if (!wholeDisk) {
      job.disk_source = null;
      return;
    }
    job.disk_source = {
      disk_number: diskNumber(d),
      disk_model: d.model || d.device_id,
      parts: [],
      bootable: isBootableCopy(d),
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
    { key: 'images', name: m.cat_images, color: '#5ab02f' },
    { key: 'video', name: m.cat_video, color: '#d8c81e' },
    { key: 'audio', name: m.cat_audio, color: '#e07a2f' },
    { key: 'documents', name: m.cat_documents, color: '#8b5cf6' },
    { key: 'windows', name: m.cat_windows, color: '#4f9be8' },
    { key: 'linux', name: m.cat_linux, color: '#14b8a6' },
    { key: 'other', name: m.cat_other, color: '#9aa0ad' },
  ] as const;
  /** «Исключено» — the eighth category colour, the same in both themes as
   *  the other seven (design: CATS). */
  const EXCLUDED_COLOR = '#c0463f';
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
    const out: { name: () => string; color: string; bytes: number; pct: number }[] = catData.map((c) => ({
      name: c.name,
      color: c.color,
      bytes: c.bytes,
      pct: (c.bytes / total) * 100,
    }));
    if (ex > 0) out.push({ name: m.cat_excluded, color: EXCLUDED_COLOR, bytes: ex, pct: (ex / total) * 100 });
    return out;
  });

  // Keep disk_source.parts in sync with the currently-selected backable
  // partitions of a disk (behaviour B: only kept volumes stay in the layout).
  function syncDiskSourceParts(d: DiskInfo) {
    if (!job.disk_source || job.disk_source.disk_number !== diskNumber(d)) return;
    const parts = d.partitions
      .filter((p) => { const dev = partDevice(p); return !!dev && job.sources.includes(dev); })
      .map((p) => ({ part_index: p.index, device: partDevice(p)!, filesystem: p.filesystem }));
    // The switch is re-read here as well as at capture: this runs on every
    // change to the selection, including the one `toggleBootableCopy` makes,
    // so it is the point where turning the switch OFF actually reaches the job.
    job.disk_source = { ...job.disk_source, parts, bootable: isBootableCopy(d) };
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

  /** Add one partition to the selection, remembering that the SWITCH put it
   *  there and not the user — turning the switch off takes back exactly these. */
  function includeForBoot(d: DiskInfo, p: PartitionInfo) {
    const dev = partDevice(p);
    if (!dev || job.sources.includes(dev)) return;
    autoIncluded.add(dev);
    job.sources = [...job.sources, dev];
    job.source_labels = [...job.source_labels, decryptedVolumes[dev]?.label || partLabel(p)];
    job.source = job.sources[0] ?? '';
    job.source_label = job.source_labels[0] ?? '';
    syncDiskSourceParts(d);
  }

  /** Turn "Загрузочная копия диска" on or off.
   *
   *  ON selects the boot chain and nothing else — data volumes are left exactly
   *  as the user had them. An encrypted volume is NOT selected: locked, it
   *  cannot be told from a data volume, so the password panel asks for it and
   *  the effect below picks it up if what comes out is part of the system.
   *
   *  OFF gives back only what the switch took, so a data volume ticked by hand
   *  survives the switch being flicked twice. */
  function toggleBootableCopy(d: DiskInfo) {
    if (sourceReadonly) return; // fixed source on the "Параметры" screen
    if (!wholeDisk) return; // a bootable copy IS a whole-disk copy
    if (!isBootableDisk(d)) return; // nothing on this disk boots
    if (isBootableCopy(d)) {
      bootCopyOn = bootCopyOn.filter((id) => id !== d.device_id);
      for (const p of d.partitions) {
        const dev = partDevice(p);
        if (!dev || !autoIncluded.has(dev)) continue;
        autoIncluded.delete(dev);
        const i = job.sources.indexOf(dev);
        if (i < 0) continue;
        job.sources = job.sources.filter((x) => x !== dev);
        job.source_labels = job.source_labels.filter((_, idx) => idx !== i);
      }
      job.source = job.sources[0] ?? '';
      job.source_label = job.source_labels[0] ?? '';
      syncDiskSourceParts(d);
      return;
    }
    bootCopyOn = [...bootCopyOn, d.device_id];
    captureDiskSource(d);
    for (const p of bootParts(d)) includeForBoot(d, p);
  }

  async function browseDest() {
    try {
      const picked = await pickPath({ directory: true, title: m.wiz_dest_pick_title() });
      if (typeof picked === 'string') job.dest_folder = picked;
    } catch (e) {
      error = () => String(e);
    }
  }

  // Exclusions — two OS-specific lists behind a Windows / Linux tab. The engine
  // applies Windows masks to NTFS/ReFS/FAT partitions and Linux masks to
  // ext/btrfs/xfs ones (Linux-FS enforcement is a follow-up).
  let exclTab = $state<'windows' | 'linux'>(isWindows() ? 'windows' : 'linux');
  /** The user picked a tab: stop choosing it for them. */
  let exclTabPicked = $state(false);
  function pickExclTab(t: 'windows' | 'linux') {
    exclTab = t;
    exclTabPicked = true;
  }
  /** The tab whose masks apply to what is being copied: Linux for ext/XFS/
   *  btrfs sources, Windows for NTFS/FAT ones; the window's own system when
   *  the sources say nothing (or both). */
  function preferredExclTab(): 'windows' | 'linux' {
    const fss = job.sources.map((dev) => {
      const p = findPart(dev);
      if (!p) return '';
      return decryptedFs(p) || fsName(p);
    });
    const linux = fss.some((f) => LINUX_FS.has(f));
    const windows = fss.some((f) => ['ntfs', 're_fs', 'fat32', 'ex_fat'].includes(f));
    if (linux && !windows) return 'linux';
    if (windows && !linux) return 'windows';
    return isWindows() ? 'windows' : 'linux';
  }
  $effect(() => {
    if (stepKey === 'exclusions' && !exclTabPicked) exclTab = preferredExclTab();
  });
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

  // The service's own answer to "is the destination on a source?" — the check
  // the save makes, by the ranges volumes occupy, so a letterless source and a
  // Linux mount are seen too. Asked as the folder is typed, a moment after the
  // last key.
  let destCheck = $state<DestinationCheck | null>(null);
  let destCheckSerial = 0;
  $effect(() => {
    const sources = [...job.sources];
    const folder = job.dest_folder.trim();
    const serial = ++destCheckSerial;
    if (sources.length === 0 || !folder) {
      destCheck = null;
      return;
    }
    const timer = setTimeout(() => {
      api.checkDestination(sources, folder).then(
        (r) => {
          if (serial === destCheckSerial) destCheck = r;
        },
        () => {
          // An older service without the check: the save still makes it.
          if (serial === destCheckSerial) destCheck = null;
        },
      );
    }, 400);
    return () => clearTimeout(timer);
  });
  /** The source the destination sits on, as the service named it. */
  const destOnSource = $derived(
    destCheck?.placement === 'on_source' && !collisionLetter ? destCheck.source : null,
  );

  // Representative timestamp for the file-name preview (fixed at open — the
  // real one is stamped when the backup actually runs).
  const pad2 = (n: number) => String(n).padStart(2, '0');
  const previewStamp = (() => {
    const d = new Date();
    return `${d.getFullYear()}${pad2(d.getMonth() + 1)}${pad2(d.getDate())}-${pad2(d.getHours())}${pad2(d.getMinutes())}${pad2(d.getSeconds())}`;
  })();

  /** Имя файла из имени задания — ровно то же правило, что в движке
   *  (`common::file_name::safe_file_stem`): пробел становится `_`, запрещённый
   *  в имени файла знак ВЫБРАСЫВАЕТСЯ, остальное остаётся как набрано. Если
   *  здесь и там правила разойдутся, показанный путь будет враньём. */
  function safeFileStem(name: string, fallback: string): string {
    const out = (name ?? '')
      .replace(/^[\s.]+|[\s.]+$/g, '')
      .split('')
      .map((c) => (c === ' ' ? '_' : /[<>:"/\\|?*\u0000-\u001f]/.test(c) ? '' : c))
      .join('');
    if (!out) return fallback;
    const reserved = /^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/i;
    return reserved.test(out) ? out + '_' : out;
  }

  // Full path of the file the first run will produce, so the user sees the
  // concrete auto-generated name. Mirrors the engine's naming: a whole-disk
  // job writes `<имя>_disk<N>_<тип>_<дата>.<ext>` (core::perform_disk_backup),
  // an older per-volume one `<имя>_<том>_…`. The <тип> is shown as "full" —
  // the real type and date are decided at run time.
  //
  // The extension follows the format: it was always ".vhdx", so a qcow2 or an
  // encrypted job showed a file name it would never write (2026-09-26).
  const previewExt = $derived(wantEncryption ? 'obk' : job.image_format === 'qcow2' ? 'qcow2' : 'vhdx');
  const previewPath = $derived.by(() => {
    const folder = job.dest_folder.trim();
    if (!folder || job.sources.length === 0) return '';
    const safe = safeFileStem(job.name, 'backup');
    const token = job.disk_source ? `disk${job.disk_source.disk_number}` : sourceToken(job.sources[0]);
    return joinPath(folder, `${safe}_${token}_full_${previewStamp}.${previewExt}`);
  });

  // Universal "folder + file name" field (2-in-1). At rest it shows the full
  // generated path so the whole name is visible; while focused it collapses to
  // the editable "folder\<имя>", and BOTH the folder and the base name can be
  // typed freely (last path segment = base name → job.name, the rest = folder →
  // job.dest_folder). The engine still appends _<диск>_<тип>_<дата>.<ext>.
  let destFocused = $state(false);
  let pathText = $state('');

  function editablePath(): string {
    const folder = job.dest_folder.trim();
    const name = job.name.trim();
    if (!folder && !name) return '';
    return joinPath(folder, name);
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
      error = m.wiz_err_no_name;
      step = 0;
      return false;
    }
    if (job.sources.length === 0) {
      error = diskChosen ? m.wiz_err_no_volume : m.wiz_err_no_disk;
      step = 0;
      return false;
    }
    if (!job.dest_folder.trim()) {
      error = m.wiz_err_no_dest;
      step = 0;
      return false;
    }
    if (collisionLetter) {
      const letter = collisionLetter;
      error = () => m.wiz_err_same_disk({ letter });
      step = 0;
      return false;
    }
    if (destOnSource) {
      const source = destOnSource;
      error = () => m.wiz_dest_on_source({ source });
      step = 0;
      return false;
    }
    if (job.schedule.kind === 'event' && !cap.schedule_events.includes(job.schedule.event_trigger)) {
      error = m.wiz_err_event_unsupported;
      goToStep('schedule');
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
    /// Only the 48-digit recovery key opens it — the service's answer, from
    /// the protectors' codes. `undefined` from a service older than the field.
    recoveryKeyOnly: boolean | undefined;
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
          recoveryKeyOnly: p.recovery_key_only,
        };
      });
  }
  // A volume whose only typeable way in is the 48-digit key. Windows does not
  // allow a passphrase protector on the system volume, so this is the normal
  // state of C: — and asking it for "пароль" is asking for something that does
  // not exist on that volume.
  function needsRecoveryKey(e: EncryptedVolume): boolean {
    if (e.recoveryKeyOnly !== undefined) return e.recoveryKeyOnly;
    // A service from before the flag names the protectors in Russian, always.
    return (
      !e.methods.includes('пароль') &&
      e.methods.some((m) => m.startsWith('ключ восстановления'))
    );
  }
  // What to ask this volume for, in the words of its own metadata.
  function promptFor(e: EncryptedVolume): string {
    return needsRecoveryKey(e) ? m.wiz_rk_placeholder() : m.wiz_pw_placeholder();
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
  function normalizeRecoveryKey(text: string): { key: string } | { error: () => string } {
    const digits = text.replace(/\D/g, '');
    if (digits.length !== 48) {
      const count = digits.length;
      return {
        error: () => m.wiz_rk_wrong_length({ count }),
      };
    }
    const groups = digits.match(/.{6}/g) as string[];
    const bad = groups.findIndex((g) => Number(g) % 11 !== 0 || Number(g) / 11 > 65535);
    if (bad >= 0) {
      const group = groups[bad];
      return { error: () => m.wiz_rk_bad_group({ n: bad + 1, group }) };
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
  let pwFormat = $state<Record<string, () => string>>({});
  let decrypting = $state<string | null>(null);

  // Keep the "needs a password" job flag in sync with the selection.
  $effect(() => {
    job.requires_password = encryptedSelected().length > 0;
  });

  // A volume that was unlocked while the switch is on gets judged the moment it
  // becomes readable, and joins the copy only if it turns out to be part of the
  // system. That is the whole reason the password is asked for: locked, the
  // volume is a black box, and the old code resolved that by including EVERY
  // unlocked volume — so a data volume the user opened just to look inside was
  // pulled into a "bootable copy" it has no business being in.
  //
  // Reactive, so the order of actions does not matter (unlock-then-switch or
  // switch-then-unlock). Converges: once a device is in `sources` — or has been
  // taken back off by hand — `autoIncluded` stops it being added again.
  //
  // Leaving a volume locked is a valid answer, and it means "no":
  // `retain_for_backup` drops it from the layout, so its space comes back free
  // on restore. Both halves are said out loud in the password panel.
  const autoIncluded = new Set<string>();
  $effect(() => {
    if (sourceReadonly) return; // the source of a saved job is fixed
    for (const d of engine.disks) {
      if (!isBootableCopy(d)) continue;
      for (const p of d.partitions) {
        const dev = partDevice(p);
        if (!dev || job.sources.includes(dev) || autoIncluded.has(dev)) continue;
        if (!decryptedVolumes[dev]) continue; // still sealed — nothing to judge
        if (bootRole(p) !== 'boot') continue; // opened, and it is data
        includeForBoot(d, p);
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
      decryptedVolumes[device] = {
        label: r.label,
        used_bytes: r.used_bytes,
        filesystem: r.filesystem,
        linux_last_mounted: r.linux_last_mounted ?? '',
      };

      // Carry the volume label into the job (file name + display), like C:/D:.
      const i = job.sources.indexOf(device);
      if (i >= 0 && r.label) {
        job.source_labels[i] = r.label;
        if (i === 0) job.source_label = r.label;
      }
      job.source = job.sources[0] ?? '';
      job.source_label = job.source_labels[0] ?? '';
    } catch (e) {
      error = () => String(e);
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
  /** Pure loader slots: they get a system onto its feet but are not the system.
   *  A copy holding only these boots into "no operating system found". */
  const LOADER_ONLY = new Set([GPT_ESP, MSR_GUID, 'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC']);
  /** Is anything that could BE the system already in the copy of this disk? */
  function systemIncluded(d: DiskInfo): boolean {
    return d.partitions.some((p) => {
      const dev = partDevice(p);
      if (!dev || !job.sources.includes(dev)) return false;
      return bootRole(p) === 'boot' && !LOADER_ONLY.has(typeGuid(p));
    });
  }
  function encryptedBlocking(): EncryptedVolume[] {
    return engine.disks.flatMap((d) => {
      const shown = encryptedForDisk(d);
      // Anything this copy will actually read has to be open — that is what the
      // password is for.
      const selected = shown.filter(({ device }) => job.sources.includes(device));
      // And on a disk asked for a BOOTABLE copy where nothing else looks like a
      // system, a locked volume is the last candidate for being it. Letting that
      // through is how a 954 GiB disk once produced a 43 MB "bootable copy" of
      // its EFI partition alone, reported as success. When the system IS already
      // in (Windows found, a Linux root selected), a locked volume is data and
      // nobody has to type anything for it.
      if (!isBootableCopy(d) || systemIncluded(d)) return selected;
      const devs = new Set(selected.map((e) => e.device));
      return [...selected, ...shown.filter((e) => !devs.has(e.device))];
    });
  }
  function allEncryptedDecrypted(): boolean {
    return encryptedBlocking().every(({ device }) => decryptedVolumes[device]);
  }

  // The source step is complete: a source is chosen, any encrypted volume is
  // unlocked, and a destination folder is set. "Далее" stays disabled until then.
  function canLeaveSource(): boolean {
    return job.sources.length > 0 && allEncryptedDecrypted() && !!job.dest_folder.trim() && !destOnSource;
  }
  function nextHint(): (() => string) | null {
    // The hint names the question the screen is actually asking: at stage 1 it
    // is the disk, and "выберите том" would point at volumes that are not drawn
    // yet.
    if (!diskChosen) return m.wiz_hint_pick_disk;
    if (job.sources.length === 0) return m.wiz_hint_pick_volume;
    // Name the volume: "расшифруйте зашифрованный том" is no help at all when
    // the screen shows two of them and one is already unlocked.
    const locked = encryptedBlocking().find(({ device }) => !decryptedVolumes[device]);
    // A volume with nothing we can type is not waiting for a password: telling
    // the user to enter one is a dead end, since no field is shown for it.
    if (locked) {
      const label = locked.label;
      if (!locked.unlockable) return () => m.wiz_hint_cannot_open({ label });
      return needsRecoveryKey(locked)
        ? () => m.wiz_hint_enter_rk({ label })
        : () => m.wiz_hint_enter_pw({ label });
    }
    if (!job.dest_folder.trim()) return m.wiz_hint_pick_dest;
    if (destOnSource) {
      const source = destOnSource;
      return () => m.wiz_dest_on_source({ source });
    }
    return null;
  }

  async function finish(run: boolean) {
    // What this engine would refuse the job for goes before it is saved: the
    // steps do not offer it, but a cloned or older job can still carry it.
    dropUnsupported(job, cap);
    if (!cap.archive_encryption && !alreadyEncrypted) wantEncryption = false;
    if (!validate()) return;
    // A run needs every encrypted source unlocked first — the password is
    // stored (and verified) when the user clicks "Расшифровать".
    if (run && !allEncryptedDecrypted()) {
      error = nextHint() ?? m.wiz_err_decrypt_first;
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
    // Settle "is this a bootable copy" at save time, for an edited job too.
    //
    // On the "Параметры" screen the source is read-only, so the switch cannot
    // be touched — and a job saved before the flag existed would otherwise
    // keep `bootable: false` forever and silently stop carrying the machine's
    // boot entries. `isBootableCopy` reads what the job ACTUALLY captures in
    // that mode, which is the honest answer to the same question.
    if (job.disk_source) {
      const d = engine.disks.find((x) => diskNumber(x) === job.disk_source!.disk_number);
      if (d) job.disk_source = { ...job.disk_source, bootable: isBootableCopy(d) };
    }
    // Turning encryption on cannot go through an ordinary save: the service
    // refuses it there on purpose, because the job and its key have to be
    // written as one transaction — and only after the recovery key has been
    // confirmed. The dialog does both and hands back the stored job.
    if (wantEncryption && !job.archive_encryption) {
      if (!encryptionReady) {
        error = encryptionProblem();
        goToStep('scheme');
        return;
      }
      runAfterKey = run;
      keyModalOpen = true;
      return;
    }
    try {
      const saved = await saveJob(job);
      if (run) await runJob(saved.id);
      onDone(saved);
    } catch (e) {
      error = () => String(e);
    }
  }

  /** The key dialog committed the job; carry on where `finish` left off. */
  async function afterKeyEnabled(saved: Job) {
    keyModalOpen = false;
    try {
      if (runAfterKey) await runJob(saved.id);
      onDone(saved);
    } catch (e) {
      error = () => String(e);
    }
  }

  // «Клонировать параметры» opens the wizard on a draft that already names its
  // volumes. The first question — which disk — is then answered, but only if
  // the answer still holds: `PhysicalDriveN` is a slot, so a draft that names a
  // disk must find it by number AND model, and one that names only volumes (a
  // job made before disks were asked for, or from the command line) must have
  // them all on one disk. Otherwise it starts from the first question like any
  // new job. Volumes that are gone since are dropped; the layout is taken
  // again from the disk as it is now.
  // svelte-ignore state_referenced_locally
  if (mode === 'wizard' && job.sources.length > 0) {
    const spec = job.disk_source;
    const owners = [...new Set(job.sources.map((dev) => diskOfDevice(dev)?.device_id ?? ''))];
    const d = spec
      ? engine.disks.find(
          (x) => diskNumber(x) === spec.disk_number && (x.model || x.device_id) === spec.disk_model,
        )
      : owners.length === 1
        ? engine.disks.find((x) => x.device_id === owners[0])
        : undefined;
    if (d) {
      const here = (dev: string) => d.partitions.some((p) => partDevice(p) === dev);
      const kept = job.sources
        .map((dev, i) => ({ dev, label: job.source_labels[i] ?? dev }))
        .filter((s) => here(s.dev));
      job.sources = kept.map((s) => s.dev);
      job.source_labels = kept.map((s) => s.label);
      job.source = job.sources[0] ?? '';
      job.source_label = job.source_labels[0] ?? '';
      pickedDiskId = d.device_id;
      if (spec?.bootable && isBootableDisk(d)) {
        bootCopyOn = [d.device_id];
        // The switch put the boot chain in, and turning it off gives it back.
        for (const p of bootParts(d)) {
          const dev = partDevice(p);
          if (dev && job.sources.includes(dev)) autoIncluded.add(dev);
        }
      }
      // One job = one disk = one image file: a clone gets the disk's layout
      // even when the job it came from had none.
      captureDiskSource(d);
    } else {
      clearSourceSelection();
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
      <svg class="lock" width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg>
    {/if}
    {#if p.is_encrypted}
      <!-- Accent, not muted: next to the grey "system" lock above, the same
           shape in the same colour would not say which one means encrypted. -->
      <span class="enc" title={m.wiz_seg_encrypted()}><svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2.4" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg></span>
    {/if}
    <span class="seg-label">{partLabel(p)}</span>
  </div>
  <div class="seg-meta">{formatBytes(p.size_bytes)}{#if partUsed(p) > 0}<span class="muted">&nbsp;{m.wiz_seg_used({ size: formatBytes(partUsed(p)) })}</span>{/if}{#if partFs(p)}<span class="muted">&nbsp;{partFs(p)}</span>{/if}{#if layoutOnly}<span class="muted">&nbsp;· {m.wiz_seg_layout_only()}</span>{/if}</div>
  {#if selected}
    <div class="check"><svg width="11" height="11" viewBox="0 0 24 24" stroke="#fff" stroke-width="3.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg></div>
  {/if}
{/snippet}

{#snippet pwPanel(list: EncryptedVolume[], showShared: boolean, whole: boolean)}
  <div class="pw-panel under-disk">
    <div class="pw-head">
      <span>
        <svg class="ico-inline" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2.2" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg> {list.every(needsRecoveryKey)
          ? m.wiz_pw_head_rk()
          : m.wiz_pw_head_pw()}
      </span>
      <!-- One toggle for the whole screen: it lives next to the single shared
           prompt, not once per disk panel. -->
      {#if showShared && sharedCandidates().length > 1}
        <label class="pw-shared-tgl" title={m.wiz_pw_shared_title()}>
          <input type="checkbox" bind:checked={sharedPw} /> {m.wiz_pw_shared()}
        </label>
      {/if}
    </div>

    {#if sharedPw && showShared}
      <div class="pw-row shared">
        <span class="pw-vol">{m.wiz_pw_for_all()}</span>
        <PasswordField
          value={sharedInput.password}
          oninput={setSharedPw}
          onenter={() => { if (!sharedBusy && sharedInput.password) decryptAllShared(); }}
          invalid={sharedFailed.length > 0}
        />
        <label class="pw-remember">
          <input type="checkbox" bind:checked={sharedInput.remember} /> {m.wiz_pw_remember()}
        </label>
        <button
          type="button"
          class="btn small primary"
          onclick={decryptAllShared}
          disabled={sharedBusy || !sharedInput.password}
        >{sharedBusy ? m.wiz_pw_decrypting() : m.wiz_pw_decrypt_all()}</button>
      </div>
      {#if sharedFailed.length > 0 && !sharedBusy}
        <div class="pw-err">
          {m.wiz_pw_shared_failed({ volumes: sharedFailed.map((f) => f.label).join(', ') })}
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
          <div class="pw-ask">{m.wiz_pw_ask_rk({ label: e.label })}</div>
        {:else}
          <span class="pw-vol" title={e.device}>{e.label}</span>
        {/if}
        {#if !e.unlockable && !decryptedVolumes[e.device]}
          <!-- Asking for a password a volume does not have, and then calling
               it wrong, is not an error message — it is the wrong question.
               The volume's own metadata says what it does have. -->
          <span class="pw-nokey">
            {m.wiz_pw_no_way({ methods: e.methods.join(', ') })}
          </span>
        {:else if decryptedVolumes[e.device]}
          <span class="pw-done"><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="var(--success)" stroke-width="2.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>{m.wiz_pw_done()}</span>
        {:else if sharedPw && !needsRecoveryKey(e)}
          <!-- One prompt for all: the per-volume row is only a status line. -->
          <span class="pw-wait">
            {#if decrypting === e.device}{m.wiz_pw_decrypting()}
            {:else if pwErrors[e.device]}{m.wiz_pw_wrong_short()}
            {:else}{m.wiz_pw_waiting_shared()}{/if}
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
              /> {m.wiz_pw_remember()}
            </label>
            <button
              type="button"
              class="btn small primary"
              onclick={() => decrypt(e.device)}
              disabled={decrypting === e.device || !pwInputs[e.device]?.password}
            >{decrypting === e.device ? m.wiz_pw_decrypting() : m.wiz_pw_decrypt()}</button>
          </div>
        {/if}
      </div>
      {#if pwFormat[e.device]}
        <div class="pw-err">{pwFormat[e.device]()}</div>
      {:else if pwErrors[e.device] && !sharedPw}
        <div class="pw-err">
          {needsRecoveryKey(e) ? m.wiz_pw_rk_wrong() : m.wiz_pw_wrong()}
        </div>
      {/if}
    {/each}
    {#if list.some(needsRecoveryKey)}
      <!-- Where to get the key, since the answer is on the machine the user is
           standing at — but in the system they are not currently running. -->
      <div class="pw-hint">
        {m.wiz_pw_rk_hint_before()}
        <code>manage-bde -protectors -get C:</code> {m.wiz_pw_rk_hint_after()}
      </div>
    {/if}
    {#if whole && list.some((e) => !decryptedVolumes[e.device])}
      <!-- Said before the copy runs, because afterwards it is a 43 MB image
           where 128 GiB were expected. A volume left locked is dropped from the
           partition table too (`DiskLayout::retain_for_backup`), not kept as an
           empty slot. -->
      <div class="pw-hint warn">
        {m.wiz_pw_locked_dropped()}
      </div>
    {/if}
    <div class="pw-hint">
      {m.wiz_pw_general_hint()}
    </div>
  </div>
{/snippet}

<div class="wizard">
    <div class="wz-head">
      <span class="wz-title">{m.wiz_title()}</span>
      <span class="wz-name">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.8"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
        {job.name}
      </span>
    </div>

    <!-- tab bar -->
    <div class="tabs">
      {#each steps as s, i}
        <button class="tab" class:active={step === i} onclick={() => (step = i)}>{s.label}</button>
      {/each}
    </div>

    <!-- The exclusions step stretches its list to the window's height: a
         fixed 260 px box left half a tall window empty below it. -->
    <div class="body" class:fill={stepKey === 'exclusions'}>
      <!-- source (top) → destination (bottom) -->
      {#if stepKey === 'source'}
        <div class="src-wrap">
          <!-- SOURCE -->
          <div class="sec-head">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
            <span class="sec-title">{sourceReadonly ? m.wiz_src_title_ro() : m.wiz_src_title()}</span>
            <span class="sec-sub">{sourceReadonly
              ? m.wiz_src_sub_ro()
              : diskChosen
                ? m.wiz_src_sub_volumes()
                : m.wiz_src_sub_disk()}</span>
            {#if diskChosen && !sourceReadonly}
              <button class="link back-disk" onclick={pickAnotherDisk}>{m.wiz_src_other_disk()}</button>
            {/if}
          </div>

          {#if engine.volumesLoading && engine.disks.length === 0}
            <div class="loading-note">{m.wiz_disks_loading()}</div>
          {:else if engine.volumesError && engine.disks.length === 0}
            <div class="warn-note">{engine.volumesError}</div>
          {:else if visibleDisks.length === 0}
            <div class="warn-note">
              {sourceReadonly
                ? m.wiz_disks_unavailable()
                : m.wiz_disks_none()}
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
                        <span class="disk-meta">{formatBytes(d.size_bytes)} · {diskTableText(d)}</span>
                      </span>
                    </div>
                    <div class="disk-body">
                      <div class="disk-ico">
                        <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.6"><ellipse cx="12" cy="6" rx="8" ry="3"/><path d="M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6"/><path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/></svg>
                        <div class="disk-name">{diskCaption(d)}</div>
                      </div>
                      <div class="bar">
                        {#each diskSegments(d) as seg}
                          {#if seg.kind === 'free'}
                            <div class="seg free" style="flex: {seg.size} 1 0"><span class="free-lbl">{m.wiz_unallocated()}</span></div>
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
                      {#if isBootableCopy(d)}<span class="whole-badge">{m.wiz_badge_bootable()}</span>{/if}
                      {#if wholeDisk}
                      <label
                        class="boot-toggle"
                        class:disabled={!isBootableDisk(d)}
                        title={isBootableDisk(d)
                          ? m.wiz_boot_toggle_title()
                          : m.wiz_boot_toggle_title_none()}
                      >
                        <input
                          type="checkbox"
                          checked={isBootableCopy(d)}
                          disabled={sourceReadonly || !isBootableDisk(d)}
                          onchange={() => toggleBootableCopy(d)}
                        />
                        <span>{m.wiz_boot_toggle()}</span>
                      </label>
                      {/if}
                      <span class="disk-meta">{formatBytes(d.size_bytes)} · {diskTableText(d)}</span>
                    </span>
                  </div>
                  <div class="disk-body">
                    <div class="disk-ico">
                      <svg width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="var(--disk-icon)" stroke-width="1.6"><ellipse cx="12" cy="6" rx="8" ry="3"/><path d="M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6"/><path d="M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/></svg>
                      <div class="disk-name">{diskCaption(d)}</div>
                    </div>
                    <div class="bar">
                      {#each diskSegments(d) as seg}
                        {#if seg.kind === 'free'}
                          <div class="seg free" style="flex: {seg.size} 1 0"><span class="free-lbl">{m.wiz_unallocated()}</span></div>
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
                              title={isLockedSource(dev) ? segTitle(seg.p) + ' · ' + m.wiz_seg_boot_required() : segTitle(seg.p)}
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
                                ? segTitle(seg.p) + ' · ' + m.wiz_seg_layout_entry()
                                : typeGuid(seg.p) === MSR_GUID
                                  ? segTitle(seg.p) + ' · ' + m.wiz_seg_msr()
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
                {#if isBootableCopy(d)}
                  {@const locked = d.partitions.filter((p) => bootRole(p) === 'locked')}
                  {#if locked.length > 0}
                    <div class="boot-hint">
                      {m.wiz_boot_locked_before({ count: locked.length })}
                      <b>{locked.map((p) => partLabel(p)).join(', ')}</b>
                      {m.wiz_boot_locked_after({ count: locked.length })}
                    </div>
                  {:else if !d.partitions.some((p) => p.has_os) && bootParts(d).every((p) => typeGuid(p) === GPT_ESP)}
                    <div class="boot-hint">
                      {m.wiz_boot_esp_only()}
                    </div>
                  {/if}
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
              <div class="cat-head">{m.wiz_cat_head()}</div>
              <div class="catbar">
                {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}
                  <div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name()}: {formatBytes(c.bytes)}"></div>
                {/each}
              </div>
              <div class="cats">
                {#each catData as c}
                  <div class="cat">
                    <span class="dot" style="background:{c.color}"></span>
                    <span class="cat-name">{c.name()}</span>
                    <span class="cat-size">{formatBytes(c.bytes)}</span>
                  </div>
                {/each}
                <div class="cat">
                  <span class="dot" style:background={EXCLUDED_COLOR}></span>
                  <span class="cat-name">{m.cat_excluded()}</span>
                  <span class="cat-size">{formatBytes(job.last_run?.excluded_bytes ?? 0)}</span>
                </div>
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
            <div class="arrow-circle"><svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M6 13l6 6 6-6"/></svg></div>
            <div class="dash"></div>
          </div>

          <!-- DESTINATION -->
          <div class="sec-head">
            <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 6a2 2 0 0 1 2-2h5l2 2h5a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z"/></svg>
            <span class="sec-title">{m.wiz_dest_title()}</span>
            <span class="sec-sub">{m.wiz_dest_sub()}</span>
          </div>
          <div class="dest-card">
            <svg width="42" height="40" viewBox="0 1 24 22" style="flex-shrink:0"><path d="M3 7a2 2 0 0 1 2-2h4.2l2 2H19a2 2 0 0 1 2 2v8.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" fill="var(--folder)"/><path d="M3 10h18v7.5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" fill="var(--folder2)"/></svg>
            <div class="dest-fields">
              <input
                type="text"
                value={destDisplay}
                oninput={onDestInput}
                onfocus={() => (destFocused = true)}
                onblur={() => (destFocused = false)}
                placeholder={examplePath('folder')}
                spellcheck="false"
                title={destDisplay}
              />
              <button class="link" onclick={browseDest}>{m.common_browse()}</button>
            </div>
          </div>
          {#if collisionLetter}
            <div class="warn-note"><svg class="ico-inline" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--error)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><path d="M9 9l6 6M15 9l-6 6"/></svg> {m.wiz_dest_collision({ letter: collisionLetter ?? '' })}</div>
          {:else if destOnSource}
            <div class="warn-note"><svg class="ico-inline" width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="var(--error)" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><path d="M9 9l6 6M15 9l-6 6"/></svg> {m.wiz_dest_on_source({ source: destOnSource })}</div>
          {:else if destCheck?.placement === 'cannot_tell'}
            <div class="note">{m.wiz_dest_cannot_tell({ reason: destCheck.reason })}</div>
          {/if}
          {#if !wholeDisk && !sourceReadonly && job.sources.length > 1}
            <div class="note">{m.wiz_volumes_separate()}</div>
          {/if}
          {/if}
        </div>
      {/if}

      <!-- schedule -->
      {#if stepKey === 'schedule'}
        <div class="sched">
          <div class="sched-kinds">
            {#each scheduleKinds as [k, lbl]}
              <label class="radio"><input type="radio" name="skind" value={k} checked={job.schedule.kind === k} onchange={() => (job.schedule.kind = k as Job['schedule']['kind'])} /> {lbl()}</label>
            {/each}
          </div>
          <div class="sched-right">
            {#if job.schedule.kind === 'weekly'}
              <div class="days">
                {#each weekdays as [d, lbl]}
                  <button class="day" class:on={job.schedule.days.includes(d)} onclick={() => toggleDay(d)}>{lbl()}</button>
                {/each}
              </div>
            {/if}
            {#if job.schedule.kind === 'monthly'}
              <label class="at">{m.wiz_sched_day_of_month()}
                <select bind:value={job.schedule.day_of_month}>
                  {#each Array.from({ length: 28 }, (_, i) => i + 1) as d}
                    <option value={d}>{d}</option>
                  {/each}
                </select>
              </label>
            {/if}
            {#if job.schedule.kind === 'daily' || job.schedule.kind === 'weekly' || job.schedule.kind === 'monthly'}
              <label class="at">{m.wiz_sched_at()} <input type="time" bind:value={job.schedule.time} /></label>
            {/if}
            {#if job.schedule.kind === 'event'}
              <div class="event-opts">
                <label class="chk"><input type="checkbox" bind:checked={job.schedule.event_once_per_day} /> {m.wiz_sched_once_a_day()}</label>
                {#each eventTriggers as [val, lbl, badge]}
                  <label class="radio">
                    <input type="radio" name="evtrig" value={val} checked={job.schedule.event_trigger === val} onchange={() => (job.schedule.event_trigger = val)} />
                    {lbl()}{#if badge}<span class="dev-badge">{badge}</span>{/if}
                  </label>
                  {#if val === 'startup' && job.schedule.event_trigger === 'startup'}
                    <label class="delay-min">{m.wiz_delay()}
                      <select bind:value={job.schedule.event_startup_delay_min}>
                        {#each Array.from({ length: 61 }, (_, i) => i) as mn}<option value={mn}>{mn}</option>{/each}
                      </select> {m.wiz_min()}
                    </label>
                  {/if}
                {/each}
              </div>
            {/if}
          </div>
        </div>
        {#if job.schedule.kind === 'monthly'}
          <div class="hint-sm">{m.wiz_sched_month_hint()}</div>
        {/if}
        {#if job.schedule.kind === 'event'}
          <div class="note info">
            <svg class="ico-inline" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="var(--info)" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg> {m.wiz_event_note()}
            {#if job.schedule.event_trigger === 'dest_connected'}
              {@const destLetter = driveLetter(job.dest_folder)}
              {destLetter
                ? m.wiz_event_dest_letter({ letter: destLetter })
                : m.wiz_event_dest_generic()}
            {/if}
          </div>
        {:else if job.schedule.kind !== 'none'}
          <div class="note info"><svg class="ico-inline" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="var(--info)" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg> {m.wiz_sched_service_note()}</div>
        {/if}
        <div class="adv-schedule">
          <!-- «Копировать только если компьютер заблокирован» и «Предотвращать
               переход в спящий режим» убраны (2026-09-26): первое не нужно,
               второе больше не выбор — во время копии компьютер не засыпает
               никогда. -->
          {#if cap.wake_to_run && (job.schedule.kind === 'daily' || job.schedule.kind === 'weekly' || job.schedule.kind === 'monthly')}
            <label class="chk"><input type="checkbox" bind:checked={job.schedule.wake_to_run} /> {m.wiz_sched_wake()}</label>
            {#if job.schedule.wake_to_run}
              <div class="note sub">
                {m.wiz_sched_wake_note()}
              </div>
            {/if}
          {/if}
          <label class="chk"><input type="checkbox" bind:checked={job.schedule.run_missed} /> {m.wiz_sched_run_missed()}</label>
          {#if job.schedule.run_missed}
            <label class="delay-min">{m.wiz_sched_missed_delay()}
              <select bind:value={job.schedule.missed_delay_min}>
                {#each Array.from({ length: 60 }, (_, i) => i + 1) as mn}
                  <option value={mn}>{mn}</option>
                {/each}
              </select> {m.wiz_min()}
            </label>
          {/if}
        </div>
      {/if}

      <!-- scheme -->
      {#if stepKey === 'scheme'}
        <div class="field-label">{m.wiz_format()}</div>
        <select
          value={wantEncryption ? 'obk' : job.image_format}
          onchange={(e) => setFormat((e.target as HTMLSelectElement).value)}
          class="wide"
          disabled={formatLocked}
        >
          <option value="vhdx">{isWindows() ? m.wiz_format_vhdx() : m.wiz_format_vhdx_plain()}</option>
          <option value="qcow2">{m.wiz_format_qcow2()}</option>
          {#if cap.archive_encryption || alreadyEncrypted || wantEncryption}
            <option value="obk">{m.wiz_format_obk()}</option>
          {/if}
        </select>
        {#if alreadyEncrypted}
          <div class="note">
            {m.wiz_format_already_encrypted()}
          </div>
        {:else if mode === 'edit' && job.last_run !== null}
          <div class="note">{m.wiz_format_locked()}</div>
        {/if}
        {#if wantEncryption && !alreadyEncrypted}
          <div class="note">
            {@html m.wiz_obk_note()}
          </div>
          <div class="enc-form">
            <!-- Side by side: the pair is one decision, and the row is wide
                 enough to hold it without scrolling past it. -->
            <div class="enc-pair">
              <div class="enc-cell">
                <div class="field-label">{m.wiz_enc_password_label()}</div>
                <PasswordField
                  value={encPassword}
                  oninput={(v) => (encPassword = v)}
                  placeholder={m.wiz_enc_password_ph()}
                />
              </div>
              <div class="enc-cell">
                <div class="field-label">{m.wiz_enc_again_label()}</div>
                <PasswordField
                  value={encPasswordAgain}
                  oninput={(v) => (encPasswordAgain = v)}
                  placeholder={m.wiz_enc_again_ph()}
                  invalid={encPasswordAgain.length > 0 && encPassword !== encPasswordAgain}
                />
              </div>
            </div>
            {#if encPassword.length > 0 && encPassword.length < MIN_ARCHIVE_PASSWORD}
              <div class="note warn">{m.wiz_enc_too_short({ count: MIN_ARCHIVE_PASSWORD })}</div>
            {:else if encPasswordAgain.length > 0 && encPassword !== encPasswordAgain}
              <div class="note warn">{m.wiz_enc_mismatch()}</div>
            {/if}
          </div>
        {:else if job.image_format === 'qcow2' && !wantEncryption}
          <div class="note">{m.wiz_qcow2_note()}</div>
        {:else if !wantEncryption}
          <div class="note">{isWindows() ? m.wiz_vhdx_note() : m.wiz_vhdx_note_linux()}</div>
        {/if}

        {#if wantEncryption || job.image_format === 'qcow2'}
          <!-- zstd only exists in these two; VHDX has nothing to set. Not
               locked by existing copies: every cluster is compressed on its
               own, so a chain may mix levels and still reads the same. -->
          <div class="field-label mt">{m.wiz_level()}</div>
          <select
            class="wide"
            value={job.compression_level == null ? 'default' : String(job.compression_level)}
            onchange={(e) => {
              const v = (e.target as HTMLSelectElement).value;
              job.compression_level = v === 'default' ? null : Number(v);
            }}
          >
            <option value="default">{m.wiz_level_default({ level: defaultLevel })}</option>
            {#each Array.from({ length: 19 }, (_, i) => i + 1) as n (n)}
              <option value={String(n)}>{n}{LEVEL_HINT[n] ? ` — ${LEVEL_HINT[n]()}` : ''}</option>
            {/each}
          </select>
          <div class="note">{m.wiz_level_note()}</div>
        {/if}

        <div class="field-label mt">{m.wiz_method()}</div>
        <select bind:value={job.scheme.method} onchange={onMethodChange} class="wide">
          <option value="full">{m.wiz_method_full()}</option>
          {#if (chainOk && cap.methods.includes('incremental')) || job.scheme.method === 'incremental'}
            <option value="incremental">{m.wiz_method_incremental()}</option>
          {/if}
          {#if (chainOk && cap.methods.includes('differential')) || job.scheme.method === 'differential'}
            <option value="differential">{m.wiz_method_differential()}</option>
          {/if}
        </select>
        {#if !cap.methods.includes('incremental') && !cap.methods.includes('differential')}
          <div class="note">{m.wiz_method_full_note()} {m.wiz_method_only_full()}</div>
        {:else if !chainOk && job.scheme.method === 'full'}
          <div class="note">{m.wiz_method_full_note()} {m.wiz_method_chain_formats({ formats: chainFormatNames })}</div>
        {:else if job.scheme.method === 'full'}
          <div class="note">{m.wiz_method_full_note()}</div>
        {:else if job.scheme.method === 'incremental'}
          <div class="note">{m.wiz_method_inc_note()}</div>
        {:else if job.scheme.method === 'differential'}
          <div class="note">{m.wiz_method_diff_note()}</div>
        {/if}

        {#if job.scheme.method === 'incremental'}
          <div class="field-label mt">{m.wiz_retention_mode()}</div>
          <label class="radio blk"><input type="radio" checked={!isConsolidation} onchange={() => setSchemeMode('standard')} /> {m.wiz_mode_standard()}</label>
          <label class="radio blk"><input type="radio" checked={isConsolidation} onchange={() => setSchemeMode('consolidation')} /> {m.wiz_mode_consolidation()}</label>
        {/if}

        {#if job.scheme.method !== 'full' && !isConsolidation}
          <!-- Стандартный режим: цепочка растёт до предела, затем начинается новая. -->
          <div class="field-label mt">{m.wiz_chain_length()}</div>
          <label class="chk-line">{m.wiz_chain_max_before()} <input type="number" min="1" max="64" bind:value={job.scheme.full_every_n} class="num" /> {job.scheme.method === 'differential' ? m.wiz_chain_max_after_diff({ count: job.scheme.full_every_n ?? 0 }) : m.wiz_chain_max_after_inc({ count: job.scheme.full_every_n ?? 0 })}</label>
          <div class="note">{m.wiz_chain_note()}<br />{m.wiz_chain_points({ n: job.scheme.full_every_n + 1 })}</div>
        {/if}

        {#if !isConsolidation}
          <!-- The engine applies these to a full-only job as well — there every
               copy is a chain of one — and the template turns "keep 3" on by
               default. Hiding the block for «Полное» deleted copies by a rule
               the screen never showed (2026-09-26). -->
          {@const full = job.scheme.method === 'full'}
          {@const keepN = job.scheme.keep_chains ?? 3}
          {@const olderDays = job.scheme.cleanup_older_than_days ?? 90}
          <div class="field-label mt">{full ? m.wiz_cleanup_copies() : m.wiz_cleanup_chains()}</div>
          <label class="chk"><input type="checkbox" checked={job.scheme.keep_chains !== null} onchange={(e) => (job.scheme.keep_chains = (e.target as HTMLInputElement).checked ? 3 : null)} /> {m.wiz_keep_before()} <input type="number" min="1" value={job.scheme.keep_chains ?? 3} oninput={(e) => (job.scheme.keep_chains = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.keep_chains === null} /> {full ? m.wiz_keep_after_copies({ count: keepN }) : m.wiz_keep_after_chains({ count: keepN })}</label>
          <label class="chk"><input type="checkbox" checked={job.scheme.cleanup_older_than_days !== null} onchange={(e) => (job.scheme.cleanup_older_than_days = (e.target as HTMLInputElement).checked ? 90 : null)} /> {full ? m.wiz_delete_copies_before() : m.wiz_delete_chains_before()} <input type="number" min="1" value={job.scheme.cleanup_older_than_days ?? 90} oninput={(e) => (job.scheme.cleanup_older_than_days = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.cleanup_older_than_days === null} /> {m.wiz_days({ count: olderDays })}</label>
          <label class="chk sub"><input type="checkbox" bind:checked={job.scheme.keep_first} disabled={job.scheme.cleanup_older_than_days === null} /> {full ? m.wiz_keep_first_copy() : m.wiz_keep_first_chain()}</label>
          {#if full}
            <div class="note">{m.wiz_cleanup_note_copies()}</div>
          {:else}
            <div class="note">{m.wiz_cleanup_note_chains()}</div>
          {/if}
        {/if}

        {#if isConsolidation}
          <!-- Консолидация: цепочка одна, её длину держит объединение. -->
          {@const rollN = job.scheme.rolling_incrementals ?? 3}
          <div class="field-label mt">{m.wiz_chain_length()}</div>
          <label class="chk-line">{m.wiz_consol_before()} <input type="number" min="1" max="64" value={job.scheme.rolling_incrementals ?? 3} oninput={(e) => (job.scheme.rolling_incrementals = +(e.target as HTMLInputElement).value)} class="num" /> {m.wiz_consol_after({ count: rollN })}</label>
          <div class="note">{m.wiz_consol_note()}<br />{m.wiz_consol_points({ total: rollN + 1, n: rollN })} {@html m.wiz_consol_shift()}</div>

          <div class="field-label mt">{m.wiz_chain_refresh()}</div>
          <label class="chk"><input type="checkbox" checked={job.scheme.cleanup_older_than_days !== null} onchange={(e) => (job.scheme.cleanup_older_than_days = (e.target as HTMLInputElement).checked ? 90 : null)} /> {m.wiz_restart_before()} <input type="number" min="1" value={job.scheme.cleanup_older_than_days ?? 90} oninput={(e) => (job.scheme.cleanup_older_than_days = +(e.target as HTMLInputElement).value)} class="num" disabled={job.scheme.cleanup_older_than_days === null} /> {m.wiz_days({ count: job.scheme.cleanup_older_than_days ?? 90 })}</label>
          {#if job.scheme.cleanup_older_than_days !== null}
            <div class="note">{m.wiz_restart_note()}</div>
          {:else}
            <div class="note warn">{m.wiz_no_restart_warn()}</div>
          {/if}
        {/if}
      {/if}

      <!-- exclusions -->
      {#if stepKey === 'exclusions'}
        {#if !cap.exclusions}
          <div class="note warn">{m.wiz_excl_unavailable()}</div>
        {/if}
        <label class="chk" class:dim={!cap.exclusions}><input type="checkbox" bind:checked={job.exclusions.enabled} disabled={!cap.exclusions && !job.exclusions.enabled} /> {m.wiz_excl_enable()}</label>
        <div class="note">{@html m.wiz_excl_note_masks()}<br />{@html m.wiz_excl_note_removal()}</div>

        <!-- Windows / Linux списки: движок применяет их по типу ФС раздела. -->
        <div class="excl-tabs">
          <button class="excl-tab" class:active={exclTab === 'windows'} onclick={() => pickExclTab('windows')}>Windows</button>
          <button class="excl-tab" class:active={exclTab === 'linux'} onclick={() => pickExclTab('linux')}>Linux</button>
        </div>
        {#if exclTab === 'windows'}
          <div class="note sub">{m.wiz_excl_windows_note()}</div>
        {:else}
          <div class="note sub">{@html m.wiz_excl_linux_note()}</div>
        {/if}

        <div class="excl-list">
          {#each exclList() as p, i (p + i)}
            <div class="excl-row"><span>{p}</span><button class="x" title={m.wiz_excl_remove()} aria-label={m.wiz_excl_remove()} onclick={() => removePattern(i)}><svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18"/></svg></button></div>
          {/each}
        </div>
        <div class="excl-add">
          <input type="text" bind:value={newPattern} placeholder={exclTab === 'linux' ? m.wiz_excl_ph_linux() : m.wiz_excl_ph_windows()} spellcheck="false" onkeydown={(e) => e.key === 'Enter' && addPattern()} />
          <button class="btn small ghost" onclick={addPattern}>+ {m.wiz_excl_add()}</button>
          <button class="link" onclick={resetExclusions}>{m.wiz_excl_reset()}</button>
        </div>
      {/if}

      <!-- advanced -->
      {#if stepKey === 'advanced'}
        <div class="adv">
          <div class="adv-group">
            <div class="adv-title">{m.wiz_adv_image_mode()}</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.sector_by_sector} /> {m.wiz_adv_sector()}</label>
          </div>
          <div class="adv-group">
            <div class="adv-title">{m.wiz_adv_validation()}</div>
            <label class="chk"><input type="checkbox" bind:checked={job.advanced.validate_after} /> {m.wiz_adv_validate_after()}</label>
            <!-- Under its parent, and inert without it: "only the latest" is a
                 way of checking, not a check of its own. -->
            <label class="chk sub-chk" class:dim={!job.advanced.validate_after}>
              <input
                type="checkbox"
                bind:checked={job.advanced.verify_latest_only}
                disabled={!job.advanced.validate_after}
              />
              {m.wiz_adv_validate_latest()}
            </label>
            <div class="sub-hint" class:dim={!job.advanced.validate_after}>
              {m.wiz_adv_validate_latest_hint()}
            </div>
          </div>
          {#if !media.active && job.verification}
            <div class="adv-group">
              <div class="adv-title">{m.wiz_verify_schedule()}</div>
              <label class="at">
                <select bind:value={job.verification.schedule.kind}>
                  <option value="none">{m.wiz_sched_none()}</option>
                  <option value="daily">{m.wiz_sched_daily()}</option>
                  <option value="weekly">{m.wiz_sched_weekly()}</option>
                  <option value="monthly">{m.wiz_sched_monthly()}</option>
                </select>
              </label>
              {#if job.verification.schedule.kind !== 'none'}
                {#if job.verification.schedule.kind === 'weekly'}
                  <div class="days">
                    {#each weekdays as [day, label]}
                      <button class="day" class:on={job.verification.schedule.days.includes(day)} onclick={() => {
                        job.verification.schedule.days = job.verification.schedule.days.includes(day)
                          ? job.verification.schedule.days.filter((d) => d !== day)
                          : [...job.verification.schedule.days, day];
                      }}>{label()}</button>
                    {/each}
                  </div>
                {/if}
                {#if job.verification.schedule.kind === 'monthly'}
                  <label class="at">{m.wiz_sched_day_of_month()}
                    <select bind:value={job.verification.schedule.day_of_month}>
                      {#each Array.from({ length: 28 }, (_, i) => i + 1) as day}<option value={day}>{day}</option>{/each}
                    </select>
                  </label>
                {/if}
                <label class="at">{m.wiz_sched_at()} <input type="time" bind:value={job.verification.schedule.time} /></label>
                <label class="chk"><input type="checkbox" bind:checked={job.verification.all_versions} /> {m.wiz_verify_all_versions()}</label>
                <div class="sub-hint">{m.wiz_verify_schedule_hint()}</div>
                <label class="chk"><input type="checkbox" bind:checked={job.verification.schedule.run_missed} /> {m.wiz_sched_run_missed()}</label>
                {#if job.verification.schedule.run_missed}
                  <label class="delay-min">{m.wiz_sched_missed_delay()}
                    <select bind:value={job.verification.schedule.missed_delay_min}>
                      {#each Array.from({ length: 60 }, (_, i) => i + 1) as minute}<option value={minute}>{minute}</option>{/each}
                    </select> {m.wiz_min()}
                  </label>
                {/if}
                {#if cap.wake_to_run}
                  <label class="chk"><input type="checkbox" bind:checked={job.verification.schedule.wake_to_run} /> {m.wiz_sched_wake()}</label>
                {/if}
              {/if}
            </div>
          {/if}
          <!-- Encryption is a FORMAT (.obk), chosen and given its password on
               the «Схема» step. Said here too, because this is where people
               look for it. -->
          <div class="adv-group">
            <div class="adv-title">{m.wiz_adv_protection()}</div>
            {#if wantEncryption}
              <div class="note">
                <svg class="ico-inline" width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="2.2" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/></svg> {alreadyEncrypted ? m.wiz_adv_enc_on_set() : m.wiz_adv_enc_on_pw()}
                <button class="link" onclick={() => goToStep('scheme')}>{m.wiz_scheme_link()}</button>.
              </div>
            {:else if !cap.archive_encryption}
              <div class="note">{m.wiz_adv_enc_unavailable()}</div>
            {:else}
              <div class="note">
                {m.wiz_adv_enc_off()}
                <button class="link" onclick={() => goToStep('scheme')}>{m.wiz_scheme_link()}</button>.
              </div>
            {/if}
          </div>
          <!-- «Pre/Post-команды» убраны (2026-09-26): служба работает от имени
               SYSTEM, а задание может сохранить любой вошедший пользователь —
               команда в задании была бы запуском чего угодно от SYSTEM. -->
          <div class="adv-group">
            <div class="adv-title">{m.wiz_adv_errors()}</div>
            <label class="chk">
              <input
                type="checkbox"
                checked={job.advanced.retry_attempts > 0}
                onchange={(e) => (job.advanced.retry_attempts = (e.target as HTMLInputElement).checked ? 2 : 0)}
              />
              {m.wiz_adv_retry()}
            </label>
            {#if job.advanced.retry_attempts > 0}
              <div class="adv-line">
                {m.wiz_adv_attempts()}
                <select bind:value={job.advanced.retry_attempts}>
                  {#each [1, 2, 3, 4, 5] as n (n)}<option value={n}>{n}</option>{/each}
                </select>
                {m.wiz_adv_every()}
                <select bind:value={job.advanced.retry_interval_min}>
                  {#each [1, 5, 10, 15, 30, 60] as n (n)}<option value={n}>{n}</option>{/each}
                </select>
                {m.wiz_min()}
              </div>
            {/if}
            <div class="adv-hint">
              {m.wiz_adv_retry_hint()}
            </div>
          </div>
          {#if !media.active && (cap.after_backup || job.advanced.after_backup !== 'none')}
            <div class="adv-group">
              <div class="adv-title">{m.wiz_adv_after()}</div>
              <select class="wide" bind:value={job.advanced.after_backup}>
                <option value="none">{m.wiz_adv_after_none()}</option>
                <option value="sleep">{m.wiz_adv_after_sleep()}</option>
                <option value="shutdown">{m.wiz_adv_after_shutdown()}</option>
              </select>
              {#if job.advanced.after_backup !== 'none'}
                <div class="adv-hint flush">
                  {job.advanced.after_backup === 'sleep'
                    ? m.wiz_adv_after_sleep_hint()
                    : m.wiz_adv_after_shutdown_hint()}
                  {m.wiz_adv_after_only_if()}
                </div>
              {/if}
            </div>
          {/if}
          {#if cap.low_priority || cap.speed_limit || job.advanced.low_priority || job.advanced.speed_limit_mbps > 0}
          <div class="adv-group">
            <div class="adv-title">{m.wiz_adv_performance()}</div>
            {#if cap.low_priority || job.advanced.low_priority}
            <label class="chk">
              <input type="checkbox" bind:checked={job.advanced.low_priority} />
              {m.wiz_adv_low_priority()}
            </label>
            <div class="adv-hint">
              {m.wiz_adv_low_priority_hint()}
            </div>
            {/if}
            {#if cap.speed_limit || job.advanced.speed_limit_mbps > 0}
            <label class="chk">
              <input
                type="checkbox"
                checked={job.advanced.speed_limit_mbps > 0}
                onchange={(e) => (job.advanced.speed_limit_mbps = (e.target as HTMLInputElement).checked ? 50 : 0)}
              />
              {m.wiz_adv_speed_limit()}
              <input
                type="number"
                min="1"
                max="10000"
                class="num"
                value={job.advanced.speed_limit_mbps || 50}
                disabled={job.advanced.speed_limit_mbps === 0}
                oninput={(e) => {
                  const v = Math.round(Number((e.target as HTMLInputElement).value));
                  if (v >= 1) job.advanced.speed_limit_mbps = v;
                }}
              />
              {m.wiz_mb_s()}
            </label>
            {/if}
          </div>
          {/if}
          {#if !cap.after_backup || !cap.low_priority || !cap.speed_limit}
            <div class="note">{m.wiz_adv_unavailable()}</div>
          {/if}
          {#if cap.keeps_awake}
            <div class="note">{m.wiz_adv_no_sleep()}</div>
          {/if}
        </div>
      {/if}
    </div>

    {#if error}<div class="wz-error">{error()}</div>{/if}

    {#if mode === 'wizard' && stepKey === 'schedule' && job.schedule.kind !== 'none' && job.requires_password && !allRemembered()}
      <div class="warn-note footer-warn">
        {m.wiz_footer_remember_warn()}
      </div>
    {/if}

    <!-- footer -->
    <div class="footer">
      <div class="spacer"></div>
      <button class="btn ghost" onclick={onCancel}>{m.common_cancel()}</button>
      {#if mode === 'wizard'}
        {#if step > 0}<button class="btn ghost" onclick={() => (step -= 1)}>{m.common_back()}</button>{/if}
        {#if step < steps.length - 1}
          <button
            class="btn primary"
            onclick={() => (step += 1)}
            disabled={(stepKey === 'source' && !canLeaveSource()) ||
              (stepKey === 'scheme' && !!encryptionHint())}
            title={stepKey === 'source' ? (nextHint()?.() ?? '') : stepKey === 'scheme' ? encryptionHint() : ''}
          >{m.common_next()}</button>
        {:else}
          <button class="btn green" onclick={() => finish(true)}>{m.wiz_create_backup()}</button>
        {/if}
      {:else}
        <button class="btn primary" onclick={() => finish(false)}>{m.common_save()}</button>
      {/if}
    </div>
</div>


<ArchiveKeyModal
  open={keyModalOpen}
  {job}
  password={encPassword}
  onDone={afterKeyEnabled}
  onCancel={() => {
    keyModalOpen = false;
    // Nothing was saved. Leaving the switch on would offer the same dialog
    // again on the next Save, which is what backing out of it means.
  }}
/>

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
  select.wide { width: 100%; padding: 8px 10px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 13px; font-family: var(--font); background: var(--input-bg); color: var(--text); }
  select.wide:disabled { background: var(--sunken); border-color: var(--border); color: var(--text-muted); }
  .link { background: none; border: none; color: var(--accent-text); font-size: 13px; cursor: pointer; padding: 0; }
  .link:hover { text-decoration: underline; }
  /* Every field on this screen: one look, one focus ring (styles/global.css
     .form-input), whatever the local rule that sizes it. */
  select.wide:focus, .dest-fields input:focus, .at input:focus, .at select:focus, .delay-min select:focus, .num:focus, .excl-add input:focus, .adv-line select:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
  .dest-fields input::placeholder, .excl-add input::placeholder { color: var(--text-muted); }
  /* An icon standing in a line of text, where an emoji used to be. */
  .ico-inline { vertical-align: -2px; flex-shrink: 0; }

  /* --- Paragon-style source map (step 0) --- */
  /* The whole width: a disk map drawn 940 px wide in the middle of a wide
     window squeezes every partition label into «Зарезерв…» for no reason. */
  .src-wrap { width: 100%; }
  .sec-head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .sec-title { font-size: 15px; font-weight: 600; color: var(--head); }
  .sec-sub { font-size: 13px; color: var(--text-muted); }
  .disks { display: flex; flex-direction: column; gap: 14px; }
  /* Kept-files category breakdown, shown under the disk map (read-only). */
  .cat-block { margin-top: 14px; padding: 14px 16px; border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); }
  .cat-head { font-size: 13px; font-weight: 600; color: var(--head); margin-bottom: 4px; }
  .catbar { height: 9px; border-radius: 5px; overflow: hidden; display: flex; gap: 2px; margin-top: 10px; background: var(--track); }
  /* Every present category stays visible: tiny slices keep 5px and the
     large ones shrink to make room, so the bar still totals 100%. */
  .catseg { min-width: 5px; }
  /* Four to a row, always, with «Исключено» as the eighth cell: two rows that
     line up column by column (see JobDashboard). */
  .cats { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 10px 8px; margin-top: 14px; }
  .cat { min-width: 0; display: flex; align-items: center; gap: 7px; }
  .dot { width: 9px; height: 9px; border-radius: 50%; flex-shrink: 0; }
  .cat-name { font-size: 13px; color: var(--text); }
  .cat-size { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .disk-card { border: 1px solid var(--border); border-radius: 8px; overflow: hidden; background: var(--card-bg); }
  /* Stage 1: the same card, clickable as a whole. Button defaults (centred
     text, own font, native border) would undo the layout, so they go. */
  button.disk-card { display: block; width: 100%; padding: 0; text-align: left; font: inherit; color: inherit; cursor: pointer; }
  button.disk-card:hover { border-color: var(--primary); }
  button.disk-card:hover .disk-head { background: var(--accent-soft); }
  /* Back to the disk question — small, next to the instruction it undoes. */
  .back-disk { margin-left: auto; font-size: 12px; }
  .disk-head { width: 100%; display: flex; align-items: center; justify-content: space-between; padding: 6px 12px; background: var(--panel2); border: 0; border-bottom: 1px solid var(--border); margin: 0; font-family: inherit; text-align: left; }
  .disk-head.whole { background: var(--accent-soft); color: var(--accent-text); }
  .disk-head.whole .disk-model { color: var(--accent-text); }
  .disk-head-right { display: flex; align-items: center; gap: 10px; }
  .boot-toggle { display: flex; align-items: center; gap: 6px; font-size: 12px; font-weight: 600; color: var(--head); cursor: pointer; user-select: none; }
  .boot-toggle input { cursor: pointer; margin: 0; }
  .boot-toggle.disabled { color: var(--text-muted); cursor: not-allowed; opacity: .55; }
  .boot-toggle.disabled input { cursor: not-allowed; }
  .whole-badge { font-size: 11px; font-weight: 700; color: #fff; background: var(--primary); padding: 2px 8px; border-radius: 10px; }
  .seg.locked { cursor: default; }
  .disk-model { font-size: 13px; font-weight: 600; color: var(--head); }
  .disk-meta { font-size: 12px; color: var(--text-muted); }
  .disk-body { display: flex; align-items: center; gap: 14px; padding: 7px 12px; }
  .disk-ico { width: 52px; flex-shrink: 0; text-align: center; color: var(--text-muted); }
  .disk-name { font-size: 12px; color: var(--text-secondary); margin-top: 1px; }
  /* The segments are translucent tints (--seg*): under them lies the card,
     as in the design, not a second grey. */
  .bar { flex: 1; display: flex; height: 44px; border: 1px solid var(--border); border-radius: 4px; overflow: hidden; background: var(--card-bg); }
  /* Smallest readable segment width: tiny partitions never shrink below this
     (so their size/FS text stays legible), larger ones grow proportionally. */
  .seg { position: relative; padding: 5px 8px; min-width: 84px; border: 0; border-right: 1px solid var(--border); margin: 0; display: flex; flex-direction: column; gap: 3px; background: var(--seg); overflow: hidden; text-align: left; font-family: inherit; color: inherit; appearance: none; -webkit-appearance: none; }
  .seg:last-child { border-right: none; }
  .seg.free { min-width: 40px; align-items: center; justify-content: center; background: repeating-linear-gradient(135deg, var(--hatch) 0 6px, transparent 6px 12px); }
  .free-lbl { font-size: 11px; color: var(--text-muted); white-space: nowrap; }
  .seg.selectable { cursor: pointer; }
  /* Hover previews the selected tint (the frame and tick stay the selection's
     own). A brightness filter did this before and vanished in the dark theme.
     Not on a read-only map, nor on a volume the boot copy holds fixed. */
  .seg.selectable:not(.ro):not(.locked):hover { background: var(--seg-sel); }
  /* Kept in the copy as a table entry only. It looks selected because it IS in
     the copy; the cursor says it is not something to click. */
  .seg.layout-only { cursor: default; }
  /* Read-only source map ("Параметры"): the selection is shown, not editable. */
  .seg.ro { cursor: default; }
  .seg.selected { background: var(--seg-sel); box-shadow: inset 0 0 0 2px var(--primary); }
  /* Left-to-right "used space" fill inside a partition — a subtle shaded strip
     with a defined right edge, so occupancy is readable at a glance. The edge
     is the same tint laid over the fill, i.e. twice as dense. On a selected
     segment the fill sits on the denser --seg-sel and darkens with it. */
  .usedfill { position: absolute; top: 0; bottom: 0; left: 0; z-index: 0; background: var(--seg-used); border-right: 1px solid var(--seg-used); pointer-events: none; }
  .seg-top { position: relative; z-index: 1; display: flex; align-items: center; gap: 5px; color: var(--text); }
  .lock { color: var(--text-muted); flex-shrink: 0; }
  .enc { display: inline-flex; line-height: 1; flex-shrink: 0; }

  .pw-panel { margin-top: 16px; padding: 16px 18px; border: 1px solid var(--border); border-radius: 8px; background: var(--panel2); }
  /* Password panel rendered directly under its disk card: tighter top margin
     and a primary border on the accent tint (the design's "selected" pair) so
     the association with the disk is clear. */
  .pw-panel.under-disk { margin-top: 6px; margin-bottom: 4px; border-color: var(--primary); background: var(--accent-soft); }
  .pw-head { display: flex; align-items: center; gap: 16px; font-size: 13px; font-weight: 700; color: var(--head); margin-bottom: 12px; }
  .pw-shared-tgl { margin-left: auto; display: flex; align-items: center; gap: 6px; font-size: 13px; font-weight: 600; white-space: nowrap; cursor: pointer; }
  .pw-row { display: flex; align-items: center; gap: 12px; margin-bottom: 6px; }
  /* The single "one password for all volumes" prompt, set off from the
     per-volume status lines below it. */
  .pw-row.shared { padding-bottom: 10px; margin-bottom: 10px; border-bottom: 1px dashed var(--border-strong); }
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
  .pw-done { display: inline-flex; align-items: center; gap: 5px; font-size: 13px; font-weight: 600; color: var(--success-text); }
  .pw-err { font-size: 12px; color: var(--error-text); font-weight: 600; margin: 0 0 6px 204px; }
  /* A volume we cannot open: a sentence in place of the password box. */
  .pw-nokey { flex: 1 1 auto; font-size: 12px; color: var(--text-muted); line-height: 1.4; }
  .pw-hint { font-size: 12px; color: var(--text-muted); margin-top: 10px; }
  /* The consequence of leaving a lock closed reads as a warning, not as
     small print: it changes what the copy contains. */
  .pw-hint.warn { color: var(--warning-text); }
  .pw-hint code { font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 11.5px; padding: 1px 5px; border-radius: 4px; background: var(--sunken); color: var(--text-secondary); }
  .btn:disabled { opacity: .45; cursor: default; }
  .seg-label { font-size: 12px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta { position: relative; z-index: 1; font-size: 11px; color: var(--text-secondary); line-height: 1.35; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .seg-meta .muted { color: var(--text-muted); }
  .check { position: absolute; top: 5px; right: 6px; z-index: 2; width: 16px; height: 16px; border-radius: 3px; background: var(--primary); display: flex; align-items: center; justify-content: center; }
  .connector { display: flex; flex-direction: column; align-items: center; margin: 20px 0; }
  .connector .dash { width: 2px; height: 14px; background: repeating-linear-gradient(180deg, var(--border-strong) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 44px; height: 44px; border-radius: 50%; background: var(--success); display: flex; align-items: center; justify-content: center; margin: 4px 0; }
  .dest-card { display: flex; align-items: center; gap: 14px; padding: 12px; border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); }
  .dest-fields { flex: 1; display: flex; align-items: center; gap: 12px; min-width: 0; }
  .dest-fields input { flex: 1; min-width: 0; padding: 8px 10px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 13px; font-family: var(--font); background: var(--input-bg); color: var(--text); }
  .dest-fields .link { font-weight: 600; }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .note { font-size: 12px; color: var(--text-muted); margin-top: 18px; }
  /* What the service does for the schedule: an information plaque, as in the
     design — the same shape as .note.warn below. */
  .note.info { color: var(--info-text); background: var(--info-soft); border: 1px solid var(--info-line); padding: 8px 12px; border-radius: 8px; }
  .warn-note { font-size: 13px; color: var(--error-text); background: var(--error-soft); border: 1px solid var(--error-line); padding: 10px 14px; border-radius: 8px; margin-top: 14px; font-weight: 600; }
  /* Same warning, docked just above the Отмена/Назад/Далее footer. */
  .footer-warn { margin: 0 34px 12px; }
  /* .note's <code> comes from translated messages ({@html}), out of reach of a scoped selector. */
  code, .note :global(code) { background: var(--sunken); color: var(--text-secondary); padding: 1px 5px; border-radius: 4px; }
  .sched { display: flex; gap: 50px; }
  .sched-kinds { display: flex; flex-direction: column; gap: 14px; }
  .sched-right { display: flex; flex-direction: column; gap: 16px; padding-top: 2px; }
  .radio { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--text); cursor: pointer; }
  .radio.blk { margin: 10px 0; }
  .days { display: flex; border: 1px solid var(--border-strong); border-radius: 6px; overflow: hidden; }
  .day { padding: 9px 12px; border: none; border-right: 1px solid var(--border); background: var(--card-bg); color: var(--text); font-size: 13px; cursor: pointer; font-family: var(--font); }
  .day:hover:not(.on) { background: var(--panel2); }
  .day.on { background: var(--primary); color: #fff; }
  .at { display: flex; align-items: center; gap: 10px; font-size: 14px; color: var(--text-secondary); }
  .hint-sm { font-size: 12px; color: var(--text-muted); margin-top: 8px; }
  .delay-min { display: flex; align-items: center; gap: 8px; font-size: 13px; color: var(--text-secondary); margin-left: 26px; }
  .delay-min select, .at select { padding: 3px 6px; border: 1px solid var(--border-strong); border-radius: 6px; background: var(--input-bg); color: var(--text); }
  .event-opts { display: flex; flex-direction: column; gap: 12px; }
  .event-opts .dev-badge { margin-left: 8px; }
  .at input { padding: 7px 10px; border: 1px solid var(--border-strong); border-radius: 6px; background: var(--input-bg); color: var(--text); }
  .adv-schedule { margin-top: 20px; display: flex; flex-direction: column; gap: 12px; }
  .chk { display: flex; align-items: center; gap: 9px; font-size: 14px; color: var(--text); cursor: pointer; }
  /* A way of doing the check above, so it sits under it and greys out with it. */
  .sub-chk { margin: 8px 0 0 26px; }
  .sub-hint { margin: 4px 0 0 52px; font-size: 12.5px; line-height: 1.45; color: var(--text-muted); }
  .sub-chk.dim, .sub-hint.dim { opacity: 0.45; cursor: default; }
  .chk.dim { opacity: 0.55; }
  .field-label { font-size: 15px; font-weight: 600; color: var(--head); margin-bottom: 10px; }
  .field-label.mt { margin-top: 26px; }
  .num { width: 60px; padding: 5px 8px; border: 1px solid var(--border-strong); border-radius: 6px; margin: 0 4px; background: var(--input-bg); color: var(--text); }
  .num:disabled { background: var(--sunken); border-color: var(--border); color: var(--text-muted); }
  .excl-tabs { display: flex; gap: 24px; margin-top: 18px; border-bottom: 1px solid var(--border); }
  .excl-tab { padding: 8px 2px 10px; background: none; border: none; border-bottom: 2px solid transparent; color: var(--text-muted); font-size: 13px; font-weight: 600; font-family: var(--font); cursor: pointer; }
  .excl-tab.active { color: var(--head); border-bottom-color: var(--primary); }
  .note.sub { margin-top: 10px; }
  /* A line of prose that happens to contain a number field — reads as a
     sentence, not as a labelled input. */
  .chk-line { display: flex; align-items: center; font-size: 14px; color: var(--text); }
  /* A checkbox that only makes sense under the one above it. */
  .chk.sub { margin-left: 26px; }
  .chk:has(input:disabled) { color: var(--text-muted); cursor: default; }
  .note.warn { color: var(--warning-text); background: var(--warning-soft); border: 1px solid var(--warning-line); padding: 8px 12px; border-radius: 8px; }
  .boot-hint { margin-top: 10px; font-size: 12.5px; line-height: 1.5; color: var(--warning-text); background: var(--warning-soft); border: 1px solid var(--warning-line); padding: 8px 12px; border-radius: 8px; }
  .excl-list { border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); max-height: 260px; overflow: auto; margin-top: 12px; }
  .body.fill { display: flex; flex-direction: column; }
  .body.fill .excl-list { flex: 1 1 0; min-height: 180px; max-height: none; }
  .excl-row { display: flex; justify-content: space-between; align-items: center; padding: 6px 14px; font-size: 13px; color: var(--text); border-bottom: 1px solid var(--border); }
  .excl-row:hover { background: var(--panel2); }
  /* 18 px square: the line box the old «×» glyph (18px, line-height 1) took. */
  .x { width: 18px; height: 18px; display: flex; align-items: center; justify-content: center; flex-shrink: 0; background: none; border: none; border-radius: 4px; color: var(--text-muted); cursor: pointer; }
  .x:hover { background: var(--track); color: var(--head); }
  .excl-add { display: flex; gap: 10px; align-items: center; margin-top: 12px; }
  .excl-add input { flex: 1; padding: 8px 12px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 13px; background: var(--input-bg); color: var(--text); }
  .adv { display: flex; flex-direction: column; gap: 4px; }
  .adv-group { padding: 12px 0; border-bottom: 1px solid var(--border); }
  /* The password form that opens under the encryption switch: indented to the
     checkbox's text, so it reads as belonging to it. */
  .enc-form { margin: 10px 0 2px 26px; max-width: 760px; }
  .enc-form .field-label { margin-top: 0; }
  /* The two password boxes share the row: on a narrow window they fall back
     to one under the other rather than shrinking into uselessness. */
  .enc-pair { display: flex; gap: 16px; flex-wrap: wrap; }
  .enc-cell { display: flex; flex-direction: column; flex: 1 1 260px; min-width: 240px; }
  .adv-title { font-size: 13px; font-weight: 700; color: var(--accent-text); margin-bottom: 8px; }
  /* A setting's details, indented to its checkbox's text. */
  .adv-line { display: flex; align-items: center; gap: 6px; margin: 8px 0 0 26px; font-size: 13px; color: var(--text-secondary); }
  .adv-line select { padding: 4px 6px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 13px; font-family: var(--font); background: var(--input-bg); color: var(--text); }
  .adv-hint { margin: 4px 0 8px 26px; font-size: 12.5px; line-height: 1.45; color: var(--text-muted); }
  .adv-hint.flush { margin-left: 0; margin-top: 6px; }
  .dev-badge { font-size: 11px; font-weight: 700; color: var(--warning-text); background: var(--warning-soft); padding: 2px 8px; border-radius: 10px; }
  .wz-error { padding: 8px 34px; color: var(--error-text); font-size: 13px; background: var(--error-soft); }
  /* .footer is global (styles/global.css) — same bar on every page. */
  .spacer { flex: 1; }
  .btn { border-radius: 6px; padding: 10px 26px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid transparent; transition: background-color .15s ease; }
  .btn.small { padding: 7px 14px; font-size: 13px; }
  .btn.ghost { background: var(--card-bg); border-color: var(--border-strong); color: var(--text); }
  .btn.ghost:hover:not(:disabled) { background: var(--panel2); }
  .btn.primary { background: var(--primary); color: #fff; }
  .btn.primary:hover:not(:disabled) { background: var(--primary-hover); }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover:not(:disabled) { background: var(--green-hover); }
</style>
