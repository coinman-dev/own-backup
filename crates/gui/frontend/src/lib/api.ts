import { invoke } from '@tauri-apps/api/core';
import type {
  BootStatus,
  BootActionResult,
  DiskInfo,
  HiddenVolume,
  RestoreSummary,
  VerifySummary,
  ImageInfo,
  LockedImage,
  Job,
  JobFilesDeleted,
  JobRun,
  JobVersions,
  JobVerifySummary,
  ExportSummary,
  QueueSnapshot,
  DecryptResult,
  DiskBackup,
  DiskRestoreSummary,
  RestorePlan,
  CheckContext,
  DryRunResult,
  RestoreRunState,
  ServiceHealth,
  PlatformInfo,
  BackupCapabilities,
  DestinationCheck,
  KeyboardLayout,
  KeyboardState,
  DirEntry,
  DirListing,
  BaseImage,
  DriverPackage,
  MediaTool,
  MediaBuildOptions,
  MediaBuildReport,
  MediaStick,
  MediaStickReport,
  MediaStickWrite,
  LinuxMediaBuild,
  LinuxMediaReport,
  LinuxMediaStatus,
  LinuxMediaRelease,
  CleanupCatalog,
  CleanupFilesPage,
  CleanupOptions,
  CleanupReport,
  LinuxMediaDownload,
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
  inspectBaseImage: (path: string) => invoke<BaseImage>('inspect_base_image', { path }),
  listDriverPackages: () => invoke<DriverPackage[]>('list_driver_packages'),
  listMediaTools: () => invoke<MediaTool[]>('list_media_tools'),
  buildBootMedia: (options: MediaBuildOptions) =>
    invoke<MediaBuildReport>('build_boot_media', { options }),
  // Linux: writing the medium's ISO onto a stick (erases it).
  listMediaSticks: () => invoke<MediaStick[]>('list_media_sticks'),
  writeMediaStick: (options: MediaStickWrite) =>
    invoke<MediaStickReport>('write_media_stick', { options }),
  // Linux: the medium built by the service from the installed package.
  linuxMediaStatus: () => invoke<LinuxMediaStatus>('linux_media_status'),
  /** «Очистка» (Windows): the catalogue, an analysis, the cleanup, its cancel.
   *  Progress comes on `cleanup-progress`. */
  cleanupCatalog: () => invoke<CleanupCatalog>('cleanup', { op: { op: 'catalog' } }),
  cleanupAnalyze: (items: string[], users: string[], options: CleanupOptions) =>
    invoke<CleanupReport>('cleanup', { op: { op: 'analyze', items, users, options } }),
  cleanupRun: (items: string[], users: string[], options: CleanupOptions) =>
    invoke<CleanupReport>('cleanup', { op: { op: 'run', items, users, options } }),
  cleanupCancel: () => invoke<unknown>('cleanup', { op: { op: 'cancel' } }),
  cleanupFiles: (item: string, offset: number, limit: number, sort: 'size' | 'path', filter: string) =>
    invoke<CleanupFilesPage>('cleanup', { op: { op: 'files', item, offset, limit, sort, filter } }),
  buildLinuxMedia: (options: LinuxMediaBuild) =>
    invoke<LinuxMediaReport>('build_linux_media', { options }),
  // Linux: the ready medium of the installed version from GitHub, downloaded
  // by the window itself (no service, no password).
  linuxMediaRelease: () => invoke<LinuxMediaRelease | null>('linux_media_release'),
  downloadLinuxMedia: (outputDir: string) =>
    invoke<LinuxMediaDownload>('download_linux_media', { outputDir }),
  cancelLinuxMediaDownload: () => invoke<void>('cancel_linux_media_download'),
  defaultMediaFolder: () => invoke<string>('default_media_folder'),
  driveSignature: () => invoke<number>('drive_signature'),
  // EFI, recovery and vendor partitions without a letter (the service decides
  // which), and giving one a letter or taking it away (`letter: null`).
  listHiddenVolumes: () => invoke<HiddenVolume[]>('list_hidden_volumes'),
  setVolumeLetter: (device: string, letter: string | null) =>
    invoke<HiddenVolume[]>('set_volume_letter', { device, letter }),

  // --- job model (phase 6) ---
  listBackupJobs: () => invoke<Job[]>('list_backup_jobs'),
  getBackupJob: (id: string) => invoke<Job | null>('get_backup_job', { id }),
  newJobTemplate: () => invoke<Job>('new_job_template'),
  defaultExclusionPatterns: () =>
    invoke<{ windows: string[]; linux: string[] }>('default_exclusion_patterns'),
  /** What the service's engine carries out; the wizard offers only that. */
  backupCapabilities: () => invoke<BackupCapabilities>('backup_capabilities'),
  /** Where a destination folder sits relative to these sources — the check the
   *  save makes, asked while the folder is being chosen. */
  /** Linux: the mount point of the drive the folder is on, the one the
   *  «destination connected» trigger waits for; null on the system disk, for
   *  a drive not mounted now, and on Windows. */
  destinationMount: (folder: string) => invoke<string | null>('destination_mount', { folder }),
  checkDestination: (sources: string[], destFolder: string) =>
    invoke<DestinationCheck>('check_destination', { sources, destFolder }),
  saveBackupJob: (job: Job) => invoke<Job>('save_backup_job', { job }),
  deleteBackupJob: (id: string, deleteFiles: boolean) =>
    invoke<JobFilesDeleted>('delete_backup_job', { id, deleteFiles }),
  // --- the menu of a job: its copies on disk ---
  listJobVersions: (id: string) => invoke<JobVersions>('list_job_versions', { id }),
  /** Delete the ticked copies. What a chain refuses stays and is named. */
  deleteJobVersions: (id: string, images: string[]) =>
    invoke<JobFilesDeleted>('delete_job_versions', { id, images }),
  /** Check the latest copy of each source, or every copy the job has.
   *  Progress arrives as `job-verify-progress`. */
  verifyJob: (id: string, allVersions: boolean, secret?: ArchiveSecret) =>
    invoke<JobVerifySummary>('verify_job', { id, allVersions, ...secretArgs(secret) }),
  /** Save the disk a copy shows as one standalone VHDX. Progress arrives as
   *  `export-progress`. */
  exportImage: (imagePath: string, targetPath: string, secret?: ArchiveSecret) =>
    invoke<ExportSummary>('export_image', { imagePath, targetPath, ...secretArgs(secret) }),
  /** Open a folder in the file manager, as the person at the screen. */
  openFolder: (path: string) => invoke<void>('open_folder', { path }),
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
  /** A refusal about duplicate identities (Linux) rejects with a JSON string
   *  `{identity_clashes, message}`; `identityChoice: 'keep'` mounts anyway. */
  mountBackup: (
    imagePath: string,
    readWrite?: boolean,
    secret?: ArchiveSecret,
    identityChoice?: 'keep',
  ) =>
    invoke<string>('mount_backup', {
      imagePath,
      readWrite,
      ...secretArgs(secret),
      identityChoice,
    }),
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
  reauthorizeArchive: (id: string, secret: ArchiveSecret) =>
    invoke<Job>('reauthorize_archive', { id, ...secretArgs(secret) }),

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
    /** Linux: once the preview showed duplicate identities, what the person
     *  chose with them in front of them — keep them, or change them on the
     *  restored copy. */
    identityChoice?: 'keep' | 'change',
    /** With 'change': also fix the restored system's fstab, crypttab and
     *  boot loader. */
    fixReferences?: boolean,
  ) =>
    invoke<RestoreRunState>('apply_restore_plan', {
      plan,
      passphrases,
      ...secretArgs(secret),
      identityChoice,
      fixReferences,
    }),
  /** The restore in progress, or the last one that finished. Survives closing
   *  and reopening the window, and a service restart. */
  getRestoreState: () => invoke<RestoreRunState | null>('get_restore_state'),
  /** Every restore the service has run, newest first — «Активность». */
  getRestoreHistory: () => invoke<RestoreRunState[]>('get_restore_history'),
  cancelRestorePlan: (planId: string) =>
    invoke<{ cancelled: boolean; reason: string }>('cancel_restore_plan', { planId }),
  /** The report has been read and closed: `getRestoreState` stops returning it.
   *  The run itself stays in the history and in «Активность». */
  ackRestoreRun: (planId: string) => invoke<void>('ack_restore_run', { planId }),

  // --- What mounting a backup image needs here (no driver of ours either way:
  // Windows' own iSCSI initiator / the in-tree NBD module on Linux) ---
  // --- как загружается машина (переменные прошивки UEFI) ---
  // Живут в материнской плате, а не на диске: копия разделов их не сохраняет.
  /** Rejects with a `CodedError`: `unimplemented` where the service does not
   *  read boot entries on its platform (Linux, for now). */
  bootStatus: () => invoke<BootStatus>('boot_status'),
  bootAction: (action: Record<string, unknown>) =>
    invoke<BootActionResult>('boot_action', { action }),

  mountBackendStatus: () => invoke<MountBackendStatus>('mount_backend_status'),
  prepareMountBackend: () => invoke<MountBackendStatus>('prepare_mount_backend'),

  // --- misc ---
  getConfig: () => invoke<Record<string, unknown>>('get_config'),
  saveConfig: (config: Record<string, unknown>) => invoke<void>('save_config', { config }),
  /** The resolved language (`ru`/`en`): the GUI process's own messages and,
   *  through the config, the service's. */
  setUiLanguage: (lang: string) => invoke<void>('set_ui_language', { lang }),
  serviceHealth: () => invoke<ServiceHealth>('service_health'),
  /** Which system the window runs on, its service channel and log folders. */
  platformInfo: () => invoke<PlatformInfo>('platform_info'),
  /** Active keyboard layout; null where it cannot be told (non-Windows). */
  keyboardLayout: () => invoke<KeyboardLayout | null>('keyboard_layout'),
  /** Layout and Caps Lock for a password field, taskbar or not. */
  keyboardState: () => invoke<KeyboardState>('keyboard_state'),
  getServiceStatus: () => invoke<ServiceStatus>('get_service_status'),
  /** The icon beside the clock; null where there is none to offer (the boot
   *  medium, not Windows). Applied at once, kept per Windows user. */
  trayIconEnabled: () => invoke<boolean | null>('tray_icon_enabled'),
  setTrayIconEnabled: (enabled: boolean) => invoke<void>('set_tray_icon_enabled', { enabled }),
  /**
   * «Интерфейс нарисовался» — единственное, что носитель принимает за доказательство.
   *
   * Зовётся один раз при монтировании. Ответить на это может только код,
   * выполняющийся внутри окна: само окно создаётся успешно и с мёртвым
   * движком, пустым и синим.
   */
  guiReady: () => invoke<void>('gui_ready'),
};

/** The kind of a failed command, when it says one (`CodedError`); `null` for a
 *  plain-text failure. Screens branch on this, never on the words. */
export function errorCode(e: unknown): string | null {
  if (e && typeof e === 'object' && 'code' in e && typeof (e as { code: unknown }).code === 'string') {
    return (e as { code: string }).code;
  }
  return null;
}

/** The words of a failed command, whichever shape it came in. */
export function errorText(e: unknown): string {
  if (e && typeof e === 'object' && 'message' in e) {
    return String((e as { message: unknown }).message);
  }
  return String(e);
}
