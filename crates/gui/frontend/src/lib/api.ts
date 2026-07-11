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
  ServiceHealth,
  ServiceStatus,
  MountBackendStatus,
} from './types';

export const api = {
  listDisks: () => invoke<DiskInfo[]>('list_disks'),
  driveSignature: () => invoke<number>('drive_signature'),

  // --- job model (phase 6) ---
  listBackupJobs: () => invoke<Job[]>('list_backup_jobs'),
  getBackupJob: (id: string) => invoke<Job | null>('get_backup_job', { id }),
  newJobTemplate: () => invoke<Job>('new_job_template'),
  defaultExclusionPatterns: () =>
    invoke<{ windows: string[]; linux: string[] }>('default_exclusion_patterns'),
  saveBackupJob: (job: Job) => invoke<Job>('save_backup_job', { job }),
  deleteBackupJob: (id: string) => invoke<void>('delete_backup_job', { id }),
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
  restoreDiskNow: (manifestPath: string, targetDisk: number, force: boolean) =>
    invoke<DiskRestoreSummary>('restore_disk_now', { manifestPath, targetDisk, force }),
  cancelEngineOp: () => invoke<void>('cancel_engine_op'),

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
