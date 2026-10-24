export interface DiskInfo {
  device_id: string;
  /** Linux: `/dev/disk/by-id/…`, the name a whole-disk job keeps (it names
   *  the same disk after a reboot, `device_id` may not). Absent on Windows,
   *  for a device-mapper volume and for a disk udev gave no such name. */
  stable_device?: string;
  model: string;
  serial: string;
  size_bytes: number;
  sector_size_logical: number;
  sector_size_physical: number;
  media_type: string;
  partition_table: string;
  disk_guid: string;
  partitions: PartitionInfo[];
}

export interface PartitionInfo {
  index: number;
  offset_bytes: number;
  size_bytes: number;
  /** Bytes in use within the volume (size − free); 0 when unknown/letterless. */
  used_bytes: number;
  gpt_type_guid: string;
  gpt_partition_guid: string;
  filesystem: string;
  volume_letter: string | null;
  /** Openable device: "C:" for lettered, `\\?\Volume{GUID}` for letterless
   *  system/hidden volumes (EFI, Recovery), "" when not backable (MSR, Linux). */
  volume_device: string;
  volume_label: string;
  fs_uuid: string;
  /** This volume runs the system RIGHT NOW (false for everything when booted
   *  from the medium — nothing on a real disk is running there). */
  is_system: boolean;
  is_boot: boolean;
  /** An OS is INSTALLED here, running or not. On the medium this is what
   *  identifies the system partition; `is_system` cannot. */
  has_os: boolean;
  /** Where a Linux filesystem records it was last mounted ("/" = system root,
   *  "/home" = data, "" = unknown). Read from the superblock without mounting;
   *  it is the only signal that tells a Linux root from a Linux data volume. */
  linux_last_mounted: string;
  /** Encrypted container (LUKS/BitLocker): filesystem is Unknown, but it can be
   *  backed up after supplying a password to decrypt it. */
  is_encrypted: boolean;
  /** How an encrypted volume can be opened, in its own metadata's words. */
  unlock_methods?: string[];
  /** Only the 48-digit recovery key can open it (BitLocker, no password
   *  protector). From the protectors' codes — `unlock_methods` are names in
   *  the window's language and decide nothing. */
  recovery_key_only?: boolean;
  /** Can Own Backup open it with something the user can type? */
  unlockable?: boolean;
  /** Linux: the kernel's name for the device (`/dev/nvme0n1p2`, `/dev/dm-0`),
   *  for showing. `volume_device` is then a name that survives a reboot
   *  (`/dev/disk/by-partuuid/…`, `/dev/mapper/…`) and is what a job keeps.
   *  Absent on Windows. */
  kernel_device?: string;
  /** Linux: `crypt` (an opened LUKS/BitLocker container — the filesystem
   *  inside) or `lvm` (a logical volume) for a device-mapper volume, listed
   *  as a disk of its own; absent for a partition. */
  mapper_kind?: string;
  /** Linux: an opened LUKS mapping. Its filesystem reads as it is, but a
   *  snapshot of it while it is in use needs the container's password — the
   *  wizard asks for it as for an encrypted volume. */
  container_password?: boolean;
}

export interface BackupJob {
  id: string;
  name: string;
  source_type: string;
  source_device: string;
  target_path: string;
  schedule: string | null;
  compression_level: number;
  encryption_enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface BackupSet {
  id: string;
  job_id: string;
  backup_type: string;
  parent_set_id: string | null;
  file_path: string;
  file_size: number | null;
  source_size: number;
  block_count: number;
  changed_block_count: number | null;
  started_at: string | null;
  completed_at: string | null;
  status: string;
  checksum: string | null;
  manifest_json: string | null;
}

export interface BackupProgress {
  backup_id: string;
  status: string;
  blocks_done: number;
  blocks_total: number;
  bytes_written: number;
  speed_mbps: number;
  eta: string;
  error_message: string;
}

export interface PreflightReport {
  boot_mode: string;
  partition_table: string;
  disk_type: string;
  filesystem: string;
  support_tier: string;
  vss_service_running: boolean;
  vss_writers_healthy: boolean;
  free_space_on_target: number;
  warnings: string[];
  blockers: string[];
}

export interface RetentionPolicy {
  job_id: string;
  max_full_backups: number | null;
  max_incremental_per_full: number | null;
  max_age_days: number | null;
  min_keep_count: number;
}

export interface ValidationResult {
  valid: boolean;
  message: string;
  blocks_checked: number;
  blocks_total: number;
}

export interface ActiveMount {
  image_path: string;
  physical_path: string;
  read_write: boolean;
  volume_letters?: string[];
}

/// Mount prerequisites status. Windows returns the AIM driver fields; Linux
/// returns the NBD/NTFS fields. `platform` selects which card to render.
// --- как загружается машина ---
// Запись загрузки UEFI живёт в памяти материнской платы и указывает на файл на
// разделе по GUID этого раздела. Файл и запись о нём — две разные вещи в двух
// разных местах: пропажа записи оставляет файл нетронутым и недостижимым.

export interface BootEntry {
  number: string;            // 000F
  name: string;
  in_menu: number | null;    // место в меню, null = прошивка его не предлагает
  active: boolean;
  current: boolean;          // машина загружена отсюда
  next: boolean;             // назначена на один следующий раз
  device_entry: boolean;     // называет устройство, а не файл
  file: string | null;
  partition_guid: string | null;
  partition_number: number | null;
  editable: boolean;         // путь разобран полностью, править можно
}

export interface EspLoader {
  path: string;
  size: number;
  kind: string;
  /** Nothing told what it is (`kind` is the generic word). Absent from a
   *  service older than the flag. */
  generic?: boolean;
  fallback: boolean;         // запасной путь съёмного носителя
  second_stage: boolean;     // его запускает другой загрузчик
  named_by: string | null;   // номер записи, которая на него ссылается
}

export interface Esp {
  /** Windows: the disk number (`PhysicalDriveN`). Linux: the kernel device
   *  (`/dev/nvme0n1`), the inventory's `device_id` for the disk. */
  disk: number | string;
  partition: number;
  guid: string;
  size_bytes: number;
  unreadable: string | null;
  loaders: EspLoader[];
}

export interface BootStatus {
  uefi: boolean;
  message?: string;
  order?: string[];
  current?: string | null;
  next?: string | null;
  timeout_secs?: number | null;
  found_by?: string;
  dangling?: string[];
  /** Where the copy taken before each change goes. Absent from an older
   *  service. */
  safety_copy_dir?: string;
  entries?: BootEntry[];
  esps?: Esp[];
}

export interface BootActionResult {
  message: string;
  safety_copy: string;
  // На загрузочном носителе копия может лечь на временный диск и не пережить
  // перезагрузку — ту самую, ради которой её и снимают.
  safety_copy_durable: boolean;
  created: string | null;
}

export interface MountBackendStatus {
  platform: 'windows' | 'linux';
  /** Windows: the built-in iSCSI initiator, which needs no install — only,
   *  sometimes, a service start. */
  iscsi?: {
    ready: boolean;
    driver_file: boolean;
    service_present: boolean;
    running: boolean;
    disabled: boolean;
    media_mode: boolean;
    message: string;
  };
  /** Linux: the in-tree NBD module and an NTFS backend. */
  linux?: {
    nbd_ok: boolean;
    ntfs_ok: boolean;
    ntfs_backend?: string | null;
    kernel: string;
  };
}

export interface ServiceStatus {
  connected: boolean;
  version: string;
  /** Seconds since the service started (0 when not connected). */
  uptime_secs: number;
  active_jobs: number;
  active_mounts: ActiveMount[];
  /** Running, but refusing this window (`PermissionDenied`). */
  access_denied?: boolean;
}

/** Startup health of the backup service (for the "install/start the service" warning). */
export interface ServiceHealth {
  reachable: boolean;
  installed: boolean;
  /** Running from the boot medium: the service is a plain process there, and
   *  there is nowhere to install it to, so `installed: false` is expected. */
  media: boolean;
  /** Reachable, and refusing this window on every call (`PermissionDenied`):
   *  on Linux, a window outside the active local session (SSH, say). */
  access_denied?: boolean;
  /** Not reachable because the access group keeps this user out (Linux):
   *  not a member, or added after this session began. `ok` otherwise. */
  group_access?: 'ok' | 'not_member' | 'relogin';
}

/** Where the window runs (the `platform_info` command). */
export interface PlatformInfo {
  /** `windows` or `linux`. */
  os: string;
  /** The service channel: the named pipe on Windows, the Unix socket elsewhere. */
  channel: string;
  /** The folder this window logs to, when it could open one. */
  ui_log_dir: string | null;
  /** The service's log folder where it is fixed (Linux); null on Windows. */
  service_log_dir: string | null;
}

/** What the service's backup engine and scheduler carry out
 *  (`own_backup_core::capabilities`, served by the service). Windows:
 *  everything. */
export interface BackupCapabilities {
  os: string;
  /** `full`, `incremental`, `differential` — those a run carries out. A chain
   *  also needs its container in `chain_formats`. */
  methods: string[];
  /** Containers a chain is written in: `vhdx`, `qcow2`, `obk` (encrypted). */
  chain_formats: string[];
  exclusions: boolean;
  /** One image of a whole disk (`disk_source`); without it, one per volume. */
  whole_disk: boolean;
  /** How `disk_source` names the disk: `number` (Windows) or `device`
   *  (Linux: `disk_source.device`, a `/dev/disk/by-id/…` path). */
  disk_named_by: 'number' | 'device';
  speed_limit: boolean;
  low_priority: boolean;
  after_backup: boolean;
  /** Encrypted archives (OBK). */
  archive_encryption: boolean;
  /** What keeps this machine's archive keys: `dpapi` (Windows), `tpm2` or
   *  `host` (Linux, systemd-creds); empty when there is no key store. Absent
   *  from an older service. */
  archive_key_protection?: string;
  /** `schedule.event_trigger` values the scheduler fires. */
  schedule_events: string[];
  wake_to_run: boolean;
  /** The machine does not sleep while a copy runs. */
  keeps_awake: boolean;
}

/** The service's answer to "is this destination on a source?". */
export interface DestinationCheck {
  placement: 'clear' | 'on_source' | 'cannot_tell';
  /** The source it is on (`on_source`). */
  source: string;
  /** Why it could not be told, in the service's words (`cannot_tell`). */
  reason: string;
  /** Linux: sources where it cannot be told whether the service's key
   *  directory lies on them (ZFS, overlayfs). The engine treats them as
   *  holding it: a copy that cannot leave it out is refused. Missing from an
   *  older service. */
  key_store_unknown?: { source: string; reason: string }[];
}

/** A command failure with a kind to act on (`CodedError` in the GUI crate):
 *  `code` is decided on, `message` is only shown. */
export interface CodedError {
  code: 'unimplemented' | 'permission_denied' | 'unavailable' | 'other';
  message: string;
}

/** The keyboard layout the focused window is typing in (boot medium only). */
export interface KeyboardLayout {
  /** Short tag for the indicator: "RU", "EN", … */
  code: string;
  /** Full native name for the tooltip: "русский", "English". */
  name: string;
}

/** What a password field shows beside its eye. Unlike the sidebar strip this
 *  answers with a taskbar on screen too. */
export interface KeyboardState {
  layout: KeyboardLayout | null;
  /** null where it cannot be told. */
  caps_lock: boolean | null;
}

export type ViewName =
  | 'backup'
  | 'restore'
  | 'boot'
  | 'tools'
  | 'activity'
  | 'settings'
  | 'about';

export interface EngineProgress {
  percent: number;
  blocks_done: number;
  blocks_total: number;
  bytes_done: number;
  speed_mbps: number;
}

export interface BackupSummary {
  image_path: string;
  sidecar_path: string;
  source_bytes: number;
  total_blocks: number;
  used_blocks: number;
  bytes_copied: number;
  duration_secs: number;
  smart_mode: boolean;
  vss_used: boolean;
}

export interface RestoreSummary {
  blocks_restored: number;
  bytes_restored: number;
  duration_secs: number;
}

export interface VerifySummary {
  valid: boolean;
  blocks_checked: number;
  mismatched_blocks: number[];
  duration_secs: number;
}

/** One encrypted partition's container, as the copy took it. */
export interface PartEncryption {
  index: number;
  /** 'bitlocker' | 'luks1' | 'luks2' | 'other' */
  kind: string;
  /** 'plaintext' — the copy holds it decrypted (a restore produces an
   *  unencrypted volume); 'sector' — the container went in whole and comes
   *  back encrypted, opened by the same password as before. */
  captured: string;
}

export interface ImageInfo {
  image_path: string;
  /** Job id that produced this image (groups copies of one archive). */
  job_id: string | null;
  image_file_name: string;
  chain_role: string;
  created_at: string;
  source_device: string;
  source_bytes: number;
  block_size: number;
  used_blocks: number;
  total_blocks: number;
  backup_mode: string;
  /** Full physical-disk snapshot embedded in the image (renders the restore
   *  card without jobs.json). null for images written before snapshots. */
  source_disk_layout: DiskInfo | null;
  /** Partition indices this backup contains — the card highlights these. */
  backed_up_parts: number[];
  /** Partitions the copy's layout recreates with no data of their own — the
   *  MSR slot, when the copy carries Windows. Empty for a per-volume image. */
  layout_only_parts: number[];
  /** Filesystem inside each backed-up partition, as the IMAGE records it.
   *  For a LUKS/BitLocker source this is the decrypted filesystem — the copy
   *  holds the volume in the clear, so the disk snapshot's "Unknown +
   *  encrypted" would misdescribe what a restore produces. */
  part_filesystems: { index: number; filesystem: string }[];
  /** Partitions whose container header this copy kept — those can be restored
   *  with their encryption intact, given the password. */
  luks_header_parts: number[];
  /** What each encrypted partition's container was, and how the copy took it. */
  encrypted_parts: PartEncryption[];
  /** File-type breakdown embedded in the image. */
  categories: CategorySizes | null;
  /** Files/bytes excluded from this image (for the "Исключено" legend line). */
  excluded_files: number;
  excluded_bytes: number;
  /** The machine's UEFI boot entries this copy carries, or null when it
   *  carries none (not a bootable copy, a BIOS machine, or a firmware that
   *  would not be read). Shown to the user before any of it is written back —
   *  a list of names is the only way the question means anything. */
  boot_entries: BootEntries | null;
  /** Logical sector size the source GPT was captured with (512 / 4096). */
  sector_size: number;
  /** True for a whole-disk (bootable) backup. */
  is_whole_disk: boolean;
  /** Version of Own Backup that wrote this copy ("" for older copies). */
  app_version: string;
}

// ===== Job model (phase 6) =====

export interface ScheduleConfig {
  kind: 'none' | 'daily' | 'weekly' | 'monthly' | 'event' | 'nonstop';
  days: string[];
  time: string;
  /** Day of month for `monthly`, 1–28 (limited so it exists in every month). */
  day_of_month: number;
  /** For kind=event: 'dest_connected'|'logon'|'logoff'|'shutdown'|'startup'
   *  ('usb_connect' in jobs saved before 2026-09-26 means dest_connected). */
  event_trigger: string;
  event_once_per_day: boolean;
  event_startup_delay_min: number;
  /** Wake the machine from sleep for a daily/weekly/monthly run. */
  wake_to_run: boolean;
  run_missed: boolean;
  /** Minutes after system/service start before catching up missed runs, 1–60. */
  missed_delay_min: number;
}

export interface SchemeConfig {
  method: 'full' | 'incremental' | 'differential';
  only_incremental: boolean;
  full_every_n: number;
  cleanup_older_than_days: number | null;
  keep_chains: number | null;
  keep_first: boolean;
  /** Sliding window: keep at most N increments, folding the oldest into the
   *  base when a new run overflows it. null = off. */
  rolling_incrementals: number | null;
}

export interface ExclusionsConfig {
  enabled: boolean;
  /** Windows-filesystem exclusion masks (NTFS/ReFS/FAT). */
  patterns: string[];
  /** Linux-filesystem exclusion masks (ext/btrfs/xfs). */
  patterns_linux: string[];
}

export interface AdvancedConfig {
  sector_by_sector: boolean;
  validate_after: boolean;
  /** Only the blocks the new copy wrote, not the chain under it. */
  verify_latest_only: boolean;
  /** A failed run is queued again this many times (0 = off)… */
  retry_attempts: number;
  /** …each this many minutes after the previous failure. */
  retry_interval_min: number;
  /** After the run, once nothing else is queued. */
  after_backup: 'none' | 'sleep' | 'shutdown';
  /** Copy at background disk/CPU priority. */
  low_priority: boolean;
  /** Cap on the read speed, MB/s; 0 = none. */
  speed_limit_mbps: number;
}

/** Byte totals per file category — real data from the backup walk. */
export interface CategorySizes {
  images: number;
  video: number;
  audio: number;
  documents: number;
  windows: number;
  linux: number;
  other: number;
}

export interface VerificationConfig {
  schedule: ScheduleConfig;
  all_versions: boolean;
}

export interface JobRun {
  operation?: string;
  verification?: JobVerifySummary | null;
  id: string;
  job_id: string;
  job_name: string;
  started_at: string;
  finished_at: string;
  status: 'success' | 'error' | 'cancelled' | string;
  /** Cancelled because the service stopped (shutdown, restart, update), not by a person. */
  interrupted?: boolean;
  /** The scheduler did not start it: something it needed (a saved volume password) was missing. */
  skipped?: boolean;
  image_path: string;
  source_bytes: number;
  bytes_copied: number;
  used_blocks: number;
  total_blocks: number;
  duration_secs: number;
  smart_mode: boolean;
  vss_used: boolean;
  error: string;
  /** full | incremental | differential; '' in pre-phase-5 runs (= full) */
  backup_kind: string;
  base_image_path: string;
  increment_index: number;
  blocks_unchanged: number;
  excluded_files: number;
  excluded_bytes: number;
  /** null when the walk did not run (exclusions off / non-NTFS / sector mode) */
  categories: CategorySizes | null;
  /** Why that walk did not run; '' when it did. Explains both the missing
   *  breakdown and exclusion rules that were never applied. */
  file_scan_skipped: string;
  /** The rules ran, but the excluded files' catalog entries stayed in the
   *  image — why; '' when they were deleted. Their contents are not in the
   *  copy either way. Absent in runs recorded before 2026-09-28. */
  excluded_entries_kept?: string;
  /** The copied partitions moved or were recreated since the previous run
   *  (Windows Update does this to WinRE) and the job followed them — said
   *  in words; '' or absent when nothing changed. */
  layout_changed?: string;
  /** Other things to know about how the copy was made, one sentence each
   *  (a snapshot store allocated in full, the service's key directory left
   *  out). Absent when there is nothing to say. */
  notes?: string[];
  /** What kind of failure `error` is, decided by the service from the
   *  error's structure: `archive_reauthorize` — authorizing the job again
   *  helps; `key_protection` — the machine's key protection (TPM,
   *  systemd-creds) failed, and authorizing again would not. Absent
   *  otherwise. */
  error_kind?: 'archive_reauthorize' | 'key_protection' | string;
  /** Set by the service while this run is not in its history files (the
   *  write failed and is retried before the next run) — said in words. */
  record_error?: string;
  /** Every image this run produced, one per source volume. `image_path` and
   *  `base_image_path` above describe only the first. Empty in runs recorded
   *  before this field existed. */
  images: RunImage[];
  /** Increments folded into their base after this run (sliding window). */
  consolidated_increments: number;
}

/** One image a run wrote, with the source it is a copy of. */
export interface RunImage {
  source: string;
  image_path: string;
  base_image_path: string;
  kind: string;
}

/** What "delete the job's copies" actually managed to do. */
export interface JobFilesDeleted {
  deleted: string[];
  /** [path, reason] for everything still on disk. */
  kept: [string, string][];
  /** Folders that were searched for this job's images. */
  folders: string[];
  /** The activity journal is full, so a destination the job used long ago may
   *  not have been searched at all. */
  history_truncated: boolean;
}

/** One stored copy of a job, as «Очистить версии» lists it. */
export interface JobVersion {
  image_path: string;
  file_name: string;
  role: 'full' | 'incremental' | 'differential';
  /** vhdx | qcow2 | obk */
  container: string;
  created_at: string;
  /** The image file, plus its external sidecar when it has one. */
  size_bytes: number;
  /** The image this one cannot be read without (its `image_path`). */
  depends_on: string | null;
}

/** A full copy and everything built on it, oldest first. */
export interface JobVersionChain {
  /** The source as the file names spell it: `C`, `DISK0`, … */
  source: string;
  /** The newest chain of its source — the next increment continues it. */
  current: boolean;
  images: JobVersion[];
}

/** What a job has on disk. */
export interface JobVersions {
  chains: JobVersionChain[];
  folders: string[];
  /** [folder, reason] for every folder that could not be read. */
  unreadable: [string, string][];
  history_truncated: boolean;
}

/** `job-verify-progress`: several images checked in one go. */
export interface JobVerifyProgress {
  /** Which image is being read, from zero, and of how many. */
  index: number;
  count: number;
  file_name: string;
  /** Of the whole operation. */
  percent: number;
  blocks_done: number;
  blocks_total: number;
}

export interface JobVerifyImage {
  image_path: string;
  file_name: string;
  valid: boolean;
  blocks_checked: number;
  mismatched_blocks: number;
  /** Why the image could not be read at all; empty when it could. */
  error: string;
}

export interface JobVerifySummary {
  valid: boolean;
  images: JobVerifyImage[];
  duration_secs: number;
}

/** Outcome of «Сохранить как VHDX». */
export interface ExportSummary {
  target_path: string;
  virtual_size: number;
  /** How much of the disk holds data — what the file actually stores. */
  bytes_stored: number;
  duration_secs: number;
}

/** A whole-disk (`.obdm`) backup discovered for restore. */
export interface DiskBackupPart {
  index: number;
  name: string;
  filesystem: string;
  size_bytes: number;
}
export interface DiskBackup {
  manifest_path: string;
  disk_model: string;
  disk_number: number;
  created_at: string;
  total_bytes: number;
  partitions: DiskBackupPart[];
}
export interface DiskRestoreSummary {
  partitions: number;
  bytes_written: number;
  duration_secs: number;
}
export interface DiskRestoreProgress {
  phase: string;
  partition_index: number;
  partition_count: number;
  part_label: string;
  percent: number;
  bytes_written: number;
}

/** One partition of a whole-disk backup: its GPT slot and source device. */
export interface DiskPartSpec {
  part_index: number;
  device: string;
  filesystem: string;
}

/** Whole-disk (bootable) backup metadata attached to a job. */
export interface DiskSourceSpec {
  disk_number: number;
  /** Linux: the disk's `/dev/disk/by-id/…` device (a number names no disk
   *  there). Absent on Windows. */
  device?: string | null;
  disk_model: string;
  parts: DiskPartSpec[];
  /** "Загрузочная копия диска" is on for this disk. Decides whether the
   *  machine's UEFI boot entries travel inside the image — they live in the
   *  motherboard, not on the disk, so a copy of a data disk carries none. */
  bootable: boolean;
}

/** A job's archive key, as the service reports it. Present = this job's copies
 *  are encrypted. Never settable through an ordinary save: it is created by the
 *  two-step below and the service refuses any other route (§7.5). */
export interface ArchiveEncryption {
  portable_key: Record<string, unknown>;
  local_key: Record<string, unknown>;
}

/** The answer to "prepare encryption for this job". The recovery key is in it
 *  ONCE — it is not stored anywhere and cannot be asked for again. */
export interface BegunArchiveEncryption {
  token: string;
  recovery_key: string;
  fingerprint: string;
  job_id: string;
}

/** What a copy says about itself without any key, so a screen can ask for a
 *  password before it offers to open it rather than after it fails. */
export interface ArchiveImageInfo {
  container: string;
  encrypted: boolean;
  job_id: string;
  fingerprint: string;
  role: string;
  created_at: string;
  chain_id: string;
  virtual_size: number;
}

/** An encrypted copy found by scanning a folder, read with NO key.
 *
 *  Everything here comes from the open header and is a claim — enough to name
 *  the file and group it with its chain, never enough to restore from. The
 *  sealed half is read once the password is supplied. */
export interface LockedImage {
  image_path: string;
  file_name: string;
  job_id: string;
  chain_id: string;
  backup_id: string;
  role: string;
  created_at: string;
  /** The file this one continues, when it continues one. */
  parent_file_name: string | null;
  virtual_size: number;
}

/** What the person typed to open an encrypted copy. Both empty for an
 *  unencrypted one. */
export interface ArchiveSecret {
  password?: string;
  recoveryKey?: string;
}

export interface Job {
  id: string;
  name: string;
  /** Primary source volume, e.g. "E:" (used for file naming / back-compat). */
  source: string;
  source_label: string;
  /** All selected source volumes; each becomes its own image in one run. */
  sources: string[];
  source_labels: string[];
  /** At least one selected source is encrypted (needs a password to back up). */
  requires_password: boolean;
  /** Set for a whole-disk (bootable) backup: which disk + how partitions map. */
  disk_source: DiskSourceSpec | null;
  /** Disk-layout snapshot captured at creation, so restore shows the archived
   *  disk instantly without querying live disks. Empty for pre-phase jobs. */
  source_disks: DiskInfo[];
  dest_folder: string;
  /** Image container: vhdx (Windows-native, chains) or qcow2 (zstd-compressed). */
  image_format: 'vhdx' | 'qcow2';
  schedule: ScheduleConfig;
  verification: VerificationConfig;
  last_verification?: JobRun | null;
  invalid_images?: string[];
  verification_blocked?: boolean;
  scheme: SchemeConfig;
  exclusions: ExclusionsConfig;
  advanced: AdvancedConfig;
  created_at: string;
  updated_at: string;
  last_run: JobRun | null;
  /** Set once the job's copies are encrypted. Read-only from here: turning it
   *  on goes through `beginArchiveEncryption` + `confirmArchiveEncryption`, and
   *  there is no route that turns it off — the copies already written would
   *  stay encrypted either way. */
  archive_encryption?: ArchiveEncryption | null;
  /** zstd level (1–19) for this job's qcow2/OBK copies; null follows the
   *  settings' default, read at each run. */
  compression_level?: number | null;
}

/** Result of decrypting an encrypted source in the wizard. */
export interface DecryptResult {
  ok: boolean;
  filesystem: string;
  label: string;
  used_bytes: number;
  /** Where the filesystem inside says it was last mounted (ext only): "/" for a
   *  Linux system root, "/home" or "/mnt/…" for data, "" when unknown. */
  linux_last_mounted: string;
}

export interface QueueSnapshot {
  current: string | null;
  pending: string[];
}

export interface JobProgress {
  job_id: string;
  /**
   * 'prepare'    = exclusion scan (blocks_done counts scanned entries)
   * 'backup'     = copying blocks
   * 'finalize'   = flushing the image to disk / detaching (no granular progress)
   * 'exclusions' = post-copy true-removal / in-image exclusion pass
   *                (blocks_done counts processed entries; minutes on big volumes)
   * 'verify'     = post-backup validation
   */
  phase: 'prepare' | 'backup' | 'finalize' | 'exclusions' | 'verify' | string;
  percent: number;
  blocks_done: number;
  blocks_total: number;
  speed_mbps: number;
  /** 0-based index of the volume being copied in a multi-volume job. */
  volume_index?: number;
  /** Total volumes in this run (1 for single-volume jobs). */
  volume_count?: number;
  /** Label of the current volume, e.g. "C: Windows". */
  volume?: string;
  /** File name of the image being written (shown after the destination). */
  image_file_name?: string;
  /** Seconds without any progress, once the service finds that worth saying
   *  (a backup disk that stopped taking writes). Absent otherwise. */
  stalled_secs?: number;
  /**
   * Разбивка оставшихся файлов по типам — приходит ОДИН раз, сразу после
   * скана, и дальше не меняется. Пары «категория → байты»: сканер видит каждый
   * файл с размером ещё до копирования, так что ждать конца копии незачем.
   */
  categories?: [string, number][];
  /** What THIS run's exclusion rules took out — sent once, with
   *  `categories`. Absent until known. */
  excluded_files?: number;
  excluded_bytes?: number;
}

// ---------------------------------------------------------------------------
// Restore plan (mirrors common::restore_plan / common::restore_check)
// ---------------------------------------------------------------------------

export type PartitionScheme = 'gpt' | 'mbr';

export type PartitionKind =
  | { scheme: 'gpt'; type_guid: string; part_guid: string; attributes: number; name: string }
  | { scheme: 'mbr'; type_byte: number; bootable: boolean; logical: boolean };

export interface PartitionSpec {
  index: number;
  first_lba: number;
  /** Inclusive — the same convention the GPT itself uses. */
  last_lba: number;
  kind: PartitionKind;
}

export interface PartitionTableSpec {
  scheme: PartitionScheme;
  sector_size: number;
  disk_size_bytes: number;
  /** GPT disk GUID or MBR signature; empty means "generate one". */
  disk_id: string;
  entries: PartitionSpec[];
}

/** Only what we can actually create. "Не форматировать" is the absence of a
 *  `format` operation, not a value here. */
export type FormatFs = 'ntfs' | 'fat32' | 'exfat' | 'refs' | 'ext4' | 'xfs' | 'btrfs';
export type RestoreMode = 'sectors' | 'files';

export interface PartRef {
  index: number;
  offset: number;
  size: number;
}

/** The machine's boot menu, as a copy holds it. */
export interface BootEntries {
  /** Which machine it came off — the entries name partitions by GUID, so a
   *  copy taken elsewhere restores menu items that point at nothing here. */
  machine: string;
  /** The machine this is being read on, so the screen can say whether the two
   *  are the same. Empty when the name is not available. */
  this_machine: string;
  taken_at: string;
  /** How many firmware variables would actually be written. */
  variables: number;
  entries: BootEntry[];
}

export interface BootEntry {
  /** `Boot0000`, `Boot000F`, … */
  name: string;
  /** "Windows 11", "Kubuntu 24.04" — what the firmware shows. */
  description: string;
  /** The loader it starts, when the entry names one. */
  file: string | null;
}

export type RestoreOp =
  | { op: 'write_partition_table'; table: PartitionTableSpec }
  | { op: 'format'; part: PartRef; fs: FormatFs; label: string; quick: boolean }
  | {
      op: 'restore_partition';
      src_index: number;
      dst: PartRef;
      mode: RestoreMode;
      wipe_free_space: boolean;
      /** Put the encrypted container back instead of leaving a bare
       *  filesystem. The password travels with the apply request, never in
       *  the plan — the plan is persisted. */
      reencrypt: boolean;
    }
  | { op: 'extend_filesystem'; part: PartRef }
  /** Added by the service on Linux, never by the window. */
  | { op: 'change_identities'; parts: PartRef[]; fix_references: boolean }
  | { op: 'assign_letter'; part: PartRef; letter: string }
  | { op: 'finalize_boot'; windows_part: PartRef; esp: PartRef | null; uefi: boolean }
  /** Copy the bootloader to `\EFI\BOOT\BOOTX64.EFI` — the only path UEFI tries
   *  without an NVRAM entry, and NVRAM does not travel with a copy. */
  | { op: 'ensure_fallback_bootloader'; esp: PartRef }
  /** Put the machine's UEFI boot entries back, from the copy's own record of
   *  them. Writes the motherboard's NVRAM, not the disk — so it is only ever in
   *  a plan when the copy carries entries, the target disk is marked bootable,
   *  and the user has been shown the list and said yes. */
  | { op: 'restore_boot_entries' }
  /** Make Windows and the app re-read the disk that was just written, instead
   *  of going on describing the layout it had before the restore. */
  | { op: 'refresh_disk_state' };

export interface RestorePlan {
  id: string;
  target_disk: number;
  /** Linux: the target disk's device. Linux disks have no stable number, so
   *  there `target_disk` names nothing; Windows ignores this. */
  target_device?: string;
  image_path: string;
  verify_after: boolean;
  ops: RestoreOp[];
  /** Partitions of the target disk the person deleted in the designer, as
   *  they are now; any other the new table drops is refused by the service. */
  removed_parts?: { offset_bytes: number; size_bytes: number }[];
}

export type CheckLevel = 'info' | 'warning' | 'error';

export interface CheckResult {
  level: CheckLevel;
  /** Stable slug, so behaviour can key off a finding without matching prose. */
  code: string;
  message: string;
  part_index: number | null;
}

export interface LetterChange {
  part_index: number;
  requested: string;
  assigned: string | null;
}

/** Facts the pure checks cannot discover for themselves. The service fills in
 *  anything left empty — it can see the machine, the GUI cannot. */
export interface CheckContext {
  source_sector_size: number;
  sources: {
    index: number;
    size_bytes: number;
    used_bytes: number;
    filesystem: string;
    label: string;
    /** The source volume was a LUKS/BitLocker container and the copy holds it
     *  decrypted — the restore produces a plain filesystem. */
    encrypted_at_source: boolean;
    /** The copy kept the container's header, so the encryption can be put
     *  back given the password. */
    luks_header_available: boolean;
  }[];
  taken_letters: string[];
  /** The target disk hosts the running Windows. Not a veto on its own — the
   *  service fills in which partitions actually cannot be touched. */
  target_is_system_disk: boolean;
  /** Partitions the running system stands on. Filled by the service. */
  protected_parts: number[];
  /** The target disk's partitions as they are now, so the checks can say what
   *  the restore is about to overwrite. */
  targets: {
    index: number;
    /** Where it starts on the disk. A plan with no partition table has nothing
     *  else to check an operation's raw byte offset against. */
    offset_bytes: number;
    size_bytes: number;
    used_bytes: number;
    filesystem: string;
    label: string;
  }[];
  /** Filesystems chosen in the "create partition" dialog, so the checks can
   *  say when one of them will not be created after all. */
  requested_formats: { index: number; fs: FormatFs }[];
}

export interface DryRunResult {
  /** The plan **as checked**: the dry run resolves drive-letter conflicts by
   *  editing it, so this is the one to apply — not the one that was sent. */
  plan: RestorePlan;
  results: CheckResult[];
  letter_changes: LetterChange[];
  /** Linux: identities the restore would duplicate on this machine. Shown as
   *  warnings in `results`; applying after they were shown keeps them. */
  identity_clashes?: unknown[];
}

// ---------------------------------------------------------------------------
// Restore run state (mirrors common::restore_state)
// ---------------------------------------------------------------------------

export type RunStatus = 'running' | 'completed' | 'failed' | 'cancelled';
export type OpStatus = 'pending' | 'running' | 'done' | 'failed' | 'skipped';

export interface OpRecord {
  index: number;
  /** Stable slug: write_partition_table, restore_sectors, format, … */
  kind: string;
  title: string;
  part_index: number | null;
  status: OpStatus;
  percent: number;
  bytes_done: number;
  bytes_total: number;
  message: string;
  cancellable: boolean;
  /** From here on there is no going back. */
  destroys_data: boolean;
  /** Housekeeping the user did not ask for: it runs, and speaks up only if it
   *  changed something. Kept out of the queue. */
  hidden?: boolean;
}

export interface RestoreRunState {
  plan_id: string;
  target_disk: number;
  started_at: string;
  finished_at: string | null;
  status: RunStatus;
  ops: OpRecord[];
  current: number | null;
  /** Whether a cancel request would be honoured right now. */
  cancellable: boolean;
  /** Things the restore could not do and the user must (fix crypttab, rerun
   *  bcdboot, …). */
  follow_ups: string[];
  /** The user read and closed the report on the restore page. */
  acknowledged?: boolean;
  /** File name of the copy restored ('' in runs recorded before 2026-09-26). */
  image_file?: string;
  /** Linux: identities the run gave the restored copy, when the person chose
   *  to change the duplicates. */
  identity_changes?: IdentityChange[];
  /** Linux: files of the restored system edited to name the new identities,
   *  as `<device>:<path>`. */
  edited_files?: string[];
}

export interface IdentityChange {
  kind: string;
  old: string;
  new: string;
  device: string;
  part_index?: number | null;
}

/** One row in the built-in file browser (see commands/files.rs). */
export interface DirEntry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number | null;
  /** Volume label — drive rows only, empty elsewhere. */
  label: string;
  /** Filesystem of the drive, shown when it has no label. */
  fs: string;
}

export interface DirListing {
  path: string;
  parent: string | null;
  entries: DirEntry[];
}

// --- boot medium ---------------------------------------------------------

/** A base image the medium can be built on, already read and vouched for. */
export interface BaseImage {
  path: string;
  /** Where it was found, in words. */
  origin: string;
  name: string;
  build: number;
  language: string;
  architecture: string;
  total_bytes: number;
  /** Reasons to hesitate, not refusals. */
  warnings: string[];
}

/** One driver package on this machine, as offered for selection. */
export interface DriverPackage {
  inf: string;
  original_name: string;
  class_name: string;
  provider: string;
  version: string;
  size_bytes: number;
  /** Bound to a present device of a class the medium needs, on a bus that
   *  class can really live on. */
  recommended: boolean;
  /** What it is, in words: "контроллер дисков", "сетевой адаптер". */
  role: string;
  /** The device's own name, which is what the user recognises. */
  device: string;
  /** Not a package: a present disk/network device on a driver built into
   *  Windows. Shown, not chosen; the build checks the image has it. */
  inbox: boolean;
}

/** A third-party program on this machine the medium could carry. Copied from
 *  the user's own installation and never distributed by us. */
export interface MediaTool {
  id: string;
  name: string;
  /** Where it was found, so the user can see WHICH installation it is. */
  source: string;
  size_bytes: number;
  files: number;
}

export interface MediaBuildOptions {
  base_wim: string;
  output_iso: string;
  /** Published names of the driver packages to include; empty means none. */
  drivers: string[];
  extra_drivers: string;
  /** Ids of the third-party tools to copy in; empty means none. */
  tools: string[];
  /** Filled in by the Rust side: whose settings the tools travel with. */
  user_appdata?: string;
  /** Leave cmd as the medium's shell instead of our own desktop. */
  classic_console: boolean;
  /** Compress WebView2 into boot.wim: smaller ISO, more RAM at boot. */
  runtime_in_image: boolean;
}

export interface MediaStage {
  id: string;
  title: string;
}

export interface MediaBuildReport {
  iso_path: string;
  iso_bytes: number;
  base_name: string;
  base_build: number;
  base_language: string;
  unpacked: boolean;
  drivers_added: number;
  tools_added: number;
  tools_bytes: number;
  runtime_files_trimmed: number;
  temp_bytes_freed: number;
  warnings: string[];
}

/** A disk the Linux medium may be written onto (`MediaStickDto`). */
export interface MediaStick {
  device: string;
  model: string;
  size_bytes: number;
  /** `usb`, `mmc` or `removable`. */
  transport: string;
  /** Mount points now; a mounted stick is not written. */
  mounted: string[];
  /** Serial number from sysfs; may be empty. */
  serial: string;
  /** Device number `major:minor`. */
  dev: string;
}

/** Which stick a write means, as listed (`MediaStickIdentityDto`). */
export interface MediaStickIdentity {
  model: string;
  serial: string;
  size_bytes: number;
  dev: string;
}

export interface MediaStickWrite {
  iso_path: string;
  device: string;
  /** The service writes only if the opened device is still this stick. */
  expect: MediaStickIdentity;
}

export interface MediaStickReport {
  device: string;
  iso_bytes: number;
  volume_id: string;
  /** The log partition's device, when one was made. */
  log_partition: string | null;
  /** Why there is none: the stick boots regardless. */
  log_skipped: string | null;
}

/** Build the Linux medium (`LinuxMediaBuildDto`). */
export interface LinuxMediaBuild {
  /** An existing folder of the person's own. */
  output_dir: string;
  /** Also write it onto this stick; empty for none. */
  stick_device: string;
  stick_expect: MediaStickIdentity | null;
  /** Development only: a .deb instead of the installed package. */
  deb_path: string;
}

export interface LinuxMediaReport {
  iso_path: string;
  iso_bytes: number;
  version: string;
  from_installed: boolean;
  warnings: string[];
  stick: MediaStickReport | null;
  cache_dir: string;
  cache_bytes: number;
}

/** The ready Linux medium of the installed version on GitHub. */
export interface LinuxMediaRelease {
  version: string;
  tag: string;
  size: number;
}

/** A ready Linux medium downloaded from GitHub. */
export interface LinuxMediaDownload {
  iso_path: string;
  iso_bytes: number;
  version: string;
  tag: string;
}

/** The `linux-media-download` event. */
export interface LinuxMediaDownloadProgress {
  stage: 'download' | 'verify';
  received: number;
  total: number;
}

/** Can the Linux medium be built now (`LinuxMediaStatusDto`). */
export interface LinuxMediaStatus {
  /** Empty when the package is not installed. */
  installed_version: string;
  cache_dir: string;
  cache_bytes: number;
  free_bytes: number;
  need_bytes: number;
  /** What stops a build now, in the person's words. */
  problems: string[];
}

export interface MediaProgress {
  /** The whole queue, sent with every event so the two sides cannot drift. */
  stages: MediaStage[];
  stage_index: number;
  /** 0–100 within the current stage. */
  percent: number;
  detail: string;
  finished: boolean;
}

/** A hidden system partition — EFI, recovery, vendor tools — that Windows
 *  keeps without a drive letter (`HiddenVolume` in common/disk.rs). */
export interface HiddenVolume {
  /** `S:` with a letter, else `\?\Volume{GUID}`: what the letter calls take. */
  device: string;
  letter: string | null;
  kind: 'esp' | 'winre' | 'other';
  disk_number: number;
  disk_model: string;
  part_index: number;
  size_bytes: number;
  filesystem: string;
  label: string;
}
