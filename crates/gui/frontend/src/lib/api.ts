import { invoke } from '@tauri-apps/api/core';
import type {
  DiskInfo,
  RestoreSummary,
  VerifySummary,
  ImageInfo,
  Job,
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
  DirEntry,
  DirListing,
  ServiceStatus,
  MountBackendStatus,
} from './types';

export const api = {
  listDisks: () => invoke<DiskInfo[]>('list_disks'),

  // --- built-in file browsing (the boot medium has no shell dialog) ---
  listRoots: () => invoke<DirEntry[]>('list_roots'),
  listDir: (path: string, dirsOnly: boolean, filesExt: string[]) =>
    invoke<DirListing>('list_dir', { path, dirsOnly, filesExt }),
  createDir: (path: string) => invoke<string>('create_dir', { path }),
  driveSignature: () => invoke<number>('drive_signature'),

  // --- job model (phase 6) ---
  listBackupJobs: () => invoke<Job[]>('list_backup_jobs'),
  getBackupJob: (id: string) => invoke<Job | null>('get_backup_job', { id }),
  newJobTemplate: () => invoke<Job>('new_job_template'),
  defaultExclusionPatterns: () =>
    invoke<{ windows: string[]; linux: string[] }>('default_exclusion_patterns'),
  saveBackupJob: (job: Job) => invoke<Job>('save_backup_job', { job }),
  deleteBackupJob: (id: string, deleteFiles: boolean) =>
    invoke<void>('delete_backup_job', { id, deleteFiles }),
  decryptVolume: (volume: string, password: string, remember: boolean) =>
    invoke<DecryptResult>('decrypt_volume', { volume, password, remember }),
  runBackupJob: (id: string) => invoke<void>('run_backup_job', { id }),
  cancelQueuedJob: (id: string) => invoke<void>('cancel_queued_job', { id }),
  cancelRunningBackup: () => invoke<void>('cancel_running_backup'),
  getRunQueue: () => invoke<QueueSnapshot>('get_run_queue'),
  listActivity: () => invoke<JobRun[]>('list_activity'),

  // --- engine ops (Restore / Tools) ---
  restoreNow: (imagePath: string, target: string) =>
    invoke<RestoreSummary>('restore_now', { imagePath, target }),
  verifyNow: (imagePath: string) => invoke<VerifySummary>('verify_now', { imagePath }),
  mountBackup: (imagePath: string, readWrite?: boolean) => invoke<string>('mount_backup', { imagePath, readWrite }),
  unmountBackup: (imagePath: string) => invoke<void>('unmount_backup', { imagePath }),
  readImageInfo: (imagePath: string) => invoke<ImageInfo>('read_image_info', { imagePath }),
  listDiskBackups: (folder: string) => invoke<DiskBackup[]>('list_disk_backups', { folder }),
  listImageBackups: (folder: string) => invoke<ImageInfo[]>('list_image_backups', { folder }),
  restoreDiskNow: (manifestPath: string, targetDisk: number, force: boolean) =>
    invoke<DiskRestoreSummary>('restore_disk_now', { manifestPath, targetDisk, force }),
  cancelEngineOp: () => invoke<void>('cancel_engine_op'),

  // --- restore plans (partition designer) ---
  // Only the dry run is wired up: it validates and writes nothing. Applying a
  // plan is destructive and stays deliberately unconnected for now.
  dryRunRestorePlan: (plan: RestorePlan, context?: CheckContext) =>
    invoke<DryRunResult>('dry_run_restore_plan', { plan, context }),
  /** Does this password open the container whose header the copy kept?
   *  `false` = wrong password; a rejected promise = the check could not be
   *  made (no stored header, unreadable image). */
  verifyLuksPassword: (imagePath: string, srcIndex: number, password: string) =>
    invoke<boolean>('verify_luks_password', { imagePath, srcIndex, password }),
  /** DESTRUCTIVE. Hand the checked plan to the service. `passphrases` maps a
   *  SOURCE partition index to the password of the container being put back;
   *  they travel with the request, never inside the plan (which is persisted).
   *  Returns immediately — progress arrives as `restore-plan-progress`. */
  applyRestorePlan: (plan: RestorePlan, passphrases?: Record<number, string>) =>
    invoke<RestoreRunState>('apply_restore_plan', { plan, passphrases }),
  /** The restore in progress, or the last one that finished. Survives closing
   *  and reopening the window, and a service restart. */
  getRestoreState: () => invoke<RestoreRunState | null>('get_restore_state'),
  cancelRestorePlan: (planId: string) =>
    invoke<{ cancelled: boolean; reason: string }>('cancel_restore_plan', { planId }),
  /** The report has been read and closed: `getRestoreState` stops returning it.
   *  The run itself stays in the history and in «Активность». */
  ackRestoreRun: (planId: string) => invoke<void>('ack_restore_run', { planId }),

  // --- Mount prerequisites: AIM driver (Windows) / NBD+NTFS status (Linux) ---
  aimDriverStatus: () => invoke<MountBackendStatus>('aim_driver_status'),
  installAimDriver: () => invoke<MountBackendStatus>('install_aim_driver'),
  uninstallAimDriver: () => invoke<void>('uninstall_aim_driver'),

  // --- misc ---
  getConfig: () => invoke<Record<string, unknown>>('get_config'),
  saveConfig: (config: Record<string, unknown>) => invoke<void>('save_config', { config }),
  serviceHealth: () => invoke<ServiceHealth>('service_health'),
  getServiceStatus: () => invoke<ServiceStatus>('get_service_status'),
};
