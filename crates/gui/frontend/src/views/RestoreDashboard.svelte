<script lang="ts">
  import { formatBytes, formatDate, formatClock } from '../lib/format';
  // The partition editor is in-memory only: every click edits `slots` and
  // `mappings` and nothing else. A disk is touched at exactly one point —
  // `api.applyRestorePlan`, below, after the dry run has passed with no errors
  // and the user has typed a confirmation.
  import {
    archiveSecretFor, engine, cancelCurrent, clearRestoreResult, needsArchiveKey,
    refreshVolumes, rememberArchiveSecret, restoreClock, restoreEtaMs,
  } from '../lib/engineStore.svelte';
  import { listen } from '@tauri-apps/api/event';
  import { jobs, lastSuccessfulRun } from '../lib/jobStore.svelte';
  import { api } from '../lib/api';
  import ArchiveUnlockModal from '../components/ArchiveUnlockModal.svelte';
  import InfoModal from '../components/InfoModal.svelte';
  import PasswordField from '../components/PasswordField.svelte';
  import DiskMap from '../components/DiskMap.svelte';
  import { mapperKind, diskCaption } from '../lib/diskNames';
  import { isWindows } from '../lib/platformStore.svelte';
  import { diskSegments, msrKept } from '../lib/diskLayout';
  import type { MapSlot } from '../components/DiskMap.svelte';
  import { m } from '../lib/paraglide/messages';
  import { esc } from '../lib/i18n.svelte';
  import type {
    Job, DiskInfo, PartitionInfo, ImageInfo, RestoreRunState, DryRunResult, OpRecord,
    RestorePlan, RestoreOp, RestoreMode, PartitionSpec, PartRef, FormatFs,
    CheckContext, CheckResult, LetterChange, ArchiveSecret, LockedImage,
  } from '../lib/types';

  interface Props {
    job: Job;
    /** Whose copies to look for in `job.dest_folder`: the job id their files
     *  carry. A job's own id for a job; for copies added by hand, the id found
     *  in the files — they may belong to no job on this machine at all. */
    chainJobId?: string;
  }
  let { job, chainJobId }: Props = $props();

  let confirmOpen = $state(false);
  let confirmText = $state('');

  // The machine's boot menu gets its own state further down, next to
  // `wholeDiskRestore` — the checkbox it depends on.
  let bootDialogOpen = $state(false);
  let restoreBootEntries = $state<boolean | null>(null);
  let formError: string | null = $state(null);
  let stubMsg: string | null = $state(null);

  const lastRun = $derived(job.last_run);
  /** The newest run that left a copy behind — not `lastRun`, which one failed
   *  run turns into a copy-less record (see `lastSuccessfulRun`). */
  const lastGood = $derived(lastSuccessfulRun(job));

  // Strings, so that a reload of the job list or the history — new objects,
  // same values — does not read the copies again (for an encrypted one that
  // is a key derivation every time).
  const recordedPath = $derived(lastGood?.image_path ?? '');
  const destFolder = $derived(job.dest_folder);
  const matchJobId = $derived(chainJobId ?? job.id);
  const samePath = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

  // --- The copies of this job: every point in time the ‹ › arrows page -------
  //
  // Found by scanning the job's folder for files that carry its id — no
  // jobs.json involved, so it works on the boot medium and for copies added by
  // hand. An ENCRYPTED copy is listed too: its date and job id are in the open
  // header, only the contents are sealed. It used to be left out until a
  // password was typed, so a job whose copies are all encrypted had arrows that
  // never moved (2026-09-25: two copies of C:, one date on screen).

  /** One point-in-time copy. `info` is what the file says about itself: null
   *  while it is sealed and no key has been typed, or while it is being read. */
  interface Version {
    path: string;
    created_at: string;
    sealed: boolean;
    info: ImageInfo | null;
    reading: boolean;
  }
  let versions = $state<Version[]>([]);
  let copyIndex = $state(0);
  /** The folder scan is still in flight. Until it lands there is nothing to say
   *  about the copy — and saying «диск этой копии не подключён» in the meantime
   *  is a red warning that flashes on every visit and is wrong every time. */
  let copiesLoading = $state(true);
  $effect(() => {
    const folder = destFolder;
    const jid = matchJobId;
    const prefer = recordedPath;
    if (!folder || !jid || jid.startsWith('file:')) {
      versions = [];
      copyIndex = 0;
      copiesLoading = false;
      return;
    }
    let cancelled = false;
    copiesLoading = true;
    void (async () => {
      const [plain, sealed] = await Promise.all([
        api.listImageBackups(folder).catch(() => [] as ImageInfo[]),
        api.listLockedImages(folder).catch(() => [] as LockedImage[]),
      ]);
      if (cancelled) return;
      const list: Version[] = [
        ...plain
          .filter((i) => i.job_id === jid)
          .map((i) => ({ path: i.image_path, created_at: i.created_at, sealed: false, info: i, reading: false })),
        ...sealed
          .filter((l) => l.job_id === jid)
          .map((l) => ({ path: l.image_path, created_at: l.created_at, sealed: true, info: null, reading: false })),
      ].sort((a, b) => (a.created_at < b.created_at ? 1 : -1)); // newest first
      versions = list;
      // Open on the copy the job's history names, when it is among them.
      copyIndex = Math.max(0, list.findIndex((v) => samePath(v.path, prefer)));
      copiesLoading = false;
      // Keys typed earlier in this session still open their copies.
      void openSealed();
    })();
    return () => { cancelled = true; };
  });

  /** Read the sealed copies that are not open yet, with `secret` or with the
   *  key already typed for each. One job, one key: the password that opened
   *  one copy opens the rest. Each costs a key derivation (Argon2id, about a
   *  second), so they are read one by one, in the background. A file that
   *  refuses the key stays sealed and asks for its own. */
  async function openSealed(secret?: ArchiveSecret) {
    for (const v of versions) {
      if (!v.sealed || v.info || v.reading) continue;
      const key = secret ?? archiveSecretFor(v.path);
      if (!key) continue;
      v.reading = true;
      try {
        v.info = await api.readImageInfo(v.path, key);
        rememberArchiveSecret(v.path, key);
      } catch {
        // Not this key's copy, or a damaged one.
      } finally {
        v.reading = false;
      }
    }
  }

  /** The copy on screen, when the folder gave any. */
  const activeVersion = $derived(versions[copyIndex] ?? null);
  const hasMultiple = $derived(versions.length > 1);

  // --- When the folder gives nothing ------------------------------------------
  // The folder may be unreadable (a disk that is not plugged in) or the copy an
  // old one without metadata. Then the one image the history names is read on
  // its own, as before.
  let fallbackInfo = $state<ImageInfo | null>(null);
  let fallbackLocked = $state(false);
  let fallbackLoading = $state(true);
  $effect(() => {
    const recorded = recordedPath;
    if (copiesLoading) return;
    if (versions.length > 0 || !recorded) {
      fallbackInfo = null;
      fallbackLocked = false;
      fallbackLoading = false;
      return;
    }
    let cancelled = false;
    fallbackLoading = true;
    void readCopy(recorded).then((found) => {
      if (cancelled) return;
      fallbackInfo = found?.info ?? null;
      fallbackLocked = found?.locked ?? false;
      fallbackLoading = false;
    });
    return () => { cancelled = true; };
  });

  /** One copy's description. `locked`: the file is there, encrypted, and no
   *  key has been typed for it yet. Null when it cannot be read at all. */
  async function readCopy(
    path: string,
  ): Promise<{ info: ImageInfo | null; locked: boolean } | null> {
    // The key for an encrypted copy is collected once per session; the
    // designer reads the copy several times and must not ask again.
    const secret = archiveSecretFor(path);
    try {
      return { info: await api.readImageInfo(path, secret), locked: false };
    } catch {
      // Refused for want of a key is not a broken file: it is a question for
      // whoever is at the screen, and the screen asks it.
      if (!secret && (await needsArchiveKey(path))) return { info: null, locked: true };
      return null;
    }
  }

  /** The file on screen and what it says about itself. */
  const currentPath = $derived(activeVersion?.path ?? recordedPath);
  /** When the copy on screen was made. A sealed copy says it in its open
   *  header, so the date is right before any password too. */
  const copyDate = $derived.by(() => {
    const at = activeVersion?.info?.created_at ?? activeVersion?.created_at;
    if (at) return formatDate(at);
    if (fallbackInfo) return formatDate(fallbackInfo.created_at);
    return lastGood ? formatDate(lastGood.finished_at) : '';
  });
  const activeInfo = $derived(activeVersion ? activeVersion.info : fallbackInfo);
  /** This read is in flight — for an ENCRYPTED copy that is Argon2id, seconds,
   *  not a blink, and that is how long a red «диск не подключён» used to stand
   *  on screen before withdrawing itself. */
  const infoLoading = $derived(activeVersion ? activeVersion.reading : fallbackLoading);
  /** The copy on screen is encrypted and no key opens it yet.
   *
   *  Everything the image says about itself — which partitions it holds, the
   *  layout, the build that wrote it — is sealed, and the service refuses to
   *  restore it without the key (§7.4). A JOB's copy never had its key asked
   *  for, so this screen drew the job's own record instead, labelled the MSR
   *  «не в бэкапе», and the restore stopped at the preview with nowhere to type
   *  the password (2026-09-25). */
  const keyNeeded = $derived(
    activeVersion
      ? activeVersion.sealed && !activeVersion.info && !activeVersion.reading
      : fallbackLocked,
  );

  // --- Opening an encrypted copy (§7.4) ---
  // Asked with a button, not by a dialog that jumps out on arrival: looking at
  // a copy is not yet deciding to restore it. «Восстановить» asks too, first,
  // because the map it would restore by is only the job's guess until then.
  let unlock = $state({ open: false, busy: false, error: '' });
  const unlockClosed = { open: false, busy: false, error: '' };
  /** «Восстановить» raised the dialog: carry on with it once the key is in. */
  let unlockThenRestore = false;

  function askKey(thenRestore = false) {
    unlockThenRestore = thenRestore;
    unlock = { open: true, busy: false, error: '' };
  }

  async function submitUnlock(secret: ArchiveSecret) {
    const path = currentPath;
    if (!path) return;
    unlock.busy = true;
    unlock.error = '';
    const info = await api.readImageInfo(path, secret).catch((e) => {
      // Stay open: a wrong password is a thing to retype.
      unlock.error = String(e);
      unlock.busy = false;
      return null;
    });
    if (!info) return;
    rememberArchiveSecret(path, secret);
    const v = versions.find((x) => samePath(x.path, path));
    if (v) v.info = info;
    else {
      fallbackInfo = info;
      fallbackLocked = false;
    }
    unlock = { ...unlockClosed };
    // The rest of the job's copies, on the same key, after the dialog closed
    // and with the copy the user is looking at already on screen.
    void openSealed(secret);
    if (unlockThenRestore) await askConfirm();
  }

  // Excluded-files line: prefer the count embedded in the active image (works
  // from a file without jobs.json), fall back to the run record.
  const excluded = $derived({
    files: activeInfo?.excluded_files ?? lastGood?.excluded_files ?? 0,
    bytes: activeInfo?.excluded_bytes ?? lastGood?.excluded_bytes ?? 0,
  });

  // Real file-category breakdown of the archived data — same source as the
  // backup dashboard (collected during the backup's file walk).
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
    // Prefer the breakdown embedded in the image (works without jobs.json);
    // fall back to the run record for images taken before it was embedded.
    const c = activeInfo?.categories ?? lastGood?.categories;
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
    const ex = excluded.bytes;
    const total = kept + ex;
    if (total <= 0) return null;
    const out: { name: string; color: string; bytes: number; pct: number }[] = catData.map((c) => ({
      name: c.name(),
      color: c.color,
      bytes: c.bytes,
      pct: (c.bytes / total) * 100,
    }));
    if (ex > 0) out.push({ name: m.cat_excluded(), color: EXCLUDED_COLOR, bytes: ex, pct: (ex / total) * 100 });
    return out;
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
  /** The volumes this copy was made of. A job says; a copy added by hand
   *  (made by the CLI, or elsewhere) has no job, and then its own record does
   *  (`source_device`). Without that a copy of a volume that is right here
   *  read "диск этой копии не подключён", and with no map there was nothing
   *  to click for step 2 — no restore at all. */
  const copySources = $derived(
    job.sources.length > 0
      ? job.sources
      : activeInfo?.source_device
        ? [activeInfo.source_device]
        : [],
  );
  /** Is this partition one the copy was made of? By the name the job keeps
   *  (`/dev/disk/by-partuuid/…`, `C:`) or, on Linux, by the kernel's name — a
   *  copy made from the command line names `/dev/vdb1`. */
  function isCopySource(p: PartitionInfo): boolean {
    const dev = partDevice(p);
    return (
      (!!dev && copySources.includes(dev)) ||
      (!!p.kernel_device && copySources.includes(p.kernel_device))
    );
  }
  /** The same, as the devices the map highlights. */
  const copySourceDevices = $derived(
    engine.disks.flatMap((d) =>
      d.partitions.filter(isCopySource).map((p) => partDevice(p) ?? ''),
    ).filter((dev) => dev !== '').concat(copySources),
  );
  function diskHasSelection(d: DiskInfo): boolean {
    if (isBootableCopy(d)) return true;
    return d.partitions.some(isCopySource);
  }
  // The archived disk map, in priority order:
  //  1. the snapshot embedded in the IMAGE FILE (works without jobs.json / on a
  //     Boot-CD; also correct when the original disk is detached),
  //  2. the snapshot stored on the job at creation (jobs.json),
  //  3. a live query of currently-attached disks (pre-snapshot backups).
  const rawBackedUpDisks = $derived(
    activeInfo?.source_disk_layout
      ? [activeInfo.source_disk_layout]
      : job.source_disks && job.source_disks.length > 0
        ? job.source_disks
        : engine.disks.filter(diskHasSelection),
  );
  /** Is this a whole-disk copy? The copy's own record says. Until it can be
   *  read — an encrypted copy nobody has unlocked yet — the job's does, by the
   *  same rule the wizard shows: a job that captures a disk writes one
   *  whole-disk image of it, and that image keeps the MSR slot when Windows is
   *  in it (`DiskLayout::retain_for_backup`). */
  const isWholeDisk = $derived(
    activeInfo
      ? activeInfo.is_whole_disk
      : !!rawBackedUpDisks[0] && isBootableCopy(rawBackedUpDisks[0]),
  );
  // For a partition that is IN the copy, the image's manifest knows the
  // filesystem better than the disk snapshot does. The snapshot is not wrong —
  // before decryption `Unknown` + "зашифрован" is the only honest reading of a
  // LUKS container — but the copy holds the volume in the clear, so showing a
  // padlock over what restores as plain btrfs would mislead (D14, R8).
  // Partitions outside the copy keep their padlock: nothing opened those.
  const backedUpDisks = $derived.by(() => {
    const fs = activeInfo?.part_filesystems ?? [];
    if (fs.length === 0) return rawBackedUpDisks;
    const byIndex = new Map(fs.map((f) => [f.index, f.filesystem]));
    return rawBackedUpDisks.map((d) => ({
      ...d,
      partitions: d.partitions.map((p) => {
        const known = byIndex.get(p.index);
        return known ? { ...p, filesystem: known, is_encrypted: false } : p;
      }),
    }));
  });
  const systemDisks = $derived(engine.disks);
  /** Disks a copy can be restored onto. Not a device-mapper volume: the
   *  inventory lists an opened LUKS container or an LVM volume as a "disk" of
   *  its own, but it has no partition table to write and is a view of another
   *  disk. And on Linux not the disk the running system stands on — the
   *  running root, its /boot and ESP are there, and that disk is restored
   *  from the boot medium. (Windows keeps its own rule: the service protects
   *  the running system's partitions and its ESP, and a data partition of
   *  that disk is still a target.) */
  function isRunningSystemDisk(d: DiskInfo): boolean {
    return !isWindows() && d.partitions.some((p) => p.is_system);
  }
  const targetDisks = $derived(
    systemDisks.filter((d) => !mapperKind(d) && !isRunningSystemDisk(d)),
  );
  const hiddenSystemDisks = $derived(
    systemDisks.filter((d) => !mapperKind(d) && isRunningSystemDisk(d)),
  );

  // --- Restore-mapping wizard (what → where) --------------------------------
  // Step 1: pick a target disk below (others collapse; source selection clears).
  // Step 2: click a source partition (turns yellow), then a destination one —
  //         it fits → both green; too small → destination red + a message.
  // Step 3: repeat, then Восстановить.
  let targetDiskId = $state<string | null>(null);
  let srcPart = $state<number | null>(null); // yellow source partition index
  let mappings = $state<{ src: number; dst: number }[]>([]);
  let badTarget = $state<number | null>(null); // last destination that didn't fit
  // A function, not a string: the note is drawn in whatever language the
  // window speaks when it is drawn.
  let fitMsg = $state<(() => string) | null>(null);
  // "загрузочный" checkbox — restore the whole bootable set (all partitions of
  // the copy + boot data). Pre-checked while no target disk is chosen when the
  // copy actually holds boot data; picking a target clears it so the user opts
  // in deliberately.
  let wholeDiskRestore = $state(false);
  $effect(() => {
    if (!targetDiskId) wholeDiskRestore = isBootable;
  });

  // ---- The machine's boot menu -------------------------------------------
  //
  // A boot entry lives in the motherboard's NVRAM, not on the disk, so a copy
  // that holds every partition holds none of the menu that starts them. A
  // bootable copy therefore carries the entries as well — and putting them
  // back is the one thing a restore does whose effect outlives the disk it was
  // restored onto.
  //
  // Hence its own question, asked last and answered explicitly, with the
  // actual list in front of the user. `restoreBootEntries` stays null until
  // they answer, and nothing goes into the plan on a null.
  /** The entries this copy carries, if it carries any. */
  const bootEntries = $derived(activeInfo?.boot_entries ?? null);
  /** Is there a question to ask at all? Only for a copy that has them, onto a
   *  disk the user marked загрузочный. */
  const bootQuestionApplies = $derived(!!bootEntries && wholeDiskRestore);
  /** The entries name partitions by GUID, so a copy from elsewhere restores
   *  menu items pointing at partitions this machine does not have. */
  const fromAnotherMachine = $derived(
    !!bootEntries &&
      !!bootEntries.this_machine &&
      bootEntries.machine.toLowerCase() !== bootEntries.this_machine.toLowerCase(),
  );

  // Partitions actually inside this copy — from the image's embedded metadata,
  // falling back to the job's selected source devices. Anything else on the disk
  // is drawn as an empty "не в бэкапе" slot.
  const backedUpParts = $derived.by(() => {
    const fromImage = activeInfo?.backed_up_parts ?? [];
    if (fromImage.length > 0) return fromImage;
    const d = backedUpDisks[0];
    if (!d) return [] as number[];
    return d.partitions
      .filter(isCopySource)
      .map((p) => p.index);
  });

  const targetDisk = $derived(targetDisks.find((d) => d.device_id === targetDiskId) ?? null);
  const srcDisk = $derived(backedUpDisks[0] ?? null);

  // A GPT stores partition positions as LBA numbers, so the same table means
  // different byte offsets on a disk with a different logical sector size —
  // a 512e layout cannot be reproduced byte-identically on a 4Kn disk (and vice
  // versa). Detect it before anything is mapped and say so plainly.
  const srcSector = $derived(activeInfo?.sector_size || srcDisk?.sector_size_logical || 0);
  const dstSector = $derived(targetDisk?.sector_size_logical ?? 0);
  const sectorMismatch = $derived(
    srcSector > 0 && dstSector > 0 && srcSector !== dstSector,
  );
  /** Microsoft Reserved: no filesystem, nothing to copy — but a whole-disk copy
   *  that carries Windows keeps its GPT entry (`DiskLayout::retain_for_backup`),
   *  and the restore recreates it. Shown as part of the copy so it does not
   *  read as omitted. */
  const layoutOnlyParts = $derived.by(() => {
    // The copy's own layout says, once it can be read.
    if (activeInfo) return activeInfo.layout_only_parts ?? [];
    // Until then, the job's record by the engine's rule. Only a whole-disk copy
    // keeps the MSR slot; a per-volume copy has nothing to do with it, and
    // calling it "в копии" there would be a lie.
    const d = rawBackedUpDisks[0];
    if (!isWholeDisk || !d) return [] as number[];
    return msrKept(d, (p) => backedUpParts.includes(p.index));
  });

  // What the copy says each encrypted partition's container was.
  function encOf(index: number) {
    return (activeInfo?.encrypted_parts ?? []).find((e) => e.index === index);
  }
  // Partitions of the copy whose ORIGINAL was an encrypted container and whose
  // copy holds it in the clear. The patched list above deliberately drops the
  // padlock (the copy holds the volume decrypted), so the fact has to be read
  // off the raw snapshot — and it matters: the restore cannot put the
  // container back.
  //
  // A container copied VERBATIM is not one of these: it comes back encrypted,
  // exactly as it was, and warning about it would be a warning about nothing.
  const decryptedParts = $derived(
    (rawBackedUpDisks[0]?.partitions ?? [])
      .filter((p) => p.is_encrypted && backedUpParts.includes(p.index))
      .filter((p) => encOf(p.index)?.captured !== 'sector')
      .map((p) => p.index),
  );
  // Copies made before the container kind was recorded say nothing about it;
  // then the screen must not name one. BitLocker and LUKS fail in different
  // directions, and telling a Windows user to rebuild their initramfs is worse
  // than telling them nothing.
  const encKinds = $derived(
    new Set(decryptedParts.map((i) => encOf(i)?.kind ?? 'unknown')),
  );
  const allBitLocker = $derived(encKinds.size === 1 && encKinds.has('bitlocker'));
  const allLuks = $derived(
    encKinds.size > 0 && [...encKinds].every((k) => k.startsWith('luks')),
  );

  // Encrypted partitions whose container header the copy kept — those can be
  // restored with their encryption intact (D19).
  const reencryptable = $derived(
    (activeInfo?.luks_header_parts ?? []).filter((i) => decryptedParts.includes(i)),
  );
  const canReencrypt = $derived(reencryptable.length > 0);

  /** A partition named the way a person recognises it: number, then what it is.
   *  A bare "раздел 3" makes the reader go and find it on the map. */
  function describePart(p: PartitionInfo | undefined | null, index: number): string {
    if (!p) return String(index);
    const bits: string[] = [];
    const label = p.volume_label || (p.volume_letter ? `${p.volume_letter}:` : '');
    if (label) bits.push(m.rd_part_label({ label }));
    if (p.size_bytes > 0) bits.push(formatBytes(p.size_bytes));
    const fs = (p.filesystem || '').replace(/^unknown$/i, '');
    if (fs) bits.push(fs === 'ntfs' ? 'NTFS' : fs.charAt(0).toUpperCase() + fs.slice(1));
    return bits.length > 0 ? `${index} (${bits.join(', ')})` : String(index);
  }
  /** Source partition inside the copy, by its index. */
  function describeSrc(index: number): string {
    return describePart(srcDisk?.partitions.find((p) => p.index === index), index);
  }
  /** Target partition, as the designer currently has it. */
  function describeDst(index: number): string {
    return describePart(curParts.find((p) => p.index === index), index);
  }

  // Partitions the whole-disk layout must reproduce on the target.
  //
  // Not the same as "partitions whose data was copied": Microsoft Reserved has
  // no filesystem and nothing to copy, but the copy keeps its GPT entry and
  // Windows expects the slot to be there. Leaving it out wrote a two-entry
  // table where the source had three, and the restored disk came out without
  // its MSR. It is placed like any other partition and simply gets no data.
  const srcBackupParts = $derived(
    (srcDisk?.partitions ?? []).filter(
      (p) => backedUpParts.includes(p.index) || layoutOnlyParts.includes(p.index),
    ),
  );
  /** Does this copy hold what a machine starts from — the EFI System Partition,
   *  or the active partition of an MBR disk (`is_boot` says either)? That, not
   *  "it is an image of the whole disk", is what «загрузочный» means. Every job
   *  writes a whole-disk image now, bootable or not, and keying the checkbox on
   *  that showed a copy of the data volume D: alone as «загрузочный», ticked
   *  (2026-09-25). A copy made with «Загрузочная копия диска» counts whatever
   *  it holds — it carries the machine's boot entries too. */
  const isBootable = $derived(
    isWholeDisk &&
      (!!activeInfo?.boot_entries ||
        !!job.disk_source?.bootable ||
        srcBackupParts.some((p) => p.is_boot)),
  );
  const srcTotalBytes = $derived(srcBackupParts.reduce((s, p) => s + p.size_bytes, 0));
  /** Room the copy needs on the target — not the same as the sum of its
   *  partitions. Each one starts on a 1 MiB boundary, and the source's sizes
   *  are not round numbers, so a few mebibytes of padding come with them. */
  const srcNeedBytes = $derived(
    srcBackupParts.reduce(
      // The last partition's end is NOT rounded up: that padding only exists to
      // align the next start, and counting it made a disk look too small for the
      // copy taken from it. Same rule as the placement in clickFreeSpace.
      (cursor, p, k) =>
        k === srcBackupParts.length - 1
          ? cursor + p.size_bytes
          : Math.ceil((cursor + p.size_bytes) / (1024 * 1024)) * 1024 * 1024,
      0,
    ),
  );

  // --- Simple partition editor on the target disk ---------------------------
  // The layout is an explicit list of cells. Deleting a partition turns ITS cell
  // into a free cell with the same boundaries, so nothing around it moves;
  // creating splits a free cell in place. Nothing is written to the disk.
  let slots = $state<MapSlot[] | null>(null);
  let previewIdx = $state<number[]>([]); // partitions the restore would create
  let menu = $state<{ x: number; y: number; part?: PartitionInfo; offset?: number; size?: number; slot: number } | null>(null);
  let delConfirm = $state<PartitionInfo | null>(null);
  const MIB = 1024 * 1024;
  const GIB = 1024 * 1024 * 1024;
  const TIB = 1024 * GIB;
  /** GPT partition-type GUIDs (the same ones `common::gpt` uses). The firmware
   *  finds the boot partition by this GUID, so it is not cosmetic. */
  const GPT_TYPES: { guid: string; name: () => string }[] = [
    { guid: 'EBD0A0A2-B9E5-4433-87C0-68B6B72699C7', name: m.rd_gpt_basic_data },
    { guid: 'C12A7328-F81F-11D2-BA4B-00A0C93EC93B', name: () => 'EFI System (ESP)' },
    { guid: 'E3C9E316-0B5C-4DB8-817D-F92DF00215AE', name: () => 'Microsoft Reserved (MSR)' },
    { guid: 'DE94BBA4-06D1-4D40-A16A-BFD50179D6AC', name: () => 'Windows Recovery' },
    { guid: '0FC63DAF-8483-4772-8E79-3D69D8477DE4', name: () => 'Linux filesystem' },
    { guid: '0657FD6D-A4AB-43C4-84E5-0933C84B4F4F', name: () => 'Linux swap' },
  ];
  /** Схему разметки задаёт КОПИЯ, а не то, что лежит на цели сейчас.
   *
   *  Восстановление целого диска переписывает таблицу целиком, поэтому
   *  единственный правильный ответ — тот, что был у исходного диска. Спрашивать
   *  цель здесь означало решать судьбу копии по случайному свойству пустого
   *  диска: Windows отвечает про неразмеченный диск «MBR» (2026-09-20, в
   *  виртуальной машине), и копия UEFI-диска с ESP восстановилась как MBR с
   *  загрузчиком BIOS — то есть в незагружаемом виде. При восстановлении
   *  ОТДЕЛЬНЫХ разделов таблица остаётся чужой, и тогда спрашивать надо цель. */
  const copyTable = $derived(activeInfo?.source_disk_layout?.partition_table ?? null);
  const isGpt = $derived(
    wholeDiskRestore && copyTable
      ? copyTable !== 'mbr'
      : (targetDisk?.partition_table ?? 'gpt') !== 'mbr',
  );
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
  /** The plan the dry run checked and returned — this is the one to apply, not
   *  the one that was sent: the dry run resolves drive-letter conflicts by
   *  editing it. */
  let checkedPlan = $state<DryRunResult['plan'] | null>(null);
  /** State of the restore the service is running (or last ran). Survives this
   *  window being closed and reopened, and a service restart. */
  let planRun = $state<RestoreRunState | null>(null);
  let applyError: string | null = $state(null);
  const planRunning = $derived(planRun?.status === 'running');

  // Pick up a restore already in progress, and follow it from here on.
  $effect(() => {
    let live = true;
    api.getRestoreState().then(
      (st) => { if (live && st) planRun = st; },
      () => {},
    );
    const stop = listen<RestoreRunState>('restore-plan-progress', (e) => {
      if (!live) return;
      const wasRunning = planRun?.status === 'running';
      planRun = e.payload;
      // The disk just changed shape, and nothing else will notice: the map is
      // refreshed when a *drive letter* appears or disappears, and a restore of
      // Linux partitions creates none. Without this the target still reads
      // "Не занято" after a restore that filled it.
      if (wasRunning && planRun.status !== 'running') refreshVolumes();
    });
    return () => {
      live = false;
      stop.then((un) => un()).catch(() => {});
    };
  });

  /** Close the finished run's report. It goes to «Активность» and does not come
   *  back here — a report that reappears on every visit reads as if the restore
   *  had run again. */
  async function dismissRun() {
    const id = planRun?.plan_id;
    planRun = null;
    resetMapping();
    pickAnotherDisk();
    if (id) await api.ackRestoreRun(id).catch(() => {});
    // Re-read the disks on the way out, so the map the user lands on is the one
    // the restore just produced.
    refreshVolumes();
  }
  /** Findings from the last dry run, shown above the queue. */
  let checks = $state<CheckResult[]>([]);
  let letterChanges = $state<LetterChange[]>([]);
  let checking = $state(false);
  let checkError: string | null = $state(null);
  /** D16 — off by default: a sector restore writes only the used blocks, so old
   *  data physically survives in the gaps unless this is on. */
  let wipeFreeSpace = $state(false);
  /** D10 — verify what was written once the plan finishes. */
  let verifyAfter = $state(false);
  /** D19 — put the encrypted container back. On by default whenever the copy
   *  can do it: leaving encryption off silently is how a Linux install stops
   *  booting after an otherwise perfect restore. */
  let reencrypt = $state(true);
  /** Passphrase per source partition, entered here and held only in memory —
   *  it goes to the service with the apply request, never into the plan. */
  let luksPw = $state<Record<number, string>>({});
  /** Result of the last check per partition. Cleared as soon as the field is
   *  edited: a tick next to a password that has since changed is a lie. */
  let luksPwState = $state<Record<number, 'idle' | 'checking' | 'ok' | 'bad' | 'error'>>({});
  let luksPwError = $state<Record<number, string>>({});

  async function checkLuksPassword(part: number) {
    const path = activeInfo?.image_path || currentPath;
    const pw = luksPw[part] ?? '';
    if (!path || !pw) return;
    luksPwState[part] = 'checking';
    try {
      // The copy's own key too: the stored container header is in its sealed
      // half, so without it the check cannot be made at all.
      luksPwState[part] = (await api.verifyLuksPassword(path, part, pw, archiveSecretFor(path)))
        ? 'ok'
        : 'bad';
    } catch (e) {
      luksPwState[part] = 'error';
      luksPwError[part] = String(e);
    }
  }
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

  // Build the initial grid: the shared disk shape (`lib/diskLayout` — the same
  // one the backup wizard and the read-only maps draw), plus this screen's
  // width budget on top of it.
  function buildSlots(d: DiskInfo): MapSlot[] {
    const raw = diskSegments(d);
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
      ? m.rd_step1()
      : step === 2
        ? wholeDiskRestore
          ? m.rd_step2_whole()
          : m.rd_step2_parts()
        : m.rd_step3(),
  );

  function resetMapping() {
    srcPart = null;
    mappings = [];
    badTarget = null;
    fitMsg = null;
    previewIdx = [];
    // The answer belongs to one target disk and one copy. Carrying a "да" from
    // a run the user abandoned to a different disk is the one way this could
    // write firmware nobody agreed to on this attempt.
    restoreBootEntries = null;
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
    if (mappings.some((m) => m.src === p.index)) return; // already mapped
    badTarget = null;
    // Microsoft Reserved holds no data — the copy keeps its place in the
    // partition table and nothing else. It is still drawn as a partition, and
    // that is how anyone reads it, so it maps like one: the two maps stay
    // paired one to one and nothing looks skipped. What it cannot do is carry
    // blocks, so the plan will hold no copy operation for it — said here as a
    // note, not as a refusal.
    fitMsg = layoutOnlyParts.includes(p.index) ? m.rd_fit_msr_no_data : null;
    srcPart = p.index;
  }

  // Whole-disk mode: drop the entire backup layout into a free stretch. The run
  // of adjacent free cells is replaced by the copy's partitions (plus whatever
  // space is left over), so the surrounding cells keep their exact geometry.
  function clickFreeSpace(offset: number, _size: number) {
    if (!wholeDiskRestore || !slots) return;
    // The copy goes down exactly once — a second click would duplicate every
    // partition into the remaining free space.
    if (previewIdx.length > 0) {
      fitMsg = m.rd_fit_already_placed;
      return;
    }
    if (sectorMismatch) {
      const src = srcSector;
      const dst = dstSector;
      fitMsg = () => m.rd_fit_sector_mismatch({ src, dst });
      return;
    }
    const i = slots.findIndex((s) => s.kind === 'free' && s.offset === offset);
    if (i < 0) return;
    const cell = slots[i] as { offset: number; size: number; share: number };
    // Every partition starts on a 1 MiB boundary, so the copy needs a little
    // more room than the sum of its parts: the source's own sizes are not round
    // (this ESP is 1.28 GiB) and each one pushes the next start up to the next
    // boundary. Reproducing the source's misalignment instead would cost a
    // read-modify-write on every write for the life of the disk.
    const start = Math.ceil(cell.offset / MIB) * MIB;
    const offsets: number[] = [];
    let cursor = start;
    srcBackupParts.forEach((sp, k) => {
      offsets.push(cursor);
      const end = cursor + sp.size_bytes;
      // Round up to the next boundary only to align the NEXT partition's start.
      // Doing it after the last one demanded up to 1 MiB that nothing would ever
      // occupy — and made the most important restore of all impossible: a disk
      // whose last partition ends at the last usable LBA (which is where Windows
      // puts it) could not be put back onto itself, short by the 33 sectors of
      // the secondary GPT.
      cursor = k === srcBackupParts.length - 1 ? end : Math.ceil(end / MIB) * MIB;
    });
    const cellEnd = cell.offset + cell.size;
    const needed = cursor - cell.offset;
    if (needed > cell.size) {
      // Exact numbers + the shortfall: rounded ones read as "нужно 130 GiB,
      // свободно 130 GiB", which tells the user nothing.
      const short = needed - cell.size;
      const need = preciseSize(needed);
      const free = preciseSize(cell.size);
      const lack = preciseSize(short);
      fitMsg = () => m.rd_fit_no_room({ need, free, short: lack });
      return;
    }
    pushHistory();
    // The region's width budget is split between the placed partitions (and any
    // leftover free space), so the cells around it do not move.
    const sizes = srcBackupParts.map((p) => p.size_bytes);
    const rest = cellEnd - cursor;
    if (rest > 0) sizes.push(rest);
    // The sub-shares are scaled by the cell's own share, so the readable floor
    // has to be scaled up by the same factor — otherwise a cell that owns 40% of
    // the bar would give its smallest partition 0.4 × the floor.
    const sub = allocateShares(sizes, (minShare * 100) / cell.share).map(
      (s) => (s * cell.share) / 100,
    );

    let next = Math.max(0, ...curParts.map((p) => p.index)) + 1;
    const placed: MapSlot[] = [];
    srcBackupParts.forEach((sp, k) => {
      placed.push({
        kind: 'part',
        p: {
          ...sp,
          index: next,
          offset_bytes: offsets[k],
          volume_device: '',
          volume_letter: null,
        },
        share: sub[k],
      });
      next += 1;
    });
    previewIdx = placed.map((s) => (s as { p: PartitionInfo }).p.index);
    if (rest > 0) placed.push({ kind: 'free', offset: cursor, size: rest, share: sub[sub.length - 1] });
    slots = normalize([...slots.slice(0, i), ...placed, ...slots.slice(i + 1)]);
    fitMsg = null;
  }

  // --- Partition editor actions --------------------------------------------
  function openPartMenu(p: PartitionInfo, ev: MouseEvent) {
    const slot = (slots ?? []).findIndex((s) => s.kind === 'part' && s.p.index === p.index);
    menu = { x: ev.clientX, y: ev.clientY, part: p, slot };
  }
  function openFreeMenu(offset: number, size: number, ev: MouseEvent) {
    const slot = (slots ?? []).findIndex((s) => s.kind === 'free' && s.offset === offset);
    menu = { x: ev.clientX, y: ev.clientY, offset, size, slot };
  }

  // Growing a partition into the unallocated space right after it. The same
  // operation is offered from both sides: "Расширить раздел" on the partition,
  // "Присоединить раздел" on the free region (it joins the partition before it).
  const canExtend = $derived.by(() => {
    const list = slots ?? [];
    const i = menu?.slot ?? -1;
    if (i < 0 || i >= list.length) return -1;
    // From a partition: the next cell must be free. From free space: the
    // previous cell must be a partition. Either way return the PARTITION's slot.
    if (list[i].kind === 'part') return list[i + 1]?.kind === 'free' ? i : -1;
    return i > 0 && list[i - 1].kind === 'part' ? i - 1 : -1;
  });

  function extendPart(partSlot: number) {
    menu = null;
    if (!slots) return;
    const ps = slots[partSlot];
    const fs = slots[partSlot + 1];
    if (!ps || ps.kind !== 'part' || !fs || fs.kind !== 'free') return;
    pushHistory();
    // Grow to the end of the free region — computing from offsets also swallows
    // any sub-MiB alignment sliver between them. Both width budgets add up, so
    // nothing else on the map moves.
    const grown: MapSlot = {
      kind: 'part',
      p: { ...ps.p, size_bytes: fs.offset + fs.size - ps.p.offset_bytes },
      share: ps.share + fs.share,
    };
    slots = normalize([...slots.slice(0, partSlot), grown, ...slots.slice(partSlot + 2)]);
    fitMsg = null;
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
      // "Не форматировать" by default: a partition created here is usually
      // about to receive a sector restore, which brings its own filesystem —
      // picking one up front only sets up an expectation the restore will not
      // meet.
      fs: '',
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
    // Choosing a Windows filesystem implies a Basic Data partition; "не
    // форматировать" leaves the type alone, since that is the case where the
    // user is most likely aiming at something else (Linux, swap, ESP).
    if (fs) createDlg.gptType = GPT_TYPES[0].guid;
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
      fitMsg = m.rd_fit_size_too_small;
      return;
    }
    if (bytes > d.max) {
      const free = formatBytes(d.max);
      fitMsg = () => m.rd_fit_size_too_big({ free });
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
        ops.push(
          m.rd_op_delete({
            name: `${p.volume_letter ? p.volume_letter + ': ' : ''}${p.volume_label || m.rd_no_label()}`,
            size: formatBytes(p.size_bytes),
          }),
        );
      }
    }
    for (const p of cur) {
      // Same partition, bigger than it was → it was extended into free space.
      const was = orig.find((o) => o.index === p.index);
      if (was && p.size_bytes !== was.size_bytes) {
        const how = p.size_bytes > was.size_bytes ? m.rd_op_extend : m.rd_op_shrink;
        ops.push(
          how({
            name: `${p.volume_letter ? p.volume_letter + ': ' : ''}${p.volume_label || m.rd_no_label()}`,
            from: preciseSize(was.size_bytes),
            to: preciseSize(p.size_bytes),
          }),
        );
      }
      if (was || previewIdx.includes(p.index)) continue;
      const meta = newPartMeta.get(p.index);
      // A partition that is about to receive a sector restore is NOT formatted:
      // the filesystem arrives from the copy, and formatting would erase it.
      // Saying otherwise here was the queue promising something the plan never
      // contained.
      const takesData = mappings.some((x) => x.dst === p.index);
      const bits = [formatBytes(p.size_bytes)];
      if (p.filesystem && !takesData) bits.push(p.filesystem.toUpperCase());
      if (meta) {
        bits.push(isGpt ? (GPT_TYPES.find((t) => t.guid === meta.gptType)?.name() ?? m.rd_op_type()) : meta.mbrKind === 'primary' ? m.rd_op_primary() : m.rd_op_logical());
        if (!isGpt && meta.active) bits.push(m.rd_op_active());
        if (meta.letter) bits.push(m.rd_op_letter({ letter: meta.letter }));
        if (takesData) {
          bits.push(m.rd_op_no_format_from_copy());
        } else if (p.filesystem) {
          bits.push(meta.quickFormat ? m.rd_op_quick_format() : m.rd_op_full_format());
        } else {
          bits.push(m.rd_op_no_format());
        }
      }
      bits.push(m.rd_op_align());
      ops.push(m.rd_op_create({ name: p.volume_label || m.rd_no_label(), details: bits.join(', ') }));
    }
    if (wholeDiskRestore && previewIdx.length > 0) {
      ops.push(m.rd_op_restore_all({ count: srcBackupParts.length, size: formatBytes(srcTotalBytes) }));
    }
    for (const mp of mappings) {
      // Mapped, but there is nothing to copy: say what will actually happen
      // instead of promising a restore the plan does not contain.
      if (layoutOnlyParts.includes(mp.src)) {
        ops.push(m.rd_op_msr({ part: describeSrc(mp.src) }));
        continue;
      }
      const sp = srcDisk?.partitions.find((x) => x.index === mp.src);
      const dp = cur.find((x) => x.index === mp.dst);
      // Same wording as the plan builder's rule, so the list and the plan
      // cannot describe different things.
      const byFiles = !!sp && !!dp && dp.size_bytes < sp.size_bytes;
      const bits = [
        formatBytes(sp?.size_bytes ?? 0),
        byFiles ? m.rd_op_by_files() : m.rd_op_by_sectors(),
      ];
      if (!byFiles && reencrypt && reencryptable.includes(mp.src)) {
        bits.push(m.rd_op_reencrypt());
      }
      ops.push(
        m.rd_op_restore_part({ src: describeSrc(mp.src), dst: describeDst(mp.dst), details: bits.join(', ') }),
      );
    }
    return ops;
  });

  // --- Building the typed plan the backend checks ---------------------------
  // The list above is prose for the user; this is the same intent as data. The
  // backend validates *this*, so the two must describe the same thing — hence
  // both are derived from the identical state (curParts / mappings / previewIdx).

  /** MBR partition type byte, picked from the filesystem the partition holds. */
  function mbrTypeByte(p: PartitionInfo): number {
    switch ((p.filesystem || '').toLowerCase()) {
      case 'fat32':
        return 0x0c; // FAT32 with LBA addressing
      case 'ext4':
      case 'xfs':
      case 'btrfs':
        return 0x83; // Linux
      default:
        return 0x07; // NTFS / exFAT / IFS
    }
  }

  /** The filesystem name as `FormatFs` spells it, or null when we cannot
   *  create it (Linux filesystems — Windows has no mkfs for them). */
  function formatFsOf(fs: string): FormatFs | null {
    switch ((fs || '').toLowerCase()) {
      case 'ntfs':
        return 'ntfs';
      case 'fat32':
        return 'fat32';
      case 'exfat':
        return 'exfat';
      case 'refs':
        return 'refs';
      default:
        return null;
    }
  }

  /** Assemble the plan. Returns null when no target disk is chosen. */
  function buildPlan(): RestorePlan | null {
    const d = targetDisk;
    if (!d) return null;
    const sector = d.sector_size_logical || 512;
    const cur = curParts;

    // The table is built first and the operations' byte offsets are derived
    // FROM it, so the two can never disagree — the backend cross-checks them
    // and a mismatch is an error, not a rounding curiosity.
    const entries: PartitionSpec[] = cur.map((p) => {
      const first = Math.floor(p.offset_bytes / sector);
      const last = Math.floor((p.offset_bytes + p.size_bytes) / sector) - 1;
      const meta = newPartMeta.get(p.index);
      return {
        index: p.index,
        first_lba: first,
        last_lba: last,
        kind: isGpt
          ? {
              scheme: 'gpt',
              type_guid: p.gpt_type_guid || meta?.gptType || GPT_TYPES[0].guid,
              // Kept verbatim for a partition that came out of the copy: the BCD
              // and UEFI reference partitions by this GUID. Empty for a new one,
              // and the writer then generates a fresh value.
              part_guid: p.gpt_partition_guid || '',
              attributes: 0,
              name: p.volume_label || '',
            }
          : {
              scheme: 'mbr',
              type_byte: mbrTypeByte(p),
              bootable: meta?.active ?? p.is_boot,
              logical: meta?.mbrKind === 'logical',
            },
      };
    });

    const refOf = (index: number): PartRef | null => {
      const e = entries.find((x) => x.index === index);
      if (!e) return null;
      return {
        index,
        offset: e.first_lba * sector,
        size: (e.last_lba + 1 - e.first_lba) * sector,
      };
    };

    const ops: RestoreOp[] = [
      {
        op: 'write_partition_table',
        table: {
          scheme: isGpt ? 'gpt' : 'mbr',
          sector_size: sector,
          disk_size_bytes: d.size_bytes,
          // Never inherited from the source: two disks carrying the same GPT
          // GUID make Windows take one of them offline.
          disk_id: '',
          entries,
        },
      },
    ];

    // Whole-disk mode: previewIdx[k] is the target partition created for
    // srcBackupParts[k] — they are placed in order, so the index lines up.
    const pairs: { src: number; dst: number }[] = (
      wholeDiskRestore
        ? previewIdx.map((dst, k) => ({ src: srcBackupParts[k]?.index ?? 0, dst }))
        : mappings
    )
      // Microsoft Reserved gets the slot, not a data copy: the backup holds no
      // blocks for it, so a restore operation would have nothing to write. This
      // guards BOTH paths — the manual one let it through and the plan was then
      // refused outright, blocking the whole restore.
      .filter((m) => !layoutOnlyParts.includes(m.src));

    // A target at least as large as the source volume takes it verbatim,
    // filesystem and all. A smaller one cannot — the filesystem would have to
    // shrink — so the data goes file by file onto a freshly made one (D1).
    const filesMode = new Set<number>();
    for (const m of pairs) {
      const dst = refOf(m.dst);
      const sp = srcDisk?.partitions.find((x) => x.index === m.src);
      if (!dst || !sp) continue;
      const mode: RestoreMode = dst.size >= sp.size_bytes ? 'sectors' : 'files';
      if (mode === 'files') {
        filesMode.add(m.dst);
        // File mode writes onto a filesystem, so one has to exist first.
        const fs = formatFsOf(sp.filesystem);
        if (fs) {
          ops.push({
            op: 'format',
            part: dst,
            fs,
            label: sp.volume_label || '',
            quick: true,
          });
        }
      }
      ops.push({
        op: 'restore_partition',
        src_index: m.src,
        dst,
        mode,
        wipe_free_space: mode === 'sectors' && wipeFreeSpace,
        // Only meaningful for a sector restore of a partition whose header the
        // copy kept; the backend refuses the other combinations outright.
        // Only where a working password was given. An empty field means the
        // user chose to restore that partition without its container.
        reencrypt: mode === 'sectors' && willReencrypt.includes(m.src),
      });
      // A larger target leaves a tail the filesystem does not know about; only
      // NTFS can claim it automatically (D2). In file mode the filesystem was
      // just made at full size, so there is nothing to extend.
      if (
        mode === 'sectors' &&
        dst.size > sp.size_bytes &&
        (sp.filesystem || '').toLowerCase() === 'ntfs'
      ) {
        ops.push({ op: 'extend_filesystem', part: dst });
      }
    }

    // Sector-restored partitions must never be formatted — formatting runs
    // later in the fixed order and would erase what was just written. File-mode
    // targets already got their Format op above.
    const receivesData = new Set(pairs.map((m) => m.dst));
    for (const p of cur) {
      const meta = newPartMeta.get(p.index);
      if (!meta) continue; // not a hand-created partition
      const ref = refOf(p.index);
      if (!ref) continue;
      // A partition that is about to receive a sector restore must NOT be
      // formatted: formatting runs later in the fixed order and would erase
      // exactly what was written. The backend refuses such a plan outright.
      const fs = receivesData.has(p.index) ? null : formatFsOf(p.filesystem);
      if (fs) {
        ops.push({
          op: 'format',
          part: ref,
          fs,
          label: p.volume_label || '',
          quick: meta.quickFormat,
        });
      }
      if (meta.letter) {
        ops.push({ op: 'assign_letter', part: ref, letter: meta.letter });
      }
    }

    // Only a bootable whole-disk copy gets its boot chain rebuilt, and only
    // when both halves are actually present.
    if (wholeDiskRestore && previewIdx.length > 0) {
      const espIdx = srcBackupParts.findIndex(
        (p) => (p.gpt_type_guid || '').toUpperCase() === GPT_TYPES[1].guid,
      );
      const winIdx = srcBackupParts.findIndex(
        (p) => p.is_system || (p.filesystem || '').toLowerCase() === 'ntfs',
      );
      const win = winIdx >= 0 ? refOf(previewIdx[winIdx]) : null;
      const esp = espIdx >= 0 ? refOf(previewIdx[espIdx]) : null;
      if (win) {
        ops.push({ op: 'finalize_boot', windows_part: win, esp, uefi: isGpt });
      }
      // A UEFI machine boots what its NVRAM names, and NVRAM stays with the
      // machine — so a copy restored onto different hardware has a loader
      // nobody asks for. `\EFI\BOOT\BOOTX64.EFI` is the one path firmware tries
      // by itself. bcdboot writes an NVRAM entry for Windows; for Linux nothing
      // did, and a restored Kubuntu landed in a UEFI shell with a perfectly
      // good shim sitting on its ESP.
      if (esp && isGpt) {
        ops.push({ op: 'ensure_fallback_bootloader', esp });
      }
      // The machine's own boot menu, and only with the user's word for it.
      // Three things have to be true at once and none of them implies the
      // others: the copy carries entries (only a bootable copy does), this
      // disk is marked загрузочный, and the list was put in front of the user
      // and answered. Firmware is the one thing a restore touches that
      // outlives the disk.
      if (bootEntries && restoreBootEntries) {
        ops.push({ op: 'restore_boot_entries' });
      }
    }
    // Always last: everything above wrote to the disk through handles opened
    // during the restore, and until something forces a re-read both Windows and
    // this app keep showing the partitions that were there before.
    ops.push({ op: 'refresh_disk_state' });

    return {
      id: crypto.randomUUID(),
      target_disk: diskNumber(d),
      image_path: activeInfo?.image_path || currentPath,
      verify_after: verifyAfter,
      ops,
    };
  }

  /** What the pure checks cannot work out for themselves. Anything left blank
   *  the service fills in — it can see the machine, this window cannot. */
  function buildCheckContext(): CheckContext {
    return {
      source_sector_size: srcSector,
      sources: (srcDisk?.partitions ?? [])
        .filter((p) => backedUpParts.includes(p.index))
        .map((p) => ({
          index: p.index,
          size_bytes: p.size_bytes,
          used_bytes: p.used_bytes,
          filesystem: p.filesystem,
          label: p.volume_label || '',
          encrypted_at_source: decryptedParts.includes(p.index),
          luks_header_available: reencryptable.includes(p.index),
        })),
      taken_letters: [],
      // Both filled in by the service: only it can see which partitions the
      // running Windows is standing on.
      target_is_system_disk: false,
      protected_parts: [],
      // What the destinations hold RIGHT NOW — from the live disk, not from the
      // edited layout, since that is what the restore will overwrite.
      targets: (targetDisk?.partitions ?? []).map((p) => ({
        index: p.index,
        // Where it starts: for a plan that writes into existing partitions
        // there is no table, and this is the only thing an operation's raw
        // byte offset can be checked against.
        offset_bytes: p.offset_bytes,
        size_bytes: p.size_bytes,
        used_bytes: p.used_bytes,
        filesystem: p.filesystem,
        label: p.volume_label || (p.volume_letter ? `${p.volume_letter}:` : ''),
      })),
      // What the create dialog picked. Deliberately reported even though the
      // plan contains no format operation for a partition that receives a
      // sector restore — that absence is exactly what the user should be told
      // about.
      requested_formats: curParts.flatMap((p) => {
        if (!newPartMeta.has(p.index)) return [];
        const fs = formatFsOf(p.filesystem);
        return fs ? [{ index: p.index, fs }] : [];
      }),
    };
  }

  function clickTargetPart(p: PartitionInfo) {
    if (wholeDiskRestore) {
      fitMsg = m.rd_fit_whole_pick_free;
      return;
    }
    if (mappings.some((m) => m.dst === p.index) || previewIdx.includes(p.index)) return; // taken
    if (srcPart == null) {
      fitMsg = m.rd_fit_pick_source_first;
      return;
    }
    const sp = srcDisk?.partitions.find((x) => x.index === srcPart);
    if (!sp) {
      srcPart = null;
      return;
    }
    // A layout-only partition (MSR) carries nothing, so "does the data fit"
    // does not apply — only the slot itself has to be there.
    const need = layoutOnlyParts.includes(sp.index)
      ? 0
      : sp.used_bytes > 0
        ? sp.used_bytes
        : sp.size_bytes;
    if (p.size_bytes < need) {
      badTarget = p.index;
      const needTxt = formatBytes(need);
      const avail = formatBytes(p.size_bytes);
      const byFiles = sp.used_bytes > 0;
      fitMsg = () =>
        m.rd_fit_too_small({ need: needTxt, avail }) +
        (byFiles ? ' ' + m.rd_fit_too_small_files() : '');
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

  const canRestore = $derived(!!(activeInfo?.image_path || currentPath));
  const backupBusy = $derived(
    jobs.queue.current !== null || jobs.queue.pending.length > 0,
  );

  const running = $derived(engine.op === 'restore');
  const progress = $derived(engine.restoreProgress);
  const summary = $derived(engine.restoreSummary);
  const errorMsg = $derived(formError ?? engine.restoreError);

  // Elapsed + ETA from recent throughput. Counted in the engine store, which
  // outlives this view — it is unmounted on every walk through the menu.
  let nowMs = $state(Date.now());
  const eta = $derived(running ? restoreEtaMs() : NaN);

  // All this view still ticks: the "Прошло" line has to move between
  // progress messages.
  $effect(() => {
    if (!running) return;
    nowMs = Date.now();
    const id = setInterval(() => (nowMs = Date.now()), 500);
    return () => clearInterval(id);
  });

  const elapsedMs = $derived(running && restoreClock.startMs ? nowMs - restoreClock.startMs : 0);

  // "Восстановить" runs the DRY RUN and shows what it found alongside the
  // queue. Applying the plan is destructive and stays deliberately unwired —
  // nothing on this screen can write to a disk.
  async function askConfirm() {
    formError = null;
    // Before anything else: the map on screen is the job's guess until the
    // copy itself can be read, and the service will not restore it without
    // the key anyway.
    if (keyNeeded) {
      askKey(true);
      return;
    }
    if (!targetDiskId) {
      fitMsg = m.rd_fit_step1;
      return;
    }
    if (operations.length === 0) {
      fitMsg = wholeDiskRestore ? m.rd_fit_step2_whole : m.rd_fit_step2_parts;
      return;
    }
    if (sectorMismatch && wholeDiskRestore) {
      const src = srcSector;
      const dst = dstSector;
      fitMsg = () => m.rd_fit_boot_sector({ dst, src });
      return;
    }
    applyError = null;
    opsOpen = true;
    await runDryRun();
  }

  /** Build the plan as it stands and have the service check it. */
  async function runDryRun() {
    checks = [];
    letterChanges = [];
    checkError = null;
    checkedPlan = null;
    const plan = buildPlan();
    if (!plan) return;
    checking = true;
    try {
      // The key travels with the preview too: the service reads the copy's own
      // description to check the plan, and an encrypted copy says nothing
      // without it. Left out, the preview judged by what this window sent and
      // the restore judged by nothing — and passed, then refused.
      const res = await api.dryRunRestorePlan(
        plan,
        buildCheckContext(),
        archiveSecretFor(plan.image_path),
      );
      checks = res.results;
      letterChanges = res.letter_changes;
      // Apply the plan that came BACK: the dry run resolves drive-letter
      // conflicts by editing it, and applying the one we sent would throw the
      // fix away.
      checkedPlan = res.plan;
    } catch (e) {
      // The checks live in the service; without it we can still show the queue,
      // but we must not imply the plan was validated.
      checkError = String(e);
    } finally {
      checking = false;
    }
  }

  /** The last step before the point of no return.
   *
   *  The boot entries get their own question here rather than riding along on
   *  the "загрузочный" checkbox: that checkbox is about the DISK, and this
   *  writes the motherboard. It is asked with the actual list in front of the
   *  user, because "restore the boot entries" means nothing without it.
   *
   *  Whatever they answer, the plan is rebuilt and checked again before the
   *  final confirmation — the plan that gets applied has to be the plan that
   *  was checked, not one edited after the fact. */
  async function onFinalRestore() {
    if (bootQuestionApplies && restoreBootEntries === null) {
      bootDialogOpen = true;
      return;
    }
    confirmText = '';
    confirmOpen = true;
  }

  async function answerBootEntries(yes: boolean) {
    restoreBootEntries = yes;
    bootDialogOpen = false;
    await runDryRun();
    if (!checkedPlan || checkErrors.length > 0) return; // the dialog says why
    confirmText = '';
    confirmOpen = true;
  }

  /** Same finding on several partitions reads as noise repeated N times, and
   *  buries the one line that is actually different. Collapse by code, naming
   *  the partitions instead. */
  function groupByCode(list: CheckResult[]): { message: string; parts: number[] }[] {
    const out: { code: string; message: string; parts: number[] }[] = [];
    for (const c of list) {
      const seen = out.find((g) => g.code === c.code);
      if (seen) {
        if (c.part_index != null) seen.parts.push(c.part_index);
      } else {
        out.push({ code: c.code, message: c.message, parts: c.part_index != null ? [c.part_index] : [] });
      }
    }
    return out.map(({ message, parts }) => ({ message, parts }));
  }
  /** "Раздел 5: ..." repeated for 5, 6 and 7 becomes "Разделы 5, 6, 7: ...". */
  function mergedText(g: { message: string; parts: number[] }): string {
    if (g.parts.length < 2) return g.message;
    // The service words these in the window's language; both openings are
    // the same shape («Раздел 5 (…)» / "Partition 5 (…)").
    const tail = g.message.replace(/^(?:Раздел|Partition) \d+(\s*\([^)]*\))?/, '').replace(/^\s*:\s*/, ': ');
    return m.rd_check_parts_merged({ parts: g.parts.map(describeDst).join('; '), rest: tail });
  }

  const checkErrors = $derived(groupByCode(checks.filter((c) => c.level === 'error')));
  const checkWarnings = $derived(groupByCode(checks.filter((c) => c.level === 'warning')));
  const checkInfos = $derived(groupByCode(checks.filter((c) => c.level === 'info')));

  /** Partitions whose container will actually be put back: the ones with a
   *  password that has been checked. */
  const willReencrypt = $derived(
    reencrypt ? reencryptable.filter((i) => luksPwState[i] === 'ok') : [],
  );
  /** Nothing is half-done: a password that was typed must be one that works.
   *  An unchecked or wrong one would let the restore write the container header,
   *  fail to open it, and stop — with the partition table already rewritten.
   *  An EMPTY one is fine: it simply means "restore this one without
   *  encryption". */
  const luksReady = $derived(
    !reencrypt ||
      reencryptable.every((i) => !(luksPw[i] ?? '').trim() || luksPwState[i] === 'ok'),
  );
  /** The plan checked out and nothing stands in the way of running it. */
  const canApply = $derived(
    !!checkedPlan && !checking && !checkError && checkErrors.length === 0 && luksReady,
  );
  /** Why the button is off, in the words that name the thing to fix. */
  const applyBlockedBy = $derived(
    checking
      ? m.rd_blocked_checking()
      : checkError
        ? m.rd_blocked_unchecked()
        : checkErrors.length > 0
          ? m.rd_blocked_errors()
          : !luksReady
            ? m.rd_blocked_luks()
            : '',
  );

  /** DESTRUCTIVE — the one place this screen writes to a disk. */
  async function doRestore() {
    if (confirmText !== m.common_yes_word() || !checkedPlan) return;
    confirmOpen = false;
    opsOpen = false;
    applyError = null;
    // Keyed by SOURCE partition: that is what the executor looks up the saved
    // container header by.
    const passphrases: Record<number, string> = {};
    if (reencrypt) {
      for (const i of reencryptable) {
        if (luksPw[i]) passphrases[i] = luksPw[i];
      }
    }
    try {
      planRun = await api.applyRestorePlan(
        { ...checkedPlan, verify_after: verifyAfter },
        passphrases,
        archiveSecretFor(checkedPlan.image_path),
      );
    } catch (e) {
      applyError = String(e);
    }
  }

  async function cancelPlanRun() {
    if (!planRun) return;
    try {
      const r = await api.cancelRestorePlan(planRun.plan_id);
      if (!r.cancelled && r.reason) stubMsg = r.reason;
    } catch (e) {
      stubMsg = String(e);
    }
  }

  const OP_STATUS_LABEL: Record<string, () => string> = {
    pending: m.rd_op_status_pending,
    running: m.rd_op_status_running,
    done: m.rd_op_status_done,
    failed: m.rd_op_status_failed,
    skipped: m.rd_op_status_skipped,
  };
  // Housekeeping stays out of the queue: a line saying we are about to touch
  // the bootloader raises a question whose answer is usually "nothing
  // happened". Such a step reports afterwards, and only if it acted.
  function shownOps(run: RestoreRunState): OpRecord[] {
    return run.ops.filter((o) => !o.hidden || o.status === 'failed');
  }

  function opClass(o: OpRecord): string {
    return `op ${o.status}`;
  }

  const targetLabel = $derived(targetDisk?.model ?? '');
</script>

<!-- Password entry for one container. Rendered both on the card and inside the
     check dialog: the dialog is what tells the user a password is needed, and
     asking for it there while the only field sits on the page behind it is a
     dead end. -->
{#snippet pwRow(part: number, label: string)}
  <span class="pw-inline">
    <span class="pw-lbl">{label}</span>
    <PasswordField
      variant="inline"
      value={luksPw[part] ?? ''}
      oninput={(v) => {
        luksPw[part] = v;
        luksPwState[part] = 'idle';
      }}
      onenter={() => checkLuksPassword(part)}
      invalid={luksPwState[part] === 'bad'}
    />
    <button
      class="btn ghost tiny"
      disabled={!(luksPw[part] ?? '') || luksPwState[part] === 'checking'}
      onclick={() => checkLuksPassword(part)}
    >{m.rd_pw_check()}</button>
    {#if luksPwState[part] === 'checking'}<span class="pw-note">{m.rd_pw_checking()}</span>{/if}
    {#if luksPwState[part] === 'ok'}<span class="pw-ok"><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>{m.rd_pw_ok()}</span>{/if}
    {#if luksPwState[part] === 'bad'}<span class="pw-bad"><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.8" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>{m.rd_pw_bad()}</span>{/if}
    {#if luksPwState[part] === 'error'}<span class="pw-bad" title={luksPwError[part]}>{m.rd_pw_check_failed()}</span>{/if}
  </span>
{/snippet}

{#snippet errIcon()}
  <svg class="bicon" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--error)" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5" /><path d="M9 9l6 6M15 9l-6 6" /></svg>
{/snippet}

{#snippet copyNav()}
  <div class="copynav">
    <button class="cn-btn" title={m.rd_older_copy()} aria-label={m.rd_older_copy()}
      disabled={!hasMultiple || copyIndex >= versions.length - 1} onclick={() => { copyIndex += 1; resetMapping(); }}
      ><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="M15 5l-7 7 7 7" /></svg></button>
    <span class="cn-date"
      title={activeInfo
        ? activeInfo.app_version
          ? m.rd_made_by({ version: activeInfo.app_version })
          : m.rd_made_by_unknown()
        : ''}
    >{copyDate}</span>
    <!-- Which build wrote this copy. Small, next to the date, and the first
         thing to look at when a copy behaves oddly. -->
    {#if activeInfo}
      <span class="cn-ver">v{activeInfo.app_version || '?'}</span>
    {/if}
    <!-- Which of how many: the arrows alone do not say there is anything to
         page to, nor where in the chain this copy stands. Oldest is 1. -->
    {#if hasMultiple}
      <span class="cn-ver" title={m.rd_copy_n_of({ n: versions.length - copyIndex, total: versions.length })}
        >{versions.length - copyIndex}/{versions.length}</span>
    {/if}
    <button class="cn-btn" title={m.rd_newer_copy()} aria-label={m.rd_newer_copy()}
      disabled={!hasMultiple || copyIndex <= 0} onclick={() => { copyIndex -= 1; resetMapping(); }}
      ><svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="M9 5l7 7-7 7" /></svg></button>
  </div>
{/snippet}

<ArchiveUnlockModal
  open={unlock.open}
  imageName={currentPath.split(/[\\/]/).pop() ?? ''}
  busy={unlock.busy}
  error={unlock.error}
  onUnlock={submitUnlock}
  onCancel={() => (unlock = { ...unlockClosed })}
/>

<div class="page">
<div class="dash" bind:clientWidth={dashWidth}>
  {#if !running && !planRun}
    <!-- WHAT: the disk this copy captured — same read-only map as Параметры → Источник -->
    <div class="sec-head">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
      <span class="sec-title">{m.rd_what_title()}</span>
      <span class="sec-sub">{m.rd_what_sub()}</span>
    </div>

    {#if keyNeeded}
      <div class="key-note">
        <svg class="kn-ico" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--info)" stroke-width="2" stroke-linecap="round"><rect x="5" y="11" width="14" height="9" rx="2" /><path d="M8 11V8a4 4 0 0 1 8 0v3" /></svg>
        <!-- An imported copy has no job to draw a map from: everything is
             inside the sealed file, and "the map below" would point at
             nothing. -->
        <span>{backedUpDisks.length > 0 ? m.rd_key_note() : m.rd_key_note_no_map()}</span>
        <button class="btn ghost tiny" onclick={() => askKey()}>{m.rd_enter_password()}</button>
      </div>
    {/if}

    {#if backedUpDisks.length > 0}
      <!-- Map is available (embedded in the file / job snapshot / live) — show it
           without waiting on the slow live-disk enumeration. -->
      <div class="disks">
        {#each backedUpDisks as d, i (d.device_id)}
          <DiskMap
            disk={d}
            center={i === 0 ? copyNav : undefined}
            selected={targetDiskId ? [] : copySourceDevices}
            selectedIndices={targetDiskId ? [] : [...(activeInfo?.backed_up_parts ?? []), ...layoutOnlyParts]}
            readonly
            boot={i === 0 && isBootable}
            bootChecked={wholeDiskRestore}
            onBootToggle={toggleWholeDisk}
            inBackup={backedUpParts.length > 0 ? [...backedUpParts, ...layoutOnlyParts] : undefined}
            layoutOnly={layoutOnlyParts}
            lockedIndices={wholeDiskRestore
              ? [...backedUpParts, ...layoutOnlyParts]
              : mappings.map((m) => m.src)}
            onPart={i === 0 && targetDiskId ? clickSourcePart : undefined}
            stateOf={i === 0 && targetDiskId ? srcState : undefined}
          />
        {/each}
      </div>
    {:else if copiesLoading || infoLoading || (engine.volumesLoading && engine.disks.length === 0)}
      <div class="loading-note">{m.rd_reading_copy()}</div>
    {:else if !keyNeeded}
      <!-- Only once everything that could describe the copy has been asked and
           answered — BOTH reads, not just the folder scan. Missing the second
           one left this reachable mid-load, so it appeared for a moment on
           every visit and then withdrew itself; on an encrypted copy that
           moment is however long Argon2id takes.
           And never while the key is still needed: a sealed copy keeps its
           map inside, so "no map" there means "not opened yet", not "the
           disk is gone" — the boot medium said exactly that over a copy on a
           disk it was showing right below (2026-10-16). -->
      <div class="warn-note">{m.rd_disk_not_connected()}</div>
    {/if}

    <!-- The copy holds the volume decrypted, so the restore cannot put the
         container back. Said here, next to the card, rather than only in the
         confirmation dialog: this decides whether the restore is useful at
         all, and it should not come as news at the last click. -->
    {#if canReencrypt}
      <!-- The copy kept the container header, so the encryption can go back
           exactly as it was. Offered here rather than buried in the final
           dialog: it decides whether the restored system boots, and the
           default is the one that keeps it working. -->
      <div class="luks-box">
        <!-- The checkbox sits in its own <label>; the password field must NOT,
             or clicking into it would toggle the checkbox. -->
        <div class="luks-row">
          <label class="luks-opt">
            <input type="checkbox" bind:checked={reencrypt} />
            <b>{reencryptable.length > 1
              ? m.rd_reencrypt_parts({ parts: reencryptable.map(describeSrc).join('; ') })
              : m.rd_reencrypt_part({ part: describeSrc(reencryptable[0]) })}</b>
          </label>
          {#if reencrypt && reencryptable.length === 1}
            {@render pwRow(reencryptable[0], m.rd_pw_enter_label())}
          {/if}
        </div>
        <div class="luks-note">{m.rd_luks_note()}</div>
        {#if reencrypt && reencryptable.length > 1}
          {#each reencryptable as part}
            <div class="pw-multi">{@render pwRow(part, m.rd_pw_part_label({ part: describeSrc(part) }))}</div>
          {/each}
        {/if}
        {#if !reencrypt}
          <div class="luks-warn inner">
            <svg class="sw-ico" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--warning)" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="11" width="14" height="9" rx="2" /><path d="M8 11V8a4 4 0 0 1 7.7-1.5" /></svg>
            <span>
              {m.rd_luks_plain_fs()}
              {@html m.rd_luks_no_boot_html()}
            </span>
          </div>
        {/if}
      </div>
    {:else if decryptedParts.length > 0}
      {@const one = decryptedParts.length === 1}
      {@const kind = allBitLocker ? ' (BitLocker)' : allLuks ? ' (LUKS)' : ''}
      <div class="luks-warn">
        <svg class="sw-ico" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--warning)" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="11" width="14" height="9" rx="2" /><path d="M8 11V8a4 4 0 0 1 7.7-1.5" /></svg>
        <span>
          <b>{m.rd_enc_lost_title()}</b>
          {one
            ? m.rd_enc_lost_one({ part: describeSrc(decryptedParts[0]), kind })
            : m.rd_enc_lost_many({ parts: decryptedParts.map(describeSrc).join('; '), kind })}
          <br />
          {#if allBitLocker}
            <!-- Windows boots from a decrypted volume without a word: the
                 loader reads plain NTFS. Nothing to repair — only protection to
                 turn back on, and only the user can decide to. -->
            {@html m.rd_bitlocker_boots_html()}
            <br />
            {m.rd_bitlocker_no_header()}
          {:else if allLuks}
            {@html m.rd_luks_no_boot_html()}
            <br />
            {m.rd_luks_no_header()}
          {:else}
            <!-- An old copy that did not record what the container was. Naming
                 one would be a guess, and the two fail in opposite ways. -->
            {@html m.rd_enc_unknown_html()}
          {/if}
        </span>
      </div>
    {/if}

    {#if catData}
      <div class="cat-block">
        <div class="cat-head">{m.rd_cat_head()}</div>
        <div class="catbar">
          {#each barData?.filter((c) => c.bytes > 0) ?? [] as c}<div class="catseg" style="width:{c.pct}%; background:{c.color}" title="{c.name}: {formatBytes(c.bytes)}"></div>{/each}
        </div>
        <div class="cats">
          {#each catData as c}
            <div class="cat"><span class="dot" style="background:{c.color}"></span><span class="cat-name">{c.name()}</span><span class="cat-size">{formatBytes(c.bytes)}</span></div>
          {/each}
          <div class="cat"><span class="dot" style:background={EXCLUDED_COLOR}></span><span class="cat-name">{m.cat_excluded()}</span><span class="cat-size">{formatBytes(excluded.bytes)}</span></div>
        </div>
      </div>
    {/if}

    <!-- arrow (at 25%) + step instruction (what to do now) -->
    <div class="connector">
      <div class="conn-arrow">
        <div class="v-dash"></div>
        <div class="arrow-circle"><svg width="20" height="20" viewBox="0 0 24 24" stroke="#fff" stroke-width="2.4" fill="none" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M6 13l6 6 6-6"/></svg></div>
        <div class="v-dash"></div>
      </div>
      <div class="conn-instr">{instr}</div>
    </div>

    <!-- WHERE: step 1 = pick a disk; step 2/3 = map partitions onto it -->
    <div class="sec-head">
      <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--accent-text)" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14" rx="3"/><circle cx="12" cy="12" r="3.5"/></svg>
      <span class="sec-title">{m.rd_where_title()}</span>
      <span class="sec-sub">
        {#if !targetDiskId}{m.rd_where_sub_disk()}{:else}{m.rd_where_sub_part()}{/if}
      </span>
    </div>
    {#if hiddenSystemDisks.length > 0 && !targetDiskId}
      <div class="hint-small">
        {m.rd_target_running_hidden({ disks: hiddenSystemDisks.map((d) => diskCaption(d)).join(', ') })}
      </div>
    {/if}
    {#if systemDisks.length === 0}
      <div class="loading-note searching">
        <span class="spinner" aria-hidden="true"></span>
        {m.rd_searching_disks()}
      </div>
    {:else if !targetDiskId}
      <div class="disks">
        {#each targetDisks as d (d.device_id)}
          <DiskMap disk={d} onDisk={() => selectTargetDisk(d)} />
        {/each}
      </div>
    {:else if targetDisk}
      <div class="disks">
        <DiskMap
          disk={targetDisk}
          slots={slots ?? undefined}
          onPart={clickTargetPart}
          stateOf={dstState}
          lockedIndices={[...mappings.map((m) => m.dst), ...previewIdx]}
          onFree={wholeDiskRestore
            ? previewIdx.length === 0
              ? clickFreeSpace
              : undefined
            : openCreate}
          freeAction={wholeDiskRestore ? 'place' : 'create'}
          needBytes={wholeDiskRestore && previewIdx.length === 0 ? srcNeedBytes : undefined}
          onPartMenu={openPartMenu}
          onFreeMenu={openFreeMenu}
        />
      </div>
      {#if sectorMismatch}
        <div class="sector-warn">
          <svg class="sw-ico" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="var(--error)" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5l9 16H3z" /><path d="M12 10v4M12 17h.01" /></svg>
          <span>
            <b>{m.rd_sector_title()}</b>
            {@html m.rd_sector_body_html({ src: esc(srcSector), dst: esc(dstSector) })}
            {#if wholeDiskRestore}
              {m.rd_sector_whole({ src: srcSector })}
            {:else}
              {m.rd_sector_parts()}
            {/if}
          </span>
        </div>
      {/if}
      <div class="hint-small">
        {#if wholeDiskRestore}
          {m.rd_hint_whole()}
        {:else}
          {m.rd_hint_parts()}
        {/if}
      </div>
      <!-- Undo everything and go back to step 1 (pick a disk). -->
      <div class="under-card">
        <button
          class="btn ghost small icon"
          title={m.rd_undo()}
          aria-label={m.rd_undo()}
          disabled={!canUndo}
          onclick={undo}
        >
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14 4 9l5-5"/><path d="M4 9h11a5 5 0 0 1 0 10h-4"/></svg>
        </button>
        <button class="btn ghost small" onclick={pickAnotherDisk}>{m.common_cancel()}</button>
      </div>
      {#if fitMsg}<div class="fit-msg" class:bad={badTarget !== null}>{fitMsg()}</div>{/if}
    {/if}
  {/if}

  <!-- A plan being executed by the service. Shown from the run state, not from
       events, so reopening this window mid-restore picks up where it left off. -->
  {#if planRun}
    <div class="run">
      <div class="run-head">
        <span class="run-title">
          {#if planRun.status === 'running'}{m.rd_run_running()}
          {:else if planRun.status === 'completed'}{m.rd_run_completed()}
          {:else if planRun.status === 'cancelled'}{m.rd_run_cancelled()}
          {:else}{m.rd_run_failed()}{/if}
        </span>
        <span class="run-sub">{m.rd_run_sub({ disk: planRun.target_disk, count: shownOps(planRun).length })}</span>
      </div>
      <ol class="ops-run">
        {#each shownOps(planRun) as o}
          <li class={opClass(o)}>
            <span class="op-mark">
              {#if o.status === 'done'}<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
              {:else if o.status === 'failed'}<svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.8" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
              {:else if o.status === 'running'}▸
              {:else if o.status === 'skipped'}–
              {:else}·{/if}
            </span>
            <span class="op-body">
              <span class="op-title">{o.title}</span>
              {#if o.status === 'running' && o.bytes_total > 0}
                <span class="op-bar"><span class="op-fill" style="width:{o.percent}%"></span></span>
              {/if}
              {#if o.message}<span class="op-msg">{o.message}</span>{/if}
            </span>
            <span class="op-state">
              {#if o.status === 'running' && o.percent > 0}{o.percent}%{:else}{OP_STATUS_LABEL[o.status]?.()}{/if}
            </span>
          </li>
        {/each}
      </ol>
      {#if planRun.follow_ups.length > 0}
        <div class="run-followups">
          <div class="run-fu-head">{m.rd_followups_head()}</div>
          {#each planRun.follow_ups as f}<div class="finding warn">{f}</div>{/each}
        </div>
      {/if}
      {#if planRun.status !== 'running'}
        <div class="under-card">
          <button class="btn ghost small" onclick={dismissRun}>
            {m.rd_back_to_choice()}
          </button>
        </div>
      {/if}
    </div>
  {/if}

  {#if applyError}<div class="banner err">{@render errIcon()}<span>{applyError}</span></div>{/if}

  {#if running}
    <div class="progress-wrap">
      <div class="progress-info">
        <span>{m.rd_run_running()}</span>
        {#if progress}
          <span>{m.rd_progress_blocks({ done: progress.blocks_done, total: progress.blocks_total, speed: progress.speed_mbps.toFixed(1) })}</span>
        {/if}
      </div>
      <div class="bar"><div class="bar-fill" style="width:{progress?.percent ?? 0}%"></div></div>
      <div class="progress-meta">
        <span class="route">{job.name} → {targetLabel}</span>
        <span class="times">{m.rd_elapsed_left({ elapsed: formatClock(elapsedMs), left: Number.isFinite(eta) ? '~' + formatClock(eta) : m.rd_estimating() })}</span>
      </div>
    </div>
  {/if}

  {#if summary && !running}
    <div class="banner ok"><span class="bdot"><svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="3.2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg></span><span>{m.rd_done_summary({ size: formatBytes(summary.bytes_restored), secs: summary.duration_secs.toFixed(1) })}</span></div>
  {/if}
  {#if errorMsg}<div class="banner err">{@render errIcon()}<span>{errorMsg}</span></div>{/if}
</div>

  <div class="footer">
    <div class="hint">
      {#if backupBusy}
        {m.rd_hint_backup_busy()}
      {:else}
        {m.rd_hint_default()}
      {/if}
    </div>
    {#if planRunning}
      <!-- Dark on a point of no return: the service refuses there, and a button
           that looks alive while nothing happens is worse than one that does not. -->
      <button class="btn danger" onclick={cancelPlanRun} disabled={!planRun?.cancellable}>{m.common_cancel()}</button>
    {:else if running}
      <button class="btn danger" onclick={cancelCurrent}>{m.common_cancel()}</button>
    {:else if !planRun}
      <button class="btn green" onclick={askConfirm} disabled={!canRestore || backupBusy}>{m.rd_restore()}</button>
    {/if}
  </div>
</div>

{#if stubMsg}<InfoModal title={m.rd_cannot_cancel()} message={stubMsg} onClose={() => (stubMsg = null)} />{/if}

<!-- Right-click menu of the simple partition editor -->
{#if menu}
  <div class="menu-backdrop" role="presentation" oncontextmenu={(e) => { e.preventDefault(); menu = null; }} onclick={() => (menu = null)}></div>
  <div class="ctx-menu" style="left:{menu.x}px; top:{menu.y}px">
    <div class="ctx-title">{m.rd_ctx_actions()}</div>
    {#if menu.part}
      {#if canExtend >= 0}
        <button onclick={() => extendPart(canExtend)}>{m.rd_ctx_extend()}</button>
      {/if}
      <!-- Last, and set off by a rule: the one item here that destroys
           something should not sit under the cursor by default. -->
      <button class="danger" onclick={() => { const p = menu!.part!; menu = null; delConfirm = p; }}>{m.rd_ctx_delete()}</button>
    {:else}
      <button onclick={() => openCreate(menu!.offset ?? 0, menu!.size ?? 0)}>{m.rd_create_part()}</button>
      {#if canExtend >= 0}
        <button onclick={() => extendPart(canExtend)}>{m.rd_ctx_join()}</button>
      {/if}
    {/if}
  </div>
{/if}

<!-- Delete confirmation -->
{#if delConfirm}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">{m.rd_del_title()}</div>
      <p>
        {@html m.rd_del_body_html({
          name: esc(`${delConfirm.volume_letter ? delConfirm.volume_letter + ': ' : ''}${delConfirm.volume_label || m.rd_no_label()}`),
          size: esc(formatBytes(delConfirm.size_bytes)),
        })}
      </p>
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (delConfirm = null)}>{m.rd_no()}</button>
        <button class="btn danger-solid" onclick={doDelete}>{m.rd_yes_delete()}</button>
      </div>
    </div>
  </div>
{/if}

<!-- Create-partition dialog -->
{#if createDlg}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">{m.rd_create_part()}</div>
      <!-- Exact, not rounded: `formatBytes` shows 130.28 GiB as "130 GiB", which
           made the pre-filled size look like it covered everything. -->
      <p>{m.rd_free_label()} <b>{preciseSize(createDlg.max)}</b></p>
      <label class="fld">
        <span>{m.rd_size()}</span>
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
          <span>{m.rd_filesystem()}</span>
          <!-- Only filesystems Windows can actually create (D9). A Linux
               partition is made by picking the GPT type on the right and
               leaving this on "не форматировать" — Windows has no mkfs for
               ext4, and offering it here would produce a partition the queue
               claims is formatted and that is in fact empty. -->
          <select value={createDlg.fs} onchange={(e) => onFsChange((e.currentTarget as HTMLSelectElement).value)}>
            <option value="">{m.rd_no_format_opt()}</option>
            <option value="ntfs">NTFS</option>
            <option value="fat32">FAT32</option>
            <option value="exfat">exFAT</option>
            <option value="refs">ReFS</option>
          </select>
        </label>
        {#if isGpt}
          <label class="fld">
            <span>{m.rd_part_type_gpt()}</span>
            <select bind:value={createDlg.gptType}>
              {#each GPT_TYPES as t}<option value={t.guid}>{t.name()}</option>{/each}
            </select>
          </label>
        {:else}
          <label class="fld">
            <span>{m.rd_part_type_mbr()}</span>
            <select bind:value={createDlg.mbrKind}>
              <option value="primary">{m.rd_mbr_primary()}</option>
              <option value="logical">{m.rd_mbr_logical()}</option>
            </select>
          </label>
        {/if}
      </div>
      {#if !isGpt}
        <label class="chk">
          <input type="checkbox" bind:checked={createDlg.active} />
          {m.rd_active_bios()}
        </label>
      {/if}
      <div class="row2">
        <label class="fld">
          <span>{m.rd_volume_label()}</span>
          <input type="text" bind:value={createDlg.label} placeholder={m.rd_volume_label_ph()} spellcheck="false" />
        </label>
        <label class="fld">
          <span>{m.rd_drive_letter()}</span>
          <select bind:value={createDlg.letter}>
            <option value="">{m.rd_no_letter()}</option>
            {#each freeLetters as l}<option value={l}>{l}:</option>{/each}
          </select>
        </label>
      </div>
      <label class="chk">
        <input type="checkbox" bind:checked={createDlg.quickFormat} />
        {m.rd_quick_format()}
      </label>
      <p class="fld-note">{m.rd_align_note()}</p>
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (createDlg = null)}>{m.common_cancel()}</button>
        <button class="btn primary-solid" onclick={doCreate}>{m.rd_create()}</button>
      </div>
    </div>
  </div>
{/if}

<!-- What "Восстановить" would do (design preview — nothing is executed yet) -->
{#if opsOpen}
  <div class="overlay">
    <div class="dialog wide">
      <div class="dialog-title">{m.rd_check_title()}</div>

      {#if checking}
        <div class="finding-line"><span class="spinner"></span> {m.rd_check_running()}</div>
      {:else if checkError}
        <div class="finding err">
          <b>{m.rd_check_failed()}</b>
          <div>{checkError}</div>
          <div class="finding-note">{m.rd_check_failed_note()}</div>
        </div>
      {:else if checks.length === 0}
        <div class="finding ok">{m.rd_check_ok()}</div>
      {:else}
        {#each checkErrors as g}
          <div class="finding err">{mergedText(g)}</div>
        {/each}
        {#each checkWarnings as g}
          <div class="finding warn">{mergedText(g)}</div>
        {/each}
        {#each checkInfos as g}
          <div class="finding info">{mergedText(g)}</div>
        {/each}
      {/if}

      {#each letterChanges as lc}
        <div class="finding info">
          {lc.assigned
            ? m.rd_letter_reassigned({ part: lc.part_index, requested: lc.requested, assigned: lc.assigned })
            : m.rd_letter_none({ part: lc.part_index, requested: lc.requested })}
        </div>
      {/each}

      <div class="dialog-title sub">{m.rd_ops_title()}</div>
      <ol class="ops">
        {#each operations as op}<li>{op}</li>{/each}
      </ol>

      <label class="opt">
        <input type="checkbox" bind:checked={wipeFreeSpace} />
        <span>
          {m.rd_wipe()} <span class="opt-cost">{m.rd_wipe_cost()}</span>
          <em>{m.rd_wipe_note()}</em>
        </span>
      </label>
      <label class="opt">
        <input type="checkbox" bind:checked={verifyAfter} />
        <span>{m.rd_verify_after()}</span>
      </label>

      <!-- The password field is not repeated here. It lives on the page, next
           to the "вернуть шифрование" checkbox that raises the question, and a
           partition left without one is already reported above by the plan
           check (EncryptionNotRestored) — in the same list as everything else
           this dialog exists to show. -->
      {#if applyBlockedBy}
        <p class="ops-note">{applyBlockedBy}</p>
      {:else}
        <p class="ops-note">{m.rd_point_of_no_return()}</p>
      {/if}
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (opsOpen = false)}>{m.common_close()}</button>
        <button
          class="btn danger-solid"
          disabled={!canApply}
          onclick={onFinalRestore}
        >{m.rd_restore()}</button>
      </div>
    </div>
  </div>
{/if}

<!-- The machine's boot menu: its own question, asked with the list in front of
     the user. The "загрузочный" checkbox is about the DISK; this writes the
     motherboard's memory, and that is the one thing here that outlives the
     disk it was restored onto. -->
{#if bootDialogOpen && bootEntries}
  <div class="overlay">
    <div class="dialog wide">
      <div class="dialog-title">{m.rd_boot_title()}</div>
      <p>{m.rd_boot_intro()}</p>
      <p class="boot-src">
        {m.rd_boot_taken({ date: formatDate(bootEntries.taken_at) })} <b>{bootEntries.machine}</b>.
      </p>

      <div class="dialog-title sub">{m.rd_boot_will_write()}</div>
      <ul class="boot-list">
        {#each bootEntries.entries as e}
          <li>
            <b>{e.description}</b>
            <span class="boot-name">{e.name}</span>
            {#if e.file}<div class="boot-file">{e.file}</div>{/if}
          </li>
        {/each}
      </ul>
      <p class="ops-note">{m.rd_boot_vars({ count: bootEntries.variables })}</p>
      {#if fromAnotherMachine}
        <p class="finding warn">
          {m.rd_boot_other_machine({ machine: bootEntries.machine, this_machine: bootEntries.this_machine })}
        </p>
      {/if}

      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => answerBootEntries(false)}>{m.rd_boot_no()}</button>
        <button class="btn danger-solid" onclick={() => answerBootEntries(true)}>{m.rd_boot_yes()}</button>
      </div>
    </div>
  </div>
{/if}

{#if confirmOpen}
  <div class="overlay">
    <div class="dialog">
      <div class="dialog-title">{m.rd_confirm_title()}</div>
      <p>
        {@html m.rd_confirm_body_html({
          disk: esc(targetDisk?.model ?? ''),
          job: esc(job.name),
          date: esc(copyDate),
        })}
      </p>
      <p class="no-undo"><b>{m.rd_no_undo_bold()}</b> {m.rd_no_undo_rest()}</p>
      <p>{@html m.rd_confirm_type_html({ word: esc(m.common_yes_word()) })}</p>
      <input type="text" bind:value={confirmText} spellcheck="false" />
      <div class="dialog-actions">
        <button class="btn ghost" onclick={() => (confirmOpen = false)}>{m.common_cancel()}</button>
        <button class="btn danger-solid" disabled={confirmText !== m.common_yes_word()} onclick={doRestore}>{m.rd_restore()}</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .page { height: 100%; display: flex; flex-direction: column; }
  .dash { flex: 1; overflow: auto; width: 100%; padding: 34px 44px; display: flex; flex-direction: column; }
  .sec-head { display: flex; align-items: center; gap: 10px; margin-bottom: 14px; }
  .sec-title { font-size: 15px; font-weight: 600; color: var(--head); }
  .sec-sub { font-size: 13px; color: var(--text-muted); }
  .disks { display: flex; flex-direction: column; gap: 14px; }
  .loading-note { font-size: 13px; color: var(--text-muted); padding: 9px 0; }
  .loading-note.searching { display: flex; align-items: center; gap: 10px; }
  .spinner { width: 16px; height: 16px; flex-shrink: 0; border: 2px solid var(--track); border-top-color: var(--primary); border-radius: 50%; animation: spin 0.7s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .warn-note { font-size: 13px; color: var(--error-text); background: var(--error-soft); border: 1px solid var(--error-line); padding: 10px 14px; border-radius: 8px; }
  .key-note { display: flex; align-items: center; gap: 14px; margin-bottom: 12px; padding: 10px 14px; border-radius: 8px; border: 1px solid var(--info-line); background: var(--info-soft); font-size: 13px; color: var(--info-text); line-height: 1.45; }
  .key-note span { flex: 1; }
  .key-note .kn-ico { flex: 0 0 auto; }
  .key-note .btn { flex: 0 0 auto; }
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
  /* Copy navigator in the disk-card header: ‹ date › to page between the copies
     of one archive. Buttons disable at the ends / when there's only one copy. */
  .copynav { display: flex; align-items: center; gap: 6px; }
  .cn-btn { width: 22px; height: 20px; display: flex; align-items: center; justify-content: center; border: 1px solid var(--border-strong); border-radius: 4px; background: var(--card-bg); color: var(--text-secondary); font-size: 18px; font-weight: 800; line-height: 1; cursor: pointer; padding: 0; font-family: var(--font); }
  .cn-btn:hover:not(:disabled) { border-color: var(--primary); color: var(--accent-text); background: var(--accent-soft); }
  .cn-btn:disabled { opacity: .45; cursor: default; }
  .cn-date { font-size: 12px; font-weight: 600; color: var(--head); white-space: nowrap; }
  .cn-ver { font-size: 11px; color: var(--text-muted); white-space: nowrap; }
  /* Arrow occupies the left half so it sits at ~25%; the instruction fills the
     right half. */
  /* The arrow's centre sits at 25% of the width (half its own 44px width is
     subtracted); the instruction starts right after it, not at the midpoint. */
  .connector { display: flex; align-items: center; margin: 20px 0; gap: 14px; }
  .conn-arrow { flex: 0 0 auto; margin-left: calc(25% - 22px); display: flex; flex-direction: column; align-items: center; }
  .conn-instr { flex: 1; font-size: 15px; font-weight: 600; color: var(--head); line-height: 1.45; }
  .connector .v-dash { width: 2px; height: 14px; background: repeating-linear-gradient(180deg, var(--border-strong) 0 4px, transparent 4px 8px); }
  .arrow-circle { width: 44px; height: 44px; border-radius: 50%; background: var(--success); display: flex; align-items: center; justify-content: center; margin: 4px 0; }
  .under-card { display: flex; justify-content: flex-end; gap: 10px; margin-top: 10px; }
  .btn.small { padding: 7px 18px; font-size: 13px; }
  .btn.icon { display: inline-flex; align-items: center; justify-content: center; padding: 7px 12px; }
  .hint-small { margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  .sector-warn { display: flex; gap: 10px; margin-top: 12px; padding: 12px 14px; border-radius: 8px; background: var(--error-soft); border: 1px solid var(--error-line); font-size: 13px; color: var(--error-text); line-height: 1.5; }
  .sw-ico { flex-shrink: 0; margin-top: 1px; }
  .luks-warn { display: flex; gap: 10px; margin-top: 14px; padding: 12px 14px; border-radius: 8px; background: var(--warning-soft); border: 1px solid var(--warning-line); font-size: 13px; color: var(--warning-text); line-height: 1.55; }
  .luks-warn :global(code) { font-family: ui-monospace, Menlo, Consolas, monospace; font-size: 12px; background: var(--sunken); padding: 1px 4px; border-radius: 3px; }
  .luks-warn.inner { margin-top: 10px; }
  .luks-box { margin-top: 14px; padding: 12px 14px; border-radius: 8px; border: 1px solid var(--border); background: var(--card-bg); }
  .luks-opt { display: flex; align-items: flex-start; gap: 10px; font-size: 13px; color: var(--head); cursor: pointer; line-height: 1.5; }
  .luks-opt input { width: auto; margin-top: 2px; flex: 0 0 auto; }
  .luks-row { display: flex; align-items: center; flex-wrap: wrap; gap: 10px 14px; }
  .luks-note { font-size: 12px; color: var(--text-muted); line-height: 1.5; margin-top: 6px; }
  .pw-inline { display: flex; align-items: center; gap: 8px; font-size: 13px; color: var(--head); }
  .pw-multi { margin-top: 8px; }
  .pw-lbl { white-space: nowrap; }
  /* The field itself (with its eye) is PasswordField, variant="inline". */
  .btn.tiny { padding: 6px 12px; font-size: 12px; }
  .pw-ok { font-size: 12px; color: var(--success-text); white-space: nowrap; }
  .pw-bad { font-size: 12px; color: var(--error-text); white-space: nowrap; }
  /* The tick / cross in front of the verdict, in the verdict's own colour. */
  .pw-ok svg, .pw-bad svg { vertical-align: -1px; margin-right: 4px; }
  .pw-note { font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  /* Right-click partition menu */
  .menu-backdrop { position: fixed; inset: 0; z-index: 190; }
  .ctx-menu { position: fixed; z-index: 200; min-width: 180px; background: var(--card-bg); border: 1px solid var(--border); border-radius: 8px; box-shadow: var(--shadow-lg); padding: 6px; }
  .ctx-title { font-size: 11px; font-weight: 700; text-transform: uppercase; color: var(--text-muted); padding: 4px 10px 6px; }
  .ctx-menu button { display: block; width: 100%; text-align: left; padding: 8px 10px; border: none; background: none; border-radius: 4px; font-size: 13px; font-family: var(--font); color: var(--text); cursor: pointer; }
  .ctx-menu button:hover { background: var(--accent-soft); color: var(--accent-text); }
  .ctx-menu button.danger { border-top: 1px solid var(--border); border-radius: 0 0 4px 4px; margin-top: 4px; padding-top: 10px; }
  .ctx-menu button.danger:hover { background: var(--error-soft); color: var(--error-text); }
  /* Create-partition form */
  .fld { display: block; margin-bottom: 12px; font-size: 13px; color: var(--head); }
  .fld > span:first-child { display: block; margin-bottom: 5px; font-weight: 600; }
  .fld input, .fld select { padding: 8px 10px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 13px; font-family: var(--font); background: var(--input-bg); color: var(--text); }
  .fld input::placeholder { color: var(--text-muted); }
  .fld input:focus, .fld select:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
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
  .dialog.wide { width: 620px; max-height: 84vh; overflow-y: auto; }
  .ops { margin: 0 0 12px 18px; padding: 0; font-size: 13px; color: var(--head); line-height: 1.7; }
  .ops-note { font-size: 12px; color: var(--text-muted); }
  /* The boot-menu question: the list is the point of the dialog, so it gets
     the room, and each line reads name-first the way the firmware shows it. */
  .boot-src { font-size: 12px; color: var(--text-muted); margin: 0 0 12px; }
  .boot-list { list-style: none; margin: 0 0 12px; padding: 0; max-height: 260px; overflow: auto; }
  .boot-list li { padding: 7px 10px; border: 1px solid var(--border); border-radius: 5px; margin-bottom: 6px; font-size: 13px; color: var(--head); }
  .boot-name { font-size: 11px; color: var(--text-muted); margin-left: 8px; }
  .boot-file { font-size: 11px; color: var(--text-muted); margin-top: 3px; word-break: break-all; }
  .dialog-title.sub { margin-top: 16px; font-size: 13px; }
  .finding-line { display: flex; align-items: center; gap: 10px; font-size: 13px; color: var(--text-muted); padding: 6px 0 12px; }
  /* Deliberately not `.chk`: that name is a checkbox row in this file, and it
     carries white-space: nowrap — which turned every finding into one long
     line running off the dialog. */
  .finding { font-size: 13px; line-height: 1.5; padding: 9px 12px; border-radius: 8px; margin-bottom: 8px; overflow-wrap: anywhere; }
  .finding-note { font-size: 12px; opacity: .8; margin-top: 4px; }
  .finding.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
  .finding.warn { background: var(--warning-soft); border: 1px solid var(--warning-line); color: var(--warning-text); }
  .finding.info { background: var(--info-soft); border: 1px solid var(--info-line); color: var(--info-text); }
  .finding.ok { background: var(--success-soft); border: 1px solid var(--success-line); color: var(--success-text); }
  .opt { display: flex; align-items: flex-start; gap: 9px; font-size: 13px; color: var(--head); margin: 4px 0 10px; cursor: pointer; }
  .opt input { width: auto; margin-top: 2px; flex: 0 0 auto; }
  .opt em { display: block; font-style: normal; font-size: 12px; color: var(--text-muted); line-height: 1.45; margin-top: 2px; }
  .btn.primary-solid { background: var(--primary); color: #fff; }
  .btn.primary-solid:hover:not(:disabled) { background: var(--primary-hover); }
  .fit-msg { margin-top: 10px; padding: 9px 14px; font-size: 13px; border-radius: 8px; background: var(--accent-soft); color: var(--text); }
  .fit-msg.bad { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); font-weight: 600; }
  .progress-wrap { margin-top: 30px; }
  .progress-info { display: flex; justify-content: space-between; font-size: 13px; color: var(--head); margin-bottom: 8px; }
  .progress-info > span:first-child { font-weight: 600; }
  .progress-info > span:last-child:not(:first-child) { color: var(--text-secondary); }
  .bar { height: 8px; border-radius: 4px; background: var(--track); overflow: hidden; }
  .bar-fill { height: 100%; border-radius: 4px; background: var(--primary); transition: width .15s linear; }
  .opt-cost { color: var(--text-muted); font-weight: 400; }
  /* Plan execution */
  .run { border: 1px solid var(--border); border-radius: 8px; background: var(--card-bg); padding: 18px 20px; }
  .run-head { display: flex; align-items: baseline; justify-content: space-between; gap: 14px; margin-bottom: 12px; }
  .run-title { font-size: 15px; font-weight: 600; color: var(--head); }
  .run-sub { font-size: 12px; color: var(--text-muted); }
  .ops-run { list-style: none; margin: 0; padding: 0; }
  .ops-run .op { display: flex; align-items: flex-start; gap: 10px; padding: 7px 0; border-top: 1px solid var(--border); font-size: 13px; }
  .ops-run .op:first-child { border-top: none; }
  .op-mark { flex: 0 0 16px; text-align: center; color: var(--text-muted); }
  .op-mark svg { vertical-align: -1px; }
  .op.done .op-mark { color: var(--success); }
  .op.failed .op-mark { color: var(--error); }
  .op.running .op-mark { color: var(--accent-text); }
  .op-body { flex: 1; min-width: 0; }
  .op-title { display: block; color: var(--head); line-height: 1.45; }
  .op.pending .op-title, .op.skipped .op-title { color: var(--text-muted); }
  .op-msg { display: block; font-size: 12px; color: var(--text-muted); margin-top: 3px; line-height: 1.45; }
  .op.failed .op-msg { color: var(--error-text); }
  .op-bar { display: block; height: 4px; border-radius: 2px; background: var(--track); margin-top: 6px; overflow: hidden; }
  .op-fill { display: block; height: 100%; background: var(--primary); transition: width .2s; }
  .op-state { flex: 0 0 auto; font-size: 12px; color: var(--text-muted); white-space: nowrap; }
  .run-followups { margin-top: 14px; }
  .run-fu-head { font-size: 13px; font-weight: 600; color: var(--head); margin-bottom: 8px; }
  .progress-meta { display: flex; justify-content: space-between; gap: 16px; margin-top: 8px; font-size: 12px; color: var(--text-muted); }
  .progress-meta .route { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1 1 auto; min-width: 0; }
  .banner { margin-top: 22px; border-radius: 8px; padding: 13px 16px; display: flex; align-items: center; gap: 12px; background: var(--accent-soft); font-size: 13px; color: var(--text); }
  .banner.err { background: var(--error-soft); border: 1px solid var(--error-line); color: var(--error-text); }
  .bdot { width: 18px; height: 18px; border-radius: 50%; background: var(--success); flex-shrink: 0; display: flex; align-items: center; justify-content: center; }
  .bicon { flex-shrink: 0; }
  /* .footer is global (styles/global.css) — full-width bar, same on every page. */
  .hint { flex: 1; font-size: 13px; color: var(--text-secondary); }
  .btn { border-radius: 6px; padding: 10px 24px; font-size: 14px; font-weight: 600; font-family: var(--font); cursor: pointer; border: 1px solid transparent; transition: background-color .15s ease; }
  .btn:disabled { opacity: .45; cursor: default; }
  .btn.green { background: var(--green); color: #fff; }
  .btn.green:hover:not(:disabled) { background: var(--green-hover); }
  .btn.danger { background: transparent; border-color: var(--error); color: var(--error-text); }
  .btn.danger:hover:not(:disabled) { background: var(--error-soft); }
  .btn.ghost { background: var(--card-bg); border-color: var(--border-strong); color: var(--text); }
  .btn.ghost:hover:not(:disabled) { background: var(--panel2); }
  .btn.danger-solid { background: var(--error); color: #fff; }
  .btn.danger-solid:hover:not(:disabled) { filter: brightness(.92); }
  .overlay { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: center; justify-content: center; z-index: 150; }
  .dialog { width: 400px; background: var(--card-bg); border: 1px solid var(--border); border-radius: 12px; padding: 24px; box-shadow: var(--shadow-lg); }
  .dialog-title { font-size: 17px; font-weight: 600; color: var(--head); margin-bottom: 12px; }
  .dialog p { font-size: 13px; color: var(--text-secondary); margin-bottom: 10px; line-height: 1.5; }
  /* :global — part of these paragraphs is a translated message ({@html}). */
  .dialog p :global(b) { color: var(--head); }
  /* The point of no return, in the colour of one (design: the red «Отката нет» plate). */
  .dialog p.no-undo, .dialog p.no-undo :global(b) { color: var(--error-text); }
  /* Only the confirm dialog's own text box — a bare `.dialog input` would also
     stretch checkboxes to full width and push their labels out of the dialog. */
  .dialog > input { width: 100%; padding: 9px 12px; border: 1px solid var(--border-strong); border-radius: 6px; font-size: 14px; margin-bottom: 16px; background: var(--input-bg); color: var(--text); }
  .dialog > input:focus { outline: none; border-color: var(--primary); box-shadow: 0 0 0 3px var(--accent-soft); }
  .dialog-actions { display: flex; justify-content: flex-end; gap: 10px; }
</style>
