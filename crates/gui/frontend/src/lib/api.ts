import { invoke } from '@tauri-apps/api/core';
import type {
  DiskInfo,
  BackupJob,
  BackupSet,
  PreflightReport,
  ServiceStatus,
  ValidationResult,
  RetentionPolicy,
  BackupSummary,
  RestoreSummary,
  VerifySummary,
  ImageInfo,
} from './types';

export const api = {
  listDisks: () => invoke<DiskInfo[]>('list_disks'),

  driveSignature: () => invoke<number>('drive_signature'),

  listJobs: () => invoke<BackupJob[]>('list_jobs'),

  createJob: (req: {
    name: string;
    source_type: string;
    source_device: string;
    target_path: string;
    compression_level?: number;
    encryption_enabled?: boolean;
  }) => invoke<BackupJob>('create_job', { req }),

  deleteJob: (id: string) => invoke<void>('delete_job', { id }),

  listSets: (jobId: string) => invoke<BackupSet[]>('list_sets', { jobId }),

  runPreflight: (deviceId: string) =>
    invoke<PreflightReport>('run_preflight', { deviceId }),

  startBackup: (jobId: string, backupType: string, password: string) =>
    invoke<string>('start_backup', { jobId, backupType, password }),

  cancelBackup: (backupId: string) =>
    invoke<void>('cancel_backup', { backupId }),

  startRestore: (
    backupSetId: string,
    targetDevice: string,
    password: string,
  ) =>
    invoke<string>('start_restore', { backupSetId, targetDevice, password }),

  getConfig: () => invoke<Record<string, unknown>>('get_config'),

  saveConfig: (config: Record<string, unknown>) =>
    invoke<void>('save_config', { config }),

  getServiceStatus: () => invoke<ServiceStatus>('get_service_status'),

  validateBackup: (filePath: string) =>
    invoke<ValidationResult>('validate_backup', { filePath }),

  getRetention: (jobId: string) =>
    invoke<RetentionPolicy | null>('get_retention', { jobId }),

  setRetention: (policy: RetentionPolicy) =>
    invoke<void>('set_retention', { policy }),

  getSchedule: (jobId: string) =>
    invoke<{ cron_expression: string } | null>('get_schedule', { jobId }),

  setSchedule: (jobId: string, cronExpression: string) =>
    invoke<void>('set_schedule', { jobId, cronExpression }),

  // --- VHDX engine (direct, plan v2 phase 1) ---

  backupNow: (source: string, outputPath: string, allSectors: boolean) =>
    invoke<BackupSummary>('backup_now', { source, outputPath, allSectors }),

  restoreNow: (imagePath: string, target: string) =>
    invoke<RestoreSummary>('restore_now', { imagePath, target }),

  verifyNow: (imagePath: string) =>
    invoke<VerifySummary>('verify_now', { imagePath }),

  mountBackup: (imagePath: string) =>
    invoke<string>('mount_backup', { imagePath }),

  unmountBackup: (imagePath: string) =>
    invoke<void>('unmount_backup', { imagePath }),

  readImageInfo: (imagePath: string) =>
    invoke<ImageInfo>('read_image_info', { imagePath }),

  cancelEngineOp: () => invoke<void>('cancel_engine_op'),
};
