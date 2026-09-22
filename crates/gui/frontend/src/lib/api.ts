import { invoke } from '@tauri-apps/api/core';
import type {
  BootStatus,
  BootActionResult,
  DiskInfo,
  RestoreSummary,
  VerifySummary,
  ImageInfo,
  LockedImage,
  Job,
  JobFilesDeleted,
  JobRun,
  QueueSnapshot,
  DecryptResult,
  DiskBackup,
  DiskRestoreSummary,
  RestorePlan,
  CheckContext,
  DryRunResult,
  RestoreRunState,
  ServiceHealth,
  KeyboardLayout,
  DirEntry,
  DirListing,
  BaseImage,
  DriverPackage,
  MediaTool,
  MediaBuildOptions,
  MediaBuildReport,
  ServiceStatus,
  MountBackendStatus,
  ArchiveImageInfo,
  ArchiveSecret,
  BegunArchiveEncryption,
} from './types';

/** Spread a secret into a Tauri call's arguments, and nothing when there is
 *  none — an absent field and an empty string mean different things to the
 *  service, and only the first one means "this copy needs no key". */
function secretArgs(secret?: ArchiveSecret): Record<string, string> {
  const out: Record<string, string> = {};
  if (secret?.password) out.password = secret.password;
  if (secret?.recoveryKey) out.recoveryKey = secret.recoveryKey;
  return out;
}

export const api = {
  listDisks: () => invoke<DiskInfo[]>('list_disks'),

  // --- built-in file browsing (the boot medium has no shell dialog) ---
  listRoots: () => invoke<DirEntry[]>('list_roots'),
  listDir: (path: string, dirsOnly: boolean, filesExt: string[]) =>
    invoke<DirListing>('list_dir', { path, dirsOnly, filesExt }),
  createDir: (path: string) => invoke<string>('create_dir', { path }),

  // --- boot medium ---
  findBaseImages: () => invoke<BaseImage[]>('find_base_images'),
  listDriverPackages: () => invoke<DriverPackage[]>('list_driver_packages'),
  listMediaTools: () => invoke<MediaTool[]>('list_media_tools'),
  buildBootMedia: (options: MediaBuildOptions) =>
    invoke<MediaBuildReport>('build_boot_media', { options }),
  driveSignature: () => invoke<number>('drive_signature'),

  // --- job model (phase 6) ---
  listBackupJobs: () => invoke<Job[]>('list_backup_jobs'),
  getBackupJob: (id: string) => invoke<Job | null>('get_backup_job', { id }),
  newJobTemplate: () => invoke<Job>('new_job_template'),
  defaultExclusionPatterns: () =>
    invoke<{ windows: string[]; linux: string[] }>('default_exclusion_patterns'),
  saveBackupJob: (job: Job) => invoke<Job>('save_backup_job', { job }),
  deleteBackupJob: (id: string, deleteFiles: boolean) =>
    invoke<JobFilesDeleted>('delete_backup_job', { id, deleteFiles }),
  decryptVolume: (volume: string, password: string, remember: boolean) =>
    invoke<DecryptResult>('decrypt_volume', { volume, password, remember }),
  runBackupJob: (id: string) => invoke<void>('run_backup_job', { id }),
  cancelQueuedJob: (id: string) => invoke<void>('cancel_queued_job', { id }),
  cancelRunningBackup: () => invoke<void>('cancel_running_backup'),
  getRunQueue: () => invoke<QueueSnapshot>('get_run_queue'),
  listActivity: () => invoke<JobRun[]>('list_activity'),

  // --- engine ops (Restore / Tools) ---
  // Each takes what the person typed to open an ENCRYPTED copy. A manual
  // operation never borrows the service's own key (§7.4), so these have to
  // carry it; `inspectImage` is how a screen learns to ask in the first place.
  restoreNow: (imagePath: string, target: string, secret?: ArchiveSecret) =>
    invoke<RestoreSummary>('restore_now', { imagePath, target, ...secretArgs(secret) }),
  verifyNow: (imagePath: string, secret?: ArchiveSecret) =>
    invoke<VerifySummary>('verify_now', { imagePath, ...secretArgs(secret) }),
  mountBackup: (imagePath: string, readWrite?: boolean, secret?: ArchiveSecret) =>
    invoke<string>('mount_backup', { imagePath, readWrite, ...secretArgs(secret) }),
  unmountBackup: (imagePath: string) => invoke<void>('unmount_backup', { imagePath }),
  readImageInfo: (imagePath: string, secret?: ArchiveSecret) =>
    invoke<ImageInfo>('read_image_info', { imagePath, ...secretArgs(secret) }),
  /** What a copy is, WITHOUT any key. */
  inspectImage: (imagePath: string) =>
    invoke<ArchiveImageInfo>('inspect_backup_image', { imagePath }),
  listDiskBackups: (folder: string) => invoke<DiskBackup[]>('list_disk_backups', { folder }),
  listImageBackups: (folder: string) => invoke<ImageInfo[]>('list_image_backups', { folder }),
  /** Encrypted copies in a folder, as far as they read WITHOUT a key. */
  listLockedImages: (folder: string) => invoke<LockedImage[]>('list_locked_images', { folder }),
  restoreDiskNow: (
    manifestPath: string,
    targetDisk: number,
    force: boolean,
    secret?: ArchiveSecret,
  ) =>
    invoke<DiskRestoreSummary>('restore_disk_now', {
      manifestPath,
      targetDisk,
      force,
      ...secretArgs(secret),
    }),
  cancelEngineOp: () => invoke<void>('cancel_engine_op'),

  // --- archive encryption (§7.5) ---
  /** Prepare encryption for a job. Writes NOTHING: the recovery key comes back
   *  once, and `confirmArchiveEncryption` commits only after it is typed back.
   *  Slow — the service seals the key with Argon2id, seconds not milliseconds. */
  beginArchiveEncryption: (job: Job, password: string) =>
    invoke<BegunArchiveEncryption>('begin_archive_encryption', { job, password }),
  /** Commit it; returns the stored job. */
  confirmArchiveEncryption: (token: string, recoveryKey: string) =>
    invoke<Job>('confirm_archive_encryption', { token, recoveryKey }),
  /** The operator went back. Nothing was written, so there is nothing to undo. */
  cancelArchiveEncryption: (token: string) =>
    invoke<void>('cancel_archive_encryption', { token }),

  // --- restore plans (partition designer) ---
  // Only the dry run is wired up: it validates and writes nothing. Applying a
  // plan is destructive and stays deliberately unconnected for now.
  dryRunRestorePlan: (plan: RestorePlan, context?: CheckContext, secret?: ArchiveSecret) =>
    invoke<DryRunResult>('dry_run_restore_plan', { plan, context, ...secretArgs(secret) }),
  /** Does this password open the container whose header the copy kept?
   *  `false` = wrong password; a rejected promise = the check could not be
   *  made (no stored header, unreadable image). */
  verifyLuksPassword: (
    imagePath: string,
    srcIndex: number,
    password: string,
    secret?: ArchiveSecret,
  ) =>
    invoke<boolean>('verify_luks_password', {
      imagePath,
      srcIndex,
      password,
      // Named apart from `password`: one opens the source's container, the
      // other opens the copy that stored its header.
      archivePassword: secret?.password,
      archiveRecoveryKey: secret?.recoveryKey,
    }),
  /** DESTRUCTIVE. Hand the checked plan to the service. `passphrases` maps a
   *  SOURCE partition index to the password of the container being put back;
   *  they travel with the request, never inside the plan (which is persisted).
   *  Returns immediately — progress arrives as `restore-plan-progress`. */
  applyRestorePlan: (
    plan: RestorePlan,
    passphrases?: Record<number, string>,
    secret?: ArchiveSecret,
  ) =>
    invoke<RestoreRunState>('apply_restore_plan', {
      plan,
      passphrases,
      ...secretArgs(secret),
    }),
  /** The restore in progress, or the last one that finished. Survives closing
   *  and reopening the window, and a service restart. */
  getRestoreState: () => invoke<RestoreRunState | null>('get_restore_state'),
  cancelRestorePlan: (planId: string) =>
    invoke<{ cancelled: boolean; reason: string }>('cancel_restore_plan', { planId }),
  /** The report has been read and closed: `getRestoreState` stops returning it.
   *  The run itself stays in the history and in «Активность». */
  ackRestoreRun: (planId: string) => invoke<void>('ack_restore_run', { planId }),

  // --- What mounting a backup image needs here (no driver of ours either way:
  // Windows' own iSCSI initiator / the in-tree NBD module on Linux) ---
  // --- как загружается машина (переменные прошивки UEFI) ---
  // Живут в материнской плате, а не на диске: копия разделов их не сохраняет.
  bootStatus: () => invoke<BootStatus>('boot_status'),
  bootAction: (action: Record<string, unknown>) =>
    invoke<BootActionResult>('boot_action', { action }),

  mountBackendStatus: () => invoke<MountBackendStatus>('mount_backend_status'),
  prepareMountBackend: () => invoke<MountBackendStatus>('prepare_mount_backend'),

  // --- misc ---
  getConfig: () => invoke<Record<string, unknown>>('get_config'),
  saveConfig: (config: Record<string, unknown>) => invoke<void>('save_config', { config }),
  serviceHealth: () => invoke<ServiceHealth>('service_health'),
  /** Active keyboard layout; null where it cannot be told (non-Windows). */
  keyboardLayout: () => invoke<KeyboardLayout | null>('keyboard_layout'),
  getServiceStatus: () => invoke<ServiceStatus>('get_service_status'),
  /**
   * «Интерфейс нарисовался» — единственное, что носитель принимает за доказательство.
   *
   * Зовётся один раз при монтировании. Ответить на это может только код,
   * выполняющийся внутри окна: само окно создаётся успешно и с мёртвым
   * движком, пустым и синим.
   */
  guiReady: () => invoke<void>('gui_ready'),
};
